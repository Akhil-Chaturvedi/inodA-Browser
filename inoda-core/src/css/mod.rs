//! CSS parsing, selector matching, and style computation.
//!
//! Parses CSS text into a `StyleSheet` of rules with pre-parsed `ComplexSelector`
//! ASTs. Matches selectors against DOM elements using pre-computed specificity
//! and O(1) in-node parent pointers for complex combinators (`>`, ` `).
//!
//! Property names are interned as `string_cache::DefaultAtom` to minimize
//! memory allocations during style tree construction. Supports compound
//! selectors, comma-separated lists, CSS inheritance for text properties,
//! and shorthand expansion for `margin`, `padding`, and `background`. Inline
//! `style` attributes are parsed natively.

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

/// A simple structure storing the selectors matching a rule block
#[derive(Debug, Default, Clone)]
pub struct StyleSheet {
    pub rules: Vec<StyleRule>,
}

#[derive(Debug, Clone)]
pub struct StyleRule {
    /// Pre-parsed complex selector list (comma-separated selectors become separate entries).
    pub selectors: Vec<ComplexSelector>,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: string_cache::DefaultAtom,
    pub value: String,
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
    tag_name: &markup5ever::LocalName,
    attributes: &[(markup5ever::LocalName, String)],
    classes: &std::collections::HashSet<markup5ever::LocalName>,
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
                let atom = markup5ever::LocalName::from(c.as_str());
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

fn match_complex_selector(
    complex: &ComplexSelector,
    node_id: crate::dom::NodeId,
    document: &crate::dom::Document,
) -> bool {
    // Fast path: does the right-most part match the current element?
    if let Some(crate::dom::Node::Element(data)) = document.nodes.get(node_id) {
        if !match_compound_selector(&complex.last, &data.tag_name, &data.attributes, &data.classes) {
            return false;
        }
    } else {
        return false;
    }

    // Now walk up the ancestors based on combinators
    let mut current_id = node_id;

    for (comb, ancestor_compound) in &complex.ancestors {
        let mut matched = false;

        loop {
            // Move to parent
            if let Some(parent_id) = document.parent_of(current_id) {
                current_id = parent_id;

                if let Some(crate::dom::Node::Element(parent_data)) = document.nodes.get(current_id)
                {
                    if match_compound_selector(
                        ancestor_compound,
                        &parent_data.tag_name,
                        &parent_data.attributes,
                        &parent_data.classes,
                    ) {
                        matched = true;
                        break;
                    }
                }

                // If it's a direct child combinator and we didn't match the immediate parent, we fail this sequence.
                if *comb == Combinator::Child {
                    break;
                }
            } else {
                // Root of document
                break;
            }
        }

        if !matched {
            return false;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Stylesheet parsing
// ---------------------------------------------------------------------------

pub fn parse_stylesheet(css: &str) -> StyleSheet {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut stylesheet = StyleSheet::default();

    parse_rules_list(&mut parser, &mut stylesheet);
    stylesheet
}

pub fn compute_styles(
    document: &crate::dom::Document,
    base_stylesheet: &StyleSheet,
) -> crate::dom::StyledNode {
    let mut combined_sheet = base_stylesheet.clone();

    for style_text in &document.style_texts {
        let inline_sheet = parse_stylesheet(style_text);
        combined_sheet.rules.extend(inline_sheet.rules);
    }

    build_styled_node(
        document,
        document.root_id,
        &combined_sheet,
        &std::rc::Rc::new(Vec::new()),
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
    parent_styles: &std::rc::Rc<Vec<(string_cache::DefaultAtom, String)>>,
) -> crate::dom::StyledNode {
    let mut specified_values = Vec::new();

    for (k, v) in parent_styles.iter() {
        if is_inheritable(k) {
            specified_values.push((k.clone(), v.clone()));
        }
    }

    let mut children_ids = Vec::new();

    if let Some(node) = document.nodes.get(node_id) {
        match node {
            crate::dom::Node::Element(data) => {
                let mut matched_rules: Vec<((u32, u32, u32), &StyleRule)> = Vec::new();
                for rule in &stylesheet.rules {
                    // Check each complex selector in the pre-parsed list
                    for complex in &rule.selectors {
                        if match_complex_selector(complex, node_id, document) {
                            matched_rules.push((complex.specificity, rule));
                            break; // matched this rule via at least one selector
                        }
                    }
                }

                // Sort stably to preserve source-order precedence for equal specificities
                matched_rules.sort_by_key(|(spec, _)| *spec);

                for (_, rule) in matched_rules {
                    for decl in &rule.declarations {
                        if let Some(pos) =
                            specified_values.iter().position(|(k, _)| k == &decl.name)
                        {
                            specified_values[pos].1 = decl.value.clone();
                        } else {
                            specified_values.push((decl.name.clone(), decl.value.clone()));
                        }
                    }
                }

                if let Some((_, style_attr)) = data.attributes.iter().find(|(k, _)| &**k == "style")
                {
                    let inline_decls = parse_inline_declarations(style_attr);
                    for decl in &inline_decls {
                        if let Some(pos) =
                            specified_values.iter().position(|(k, _)| k == &decl.name)
                        {
                            specified_values[pos].1 = decl.value.clone();
                        } else {
                            specified_values.push((decl.name.clone(), decl.value.clone()));
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

    let specified_values_rc = std::rc::Rc::new(specified_values);

    let children = children_ids
        .into_iter()
        .map(|id| build_styled_node(document, id, stylesheet, &specified_values_rc))
        .collect();

    crate::dom::StyledNode {
        node_id,
        specified_values: specified_values_rc,
        children,
    }
}

fn parse_rules_list<'i, 't>(parser: &mut Parser<'i, 't>, stylesheet: &mut StyleSheet) {
    while !parser.is_exhausted() {
        match parse_rule(parser) {
            Ok(Some(rule)) => stylesheet.rules.push(rule),
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
                                value: parts[0].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(right),
                                value: parts[0].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(bottom),
                                value: parts[0].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(left),
                                value: parts[0].to_string(),
                            });
                        }
                        2 => {
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(top),
                                value: parts[0].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(bottom),
                                value: parts[0].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(left),
                                value: parts[1].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(right),
                                value: parts[1].to_string(),
                            });
                        }
                        4 => {
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(top),
                                value: parts[0].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(right),
                                value: parts[1].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(bottom),
                                value: parts[2].to_string(),
                            });
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(left),
                                value: parts[3].to_string(),
                            });
                        }
                        _ => {
                            declarations.push(Declaration {
                                name: string_cache::DefaultAtom::from(name_str),
                                value,
                            });
                        }
                    }
                } else if name_str == "background" {
                    declarations.push(Declaration {
                        name: string_cache::DefaultAtom::from("background-color"),
                        value,
                    });
                } else {
                    declarations.push(Declaration {
                        name: string_cache::DefaultAtom::from(name_str),
                        value,
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
            value: value.trim().to_string(),
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
