//! HTML parsing module.
//!
//! Uses `html5gum` to stream atomized tokens into the `generational_arena`-backed
//! `Document` in a single pass.
//!
//! Extracts raw CSS text from `<style>` elements into `Document::style_texts`.

use std::collections::HashSet;
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
                let tag_name_str = String::from_utf8_lossy(&tag.name);
                let tag_name = DefaultAtom::from(tag_name_str.as_ref());
                
                if &*tag_name == "style" {
                    inside_style = true;
                    current_style_text.clear();
                }

                let mut attributes = Vec::new();
                let mut classes = HashSet::new();
                let mut id_val = None;

                for (key, value) in tag.attributes {
                    let k_str = String::from_utf8_lossy(&key);
                    let v_str = String::from_utf8_lossy(&value);
                    let k_atom = DefaultAtom::from(k_str.as_ref());

                    if &*k_atom == "class" {
                        for c in v_str.split_whitespace() {
                            classes.insert(DefaultAtom::from(c));
                        }
                    } else if &*k_atom == "id" {
                        id_val = Some(v_str.to_string());
                    }
                    attributes.push((k_atom, v_str.to_string()));
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
                let tag_name_str = String::from_utf8_lossy(&tag.name);
                let tag_name = DefaultAtom::from(tag_name_str.as_ref());

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
                let text_str = String::from_utf8_lossy(&s).to_string();
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
