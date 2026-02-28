//! HTML parsing module.
//!
//! Uses `html5gum` to stream tokens into the `generational_arena`-backed
//! `Document` in a single pass. Implicit tag auto-closing walks up the
//! ancestor chain to find the matching tag before block-level boundaries.
//!
//! Content inside `<script>` and `<style>` tags is treated as raw text and
//! not parsed as HTML. CSS text from `<style>` elements is collected into
//! `Document::style_texts`.

use html5gum::{Token, Tokenizer};
use string_cache::DefaultAtom;
use crate::dom::{Document, ElementData, Node, TextData};

pub fn parse_html(html: &str) -> Document {
    let mut doc = Document::default();
    let mut current_parent = doc.root_id;
    let mut inside_raw_tag: Option<DefaultAtom> = None;
    let mut current_style_text = String::new();

    for token in Tokenizer::new(html).infallible() {
        match token {
            Token::StartTag(tag) => {
                let tag_name_str = std::str::from_utf8(&tag.name).unwrap_or("");
                let tag_name = DefaultAtom::from(tag_name_str);
                
                if let Some(ref raw) = inside_raw_tag {
                    if &**raw == "style" {
                        current_style_text.push_str("<");
                        current_style_text.push_str(tag_name_str);
                        for (k, v) in tag.attributes.iter() {
                            let k_str = std::str::from_utf8(k).unwrap_or("");
                            let v_str = std::str::from_utf8(v).unwrap_or("");
                            current_style_text.push_str(" ");
                            current_style_text.push_str(k_str);
                            current_style_text.push_str("=\"");
                            current_style_text.push_str(v_str);
                            current_style_text.push_str("\"");
                        }
                        if tag.self_closing { current_style_text.push_str("/>"); } else { current_style_text.push_str(">"); }
                    } else {
                        if let Some(last_child) = doc.last_child_of(current_parent) {
                            if let Some(Node::Text(existing)) = doc.nodes.get_mut(last_child) {
                                existing.text.push_str("<");
                                existing.text.push_str(tag_name_str);
                                for (k, v) in tag.attributes.iter() {
                                    let k_str = std::str::from_utf8(k).unwrap_or("");
                                    let v_str = std::str::from_utf8(v).unwrap_or("");
                                    existing.text.push_str(" ");
                                    existing.text.push_str(k_str);
                                    existing.text.push_str("=\"");
                                    existing.text.push_str(v_str);
                                    existing.text.push_str("\"");
                                }
                                if tag.self_closing { existing.text.push_str("/>"); } else { existing.text.push_str(">"); }
                            }
                        }
                    }
                    continue;
                }
                
                if &*tag_name == "style" || &*tag_name == "script" {
                    inside_raw_tag = Some(tag_name.clone());
                }

                let mut check_node = current_parent;
                let mut found_close_target = false;
                
                while let Some(Node::Element(data)) = doc.nodes.get(check_node) {
                    let p_tag = &*data.tag_name;
                    let should_close = match &*tag_name {
                        "li" => p_tag == "li",
                        "td" | "th" => p_tag == "td" || p_tag == "th",
                        "tr" => p_tag == "tr",
                        "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "table" => p_tag == "p",
                        _ => false,
                    };
                    
                    if should_close {
                        found_close_target = true;
                        break;
                    }
                    
                    if matches!(p_tag, "div" | "body" | "td" | "th" | "table") {
                        break;
                    }
                    
                    check_node = doc.parent_of(check_node).unwrap_or(doc.root_id);
                }
                
                if found_close_target {
                    current_parent = doc.parent_of(check_node).unwrap_or(doc.root_id);
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

                if let Some(ref raw) = inside_raw_tag {
                    if raw != &tag_name {
                        if &**raw == "style" {
                            current_style_text.push_str("</");
                            current_style_text.push_str(tag_name_str);
                            current_style_text.push_str(">");
                        } else {
                            if let Some(last_child) = doc.last_child_of(current_parent) {
                                if let Some(Node::Text(existing)) = doc.nodes.get_mut(last_child) {
                                    existing.text.push_str("</");
                                    existing.text.push_str(tag_name_str);
                                    existing.text.push_str(">");
                                }
                            }
                        }
                        continue;
                    } else {
                        inside_raw_tag = None;
                        if &*tag_name == "style" && !current_style_text.is_empty() {
                            doc.style_texts.push(std::mem::take(&mut current_style_text));
                        }
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
                let text_str = std::str::from_utf8(&s).unwrap_or("");
                if text_str.is_empty() {
                    continue;
                }
                
                if let Some(ref raw) = inside_raw_tag {
                    if &**raw == "style" {
                        current_style_text.push_str(text_str);
                        continue;
                    }
                }

                let mut combined = false;
                if let Some(last_child) = doc.last_child_of(current_parent) {
                    if let Some(Node::Text(existing)) = doc.nodes.get_mut(last_child) {
                        existing.text.push_str(text_str);
                        combined = true;
                    }
                }
                if !combined {
                    let id = doc.add_node(Node::Text(TextData {
                        text: text_str.to_string(),
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
