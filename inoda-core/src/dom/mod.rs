//! Arena-based DOM tree.
//!
//! Uses `generational_arena` for O(1) insertion and deletion without
//! index invalidation. Nodes can be safely removed and their indices
//! will not be reused until the generation wraps.
//!
//! Standard HTML tag names are interned as `string_cache::DefaultAtom`
//! for pointer-equality comparison. Custom element names and all attribute
//! keys and values are stored as `String` to prevent unbounded growth of
//! the global intern pool.
//!
//! Parent pointers are stored directly on nodes to keep parent traversal
//! cache-friendly and avoid hashmap overhead in embedded environments.
//!
//! `ComputedStyle` is stored directly inside `ElementData` and `TextData`, populated once
//! during style resolution. Layout and rendering read directly from it rather
//! than scanning dynamic trees.

use generational_arena::{Arena, Index};

mod tags;

pub const MAX_ATTRIBUTES: usize = 32;
pub const MAX_ATTRIBUTE_VALUE_LEN: usize = 16384; // 16 KB per attribute
/// Maximum number of DOM nodes allowed per document.
/// Prevents memory exhaustion on malicious or malformed HTML.
pub const MAX_NODES: usize = 65536;

#[derive(Debug, Clone, Copy)]
pub struct TextMeasureContext {
    pub node_id: NodeId,
    pub font_size: f32,
    pub max_intrinsic_width: f32,
    pub min_intrinsic_width: f32,
    /// Last definite width from Taffy's measure callback (cache when unchanged between probes).
    pub last_measure_width: Option<f32>,
    pub last_line_count: f32,
}

