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
        let mut classes = std::collections::HashSet::new();
        let mut id_val = None;
        for (k, v) in &attributes {
            if &**k == "class" {
                for c in v.split_whitespace() {
                    classes.insert(LocalName::from(c));
                }
            } else if &**k == "id" {
                id_val = Some(v.clone());
            }
        }

        let tag_name = name.local;
        let node = Node::Element(ElementData {
            tag_name,
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
        node_id
    }

    fn create_comment(&self, _text: StrTendril) -> NodeId {
        // Store comments as empty text nodes (ignored during layout/render).
        let mut doc = self.doc.borrow_mut();
        doc.add_node(Node::Text(TextData {
            text: String::new(),
            parent: None,
            prev_sibling: None,
            next_sibling: None,
        }))
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        let mut doc = self.doc.borrow_mut();
        doc.add_node(Node::Text(TextData {
            text: String::new(),
            parent: None,
            prev_sibling: None,
            next_sibling: None,
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
                if let Some(last_child_id) = doc.last_child_of(*parent) {
                    if let Some(Node::Text(existing)) = doc.nodes.get_mut(last_child_id) {
                        existing.text.push_str(&text_str);
                        return;
                    }
                }

                let id = doc.add_node(Node::Text(TextData {
                    text: text_str,
                    parent: None,
                    prev_sibling: None,
                    next_sibling: None,
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
                    prev_sibling: None,
                    next_sibling: None,
                }))
            }
        };

        let parent_id = match doc.parent_of(sibling_id) {
            Some(pid) => pid,
            None => return,
        };

        doc.append_child(parent_id, new_id);

        // Intrusive shift to place new_id before sibling_id
        let prev_sibling_of_new = doc.prev_sibling_of(new_id);
        
        if let Some(parent) = doc.nodes.get_mut(parent_id) {
            match parent {
                Node::Element(d) => {
                    if d.last_child == Some(new_id) {
                        d.last_child = prev_sibling_of_new;
                    }
                },
                Node::Root(c) => {
                    if c.last_child == Some(new_id) {
                        c.last_child = prev_sibling_of_new;
                    }
                },
                _ => return,
            }
        }
        
        let old_prev = doc.prev_sibling_of(sibling_id);
        
        // Remove new_id from its appending position (end)
        let new_prev = doc.prev_sibling_of(new_id);
        if let Some(p) = new_prev {
            if let Some(n) = doc.nodes.get_mut(p) {
                match n {
                    Node::Element(d) => d.next_sibling = None,
                    Node::Text(d) => d.next_sibling = None,
                    _ => {}
                }
            }
        }

        // Insert new_id before sibling_id
        if let Some(n) = doc.nodes.get_mut(new_id) {
            match n {
                Node::Element(d) => { d.next_sibling = Some(sibling_id); d.prev_sibling = old_prev; },
                Node::Text(d) => { d.next_sibling = Some(sibling_id); d.prev_sibling = old_prev; },
                _ => {}
            }
        }

        if let Some(s) = doc.nodes.get_mut(sibling_id) {
            match s {
                Node::Element(d) => d.prev_sibling = Some(new_id),
                Node::Text(d) => d.prev_sibling = Some(new_id),
                _ => {}
            }
        }
        
        if let Some(p) = old_prev {
            if let Some(n) = doc.nodes.get_mut(p) {
                match n {
                    Node::Element(d) => d.next_sibling = Some(new_id),
                    Node::Text(d) => d.next_sibling = Some(new_id),
                    _ => {}
                }
            }
        } else {
            // It's the new first child
            if let Some(parent) = doc.nodes.get_mut(parent_id) {
                match parent {
                    Node::Element(d) => d.first_child = Some(new_id),
                    Node::Root(c) => c.first_child = Some(new_id),
                    _ => {}
                }
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
        
        let mut children = Vec::new();
        let mut child = doc.first_child_of(*node);
        while let Some(c) = child {
            children.push(c);
            child = doc.next_sibling_of(c);
        }

        // Clear old parent's children
        match doc.nodes.get_mut(*node) {
            Some(Node::Element(d)) => { d.first_child = None; d.last_child = None; },
            Some(Node::Root(c)) => { c.first_child = None; c.last_child = None; },
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
        
        let mut child_id_opt = doc.first_child_of(style_id);
        while let Some(child_id) = child_id_opt {
            if let Some(Node::Text(txt)) = doc.nodes.get(child_id) {
                style_text.push_str(&txt.text);
            }
            child_id_opt = doc.next_sibling_of(child_id);
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
