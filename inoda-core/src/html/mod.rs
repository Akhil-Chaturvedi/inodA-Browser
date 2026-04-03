//! HTML parsing module.
//!
//! Uses `html5gum` to stream tokens into the `generational_arena`-backed
//! `Document` in a single pass. Implicit tag auto-closing walks up the
//! ancestor chain to find the matching tag before block-level boundaries.
//!
//! Content inside `<script>` and `<style>` tags is treated as raw text.
//! CSS text from `<style>` elements is parsed immediately into
//! `document.stylesheet` via `css::append_stylesheet()`.

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
                let tag_name_str = String::from_utf8_lossy(&tag.name);
                let tag_name = crate::dom::LocalName::new(&tag_name_str);

                if &*tag_name == "script" || &*tag_name == "style" {
                    inside_raw_tag = Some(tag_name.clone());
                    if &*tag_name == "style" {
                        current_style_text.clear();
                    }
                }

                let mut attributes = Vec::new();
                let mut classes = String::new();
                let mut cached_inline_styles = None;
                let mut id_val = None;

                for (key, value) in tag.attributes {
                    if attributes.len() >= crate::dom::MAX_ATTRIBUTES {
                        break;
                    }
                    if let (Ok(k_str), Ok(v_str)) = (std::str::from_utf8(&key), std::str::from_utf8(&value)) {
                        if k_str == "class" {
                            classes = v_str.to_string();
                        } else if k_str == "style" {
                            let decls = crate::css::parse_inline_declarations(v_str);
                            cached_inline_styles = Some(decls.into_iter().map(|d| (d.name, d.value)).collect());
                        } else if k_str == "id" {
                            id_val = Some(v_str.to_string());
                            attributes.push((k_str.to_string(), v_str.to_string()));
                        } else {
                            attributes.push((k_str.to_string(), v_str.to_string()));
                        }
                    }
                }

                let mut data = ElementData::new(tag_name.clone());
                data.attributes = attributes;
                data.classes = classes;
                data.cached_inline_styles = cached_inline_styles;

                let node = Node::Element(data);
                let node_id = doc.add_node(node);
                if let Some(id_str) = id_val {
                    doc.id_map.insert(id_str, node_id);
                }

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
                let tag_name_str = String::from_utf8_lossy(&tag.name);
                let tag_name = crate::dom::LocalName::new(&tag_name_str);

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

    doc
}
