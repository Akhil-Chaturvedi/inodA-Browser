//! CSS parsing, selector matching, and style computation.
//!
//! Parses CSS text into a `StyleSheet` of rules with pre-parsed `ComplexSelector`
//! ASTs. Matches selectors against DOM elements using pre-computed specificity
//! and in-node parent pointers for complex combinators (`>`, ` `).
//!
//! Property values are parsed into typed `StyleValue` enums at cascade time.
//! Property names are interned as `string_cache::DefaultAtom`. Supports
//! compound selectors, comma-separated lists, CSS inheritance for text
//! properties, and shorthand expansion for `margin`, `padding`, and
//! `background`. Inline `style` attributes are parsed via `cssparser`'s
//! `DeclarationParser` trait.

use cssparser::{
    AtRuleParser, DeclarationParser, ParserState, QualifiedRuleParser, RuleBodyItemParser,
    RuleBodyParser,
};
use cssparser::{Parser, ParserInput, Token};

// ---------------------------------------------------------------------------
// Selector AST -- parsed once, matched many times without string operations.
// ---------------------------------------------------------------------------

/// A single selector component.
#[derive(Debug, Clone, PartialEq)]
pub enum SimpleSelector {
    Tag(String),
    Class(String),
    Id(String),
    PseudoClass(String),
    Universal,
}

