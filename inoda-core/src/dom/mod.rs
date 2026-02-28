//! Arena-based DOM tree.
//!
//! Uses `generational_arena` for O(1) insertion and deletion without
//! index invalidation. Nodes can be safely removed and their indices
//! will not be reused until the generation wraps.
//!
//! Tag names and attribute keys are interned using `string_cache::DefaultAtom`
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
    /// O(1) lookup map for `getElementById`.
    pub id_map: std::collections::HashMap<String, NodeId>,
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
    pub tag_name: string_cache::DefaultAtom,
    pub attributes: Vec<(string_cache::DefaultAtom, String)>,
    pub classes: std::collections::HashSet<string_cache::DefaultAtom>,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub struct TextData {
    pub text: String,
    pub parent: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub struct RootData {
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
}

/// A node mapped with its active computed CSS style properties.
#[derive(Debug)]
pub struct StyledNode {
    pub node_id: NodeId,
    pub specified_values: std::rc::Rc<Vec<(string_cache::DefaultAtom, String)>>,
    pub children: Vec<StyledNode>,
}

impl Default for Document {
    fn default() -> Self {
        let mut arena = Arena::new();
        let root_id = arena.insert(Node::Root(RootData {
            first_child: None,
            last_child: None,
        }));
        Document {
            nodes: arena,
            root_id,
            style_texts: Vec::new(),
            id_map: std::collections::HashMap::new(),
        }
    }
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, node: Node) -> NodeId {
        let id = self.nodes.insert(node);
        if let Some(Node::Element(data)) = self.nodes.get(id) {
            if let Some((_, id_val)) = data.attributes.iter().find(|(k, _)| &**k == "id") {
                self.id_map.insert(id_val.clone(), id);
            }
        }
        id
    }

    pub fn remove_node(&mut self, id: NodeId) -> Option<Node> {
        // 1. Unlink from parent and siblings
        if let Some(parent_id) = self.parent_of(id) {
            self.remove_child(parent_id, id);
        }

        // 2. Remove id from id_map
        if let Some(Node::Element(data)) = self.nodes.get(id) {
            if let Some((_, id_val)) = data.attributes.iter().find(|(k, _)| &**k == "id") {
                self.id_map.remove(id_val);
            }
        }

        // 3. Recursively remove children using intrusive list
        let mut current_child = self.first_child_of(id);
        while let Some(child_id) = current_child {
            current_child = self.next_sibling_of(child_id);
            self.remove_node(child_id);
        }

        self.nodes.remove(id)
    }

    pub fn append_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        if let Some(old_parent) = self.parent_of(child_id) {
            self.remove_child(old_parent, child_id);
        }

        let old_last_child = match self.nodes.get_mut(parent_id) {
            Some(Node::Element(data)) => {
                let last = data.last_child;
                if data.first_child.is_none() { data.first_child = Some(child_id); }
                data.last_child = Some(child_id);
                last
            }
            Some(Node::Root(root)) => {
                let last = root.last_child;
                if root.first_child.is_none() { root.first_child = Some(child_id); }
                root.last_child = Some(child_id);
                last
            }
            _ => return,
        };

        if let Some(old_last) = old_last_child {
            self.set_next_sibling(old_last, Some(child_id));
        }
        
        self.set_prev_sibling(child_id, old_last_child);
        self.set_next_sibling(child_id, None);
        self.set_parent(child_id, Some(parent_id));
    }

    pub fn remove_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        let prev = self.prev_sibling_of(child_id);
        let next = self.next_sibling_of(child_id);

        if let Some(parent) = self.nodes.get_mut(parent_id) {
            match parent {
                Node::Element(data) => {
                    if data.first_child == Some(child_id) { data.first_child = next; }
                    if data.last_child == Some(child_id) { data.last_child = prev; }
                }
                Node::Root(root) => {
                    if root.first_child == Some(child_id) { root.first_child = next; }
                    if root.last_child == Some(child_id) { root.last_child = prev; }
                }
                Node::Text(_) => {}
            }
        }

        if let Some(p) = prev { self.set_next_sibling(p, next); }
        if let Some(n) = next { self.set_prev_sibling(n, prev); }

        self.set_parent(child_id, None);
        self.set_prev_sibling(child_id, None);
        self.set_next_sibling(child_id, None);
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

    /// Get the first child of a node via O(1) in-node lookup.
    pub fn first_child_of(&self, node_id: NodeId) -> Option<NodeId> {
        match self.nodes.get(node_id)? {
            Node::Element(data) => data.first_child,
            Node::Root(data) => data.first_child,
            Node::Text(_) => None,
        }
    }

    /// Get the last child of a node.
    pub fn last_child_of(&self, node_id: NodeId) -> Option<NodeId> {
        match self.nodes.get(node_id)? {
            Node::Element(data) => data.last_child,
            Node::Root(data) => data.last_child,
            Node::Text(_) => None,
        }
    }

    /// Get the next sibling of a node.
    pub fn next_sibling_of(&self, node_id: NodeId) -> Option<NodeId> {
        match self.nodes.get(node_id)? {
            Node::Element(data) => data.next_sibling,
            Node::Text(data) => data.next_sibling,
            Node::Root(_) => None,
        }
    }

    /// Get the previous sibling of a node.
    pub fn prev_sibling_of(&self, node_id: NodeId) -> Option<NodeId> {
        match self.nodes.get(node_id)? {
            Node::Element(data) => data.prev_sibling,
            Node::Text(data) => data.prev_sibling,
            Node::Root(_) => None,
        }
    }

    fn set_next_sibling(&mut self, node_id: NodeId, next: Option<NodeId>) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            match node {
                Node::Element(data) => data.next_sibling = next,
                Node::Text(data) => data.next_sibling = next,
                Node::Root(_) => {}
            }
        }
    }

    fn set_prev_sibling(&mut self, node_id: NodeId, prev: Option<NodeId>) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            match node {
                Node::Element(data) => data.prev_sibling = prev,
                Node::Text(data) => data.prev_sibling = prev,
                Node::Root(_) => {}
            }
        }
    }
}
