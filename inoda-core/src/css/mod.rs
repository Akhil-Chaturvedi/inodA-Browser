//! CSS parsing, selector matching, and style computation.
//!
//! Parses CSS text into a `StyleSheet` of rules. Matches selectors against DOM
//! elements using specificity scoring (id, class, tag). Supports compound
//! selectors, comma-separated selector lists, CSS inheritance for text
//! properties, and shorthand expansion for `margin`, `padding`, and `background`.
//! Inline `style` attributes are parsed natively via `cssparser`'s
//! `DeclarationParser` trait.

use cssparser::{Parser, ParserInput, Token};
use cssparser::{DeclarationParser, AtRuleParser, QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, ParserState};

/// A simple structure storing the selectors matching a rule block
#[derive(Debug, Default, Clone)]
pub struct StyleSheet {
    pub rules: Vec<StyleRule>,
}

#[derive(Debug, Clone)]
pub struct StyleRule {
    pub selectors: String, // Keeping it simple for the initial bridge: we can just match exact selector strings first
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: String,
    pub value: String,
}

pub fn parse_stylesheet(css: &str) -> StyleSheet {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut stylesheet = StyleSheet::default();

    parse_rules_list(&mut parser, &mut stylesheet);
    stylesheet
}

pub fn compute_styles(document: &crate::dom::Document, base_stylesheet: &StyleSheet) -> crate::dom::StyledNode {
    let mut combined_sheet = base_stylesheet.clone();
    
    for style_text in &document.style_texts {
        let inline_sheet = parse_stylesheet(style_text);
        combined_sheet.rules.extend(inline_sheet.rules);
    }

    build_styled_node(document, document.root_id, &combined_sheet, &[])
}

#[inline]
fn is_inheritable(property: &str) -> bool {
    matches!(property, "color" | "font-family" | "font-size" | "font-weight" | "line-height" | "text-align" | "visibility")
}

fn match_simple_selector(selector: &str, tag_name: &str, attributes: &[(String, String)]) -> (bool, (u32, u32, u32)) {
    let mut s = selector;
    let mut spec = (0, 0, 0);
    
    // Tag
    let mut parsed_tag = "";
    if !s.starts_with('.') && !s.starts_with('#') && !s.starts_with(':') {
        let end = s.find(|c| c == '.' || c == '#' || c == ':').unwrap_or(s.len());
        parsed_tag = &s[..end];
        s = &s[end..];
        spec.2 += 1;
    }
    if !parsed_tag.is_empty() && parsed_tag != tag_name && parsed_tag != "*" {
        return (false, (0,0,0));
    }
    
    // Classes, IDs, Pseudo
    let class_attr = attributes.iter().find(|(k, _)| k == "class").map(|(_, v)| v.as_str()).unwrap_or("");
    let classes: Vec<&str> = class_attr.split_whitespace().collect();
    let id_attr = attributes.iter().find(|(k, _)| k == "id").map(|(_, v)| v.as_str());

    while !s.is_empty() {
        if s.starts_with('#') {
            s = &s[1..];
            let end = s.find(|c| c == '.' || c == '#' || c == ':').unwrap_or(s.len());
            if Some(&s[..end]) != id_attr { return (false, (0,0,0)); }
            spec.0 += 1;
            s = &s[end..];
        } else if s.starts_with('.') {
            s = &s[1..];
            let end = s.find(|c| c == '.' || c == '#' || c == ':').unwrap_or(s.len());
            if !classes.contains(&&s[..end]) { return (false, (0,0,0)); }
            spec.1 += 1;
            s = &s[end..];
        } else if s.starts_with(':') {
            s = &s[1..];
            let end = s.find(|c| c == '.' || c == '#' || c == ':').unwrap_or(s.len());
            spec.1 += 1; // Pseudo-classes have class specificity
            s = &s[end..];
        } else {
            break;
        }
    }
    if selector.is_empty() { return (false, (0,0,0)); }
    (true, spec)
}

fn build_styled_node(
    document: &crate::dom::Document,
    node_id: crate::dom::NodeId,
    stylesheet: &StyleSheet,
    parent_styles: &[(String, String)]
) -> crate::dom::StyledNode {
    let mut specified_values = Vec::new();
    
    for (k, v) in parent_styles {
        if is_inheritable(k) {
            specified_values.push((k.clone(), v.clone()));
        }
    }

    let mut children_ids = Vec::new();
    
    if let Some(node) = document.nodes.get(node_id) {
        match node {
            crate::dom::Node::Element(data) => {
                let mut matched_rules = Vec::new();
                for rule in &stylesheet.rules {
                    for selector_part in rule.selectors.split(',') {
                        let selector_part = selector_part.trim();
                        let (matches, spec) = match_simple_selector(selector_part, &data.tag_name, &data.attributes);
                        if matches {
                            matched_rules.push((spec, rule));
                            break; // matched this rule
                        }
                    }
                }
                
                // Sort stably to preserve source-order precedence for equal specificities
                matched_rules.sort_by_key(|(spec, _)| *spec);
                
                for (_, rule) in matched_rules {
                    for decl in &rule.declarations {
                        if let Some(pos) = specified_values.iter().position(|(k, _)| k == &decl.name) {
                            specified_values[pos].1 = decl.value.clone();
                        } else {
                            specified_values.push((decl.name.clone(), decl.value.clone()));
                        }
                    }
                }
                
                if let Some((_, style_attr)) = data.attributes.iter().find(|(k, _)| k == "style") {
                    let inline_decls = parse_inline_declarations(style_attr);
                    for decl in &inline_decls {
                        if let Some(pos) = specified_values.iter().position(|(k, _)| k == &decl.name) {
                            specified_values[pos].1 = decl.value.clone();
                        } else {
                            specified_values.push((decl.name.clone(), decl.value.clone()));
                        }
                    }
                }
                children_ids = data.children.clone();
            }
            crate::dom::Node::Root(kids) => {
                children_ids = kids.clone();
            }
            crate::dom::Node::Text(_) => {}
        }
    }

    let children = children_ids.into_iter()
        .map(|id| build_styled_node(document, id, stylesheet, &specified_values))
        .collect();

    crate::dom::StyledNode {
        node_id,
        specified_values,
        children,
    }
}

