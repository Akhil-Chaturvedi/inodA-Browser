//! Arena-based DOM tree.
//!
//! Uses `generational_arena` for O(1) insertion and deletion without
//! index invalidation. Nodes can be safely removed and their indices
//! will not be reused until the generation wraps.

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
    Text(String),
    Root(Vec<NodeId>),
}

#[derive(Debug, Clone)]
pub struct ElementData {
    pub tag_name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<NodeId>,
}

/// A node mapped with its active computed CSS style properties.
#[derive(Debug)]
pub struct StyledNode {
    pub node_id: NodeId,
    pub specified_values: Vec<(String, String)>,
    pub children: Vec<StyledNode>,
}

impl Default for Document {
    fn default() -> Self {
        let mut arena = Arena::new();
        let root_id = arena.insert(Node::Root(Vec::new()));
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
        self.nodes.remove(id)
    }

    pub fn append_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        if let Some(parent) = self.nodes.get_mut(parent_id) {
            match parent {
                Node::Element(data) => {
                    data.children.push(child_id);
                }
                Node::Root(children) => {
                    children.push(child_id);
                }
                Node::Text(_) => {
                    // Cannot append to text
                }
            }
        }
    }

    /// Remove child_id from the children list of parent_id.
    pub fn remove_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        if let Some(parent) = self.nodes.get_mut(parent_id) {
            match parent {
                Node::Element(data) => {
                    data.children.retain(|id| *id != child_id);
                }
                Node::Root(children) => {
                    children.retain(|id| *id != child_id);
                }
                Node::Text(_) => {}
            }
        }
    }

    /// Get the children of a node, if it has any.
    pub fn children_of(&self, node_id: NodeId) -> Option<&[NodeId]> {
        match self.nodes.get(node_id)? {
            Node::Element(data) => Some(&data.children),
            Node::Root(children) => Some(children),
            Node::Text(_) => None,
        }
    }
}
