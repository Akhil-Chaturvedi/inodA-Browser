//! HTML parsing module.
//!
//! Uses `html5gum` to stream atomized tokens into the `generational_arena`-backed
//! `Document` in a single pass.
//!
//! Extracts raw CSS text from `<style>` elements into `Document::style_texts`.

use html5gum::{Token, Tokenizer};
use string_cache::DefaultAtom;
use crate::dom::{Document, ElementData, Node, TextData};

pub fn parse_html(html: &str) -> Document {
    let mut doc = Document::default();
    let mut current_parent = doc.root_id;
    let mut inside_style = false;
    let mut current_style_text = String::new();

    for token in Tokenizer::new(html).infallible() {
        match token {
            Token::StartTag(tag) => {
                let tag_name_str = std::str::from_utf8(&tag.name).unwrap_or("");
                let tag_name = DefaultAtom::from(tag_name_str);
                
                if let Some(Node::Element(parent_data)) = doc.nodes.get(current_parent) {
                    let p_tag = &*parent_data.tag_name;
                    let auto_close = match &*tag_name {
                        "li" => p_tag == "li",
                        "td" | "th" => p_tag == "td" || p_tag == "th",
                        "tr" => p_tag == "tr",
                        "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "table" => p_tag == "p",
                        _ => false,
                    };
                    if auto_close {
                        current_parent = doc.parent_of(current_parent).unwrap_or(doc.root_id);
                    }
                }

                if &*tag_name == "style" {
                    inside_style = true;
                    current_style_text.clear();
                }

                let mut attributes = Vec::new();
                let mut classes = Vec::new();
                let mut id_val = None;

                for (key, value) in tag.attributes {
                    if let (Ok(k_str), Ok(v_str)) = (std::str::from_utf8(&key), std::str::from_utf8(&value)) {
                        let k_atom = DefaultAtom::from(k_str);

                        if &*k_atom == "class" {
                            for c in v_str.split_whitespace() {
                                let class_atom = DefaultAtom::from(c);
                                if !classes.contains(&class_atom) {
                                    classes.push(class_atom);
                                }
                            }
                        } else if &*k_atom == "id" {
                            id_val = Some(v_str.to_string());
                        }
                        attributes.push((k_atom, v_str.to_string()));
                    }
                }

                let node = Node::Element(ElementData {
                    tag_name: tag_name.clone(),
                    attributes,
                    classes,
                    parent: None,
                    first_child: None,
                    last_child: None,
                    prev_sibling: None,
                    next_sibling: None,
                });

                let node_id = doc.add_node(node);
                if let Some(id_str) = id_val {
                    doc.id_map.insert(id_str, node_id);
                }

                doc.append_child(current_parent, node_id);
                
                let is_void = matches!(
                    &*tag_name,
                    "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr"
                );

                if !is_void && !tag.self_closing {
                    current_parent = node_id;
                }
            }
            Token::EndTag(tag) => {
                let tag_name_str = std::str::from_utf8(&tag.name).unwrap_or("");
                let tag_name = DefaultAtom::from(tag_name_str);

                if &*tag_name == "style" {
                    inside_style = false;
                    if !current_style_text.is_empty() {
                        doc.style_texts.push(std::mem::take(&mut current_style_text));
                    }
                }

                let mut p = Some(current_parent);
                while let Some(pid) = p {
                    if let Some(Node::Element(data)) = doc.nodes.get(pid) {
                        if data.tag_name == tag_name {
                            current_parent = doc.parent_of(pid).unwrap_or(doc.root_id);
                            break;
                        }
                    } else if let Some(Node::Root(_)) = doc.nodes.get(pid) {
                        break;
                    }
                    p = doc.parent_of(pid);
                }
            }
            Token::String(s) => {
                let text_str = std::str::from_utf8(&s).unwrap_or("").to_string();
                if text_str.is_empty() {
                    continue;
                }
                
                if inside_style {
                    current_style_text.push_str(&text_str);
                    continue;
                }

                let mut combined = false;
                if let Some(last_child) = doc.last_child_of(current_parent) {
                    if let Some(Node::Text(existing)) = doc.nodes.get_mut(last_child) {
                        existing.text.push_str(&text_str);
                        combined = true;
                    }
                }
                if !combined {
                    let id = doc.add_node(Node::Text(TextData {
                        text: text_str,
                        parent: None,
                        prev_sibling: None,
                        next_sibling: None,
                    }));
                    doc.append_child(current_parent, id);
                }
            }
            Token::Comment(_) | Token::Doctype(_) | Token::Error(_) => {}
        }
    }

    doc
}