fn parse_rules_list<'i, 't>(parser: &mut Parser<'i, 't>, stylesheet: &mut StyleSheet) {
    while !parser.is_exhausted() {
        // We advance through the rules list, skipping whitespace and comments natively via cssparser
        match parse_rule(parser) {
            Ok(Some(rule)) => stylesheet.rules.push(rule),
            Ok(None) => {
                // Not a style rule, skip it
            }
            Err(_) => {
                // Recover from error by skipping to the next top-level rule
                let _ = parser.parse_until_before(cssparser::Delimiter::CurlyBracketBlock, |p| {
                    while p.next().is_ok() {}
                    Ok::<(), cssparser::ParseError<()>>(())
                });
                let _ = parser.next(); // Consume the curly bracket block itself preventing infinite loop
            }
        }
    }
}

fn parse_rule<'i, 't>(parser: &mut Parser<'i, 't>) -> Result<Option<StyleRule>, cssparser::ParseError<'i, ()>> {
    let mut selectors = String::new();
    while let Ok(token) = parser.next() {
        if matches!(token, Token::CurlyBracketBlock) { break; }
        match token {
            Token::Ident(n) => selectors.push_str(n),
            Token::Hash(n) | Token::IDHash(n) => { selectors.push('#'); selectors.push_str(n); },
            Token::Delim(c) => selectors.push(*c),
            Token::WhiteSpace(_) => selectors.push(' '),
            Token::Comma => selectors.push(','),
            Token::Colon => selectors.push(':'),
            _ => {}
        }
    }

    if selectors.is_empty() {
        return Ok(None);
    }

    // 2. Parse Declarations (inside `{...}`)
    let mut declarations = Vec::new();
    let result = parser.parse_nested_block(|p| {
        while !p.is_exhausted() {
            if let Ok(ident) = p.expect_ident() {
                let name = ident.as_ref().to_owned();
                let _ = p.expect_colon();
                
                // Parse value until semicolon
                let mut value = String::new();
                while let Ok(token) = p.next() {
                     if matches!(token, Token::Semicolon) { break; }
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
                if name == "margin" || name == "padding" {
                    let parts: Vec<&str> = value_trimmed.split_whitespace().collect();
                    match parts.len() {
                        1 => {
                            declarations.push(Declaration { name: format!("{}-top", name), value: parts[0].to_string() });
                            declarations.push(Declaration { name: format!("{}-right", name), value: parts[0].to_string() });
                            declarations.push(Declaration { name: format!("{}-bottom", name), value: parts[0].to_string() });
                            declarations.push(Declaration { name: format!("{}-left", name), value: parts[0].to_string() });
                        }
                        2 => {
                            declarations.push(Declaration { name: format!("{}-top", name), value: parts[0].to_string() });
                            declarations.push(Declaration { name: format!("{}-bottom", name), value: parts[0].to_string() });
                            declarations.push(Declaration { name: format!("{}-left", name), value: parts[1].to_string() });
                            declarations.push(Declaration { name: format!("{}-right", name), value: parts[1].to_string() });
                        }
                        4 => {
                            declarations.push(Declaration { name: format!("{}-top", name), value: parts[0].to_string() });
                            declarations.push(Declaration { name: format!("{}-right", name), value: parts[1].to_string() });
                            declarations.push(Declaration { name: format!("{}-bottom", name), value: parts[2].to_string() });
                            declarations.push(Declaration { name: format!("{}-left", name), value: parts[3].to_string() });
                        }
                        _ => {
                            // Support 3-part or other invalid shorthands as fallback
                            declarations.push(Declaration { name, value });
                        }
                    }
                } else if name == "background" {
                    // Simple shorthand mapping for background color (ex: `background: red`)
                    declarations.push(Declaration { name: "background-color".to_string(), value });
                } else {
                    declarations.push(Declaration { name, value });
                }
            } else {
                let _ = p.next(); // skip unknown
            }
        }
        Ok::<(), cssparser::ParseError<()>>(())
    });

    if result.is_ok() {
         Ok(Some(StyleRule { selectors, declarations }))
    } else {
         Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Inline style parsing via cssparser's DeclarationParser trait.
// This replaces the old `format!("dummy {{ ... }}")` workaround.
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
            name: name.to_string(),
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
    fn parse_declarations(&self) -> bool { true }
    fn parse_qualified(&self) -> bool { false }
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
