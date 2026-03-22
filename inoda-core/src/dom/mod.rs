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
//!
//! `ComputedStyle` is stored directly inside `ElementData` and `TextData`, populated once
//! during style resolution. Layout and rendering read directly from it rather
//! than scanning dynamic trees.

use generational_arena::{Arena, Index};

#[derive(Debug, Clone, Copy)]
pub struct TextMeasureContext {
    pub node_id: NodeId,
    pub font_size: f32,
}

/// A DOM document backed by a generational arena.
pub struct Document {
    pub nodes: Arena<Node>,
    pub root_id: NodeId,
    /// Persistent CSS parsed actively when `<style>` tags change.
    pub stylesheet: crate::css::StyleSheet,
    /// O(1) lookup map for `getElementById`.
    pub id_map: std::collections::HashMap<String, NodeId>,
    /// Iterative deletion queue used by `remove_node` to avoid recursive stack overflow.
    pub dead_nodes: Vec<NodeId>,
    /// Layout invalidation flag. True if DOM was mutated since the last render.
    pub dirty: bool,
    pub taffy_tree: taffy::TaffyTree<TextMeasureContext>,
}

/// A handle into the arena. Generational indices prevent ABA problems.
pub type NodeId = Index;

#[derive(Debug, Clone)]
pub enum Node {
    Element(ElementData),
    Text(TextData),
    Root(RootData),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocalName {
    Standard(string_cache::DefaultAtom),
    Custom(String),
}

impl LocalName {
    pub fn new(tag: &str) -> Self {
        if matches!(
            tag,
            "a" | "abbr"
                | "address"
                | "area"
                | "article"
                | "aside"
                | "audio"
                | "b"
                | "base"
                | "bdi"
                | "bdo"
                | "blockquote"
                | "body"
                | "br"
                | "button"
                | "canvas"
                | "caption"
                | "cite"
                | "code"
                | "col"
                | "colgroup"
                | "data"
                | "datalist"
                | "dd"
                | "del"
                | "details"
                | "dfn"
                | "dialog"
                | "div"
                | "dl"
                | "dt"
                | "em"
                | "embed"
                | "fieldset"
                | "figcaption"
                | "figure"
                | "footer"
                | "form"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "head"
                | "header"
                | "hr"
                | "html"
                | "i"
                | "iframe"
                | "img"
                | "input"
                | "ins"
                | "kbd"
                | "label"
                | "legend"
                | "li"
                | "link"
                | "main"
                | "map"
                | "mark"
                | "meta"
                | "meter"
                | "nav"
                | "noscript"
                | "object"
                | "ol"
                | "optgroup"
                | "option"
                | "output"
                | "p"
                | "param"
                | "picture"
                | "pre"
                | "progress"
                | "q"
                | "rp"
                | "rt"
                | "ruby"
                | "s"
                | "samp"
                | "script"
                | "section"
                | "select"
                | "small"
                | "source"
                | "span"
                | "strong"
                | "style"
                | "sub"
                | "summary"
                | "sup"
                | "table"
                | "tbody"
                | "td"
                | "template"
                | "textarea"
                | "tfoot"
                | "th"
                | "thead"
                | "time"
                | "title"
                | "tr"
                | "track"
                | "u"
                | "ul"
                | "var"
                | "video"
                | "wbr"
        ) {
            LocalName::Standard(string_cache::DefaultAtom::from(tag))
        } else {
            LocalName::Custom(tag.to_string())
        }
    }
}

impl std::ops::Deref for LocalName {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        match self {
            LocalName::Standard(atom) => &**atom,
            LocalName::Custom(s) => s.as_str(),
        }
    }
}

impl std::fmt::Display for LocalName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalName::Standard(atom) => write!(f, "{}", atom),
            LocalName::Custom(s) => write!(f, "{}", s),
        }
    }
}

