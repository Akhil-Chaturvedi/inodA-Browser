//! Arena-based DOM tree.
//!
//! Uses `generational_arena` for O(1) insertion and deletion without
//! index invalidation. Nodes can be safely removed and their indices
//! will not be reused until the generation wraps.
//!
//! Tag names and attribute keys are interned using `markup5ever::LocalName`
//! to minimize memory usage and enable O(1) comparison.
//!
//! Parent pointers are stored directly on nodes to keep parent traversal
//! cache-friendly and avoid hashmap overhead in embedded environments.

use generational_arena::{Arena, Index};

/// A DOM document backed by a generational arena.
#[derive(Debug, Clone)]
pub struct Document {
    pub nodes: Arena<Node>,
    pub root_id: NodeId,
    /// Raw CSS text extracted from `<style>` elements during parsing.
    pub style_texts: Vec<String>,
}

/// A handle into the arena. Generational indices prevent ABA problems.
pub type NodeId = Index;

#[derive(Debug, Clone)]
pub enum Node {
    Element(ElementData),
    Text(TextData),
    Root(RootData),
}

#[derive(Debug, Clone)]
pub struct ElementData {
    pub tag_name: markup5ever::LocalName,
    pub attributes: Vec<(markup5ever::LocalName, String)>,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub struct TextData {
    pub text: String,
    pub parent: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub struct RootData {
    pub children: Vec<NodeId>,
}

/// A node mapped with its active computed CSS style properties.
#[derive(Debug)]
pub struct StyledNode {
    pub node_id: NodeId,
    pub specified_values: Vec<(string_cache::DefaultAtom, String)>,
    pub children: Vec<StyledNode>,
}

impl Default for Document {
    fn default() -> Self {
        let mut arena = Arena::new();
        let root_id = arena.insert(Node::Root(RootData { children: Vec::new() }));
        Document {
            nodes: arena,
            root_id,
            style_texts: Vec::new(),
        }
    }
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, node: Node) -> NodeId {
        self.nodes.insert(node)
    }

    pub fn remove_node(&mut self, id: NodeId) -> Option<Node> {
        if let Some(parent_id) = self.parent_of(id) {
            self.remove_child(parent_id, id);
        }
        self.nodes.remove(id)
    }

    pub fn append_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        if let Some(old_parent) = self.parent_of(child_id) {
            self.remove_child(old_parent, child_id);
        }

        if let Some(parent) = self.nodes.get_mut(parent_id) {
            match parent {
                Node::Element(data) => data.children.push(child_id),
                Node::Root(root) => root.children.push(child_id),
                Node::Text(_) => return,
            }
        }

        self.set_parent(child_id, Some(parent_id));
    }

    /// Remove child_id from the children list of parent_id.
    pub fn remove_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        if let Some(parent) = self.nodes.get_mut(parent_id) {
            match parent {
                Node::Element(data) => data.children.retain(|id| *id != child_id),
                Node::Root(root) => root.children.retain(|id| *id != child_id),
                Node::Text(_) => {}
            }
        }
        self.set_parent(child_id, None);
    }

    fn set_parent(&mut self, node_id: NodeId, parent: Option<NodeId>) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            match node {
                Node::Element(data) => data.parent = parent,
                Node::Text(data) => data.parent = parent,
                Node::Root(_) => {}
            }
        }
    }

    /// Get the parent of a node via O(1) in-node lookup.
    pub fn parent_of(&self, node_id: NodeId) -> Option<NodeId> {
        match self.nodes.get(node_id)? {
            Node::Element(data) => data.parent,
            Node::Text(data) => data.parent,
            Node::Root(_) => None,
        }
    }

    /// Get the children of a node, if it has any.
    pub fn children_of(&self, node_id: NodeId) -> Option<&[NodeId]> {
        match self.nodes.get(node_id)? {
            Node::Element(data) => Some(&data.children),
            Node::Root(root) => Some(&root.children),
            Node::Text(_) => None,
        }
    }
}