/// A compound selector is a sequence of simple selectors that all apply to
/// the same element (e.g., `div.card#main` = [Tag("div"), Class("card"), Id("main")]).
#[derive(Debug, Clone)]
pub struct CompoundSelector {
    pub parts: Vec<SimpleSelector>,
    /// Pre-computed specificity: (id_count, class_count, tag_count).
    pub specificity: (u32, u32, u32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Combinator {
    Descendant,
    Child,
}

#[derive(Debug, Clone)]
pub struct ComplexSelector {
    pub last: CompoundSelector,
    pub ancestors: Vec<(Combinator, CompoundSelector)>,
    pub specificity: (u32, u32, u32),
}

#[derive(Debug, Clone)]
pub struct IndexedRule {
    pub selector: ComplexSelector,
    pub declarations: std::rc::Rc<Vec<Declaration>>,
    pub rule_index: usize,
}

#[derive(Debug, Default, Clone)]
pub struct StyleSheet {
    pub by_id: std::collections::HashMap<string_cache::DefaultAtom, Vec<IndexedRule>>,
    pub by_class: std::collections::HashMap<string_cache::DefaultAtom, Vec<IndexedRule>>,
    pub by_tag: std::collections::HashMap<string_cache::DefaultAtom, Vec<IndexedRule>>,
    pub universal: Vec<IndexedRule>,
    pub next_rule_index: usize,
}

impl StyleSheet {
    pub fn add_rule(&mut self, rule: StyleRule) {
        let decls = std::rc::Rc::new(rule.declarations);
        for selector in rule.selectors {
            let indexed = IndexedRule {
                selector: selector.clone(),
                declarations: std::rc::Rc::clone(&decls),
                rule_index: self.next_rule_index,
            };
            self.next_rule_index += 1;

            let mut id_key = None;
            let mut class_key = None;
            let mut tag_key = None;

            for part in &selector.last.parts {
                match part {
                    SimpleSelector::Id(id) => { id_key = Some(id.clone()); }
                    SimpleSelector::Class(c) => { if class_key.is_none() { class_key = Some(c.clone()); } }
                    SimpleSelector::Tag(t) => { tag_key = Some(t.clone()); }
                    _ => {}
                }
            }

            if let Some(id) = id_key {
                self.by_id.entry(string_cache::DefaultAtom::from(id.as_str())).or_default().push(indexed);
            } else if let Some(class) = class_key {
                self.by_class.entry(string_cache::DefaultAtom::from(class.as_str())).or_default().push(indexed);
            } else if let Some(tag) = tag_key {
                self.by_tag.entry(string_cache::DefaultAtom::from(tag.as_str())).or_default().push(indexed);
            } else {
                self.universal.push(indexed);
            }
        }
    }

    pub fn sort_rules(&mut self) {
        let sort_fn = |a: &IndexedRule, b: &IndexedRule| {
            a.selector.specificity.cmp(&b.selector.specificity).then_with(|| a.rule_index.cmp(&b.rule_index))
        };
        for list in self.by_id.values_mut() { list.sort_by(sort_fn); }
        for list in self.by_class.values_mut() { list.sort_by(sort_fn); }
        for list in self.by_tag.values_mut() { list.sort_by(sort_fn); }
        self.universal.sort_by(sort_fn);
    }
}

#[derive(Debug, Clone)]
pub struct StyleRule {
    pub selectors: Vec<ComplexSelector>,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: string_cache::DefaultAtom,
    pub value: crate::dom::StyleValue,
}

#[inline]
fn parse_color(val: &str) -> Option<(u8, u8, u8)> {
    match val {
        "red" => Some((255, 0, 0)),
        "green" => Some((0, 255, 0)),
        "blue" => Some((0, 0, 255)),
        "black" => Some((0, 0, 0)),
        "white" => Some((255, 255, 255)),
        hex if hex.starts_with('#') && hex.len() == 7 => {
            let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
            let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
            let b = u8::from_str_radix(&hex[5..7], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}

pub fn parse_style_value(val: &str) -> crate::dom::StyleValue {
    let trimmed = val.trim();
    if trimmed == "auto" {
        return crate::dom::StyleValue::Auto;
    }
    if let Some(num_str) = trimmed.strip_suffix("px") {
        if let Ok(num) = num_str.parse::<f32>() {
            return crate::dom::StyleValue::LengthPx(num);
        }
    }
    if let Some(num_str) = trimmed.strip_suffix("%") {
        if let Ok(num) = num_str.parse::<f32>() {
            return crate::dom::StyleValue::Percent(num);
        }
    }
    if let Some(num_str) = trimmed.strip_suffix("vw") {
        if let Ok(num) = num_str.parse::<f32>() {
            return crate::dom::StyleValue::ViewportWidth(num);
        }
    }
    if let Some(num_str) = trimmed.strip_suffix("vh") {
        if let Ok(num) = num_str.parse::<f32>() {
            return crate::dom::StyleValue::ViewportHeight(num);
        }
    }
    if let Some(num_str) = trimmed.strip_suffix("rem") {
        if let Ok(num) = num_str.parse::<f32>() {
            return crate::dom::StyleValue::Rem(num);
        }
    }
    if let Some(num_str) = trimmed.strip_suffix("em") {
        if let Ok(num) = num_str.parse::<f32>() {
            return crate::dom::StyleValue::Em(num);
        }
    }
    if let Some(color) = parse_color(trimmed) {
        return crate::dom::StyleValue::Color(color.0, color.1, color.2);
    }
    if let Ok(num) = trimmed.parse::<f32>() {
        return crate::dom::StyleValue::Number(num);
    }
    crate::dom::StyleValue::Keyword(string_cache::DefaultAtom::from(trimmed))
}

// ---------------------------------------------------------------------------
// Selector parsing
// ---------------------------------------------------------------------------

/// Parse a raw selector string like `"div.card, #main"` into a Vec of ComplexSelectors.
fn parse_selector_list(raw: &str) -> Vec<ComplexSelector> {
    raw.split(',')
        .map(|s| parse_complex_selector(s.trim()))
        .filter(|cs| !cs.last.parts.is_empty())
        .collect()
}

fn parse_complex_selector(raw: &str) -> ComplexSelector {
    let mut list: Vec<(Combinator, String)> = Vec::new();
    let mut current = String::new();
    let mut next_combinator = Combinator::Descendant;

    let push_current =
        |list: &mut Vec<(Combinator, String)>, current: &mut String, comb: Combinator| {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                list.push((comb, trimmed.to_string()));
                current.clear();
            }
        };

    for ch in raw.trim().chars() {
        if ch == '>' {
            push_current(&mut list, &mut current, next_combinator.clone());
            next_combinator = Combinator::Child;
            continue;
        }

        if ch.is_whitespace() {
            push_current(&mut list, &mut current, next_combinator.clone());
            if next_combinator != Combinator::Child {
                next_combinator = Combinator::Descendant;
            }
            continue;
        }

        current.push(ch);
    }

    push_current(&mut list, &mut current, next_combinator);

    if list.is_empty() {
        return ComplexSelector {
            last: parse_compound_selector(""),
            ancestors: vec![],
            specificity: (0, 0, 0),
        };
    }

    list[0].0 = Combinator::Descendant; // Dummy

    let mut total_spec = (0, 0, 0);
    let mut parsed_list = Vec::new();

    for (comb, txt) in list {
        let cs = parse_compound_selector(&txt);
        total_spec.0 += cs.specificity.0;
        total_spec.1 += cs.specificity.1;
        total_spec.2 += cs.specificity.2;
        parsed_list.push((comb, cs));
    }

    let (mut last_comb, last) = parsed_list.pop().unwrap();
    let mut ancestors = Vec::new();

    while let Some((next_comb, prev_cs)) = parsed_list.pop() {
        ancestors.push((last_comb, prev_cs));
        last_comb = next_comb;
    }

    ComplexSelector {
        last,
        ancestors,
        specificity: total_spec,
    }
}

/// Parse a single compound selector string like `"div.card#main"`.
fn parse_compound_selector(s: &str) -> CompoundSelector {
    let mut parts = Vec::new();
    let mut spec = (0u32, 0u32, 0u32);
    let mut remaining = s;

    // Leading tag name (no prefix)
    if !remaining.is_empty()
        && !remaining.starts_with('.')
        && !remaining.starts_with('#')
        && !remaining.starts_with(':')
    {
        let end = remaining
            .find(|c| c == '.' || c == '#' || c == ':')
            .unwrap_or(remaining.len());
        let tag = &remaining[..end];
        if tag == "*" {
            parts.push(SimpleSelector::Universal);
        } else if !tag.is_empty() {
            parts.push(SimpleSelector::Tag(tag.to_string()));
            spec.2 += 1;
        }
        remaining = &remaining[end..];
    }

    // Remaining: classes, ids, pseudo-classes
    while !remaining.is_empty() {
        if remaining.starts_with('#') {
            remaining = &remaining[1..];
            let end = remaining
                .find(|c| c == '.' || c == '#' || c == ':')
                .unwrap_or(remaining.len());
            parts.push(SimpleSelector::Id(remaining[..end].to_string()));
            spec.0 += 1;
            remaining = &remaining[end..];
        } else if remaining.starts_with('.') {
            remaining = &remaining[1..];
            let end = remaining
                .find(|c| c == '.' || c == '#' || c == ':')
                .unwrap_or(remaining.len());
            parts.push(SimpleSelector::Class(remaining[..end].to_string()));
            spec.1 += 1;
            remaining = &remaining[end..];
        } else if remaining.starts_with(':') {
            remaining = &remaining[1..];
            let end = remaining
                .find(|c| c == '.' || c == '#' || c == ':')
                .unwrap_or(remaining.len());
            parts.push(SimpleSelector::PseudoClass(remaining[..end].to_string()));
            spec.1 += 1; // pseudo-classes have class-level specificity
            remaining = &remaining[end..];
        } else {
            break;
        }
    }

    CompoundSelector {
        parts,
        specificity: spec,
    }
}

// ---------------------------------------------------------------------------
// Selector matching -- enum comparison, no string parsing.
// ---------------------------------------------------------------------------

/// Match a pre-parsed compound selector against an element's tag name and attributes.
fn match_compound_selector(
    compound: &CompoundSelector,
    tag_name: &string_cache::DefaultAtom,
    attributes: &[(string_cache::DefaultAtom, String)],
    classes: &[string_cache::DefaultAtom],
) -> bool {
    if compound.parts.is_empty() {
        return false;
    }

    let id_attr = attributes
        .iter()
        .find(|(k, _)| &**k == "id")
        .map(|(_, v)| v.as_str());

    for part in &compound.parts {
        match part {
            SimpleSelector::Tag(t) => {
                if t != &**tag_name {
                    return false;
                }
            }
            SimpleSelector::Class(c) => {
                let atom = string_cache::DefaultAtom::from(c.as_str());
                if !classes.contains(&atom) {
                    return false;
                }
            }
            SimpleSelector::Id(id) => {
                if Some(id.as_str()) != id_attr {
                    return false;
                }
            }
            SimpleSelector::PseudoClass(_) => {
                // Pseudo-classes are not matched against DOM state yet.
                // Treat as always-matching for now.
            }
            SimpleSelector::Universal => {
                // Always matches.
            }
        }
    }
    true
}

fn match_ancestors_recursive(
    ancestors: &[(Combinator, CompoundSelector)],
    ancestor_idx: usize,
    current_node_id: crate::dom::NodeId,
    document: &crate::dom::Document,
) -> bool {
    if ancestor_idx == ancestors.len() {
        return true;
    }

    let (comb, compound) = &ancestors[ancestor_idx];
    let mut check_id = document.parent_of(current_node_id);

    while let Some(pid) = check_id {
        if let Some(crate::dom::Node::Element(data)) = document.nodes.get(pid) {
            if match_compound_selector(compound, &data.tag_name, &data.attributes, &data.classes) {
                if match_ancestors_recursive(ancestors, ancestor_idx + 1, pid, document) {
                    return true;
                }
            }
        }
        
        if *comb == Combinator::Child {
            break;
        }

        check_id = document.parent_of(pid);
    }

    false
}

fn match_complex_selector(
    complex: &ComplexSelector,
    node_id: crate::dom::NodeId,
    document: &crate::dom::Document,
) -> bool {
    if let Some(crate::dom::Node::Element(data)) = document.nodes.get(node_id) {
        if !match_compound_selector(&complex.last, &data.tag_name, &data.attributes, &data.classes) {
            return false;
        }
    } else {
        return false;
    }

    match_ancestors_recursive(&complex.ancestors, 0, node_id, document)
}

// ---------------------------------------------------------------------------
// Stylesheet parsing
// ---------------------------------------------------------------------------

pub fn parse_stylesheet(css: &str) -> StyleSheet {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut stylesheet = StyleSheet::default();

    parse_rules_list(&mut parser, &mut stylesheet);
    stylesheet.sort_rules();
    stylesheet
}

pub fn compute_styles(
    document: &crate::dom::Document,
    base_stylesheet: &StyleSheet,
) -> crate::dom::StyledNode {
    let mut combined_sheet = base_stylesheet.clone();

    for style_text in &document.style_texts {
        let mut input = ParserInput::new(style_text);
        let mut parser = Parser::new(&mut input);
        parse_rules_list(&mut parser, &mut combined_sheet);
    }
    
    combined_sheet.sort_rules();

    build_styled_node(
        document,
        document.root_id,
        &combined_sheet,
        &None,
    )
}

#[inline]
fn is_inheritable(property: &string_cache::DefaultAtom) -> bool {
    matches!(
        &**property,
        "color"
            | "font-family"
            | "font-size"
            | "font-weight"
            | "line-height"
            | "text-align"
            | "visibility"
    )
}

fn build_styled_node(
    document: &crate::dom::Document,
    node_id: crate::dom::NodeId,
    stylesheet: &StyleSheet,
    parent_styles: &Option<std::rc::Rc<Vec<(string_cache::DefaultAtom, crate::dom::StyleValue)>>>,
) -> crate::dom::StyledNode {
    let mut new_declarations = Vec::new();
    let mut children_ids = Vec::new();

    if let Some(node) = document.nodes.get(node_id) {
        match node {
            crate::dom::Node::Element(data) => {
                let id_attr = data
                    .attributes
                    .iter()
                    .find(|(k, _)| &**k == "id")
                    .map(|(_, v)| string_cache::DefaultAtom::from(v.as_str()));

                let mut lists: Vec<&[IndexedRule]> = Vec::new();

                if let Some(id) = &id_attr {
                    if let Some(rules) = stylesheet.by_id.get(id) {
                        lists.push(rules.as_slice());
                    }
                }
                for class in &data.classes {
                    if let Some(rules) = stylesheet.by_class.get(class) {
                        lists.push(rules.as_slice());
                    }
                }
                if let Some(rules) = stylesheet.by_tag.get(&data.tag_name) {
                    lists.push(rules.as_slice());
                }
                if !stylesheet.universal.is_empty() {
                    lists.push(stylesheet.universal.as_slice());
                }

                // Linear merge of pre-sorted specificity buckets instead of dynamic sorting.
                while !lists.is_empty() {
                    let mut min_idx = 0;
                    for i in 1..lists.len() {
                        let a = &lists[i][0];
                        let b = &lists[min_idx][0];
                        if a.selector.specificity.cmp(&b.selector.specificity).then_with(|| a.rule_index.cmp(&b.rule_index)) == std::cmp::Ordering::Less {
                            min_idx = i;
                        }
                    }

                    let rule = &lists[min_idx][0];
                    if match_complex_selector(&rule.selector, node_id, document) {
                        for decl in rule.declarations.iter() {
                            if let Some(pos) = new_declarations.iter().position(|(k, _)| k == &decl.name) {
                                new_declarations[pos].1 = decl.value.clone();
                            } else {
                                new_declarations.push((decl.name.clone(), decl.value.clone()));
                            }
                        }
                    }

                    lists[min_idx] = &lists[min_idx][1..];
                    if lists[min_idx].is_empty() {
                        lists.remove(min_idx);
                    }
                }

                if let Some((_, style_attr)) = data.attributes.iter().find(|(k, _)| &**k == "style") {
                    let inline_decls = parse_inline_declarations(style_attr);
                    for decl in &inline_decls {
                        if let Some(pos) = new_declarations.iter().position(|(k, _)| k == &decl.name) {
                            new_declarations[pos].1 = decl.value.clone();
                        } else {
                            new_declarations.push((decl.name.clone(), decl.value.clone()));
                        }
                    }
                }

                let mut child = document.first_child_of(node_id);
                while let Some(c) = child {
                    children_ids.push(c);
                    child = document.next_sibling_of(c);
                }
            }
            crate::dom::Node::Root(_) => {
                let mut child = document.first_child_of(node_id);
                while let Some(c) = child {
                    children_ids.push(c);
                    child = document.next_sibling_of(c);
                }
            }
            crate::dom::Node::Text(_) => {}
        }
    }

    let inherited_styles = if new_declarations.is_empty() {
        parent_styles.clone()
    } else {
        let mut appended_styles = if let Some(parent) = parent_styles {
            parent.iter().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for (k, v) in &new_declarations {
            if is_inheritable(k) {
                if let Some(pos) = appended_styles.iter().position(|(ek, _)| ek == k) {
                    appended_styles[pos].1 = v.clone();
                } else {
                    appended_styles.push((k.clone(), v.clone()));
                }
            }
        }
        if appended_styles.is_empty() { None } else { Some(std::rc::Rc::new(appended_styles)) }
    };
            
    let children = children_ids
        .into_iter()
        .map(|id| build_styled_node(document, id, stylesheet, &inherited_styles))
        .collect();

    crate::dom::StyledNode {
        node_id,
        local: new_declarations,
        inherited: inherited_styles,
        children,
    }


}

fn parse_rules_list<'i, 't>(parser: &mut Parser<'i, 't>, stylesheet: &mut StyleSheet) {
    while !parser.is_exhausted() {
        match parse_rule(parser) {
            Ok(Some(rule)) => stylesheet.add_rule(rule),
            Ok(None) => {}
            Err(_) => {
                let _ = parser.parse_until_before(cssparser::Delimiter::CurlyBracketBlock, |p| {
                    while p.next().is_ok() {}
                    Ok::<(), cssparser::ParseError<()>>(())
                });
                let _ = parser.next();
            }
        }
    }
}

fn parse_rule<'i, 't>(
    parser: &mut Parser<'i, 't>,
) -> Result<Option<StyleRule>, cssparser::ParseError<'i, ()>> {
    // 1. Collect raw selector text
    let mut raw_selectors = String::new();
    while let Ok(token) = parser.next_including_whitespace() {
        if matches!(token, Token::CurlyBracketBlock) {
            break;
        }
        match token {
            Token::Ident(n) => raw_selectors.push_str(n.as_ref()),
            Token::Hash(n) | Token::IDHash(n) => {
                raw_selectors.push('#');
                raw_selectors.push_str(n.as_ref());
            }
            Token::Delim(c) => raw_selectors.push(*c),
            Token::WhiteSpace(_) => raw_selectors.push(' '),
            Token::Comma => raw_selectors.push(','),
            Token::Colon => raw_selectors.push(':'),
            _ => {}
        }
    }

    if raw_selectors.is_empty() {
        return Ok(None);
    }

    // 2. Parse the raw selector string into an AST
    let selectors = parse_selector_list(&raw_selectors);
    if selectors.is_empty() {
        return Ok(None);
    }

    // 3. Parse declarations (inside `{...}`)
    let mut declarations = Vec::new();
    let result = parser.parse_nested_block(|p| {
        while !p.is_exhausted() {
            if let Ok(ident) = p.expect_ident() {
                let name = ident.as_ref().to_owned();
                let _ = p.expect_colon();

                let mut value = String::new();
                while let Ok(token) = p.next() {
                    if matches!(token, Token::Semicolon) {
                        break;
                    }
                    match token {
                        Token::Ident(n) => value.push_str(n),
                        Token::Number { value: v, .. } => value.push_str(&v.to_string()),
                        Token::Dimension { value: v, unit, .. } => {
                            value.push_str(&v.to_string());
                            value.push_str(unit.as_ref());
                        }
                        Token::QuotedString(s) => value.push_str(s),
                        _ => {}
                    }
                }

                // Expand shorthand properties
                let value_trimmed = value.trim();
                let name_str = name; // string_cache interning input string
                if name_str == "margin" || name_str == "padding" {
                    let parts: Vec<&str> = value_trimmed.split_whitespace().collect();
                    let (top, right, bottom, left) = if name_str == "margin" {
                        ("margin-top", "margin-right", "margin-bottom", "margin-left")
                    } else {
                        (
                            "padding-top",
                            "padding-right",
                            "padding-bottom",
                            "padding-left",
                        )
                    };
                    match parts.len() {
                        1 => {
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(top),
                                value: parse_style_value(parts[0]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(right),
                                value: parse_style_value(parts[0]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(bottom),
                                value: parse_style_value(parts[0]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(left),
                                value: parse_style_value(parts[0]),
                            });
                        }
                        2 => {
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(top),
                                value: parse_style_value(parts[0]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(bottom),
                                value: parse_style_value(parts[0]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(left),
                                value: parse_style_value(parts[1]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(right),
                                value: parse_style_value(parts[1]),
                            });
                        }
                        4 => {
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(top),
                                value: parse_style_value(parts[0]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(right),
                                value: parse_style_value(parts[1]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(bottom),
                                value: parse_style_value(parts[2]),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(left),
                                value: parse_style_value(parts[3]),
                            });
                        }
                        _ => {
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(name_str),
                                value: parse_style_value(&value),
                            });
                        }
                    }
                } else if name_str == "background" {
                    declarations.push(Declaration {
                        name: string_cache::DefaultAtom::from("background-color"),
                        value: parse_style_value(&value),
                    });
                } else {
                    declarations.push(Declaration {
                        name: string_cache::DefaultAtom::from(name_str),
                        value: parse_style_value(&value),
                    });
                }
            } else {
                let _ = p.next();
            }
        }
        Ok::<(), cssparser::ParseError<()>>(())
    });

    if result.is_ok() {
        Ok(Some(StyleRule {
            selectors,
            declarations,
        }))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Inline style parsing via cssparser's DeclarationParser trait.
// ---------------------------------------------------------------------------

struct InlineStyleParser;

impl<'i> DeclarationParser<'i> for InlineStyleParser {
    type Declaration = Declaration;
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: cssparser::CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _start: &ParserState,
    ) -> Result<Declaration, cssparser::ParseError<'i, ()>> {
        let mut value = String::new();
        while let Ok(token) = input.next() {
            match token {
                Token::Ident(n) => value.push_str(n),
                Token::Number { value: v, .. } => value.push_str(&v.to_string()),
                Token::Dimension { value: v, unit, .. } => {
                    value.push_str(&v.to_string());
                    value.push_str(unit.as_ref());
                }
                Token::Percentage { unit_value, .. } => {
                    value.push_str(&(unit_value * 100.0).to_string());
                    value.push('%');
                }
                Token::Hash(s) | Token::IDHash(s) => {
                    value.push('#');
                    value.push_str(s);
                }
                Token::QuotedString(s) => value.push_str(s),
                Token::WhiteSpace(_) => value.push(' '),
                Token::Comma => value.push(','),
                Token::Delim(c) => value.push(*c),
                _ => {}
            }
        }
        Ok(Declaration {
            name: string_cache::DefaultAtom::from(name.as_ref()),
            value: parse_style_value(&value),
        })
    }
}

impl<'i> AtRuleParser<'i> for InlineStyleParser {
    type Prelude = ();
    type AtRule = Declaration;
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for InlineStyleParser {
    type Prelude = ();
    type QualifiedRule = Declaration;
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, Declaration, ()> for InlineStyleParser {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

/// Parse inline style declarations (e.g., from a `style="..."` attribute)
/// using cssparser's native declaration parsing infrastructure.
pub fn parse_inline_declarations(style_text: &str) -> Vec<Declaration> {
    let mut input = ParserInput::new(style_text);
    let mut parser = Parser::new(&mut input);
    let mut style_parser = InlineStyleParser;

    let iter = RuleBodyParser::new(&mut parser, &mut style_parser);
    let mut declarations = Vec::new();
    for result in iter {
        if let Ok(decl) = result {
            declarations.push(decl);
        }
    }
    declarations
}