/// Strongly-typed CSS property names for O(1) matching during the cascade.
/// Avoids dereferencing `DefaultAtom` to `&str` for every property comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertyName {
    Display,
    FlexDirection,
    Width,
    Height,
    MarginTop,
    MarginRight,
    MarginBottom,
    MarginLeft,
    PaddingTop,
    PaddingRight,
    PaddingBottom,
    PaddingLeft,
    BorderTopWidth,
    BorderRightWidth,
    BorderBottomWidth,
    BorderLeftWidth,
    BackgroundColor,
    BorderColor,
    Color,
    FontSize,
    FontFamily,
    FontWeight,
    LineHeight,
    TextAlign,
    Visibility,
    Other(u64), // hash of unrecognized property name
}

impl PropertyName {
    pub fn from_str(s: &str) -> Self {
        match s {
            "display" => PropertyName::Display,
            "flex-direction" => PropertyName::FlexDirection,
            "width" => PropertyName::Width,
            "height" => PropertyName::Height,
            "margin-top" => PropertyName::MarginTop,
            "margin-right" => PropertyName::MarginRight,
            "margin-bottom" => PropertyName::MarginBottom,
            "margin-left" => PropertyName::MarginLeft,
            "padding-top" => PropertyName::PaddingTop,
            "padding-right" => PropertyName::PaddingRight,
            "padding-bottom" => PropertyName::PaddingBottom,
            "padding-left" => PropertyName::PaddingLeft,
            "border-top-width" => PropertyName::BorderTopWidth,
            "border-right-width" => PropertyName::BorderRightWidth,
            "border-bottom-width" => PropertyName::BorderBottomWidth,
            "border-left-width" => PropertyName::BorderLeftWidth,
            "background-color" => PropertyName::BackgroundColor,
            "border-color" => PropertyName::BorderColor,
            "color" => PropertyName::Color,
            "font-size" => PropertyName::FontSize,
            "font-family" => PropertyName::FontFamily,
            "font-weight" => PropertyName::FontWeight,
            "line-height" => PropertyName::LineHeight,
            "text-align" => PropertyName::TextAlign,
            "visibility" => PropertyName::Visibility,
            other => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                other.hash(&mut hasher);
                PropertyName::Other(hasher.finish())
            }
        }
    }

    /// Returns true if this property is CSS-inheritable.
    pub fn is_inheritable(&self) -> bool {
        matches!(
            self,
            PropertyName::Color
                | PropertyName::FontFamily
                | PropertyName::FontSize
                | PropertyName::FontWeight
                | PropertyName::LineHeight
                | PropertyName::TextAlign
                | PropertyName::Visibility
        )
    }
}

#[derive(Debug, Clone)]
pub struct ElementData {
    pub tag_name: LocalName,
    pub attributes: Vec<(string_cache::DefaultAtom, String)>,
    pub classes: Vec<String>,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub computed: ComputedStyle,
    pub taffy_node: Option<taffy::NodeId>,
}

#[derive(Debug, Clone)]
pub struct TextData {
    pub text: String,
    pub parent: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub computed: ComputedStyle,
    pub taffy_node: Option<taffy::NodeId>,
}

#[derive(Debug, Clone)]
pub struct RootData {
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub taffy_node: Option<taffy::NodeId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StyleValue {
    Keyword(string_cache::DefaultAtom),
    LengthPx(f32),
    Percent(f32),
    ViewportWidth(f32),
    ViewportHeight(f32),
    Em(f32),
    Rem(f32),
    Number(f32),
    Color(u8, u8, u8),
    Auto,
    None,
}

/// Pre-calculated native CSS properties to eliminate O(N) tuple lookups during Layout and Rendering loops.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub display: string_cache::DefaultAtom,
    pub flex_direction: string_cache::DefaultAtom,
    pub width: StyleValue,
    pub height: StyleValue,
    pub margin: [StyleValue; 4],
    pub padding: [StyleValue; 4],
    pub border_width: [StyleValue; 4],
    pub bg_color: Option<(u8, u8, u8)>,
    pub border_color: Option<(u8, u8, u8)>,
    pub font_size: f32,
    pub color: (u8, u8, u8),
}

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle {
            display: string_cache::DefaultAtom::from("block"),
            flex_direction: string_cache::DefaultAtom::from("row"),
            width: StyleValue::Auto,
            height: StyleValue::Auto,
            margin: [
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
            ],
            padding: [
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
            ],
            border_width: [
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
                StyleValue::LengthPx(0.0),
            ],
            bg_color: None,
            border_color: None,
            font_size: 16.0,
            color: (0, 0, 0),
        }
    }
}

