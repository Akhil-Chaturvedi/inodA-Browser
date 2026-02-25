//! HTML parsing module.
//!
//! Implements `html5ever::TreeSink` directly on a `DocumentBuilder` wrapper,
//! streaming atomized `LocalName` tokens into the `generational_arena`-backed
//! `Document` in a single pass. No intermediate `RcDom` allocation.
//!
//! Extracts raw CSS text from `<style>` elements into `Document::style_texts`.

use std::borrow::Cow;
use std::cell::RefCell;

use html5ever::parse_document;
use html5ever::tendril::{StrTendril, TendrilSink};
use markup5ever::interface::tree_builder::{
    ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink,
};
use markup5ever::interface::{Attribute, QualName};
use markup5ever::{LocalName, Namespace, local_name};

use crate::dom::{Document, ElementData, Node, NodeId, TextData};

/// Wraps a `Document` in a `RefCell` so that `TreeSink` (which takes `&self`)
/// can mutate the arena.
struct DocumentBuilder {
    doc: RefCell<Document>,
}

/// The ElemName implementation for our handles.
#[derive(Debug)]
struct InodaElemName {
    ns: Namespace,
    local: LocalName,
}

impl ElemName for InodaElemName {
    fn ns(&self) -> &Namespace {
        &self.ns
    }
    fn local_name(&self) -> &LocalName {
        &self.local
    }
}

impl TreeSink for DocumentBuilder {
    type Handle = NodeId;
    type Output = Document;
    type ElemName<'a> = InodaElemName;

    fn finish(self) -> Document {
        self.doc.into_inner()
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {
        // Silently ignore parse errors for now.
    }

    fn get_document(&self) -> NodeId {
        self.doc.borrow().root_id
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> InodaElemName {
        let doc = self.doc.borrow();
        if let Some(Node::Element(data)) = doc.nodes.get(*target) {
            InodaElemName {
                ns: Namespace::from("http://www.w3.org/1999/xhtml"),
                local: data.tag_name.clone(),
            }
        } else {
            InodaElemName {
                ns: Namespace::from(""),
                local: LocalName::from(""),
            }
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> NodeId {
        let mut doc = self.doc.borrow_mut();
        let attributes: Vec<(LocalName, String)> = attrs
            .into_iter()
            .map(|a| (a.name.local, a.value.to_string()))
            .collect();
        let tag_name = name.local;
        let node = Node::Element(ElementData {
            tag_name,
            attributes,
            children: Vec::new(),
            parent: None,
        });
        doc.add_node(node)
    }

    fn create_comment(&self, _text: StrTendril) -> NodeId {
        // Store comments as empty text nodes (ignored during layout/render).
        let mut doc = self.doc.borrow_mut();
        doc.add_node(Node::Text(TextData {
            text: String::new(),
            parent: None,
        }))
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        let mut doc = self.doc.borrow_mut();
        doc.add_node(Node::Text(TextData {
            text: String::new(),
            parent: None,
        }))
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut doc = self.doc.borrow_mut();
        match child {
            NodeOrText::AppendNode(node_id) => {
                doc.append_child(*parent, node_id);
            }
            NodeOrText::AppendText(text) => {
                let text_str = text.to_string();
                // Check if we should merge with the last child if it is also text
                let children_slice: Option<Vec<NodeId>> = match doc.nodes.get(*parent) {
                    Some(Node::Element(d)) => Some(d.children.clone()),
                    Some(Node::Root(c)) => Some(c.children.clone()),
                    _ => None,
                };
                if let Some(children) = children_slice {
                    if let Some(last_child_id) = children.last().copied() {
                        if let Some(Node::Text(existing)) = doc.nodes.get_mut(last_child_id) {
                            existing.text.push_str(&text_str);
                            return;
                        }
                    }
                }

                let id = doc.add_node(Node::Text(TextData {
                    text: text_str,
                    parent: None,
                }));
                doc.append_child(*parent, id);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        _prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        // Simplified: always append to the element.
        self.append(element, child);
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
        // DOCTYPE is ignored in our minimal engine.
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        // We don't support <template>; just return the element itself.
        *target
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        *x == *y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {
        // Ignored.
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut doc = self.doc.borrow_mut();
        let sibling_id = *sibling;

        let new_id = match new_node {
            NodeOrText::AppendNode(id) => id,
            NodeOrText::AppendText(text) => {
                let text_str = text.to_string();
                doc.add_node(Node::Text(TextData {
                    text: text_str,
                    parent: None,
                }))
            }
        };

        let parent_id = match doc.parent_of(sibling_id) {
            Some(pid) => pid,
            None => return,
        };

        doc.append_child(parent_id, new_id);

        if let Some(parent) = doc.nodes.get_mut(parent_id) {
            let children = match parent {
                Node::Element(d) => &mut d.children,
                Node::Root(c) => &mut c.children,
                _ => return,
            };
            if let Some(inserted_pos) = children.iter().position(|id| *id == new_id) {
                children.remove(inserted_pos);
            }
            if let Some(pos) = children.iter().position(|id| *id == sibling_id) {
                children.insert(pos, new_id);
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let mut doc = self.doc.borrow_mut();
        if let Some(Node::Element(data)) = doc.nodes.get_mut(*target) {
            for attr in attrs {
                let name = attr.name.local;
                if !data.attributes.iter().any(|(k, _)| k == &name) {
                    data.attributes.push((name, attr.value.to_string()));
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        let target_id = *target;
        let mut doc = self.doc.borrow_mut();

        if let Some(parent_id) = doc.parent_of(target_id) {
            doc.remove_child(parent_id, target_id);
        }
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut doc = self.doc.borrow_mut();
        let children: Vec<NodeId> = match doc.nodes.get(*node) {
            Some(Node::Element(d)) => d.children.clone(),
            Some(Node::Root(c)) => c.children.clone(),
            _ => return,
        };

        // Clear old parent's children
        match doc.nodes.get_mut(*node) {
            Some(Node::Element(d)) => d.children.clear(),
            Some(Node::Root(c)) => c.children.clear(),
            _ => return,
        }

        for child_id in children {
            doc.append_child(*new_parent, child_id);
        }
    }
}

/// Extract CSS text from `<style>` elements after parsing.
fn extract_style_texts(doc: &mut Document) {
    let style_element_ids: Vec<NodeId> = doc
        .nodes
        .iter()
        .filter_map(|(id, node)| {
            if let Node::Element(data) = node {
                if data.tag_name == local_name!("style") {
                    return Some(id);
                }
            }
            None
        })
        .collect();

    for style_id in style_element_ids {
        let mut style_text = String::new();
        if let Some(children) = doc.children_of(style_id).map(|c| c.to_vec()) {
            for child_id in children {
                if let Some(Node::Text(txt)) = doc.nodes.get(child_id) {
                    style_text.push_str(&txt.text);
                }
            }
        }
        if !style_text.is_empty() {
            doc.style_texts.push(style_text);
        }
    }
}

pub fn parse_html(html: &str) -> Document {
    let builder = DocumentBuilder {
        doc: RefCell::new(Document::new()),
    };

    let mut doc = parse_document(builder, Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap();

    extract_style_texts(&mut doc);
    doc
}
