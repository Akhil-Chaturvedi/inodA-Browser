//! HTML parsing module.
//!
//! Converts an HTML string into the arena-based `Document` using html5ever.
//! Extracts raw CSS text from `<style>` elements into `Document::style_texts`.
//! Whitespace-only text nodes are discarded to reduce arena size.

use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::dom::{Document, ElementData, Node, NodeId};

pub fn parse_html(html: &str) -> Document {
    let dom = parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap();

    let mut arena_doc = Document::new();
    let root_id = arena_doc.root_id;
    walk_rcdom(&dom.document, &mut arena_doc, root_id);

    arena_doc
}

fn walk_rcdom(rc_node: &Handle, document: &mut Document, parent_id: NodeId) {
    let mut current_id = parent_id;

    match rc_node.data {
        NodeData::Element { ref name, ref attrs, .. } => {
            let mut attributes = Vec::with_capacity(attrs.borrow().len());
            for attr in attrs.borrow().iter() {
                attributes.push((
                    attr.name.local.to_string(),
                    attr.value.to_string(),
                ));
            }

            let tag_name = name.local.to_string();

            if tag_name == "style" {
                let mut style_text = String::new();
                for child in rc_node.children.borrow().iter() {
                    if let NodeData::Text { ref contents } = child.data {
                        style_text.push_str(&contents.borrow());
                    }
                }
                document.style_texts.push(style_text);
            }

            let element_data = ElementData {
                tag_name,
                attributes,
                children: Vec::new(),
            };

            let node = Node::Element(element_data);
            let id = document.add_node(node);
            document.append_child(parent_id, id);
            current_id = id;
        }
        NodeData::Text { ref contents } => {
            let text = contents.borrow().to_string();
            // Ignore pure whitespace nodes for memory efficiency
            if !text.trim().is_empty() {
                let node = Node::Text(text);
                let id = document.add_node(node);
                document.append_child(parent_id, id);
            }
        }
        _ => {}
    }

    // Recursively walk children
    for child in rc_node.children.borrow().iter() {
        walk_rcdom(child, document, current_id);
    }
}