impl Default for Document {
    fn default() -> Self {
        let mut arena = Arena::new();
        let root_id = arena.insert(Node::Root(RootData {
            first_child: None,
            last_child: None,
            taffy_node: None,
        }));
        Document {
            nodes: arena,
            root_id,
            stylesheet: crate::css::StyleSheet::default(),
            id_map: std::collections::HashMap::new(),
            dead_nodes: Vec::new(),
            dirty: true,
            taffy_tree: taffy::TaffyTree::new(),
        }
    }
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, node: Node) -> NodeId {
        self.dirty = true;
        let id = self.nodes.insert(node);
        if let Some(Node::Element(data)) = self.nodes.get(id) {
            if let Some((_, id_val)) = data.attributes.iter().find(|(k, _)| &**k == "id") {
                self.id_map.insert(id_val.clone(), id);
            }
        }
        id
    }

    pub fn remove_node(&mut self, id: NodeId) -> Option<Node> {
        self.dirty = true;
        // 1. Unlink from parent and siblings
        if let Some(parent_id) = self.parent_of(id) {
            self.remove_child(parent_id, id);
        }

        self.dead_nodes.push(id);
        let mut root_node = None;

        while let Some(current_id) = self.dead_nodes.pop() {
            let mut current_child = self.first_child_of(current_id);
            while let Some(child_id) = current_child {
                current_child = self.next_sibling_of(child_id);
                self.dead_nodes.push(child_id);
            }

            if let Some(node) = self.nodes.remove(current_id) {
                if let Node::Element(ref data) = node {
                    if let Some((_, id_val)) = data.attributes.iter().find(|(k, _)| &**k == "id") {
                        if self.id_map.get(id_val) == Some(&current_id) {
                            self.id_map.remove(id_val);
                        }
                    }
                }
                if current_id == id {
                    root_node = Some(node);
                }
            }
        }

        root_node
    }

    pub fn append_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        self.dirty = true;
        if let Some(old_parent) = self.parent_of(child_id) {
            self.remove_child(old_parent, child_id);
        }

        let old_last_child = match self.nodes.get_mut(parent_id) {
            Some(Node::Element(data)) => {
                let last = data.last_child;
                if data.first_child.is_none() {
                    data.first_child = Some(child_id);
                }
                data.last_child = Some(child_id);
                last
            }
            Some(Node::Root(root)) => {
                let last = root.last_child;
                if root.first_child.is_none() {
                    root.first_child = Some(child_id);
                }
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
        self.dirty = true;
        let prev = self.prev_sibling_of(child_id);
        let next = self.next_sibling_of(child_id);

        if let Some(parent) = self.nodes.get_mut(parent_id) {
            match parent {
                Node::Element(data) => {
                    if data.first_child == Some(child_id) {
                        data.first_child = next;
                    }
                    if data.last_child == Some(child_id) {
                        data.last_child = prev;
                    }
                }
                Node::Root(root) => {
                    if root.first_child == Some(child_id) {
                        root.first_child = next;
                    }
                    if root.last_child == Some(child_id) {
                        root.last_child = prev;
                    }
                }
                Node::Text(_) => {}
            }
        }

        if let Some(p) = prev {
            self.set_next_sibling(p, next);
        }
        if let Some(n) = next {
            self.set_prev_sibling(n, prev);
        }

        self.set_parent(child_id, None);
        self.set_prev_sibling(child_id, None);
        self.set_next_sibling(child_id, None);
    }

    pub fn is_attached_to_root(&self, node_id: NodeId) -> bool {
        let mut current_id = node_id;
        while current_id != self.root_id {
            if let Some(parent_id) = self.parent_of(current_id) {
                current_id = parent_id;
            } else {
                return false;
            }
        }
        true
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