/// A DOM document backed by a generational arena.
pub struct Document {
    pub nodes: Arena<Node>,
    pub root_id: NodeId,
    pub root_font_size: f32,
    /// Persistent CSS parsed actively when `<style>` tags change.
    pub stylesheet: crate::css::StyleSheet,
    /// O(1) lookup map for `getElementById`.
    pub id_map: std::collections::HashMap<String, NodeId>,
    /// Iterative deletion queue used by `remove_node` to avoid recursive stack overflow.
    pub dead_nodes: Vec<NodeId>,
    /// Layout invalidation flag. True if DOM was mutated since the last render.
    pub dirty: bool,
    /// Stylesheet invalidation flag. True if `<style>` tags were added or removed.
    pub styles_dirty: bool,
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
    /// `tag` must be ASCII-lowercase (HTML tokenizer and `createElement` enforce this).
    pub fn new(tag: &str) -> Self {
        if tags::HTML_TAGS.contains(tag) {
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
    AlignItems,
    JustifyContent,
    FlexWrap,
    FlexGrow,
    FlexShrink,
    RowGap,
    ColumnGap,
    MinWidth,
    MaxWidth,
    MinHeight,
    MaxHeight,
}

pub const NUM_PROPERTIES: usize = 36;

impl PropertyName {
    pub fn to_index(self) -> usize {
        match self {
            PropertyName::Display => 0,
            PropertyName::FlexDirection => 1,
            PropertyName::Width => 2,
            PropertyName::Height => 3,
            PropertyName::MarginTop => 4,
            PropertyName::MarginRight => 5,
            PropertyName::MarginBottom => 6,
            PropertyName::MarginLeft => 7,
            PropertyName::PaddingTop => 8,
            PropertyName::PaddingRight => 9,
            PropertyName::PaddingBottom => 10,
            PropertyName::PaddingLeft => 11,
            PropertyName::BorderTopWidth => 12,
            PropertyName::BorderRightWidth => 13,
            PropertyName::BorderBottomWidth => 14,
            PropertyName::BorderLeftWidth => 15,
            PropertyName::BackgroundColor => 16,
            PropertyName::BorderColor => 17,
            PropertyName::Color => 18,
            PropertyName::FontSize => 19,
            PropertyName::FontFamily => 20,
            PropertyName::FontWeight => 21,
            PropertyName::LineHeight => 22,
            PropertyName::TextAlign => 23,
            PropertyName::Visibility => 24,
            PropertyName::AlignItems => 25,
            PropertyName::JustifyContent => 26,
            PropertyName::FlexWrap => 27,
            PropertyName::FlexGrow => 28,
            PropertyName::FlexShrink => 29,
            PropertyName::RowGap => 30,
            PropertyName::ColumnGap => 31,
            PropertyName::MinWidth => 32,
            PropertyName::MaxWidth => 33,
            PropertyName::MinHeight => 34,
            PropertyName::MaxHeight => 35,
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
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
            "align-items" => PropertyName::AlignItems,
            "justify-content" => PropertyName::JustifyContent,
            "flex-wrap" => PropertyName::FlexWrap,
            "flex-grow" => PropertyName::FlexGrow,
            "flex-shrink" => PropertyName::FlexShrink,
            "row-gap" => PropertyName::RowGap,
            "column-gap" => PropertyName::ColumnGap,
            "min-width" => PropertyName::MinWidth,
            "max-width" => PropertyName::MaxWidth,
            "min-height" => PropertyName::MinHeight,
            "max-height" => PropertyName::MaxHeight,
            _ => return None,
        })
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

    /// Returns the CSS property name string (kebab-case) for this variant.
    /// Used by `getAttribute("style")` to reconstruct inline style text.
    pub fn as_str(self) -> &'static str {
        match self {
            PropertyName::Display => "display",
            PropertyName::FlexDirection => "flex-direction",
            PropertyName::Width => "width",
            PropertyName::Height => "height",
            PropertyName::MarginTop => "margin-top",
            PropertyName::MarginRight => "margin-right",
            PropertyName::MarginBottom => "margin-bottom",
            PropertyName::MarginLeft => "margin-left",
            PropertyName::PaddingTop => "padding-top",
            PropertyName::PaddingRight => "padding-right",
            PropertyName::PaddingBottom => "padding-bottom",
            PropertyName::PaddingLeft => "padding-left",
            PropertyName::BorderTopWidth => "border-top-width",
            PropertyName::BorderRightWidth => "border-right-width",
            PropertyName::BorderBottomWidth => "border-bottom-width",
            PropertyName::BorderLeftWidth => "border-left-width",
            PropertyName::BackgroundColor => "background-color",
            PropertyName::BorderColor => "border-color",
            PropertyName::Color => "color",
            PropertyName::FontSize => "font-size",
            PropertyName::FontFamily => "font-family",
            PropertyName::FontWeight => "font-weight",
            PropertyName::LineHeight => "line-height",
            PropertyName::TextAlign => "text-align",
            PropertyName::Visibility => "visibility",
            PropertyName::AlignItems => "align-items",
            PropertyName::JustifyContent => "justify-content",
            PropertyName::FlexWrap => "flex-wrap",
            PropertyName::FlexGrow => "flex-grow",
            PropertyName::FlexShrink => "flex-shrink",
            PropertyName::RowGap => "row-gap",
            PropertyName::ColumnGap => "column-gap",
            PropertyName::MinWidth => "min-width",
            PropertyName::MaxWidth => "max-width",
            PropertyName::MinHeight => "min-height",
            PropertyName::MaxHeight => "max-height",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ElementData {
    pub tag_name: LocalName,
    pub attributes: Vec<(String, String)>,
    /// Space-separated class list. Stored as a flat string to minimize heap fragments.
    pub classes: String,
    /// Pre-parsed inline styles to bypass re-parsing during the CSS cascade.
    pub cached_inline_styles: Option<Vec<(PropertyName, StyleValue)>>,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub computed: ComputedStyle,
    pub taffy_node: Option<taffy::NodeId>,
    pub js_handles: usize,
    /// Set true when styles or content change, triggering a text re-shape.
    pub layout_dirty: bool,
    /// Set true when attributes or classes mutate, demanding a CSS cascade recompute.
    pub styles_dirty: bool,
}

impl ElementData {
    pub fn new(tag_name: LocalName) -> Self {
        ElementData {
            tag_name,
            attributes: Vec::with_capacity(4),
            classes: String::new(),
            cached_inline_styles: None,
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            computed: ComputedStyle::default(),
            taffy_node: None,
            js_handles: 0,
            layout_dirty: false,
            styles_dirty: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextData {
    pub text: String,
    pub parent: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    /// Lightweight computed style for text nodes — only carries the two
    /// inheritable properties that layout and rendering actually read
    /// (`font_size`, `color`).  Avoids allocating the full 36-field
    /// `ComputedStyle` struct for every text node in the document.
    pub computed: TextComputedStyle,
    pub taffy_node: Option<taffy::NodeId>,
    pub js_handles: usize,
    /// Set true when text content changes, triggering a re-shape.
    pub layout_dirty: bool,
    /// Triggers inheritable CSS recompute
    pub styles_dirty: bool,
}

impl TextData {
    pub fn new(text: String) -> Self {
        TextData {
            text,
            parent: None,
            prev_sibling: None,
            next_sibling: None,
            computed: TextComputedStyle::default(),
            taffy_node: None,
            js_handles: 0,
            layout_dirty: false,
            styles_dirty: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RootData {
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub taffy_node: Option<taffy::NodeId>,
    pub js_handles: usize,
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
    Color(u8, u8, u8, u8),
    Auto,
    None,
}

impl Eq for StyleValue {}



#[derive(Debug, Clone, PartialEq)]
pub enum DisplayKeyword { Block, Inline, InlineBlock, Flex, Grid, None, ListItem }

#[derive(Debug, Clone, PartialEq)]
pub enum FlexDirectionKeyword { Row, Column }

#[derive(Debug, Clone, PartialEq)]
pub enum AlignItemsKeyword { FlexStart, FlexEnd, Center, Baseline, Stretch }

#[derive(Debug, Clone, PartialEq)]
pub enum JustifyContentKeyword { FlexStart, FlexEnd, Center, SpaceBetween, SpaceAround, SpaceEvenly }

#[derive(Debug, Clone, PartialEq)]
pub enum FlexWrapKeyword { Wrap, WrapReverse, NoWrap }

/// Pre-calculated native CSS properties to eliminate O(N) tuple lookups during Layout and Rendering loops.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub display: DisplayKeyword,
    pub flex_direction: FlexDirectionKeyword,
    pub align_items: AlignItemsKeyword,
    pub justify_content: JustifyContentKeyword,
    pub flex_wrap: FlexWrapKeyword,
    pub width: StyleValue,
    pub height: StyleValue,
    pub min_width: StyleValue,
    pub max_width: StyleValue,
    pub min_height: StyleValue,
    pub max_height: StyleValue,
    /// Top, Right, Bottom, Left. Inline for cache locality and to reduce allocation overhead.
    pub margin: [StyleValue; 4],
    pub padding: [StyleValue; 4],
    pub border_width: [StyleValue; 4],
    pub row_gap: StyleValue,
    pub column_gap: StyleValue,
    pub bg_color: Option<(u8, u8, u8, u8)>,
    pub border_color: Option<(u8, u8, u8, u8)>,
    pub font_size: f32,
    pub color: (u8, u8, u8, u8),
    pub flex_grow: f32,
    pub flex_shrink: f32,
}

impl Eq for ComputedStyle {}

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle {
            display: DisplayKeyword::Block,
            flex_direction: FlexDirectionKeyword::Row,
            align_items: AlignItemsKeyword::Stretch,
            justify_content: JustifyContentKeyword::FlexStart,
            flex_wrap: FlexWrapKeyword::NoWrap,
            width: StyleValue::Auto,
            height: StyleValue::Auto,
            min_width: StyleValue::Auto,
            max_width: StyleValue::Auto,
            min_height: StyleValue::Auto,
            max_height: StyleValue::Auto,
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
            row_gap: StyleValue::LengthPx(0.0),
            column_gap: StyleValue::LengthPx(0.0),
            bg_color: None,
            border_color: None,
            font_size: 16.0,
            color: (0, 0, 0, 255),
            flex_grow: 0.0,
            flex_shrink: 1.0,
        }
    }
}

/// Lightweight computed style for text nodes.  Text nodes only carry the two
/// inheritable properties that layout and rendering actually read (`font_size`
/// and `color`), avoiding the 36-field `ComputedStyle` overhead for every text
/// node in the document.
#[derive(Debug, Clone, PartialEq)]
pub struct TextComputedStyle {
    pub font_size: f32,
    pub color: (u8, u8, u8, u8),
}

impl Eq for TextComputedStyle {}

impl Default for TextComputedStyle {
    fn default() -> Self {
        TextComputedStyle {
            font_size: 16.0,
            color: (0, 0, 0, 255),
        }
    }
}

impl TextComputedStyle {
    /// Construct from the inheritable fields of a parent `ComputedStyle`.
    /// Used by the cascade to propagate inherited values into text nodes.
    pub fn from_computed(src: &ComputedStyle) -> Self {
        TextComputedStyle {
            font_size: src.font_size,
            color: src.color,
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
            js_handles: 0,
        }));
        Document {
            nodes: arena,
            root_id,
            root_font_size: 16.0,
            stylesheet: crate::css::StyleSheet::default(),
            id_map: std::collections::HashMap::new(),
            dead_nodes: Vec::new(),
            dirty: true,
            styles_dirty: true,
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
        
        let node_copy = self.nodes.get(id).cloned();

        // 1. Unlink from parent and siblings
        if let Some(parent_id) = self.parent_of(id) {
            self.remove_child(parent_id, id);
        }

        // 2. If it's now detached and the whole tree has 0 handles, wipe it.
        // This keeps the Arena clean for non-JS-referenced nodes.
        if id != self.root_id && self.can_wipe_detached_tree(id) {
            self.wipe_node_recursive(id);
        }

        node_copy
    }

    /// Recursively wipes a node and its descendants from the arena.
    /// Internal use only, assumes node is already detached from the root-connected tree.
    fn wipe_node_recursive(&mut self, id: NodeId) {
        let mut to_wipe = vec![id];
        while let Some(current_id) = to_wipe.pop() {
            let mut child = self.first_child_of(current_id);
            while let Some(c) = child {
                to_wipe.push(c);
                child = self.next_sibling_of(c);
            }

            if let Some(node) = self.nodes.remove(current_id) {
                if let Node::Element(data) = &node {
                    if let Some((_, id_val)) = data.attributes.iter().find(|(k, _)| &**k == "id") {
                        if self.id_map.get(id_val) == Some(&current_id) {
                            self.id_map.remove(id_val);
                        }
                    }
                }
            }
        }
    }

    /// Checks if a detached tree can be safely deleted.
    /// Returns true if no node in the tree (starting from `id`) has any active JS handles.
    pub fn can_wipe_detached_tree(&self, id: NodeId) -> bool {
        let mut stack = vec![id];
        while let Some(current_id) = stack.pop() {
            let node = match self.nodes.get(current_id) {
                Some(n) => n,
                None => continue,
            };

            let handles = match node {
                Node::Element(d) => d.js_handles,
                Node::Text(d) => d.js_handles,
                Node::Root(d) => d.js_handles,
            };

            if handles > 0 {
                return false;
            }

            let mut child = self.first_child_of(current_id);
            while let Some(c) = child {
                stack.push(c);
                child = self.next_sibling_of(c);
            }
        }
        true
    }

    /// Invoked by the GC bridge when a JS NodeHandle is dropped.
    pub fn try_cleanup_node(&mut self, id: NodeId) {
        if let Some(node) = self.nodes.get_mut(id) {
            let handles = match node {
                Node::Element(d) => &mut d.js_handles,
                Node::Text(d) => &mut d.js_handles,
                Node::Root(d) => &mut d.js_handles,
            };
            if *handles > 0 {
                *handles -= 1;
            }
            if *handles == 0 {
                self.dead_nodes.push(id);
            }
        }
    }

    /// Performs a batched sweep of potential dead nodes. 
    /// Should be called by the host application at the end of each frame.
    pub fn collect_garbage(&mut self) {
        let potential = std::mem::take(&mut self.dead_nodes);
        let mut detached_roots = std::collections::HashSet::new();

        for id in potential {
            if !self.nodes.contains(id) { continue; }
            
            // Find the "detached root" of this node branch
            let mut curr = id;
            while let Some(parent) = self.parent_of(curr) {
                curr = parent;
            }

            if curr != self.root_id {
                detached_roots.insert(curr);
            }
        }

        for root in detached_roots {
            if self.can_wipe_detached_tree(root) {
                self.wipe_node_recursive(root);
            }
        }
    }

    /// Rebuilds the internal stylesheet by collecting all currently attached `<style>` tags.
    pub fn rebuild_styles(&mut self) {
        if !self.styles_dirty {
            return;
        }

        let mut all_css = String::new();
        let mut stack = vec![self.root_id];

        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(id) {
                match node {
                    Node::Element(data) => {
                        if &*data.tag_name == "style" {
                            // Collect inner text from children
                            let mut child_id = data.first_child;
                            while let Some(c) = child_id {
                                if let Some(Node::Text(text_data)) = self.nodes.get(c) {
                                    all_css.push_str(&text_data.text);
                                }
                                child_id = self.next_sibling_of(c);
                            }
                        }

                        // Traverse children (reverse for stack)
                        let mut children = Vec::new();
                        let mut child_id = data.first_child;
                        while let Some(c) = child_id {
                            children.push(c);
                            child_id = self.next_sibling_of(c);
                        }
                        for c in children.into_iter().rev() {
                            stack.push(c);
                        }
                    }
                    Node::Root(data) => {
                        let mut children = Vec::new();
                        let mut child_id = data.first_child;
                        while let Some(c) = child_id {
                            children.push(c);
                            child_id = self.next_sibling_of(c);
                        }
                        for c in children.into_iter().rev() {
                            stack.push(c);
                        }
                    }
                    Node::Text(_) => {}
                }
            }
        }

        self.stylesheet = crate::css::parse_stylesheet(&all_css);
        self.styles_dirty = false;
    }

    pub fn append_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        // Cycle check: Ensure child_id is not an ancestor of parent_id
        let mut curr = Some(parent_id);
        while let Some(pid) = curr {
            if pid == child_id {
                return; // Cycle detected, abort append
            }
            curr = self.parent_of(pid);
        }

        self.dirty = true;
        
        // If we are appending a <style> tag, mark styles as dirty
        if let Some(Node::Element(data)) = self.nodes.get(child_id) {
            if &*data.tag_name == "style" {
                self.styles_dirty = true;
            }
        }

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
        
        // If we are removing a <style> tag, mark styles as dirty
        if let Some(Node::Element(data)) = self.nodes.get(child_id) {
            if &*data.tag_name == "style" {
                self.styles_dirty = true;
            }
        }

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
    pub fn hit_test(&self, px: f32, py: f32) -> Option<NodeId> {
        let root_taffy = match self.nodes.get(self.root_id) {
            Some(Node::Root(r)) => r.taffy_node?,
            _ => return None,
        };

        let mut hit = None;
        let mut stack = vec![(self.root_id, root_taffy, 0.0, 0.0)];
        // Reusable scratch buffer to avoid per-node Vec allocation
        let mut children_buf = Vec::new();

        while let Some((node_id, taffy_id, offset_x, offset_y)) = stack.pop() {
            if let Ok(layout) = self.taffy_tree.layout(taffy_id) {
                let abs_x = offset_x + layout.location.x;
                let abs_y = offset_y + layout.location.y;

                if px >= abs_x && px <= abs_x + layout.size.width &&
                   py >= abs_y && py <= abs_y + layout.size.height {

                    hit = Some(node_id);

                    children_buf.clear();
                    let mut child_id = self.first_child_of(node_id);
                    while let Some(c) = child_id {
                        children_buf.push(c);
                        child_id = self.next_sibling_of(c);
                    }

                    for c in &children_buf {
                        let c_taffy = match self.nodes.get(*c) {
                            Some(Node::Element(d)) => d.taffy_node,
                            Some(Node::Text(d)) => d.taffy_node,
                            _ => None,
                        };
                        if let Some(t) = c_taffy {
                            stack.push((*c, t, abs_x, abs_y));
                        }
                    }
                }
            }
        }

        hit
    }
}
