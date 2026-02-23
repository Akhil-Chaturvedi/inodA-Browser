/// A simple, flat Arena-based DOM Tree to minimize pointer allocations and reduce
/// memory footprint on constrained devices.
#[derive(Debug, Clone)]
pub struct Document {
    pub nodes: Vec<Node>,
    pub root_id: NodeId,
    pub style_texts: Vec<String>,
}

pub type NodeId = usize;

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
        Document {
            nodes: vec![Node::Root(Vec::new())],
            root_id: 0,
            style_texts: Vec::new(),
        }
    }
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, node: Node) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(node);
        id
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
}
