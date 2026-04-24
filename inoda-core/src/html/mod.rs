//! HTML parsing module.
//!
//! This is **not** a WHATWG HTML tree-construction algorithm. `html5gum` is used
//! as a streaming tokenizer; `parse_html` applies small, local insertion and
//! end-tag reconciliation rules on top to build an arena `Document`. Behavior
//! will diverge from full browsers where the specification's tree builder would
//! apply foster parenting, implied elements, adoption agency steps, etc.
//!
//! Uses `html5gum` to stream tokens into the `generational_arena`-backed
//! `Document` in a single pass. Implicit tag auto-closing walks up the
//! ancestor chain to find the matching tag before block-level boundaries.
//!
//! Content inside `<script>` and `<style>` tags is treated as raw text.
//! CSS text from `<style>` elements is parsed immediately into
//! `document.stylesheet` via `css::append_stylesheet()`.
//!
//! Byte slices from `html5gum` tokens are validated as UTF-8 via
//! `std::str::from_utf8()` (zero-allocation for tag names). Attribute
//! values that must be owned are converted with `from_utf8().unwrap_or_default()`.
//! Truncation of attribute values at `MAX_ATTRIBUTE_VALUE_LEN` uses
//! `is_char_boundary()` to avoid splitting multi-byte UTF-8 sequences.

use crate::dom::{Document, ElementData, Node, TextData};
use html5gum::{Token, Tokenizer};

pub fn parse_html(html: &str) -> Document {
    let mut doc = Document::default();
    let mut current_parent = doc.root_id;
    let mut inside_raw_tag: Option<crate::dom::LocalName> = None;
    let mut current_style_text = String::new();

    for token in Tokenizer::new(html).infallible() {
        match token {
            Token::StartTag(tag) => {
                // Zero-allocation UTF-8 validation for tag names (html5gum emits valid UTF-8)
                let tag_name_str = std::str::from_utf8(&tag.name).unwrap_or("");
                let tag_name = crate::dom::LocalName::new(tag_name_str);

                if &*tag_name == "script" || &*tag_name == "style" {
                    inside_raw_tag = Some(tag_name.clone());
                    if &*tag_name == "style" {
                        current_style_text.clear();
                    }
                }

                let mut attributes = Vec::new();
                let mut classes = String::new();
                let mut cached_inline_styles = None;

                for (key, value) in tag.attributes {
                    if attributes.len() >= crate::dom::MAX_ATTRIBUTES {
                        break;
                    }
                    // Zero-allocation UTF-8 validation for attribute keys
                    let k_str = std::str::from_utf8(&key).unwrap_or("").to_string();
                    // Attribute values must be owned; validate UTF-8 without lossy replacement
                    let mut v_str = std::str::from_utf8(&value).unwrap_or("").to_string();

                    // Safe truncation: avoid splitting multi-byte UTF-8 characters
                    if v_str.len() > crate::dom::MAX_ATTRIBUTE_VALUE_LEN {
                        let mut cap = crate::dom::MAX_ATTRIBUTE_VALUE_LEN;
                        while !v_str.is_char_boundary(cap) {
                            cap -= 1;
                        }
                        v_str.truncate(cap);
                    }

                    if k_str == "class" {
                        classes = v_str.to_string();
                    } else if k_str == "style" {
                        let decls = crate::css::parse_inline_declarations(&v_str);
                        cached_inline_styles = Some(decls.into_iter().map(|d| (d.name, d.value)).collect());
                    } else if k_str == "id" {
                        attributes.push((k_str, v_str));
                    } else {
                        attributes.push((k_str, v_str));
                    }
                }

                let mut data = ElementData::new(tag_name.clone());
                data.attributes = attributes;
                data.classes = classes;
                data.cached_inline_styles = cached_inline_styles;

                let node = Node::Element(data);
                // add_node handles id_map insertion internally
                let node_id = doc.add_node(node);

                doc.append_child(current_parent, node_id);

                let is_void = matches!(
                    &*tag_name,
                    "area"
                    | "base"
                    | "br"
                    | "col"
                    | "embed"
                    | "hr"
                    | "img"
                    | "input"
                    | "link"
                    | "meta"
                    | "param"
                    | "source"
                    | "track"
                    | "wbr"
                );

                if !is_void && !tag.self_closing {
                    current_parent = node_id;
                }
            }
            Token::EndTag(tag) => {
                let tag_name_str = std::str::from_utf8(&tag.name).unwrap_or("");
                let tag_name = crate::dom::LocalName::new(tag_name_str);

                if let Some(ref raw) = inside_raw_tag {
                    if &**raw == &*tag_name {
                        inside_raw_tag = None;
                        if &*tag_name == "style" && !current_style_text.is_empty() {
                            crate::css::append_stylesheet(&current_style_text, &mut doc.stylesheet);
                            current_style_text.clear();
                        }
                    } else {
                        // Skip EndTags inside Rawtext tags unless they match
                        continue;
                    }
                } else {
                    // Walk up to find matching tag
                    let mut p = Some(current_parent);
                    while let Some(pid) = p {
                        if let Some(Node::Element(data)) = doc.nodes.get(pid) {
                            if data.tag_name == tag_name {
                                current_parent = doc.parent_of(pid).unwrap_or(doc.root_id);
                                break;
                            }
                        }
                        p = doc.parent_of(pid);
                    }
                }
            }
            Token::String(s) => {
                let text = std::str::from_utf8(&s).unwrap_or("").to_string();
                if text.is_empty() {
                    continue;
                }

                if let Some(ref raw) = inside_raw_tag {
                    if &**raw == "style" {
                        current_style_text.push_str(&text);
                        continue;
                    }
                    if &**raw == "script" {
                        // Scripts are ignored for now but we consume their content
                        continue;
                    }
                }

                let node = Node::Text(TextData::new(text));
                let node_id = doc.add_node(node);
                doc.append_child(current_parent, node_id);
            }
            _ => {}
        }
    }

    doc.dirty = true;
    doc
}
