//! Layout computation module.
//!
//! Walks the arena DOM and builds a parallel `TaffyTree<TextMeasureContext>`. Before
//! running the solver, orphaned `cosmic-text::Buffer` entries (for nodes removed
//! from the DOM since the last frame) are evicted from the caller-owned buffer cache.
//! A pre-pass then creates `Buffer` objects for all current text nodes, performing
//! HarfBuzz shaping once per node.
//!
//! The Taffy measure closure calls `buffer.set_size()` to adjust the width
//! constraint, then `buffer.shape_until_scroll()` to re-wrap text. Layout
//! properties are read directly from `node.computed`.
//!
//! To ensure high per-frame performance, `build_taffy_node` performs work
//! conditionally:
//! - **Structural Updates**: Taffy node children are only updated via
//!   `set_children` if a node is new or the `document.dirty` flag is set.
//! - **Text Measurement**: Intrinsic width calculation and shaping are
//!   only re-run if a node is new or its `layout_dirty` flag is set.
//!
//! `<img>` elements use intrinsic sizing from HTML `width`/`height` attributes
//! and Taffy's `aspect_ratio` property.
//!
//! Supported dimension units: px, %, vw, vh, em, rem, auto.
//! Supported display modes: flex, grid, block, none. Note: inline and inline-block are normalized to block.
//! Box model properties mapped: margin-*, padding-*, border-*-width.

use std::{cell::RefCell, collections::HashMap};

use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, Wrap};
use taffy::{
    TaffyTree,
    prelude::*,
    style::{Dimension, Style},
};

// TextMeasureContext moved to crate::dom

pub fn compute_layout(
    document: &mut crate::dom::Document,

    viewport_width: f32,
    viewport_height: f32,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) -> (taffy::NodeId, HashMap<crate::dom::NodeId, Buffer>) {
    // Evict cached text buffers for nodes that have been removed from the DOM
    buffer_cache.retain(|node_id, _| document.nodes.contains(*node_id));

    prepare_text_buffers(document, document.root_id, font_system, buffer_cache);

    let mut scratchpad = Vec::with_capacity(128);
    let root_taffy_node =
        build_taffy_node(document, document.root_id, viewport_width, viewport_height, buffer_cache, &mut scratchpad);

    let tree = &mut document.taffy_tree;
    let available_space = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::Definite(viewport_height),
    };

    let font_system_cell = RefCell::new(font_system);
    {
        let buffer_cache_cell = RefCell::new(&mut *buffer_cache);

        tree.compute_layout_with_measure(
            root_taffy_node,
            available_space,
            |_known_dimensions,
             available_space,
             _node_id,
             context: Option<&mut crate::dom::TextMeasureContext>,
             _style| {
                let Some(ctx) = context else {
                    return taffy::geometry::Size::ZERO;
                };

                let width_constraint = match available_space.width {
                    AvailableSpace::Definite(w) if w.is_finite() && w > 0.0 => w,
                    _ => viewport_width.max(1.0),
                };

                let mut fs = font_system_cell.borrow_mut();
                let mut bc = buffer_cache_cell.borrow_mut();

                if let Some(buffer) = bc.get_mut(&ctx.node_id) {
                    // Adjust width and trigger re-wrap (not full re-shape)
                    buffer.set_size(&mut fs, Some(width_constraint), None);
                    let line_count = buffer.layout_runs().count() as f32;
                    let line_height = (ctx.font_size * 1.2).max(1.0);
                    
                    let width = ctx.max_intrinsic_width.min(width_constraint);
                    let height = (line_count * line_height).max(line_height);

                    taffy::geometry::Size { width, height }
                } else {
                    // Fallback to minimal approximation if buffer is missing
                    let width = ctx.max_intrinsic_width.min(width_constraint);
                    let height = (ctx.font_size * 1.2).max(1.0);
                    taffy::geometry::Size { width, height }
                }
            },
        )
        .unwrap();
    }

    // We walk the tree one last time to enforce exact text heights for empty nodes
    finalize_text_measurements(
        &document.taffy_tree,
        root_taffy_node,
        font_system_cell.into_inner(),
        buffer_cache,
    );

    (root_taffy_node, std::mem::take(buffer_cache))
}

fn prepare_text_buffers(
    document: &mut crate::dom::Document,
    node_id: crate::dom::NodeId,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    if let Some(crate::dom::Node::Text(data)) = document.nodes.get_mut(node_id) {
        if data.layout_dirty {
            buffer_cache.remove(&node_id);
            data.layout_dirty = false;
        }

        let font_size = data.computed.font_size;
        let line_height = (font_size * 1.2).max(1.0);
        
        buffer_cache.entry(node_id).or_insert_with(|| {
            let mut b = Buffer::new(font_system, Metrics::new(font_size, line_height));
            b.set_wrap(font_system, Wrap::WordOrGlyph);
            b.set_text(font_system, &data.text, Attrs::new(), Shaping::Advanced);
            
            // Shape ONCE in pre-pass to resolve intrinsic widths
            b.set_size(font_system, Some(f32::INFINITY), Some(f32::INFINITY));
            b.shape_until_scroll(font_system, false);
            b
        });
    }

    let mut child_id = document.first_child_of(node_id);
    while let Some(c) = child_id {
        prepare_text_buffers(document, c, font_system, buffer_cache);
        child_id = document.next_sibling_of(c);
    }
}

fn finalize_text_measurements(
    tree: &TaffyTree<crate::dom::TextMeasureContext>,
    taffy_node: NodeId,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    if let Some(ctx) = tree.get_node_context(taffy_node) {
        if let Ok(layout) = tree.layout(taffy_node) {
            let width_constraint = layout.size.width;

            let buffer = buffer_cache.get_mut(&ctx.node_id).unwrap();
            buffer.set_size(
                font_system,
                Some(width_constraint.max(1.0)),
                Some(f32::INFINITY),
            );
            buffer.shape_until_scroll(font_system, false);
        }
    }

    if let Ok(children) = tree.children(taffy_node) {
        for child in children {
            finalize_text_measurements(tree, child, font_system, buffer_cache);
        }
    }
}

fn build_taffy_node(
    document: &mut crate::dom::Document,
    node_id: crate::dom::NodeId,
    vw: f32,
    vh: f32,
    buffer_cache: &HashMap<crate::dom::NodeId, Buffer>,
    scratchpad: &mut Vec<taffy::NodeId>,
) -> taffy::NodeId {
    // 1. Get or create the Taffy node and determine if this is a text node.
    let (t_node, is_text, is_new_taffy_node) = {
        let entry = document.nodes.get(node_id).expect("Invalid node_id");
        match entry {
            crate::dom::Node::Element(d) => (d.taffy_node, false, d.taffy_node.is_none()),
            crate::dom::Node::Text(d) => (d.taffy_node, true, d.taffy_node.is_none()),
            crate::dom::Node::Root(d) => (d.taffy_node, false, d.taffy_node.is_none()),
        }
    };

    let t_node = if let Some(t) = t_node {
        t
    } else {
        let t = document.taffy_tree.new_leaf(Style::DEFAULT).unwrap();
        if let Some(node) = document.nodes.get_mut(node_id) {
            match node {
                crate::dom::Node::Element(d) => d.taffy_node = Some(t),
                crate::dom::Node::Text(d) => d.taffy_node = Some(t),
                crate::dom::Node::Root(d) => d.taffy_node = Some(t),
            }
        }
        t
    };

    // 2. Now that we have the Taffy node ID, define the style.
    // We fetch computed styles in a separate scope to avoid long-lived borrows of document.nodes.
    let mut style = Style::default();
    let computed_fallback = crate::dom::ComputedStyle::default();
    
    let computed = match document.nodes.get(node_id) {
        Some(crate::dom::Node::Element(d)) => &d.computed,
        Some(crate::dom::Node::Text(d)) => &d.computed,
        _ => &computed_fallback,
    };
    
    let font_size = computed.font_size;

    match &*computed.display {
        "flex" => style.display = Display::Flex,
        "grid" => style.display = Display::Grid,
        "none" => style.display = Display::None,
        "block" | "inline" | "inline-block" | "list-item" => style.display = Display::Block,
        _ => {}
    }

    match &*computed.flex_direction {
        "row" => style.flex_direction = FlexDirection::Row,
        "column" => style.flex_direction = FlexDirection::Column,
        _ => {}
    }

    let is_flex = &*computed.display == "flex";
    let has_flex_dir = &*computed.flex_direction == "row" || &*computed.flex_direction == "column";

    if !is_flex && !has_flex_dir {
        style.flex_direction = FlexDirection::Column;
    }

    match &*computed.align_items {
        "flex-start" | "start" => style.align_items = Some(AlignItems::FlexStart),
        "flex-end" | "end" => style.align_items = Some(AlignItems::FlexEnd),
        "center" => style.align_items = Some(AlignItems::Center),
        "baseline" => style.align_items = Some(AlignItems::Baseline),
        "stretch" => style.align_items = Some(AlignItems::Stretch),
        _ => {}
    }

    match &*computed.justify_content {
        "flex-start" | "start" => style.justify_content = Some(JustifyContent::FlexStart),
        "flex-end" | "end" => style.justify_content = Some(JustifyContent::FlexEnd),
        "center" => style.justify_content = Some(JustifyContent::Center),
        "space-between" => style.justify_content = Some(JustifyContent::SpaceBetween),
        "space-around" => style.justify_content = Some(JustifyContent::SpaceAround),
        "space-evenly" => style.justify_content = Some(JustifyContent::SpaceEvenly),
        _ => {}
    }

    match &*computed.flex_wrap {
        "wrap" => style.flex_wrap = FlexWrap::Wrap,
        "wrap-reverse" => style.flex_wrap = FlexWrap::WrapReverse,
        "nowrap" => style.flex_wrap = FlexWrap::NoWrap,
        _ => {}
    }

    style.flex_grow = computed.flex_grow;
    style.flex_shrink = computed.flex_shrink;

    if let Some(dim) = parse_length_percentage(&computed.row_gap, vw, vh, font_size) {
        style.gap.height = dim;
    }
    if let Some(dim) = parse_length_percentage(&computed.column_gap, vw, vh, font_size) {
        style.gap.width = dim;
    }

    if let Some(dim) = parse_dimension(&computed.min_width, vw, vh, font_size) {
        style.min_size.width = dim;
    }
    if let Some(dim) = parse_dimension(&computed.max_width, vw, vh, font_size) {
        style.max_size.width = dim;
    }
    if let Some(dim) = parse_dimension(&computed.min_height, vw, vh, font_size) {
        style.min_size.height = dim;
    }
    if let Some(dim) = parse_dimension(&computed.max_height, vw, vh, font_size) {
        style.max_size.height = dim;
    }

    if let Some(dim) = parse_dimension(&computed.width, vw, vh, font_size) {
        style.size.width = dim;
    }

    if let Some(dim) = parse_dimension(&computed.height, vw, vh, font_size) {
        style.size.height = dim;
    }

    if let Some(dim) = parse_length_percentage_auto(&computed.margin[0], vw, vh, font_size) {
        style.margin.top = dim;
    }
    if let Some(dim) = parse_length_percentage_auto(&computed.margin[1], vw, vh, font_size) {
        style.margin.right = dim;
    }
    if let Some(dim) = parse_length_percentage_auto(&computed.margin[2], vw, vh, font_size) {
        style.margin.bottom = dim;
    }
    if let Some(dim) = parse_length_percentage_auto(&computed.margin[3], vw, vh, font_size) {
        style.margin.left = dim;
    }

    if let Some(dim) = parse_length_percentage(&computed.padding[0], vw, vh, font_size) {
        style.padding.top = dim;
    }
    if let Some(dim) = parse_length_percentage(&computed.padding[1], vw, vh, font_size) {
        style.padding.right = dim;
    }
    if let Some(dim) = parse_length_percentage(&computed.padding[2], vw, vh, font_size) {
        style.padding.bottom = dim;
    }
    if let Some(dim) = parse_length_percentage(&computed.padding[3], vw, vh, font_size) {
        style.padding.left = dim;
    }

    if let Some(dim) = parse_length_percentage(&computed.border_width[0], vw, vh, font_size) {
        style.border.top = dim;
    }
    if let Some(dim) = parse_length_percentage(&computed.border_width[1], vw, vh, font_size) {
        style.border.right = dim;
    }
    if let Some(dim) = parse_length_percentage(&computed.border_width[2], vw, vh, font_size) {
        style.border.bottom = dim;
    }
    if let Some(dim) = parse_length_percentage(&computed.border_width[3], vw, vh, font_size) {
        style.border.left = dim;
    }

    let mut aspect_ratio = None;
    if let Some(crate::dom::Node::Element(d)) = document.nodes.get(node_id) {
        if &*d.tag_name == "img" {
            let mut img_w = None;
            let mut img_h = None;
            for (k, v) in &d.attributes {
                if k == "width" {
                    if let Ok(w) = v.parse::<f32>() { img_w = Some(w); }
                } else if k == "height" {
                    if let Ok(h) = v.parse::<f32>() { img_h = Some(h); }
                }
            }
            if let (Some(w), Some(h)) = (img_w, img_h) {
                if h > 0.0 {
                    aspect_ratio = Some(w / h);
                }
                if style.size.width == Dimension::auto() {
                    style.size.width = Dimension::length(w);
                }
                if style.size.height == Dimension::auto() {
                    style.size.height = Dimension::length(h);
                }
            }
        }
    }
    style.aspect_ratio = aspect_ratio;

    document.taffy_tree.set_style(t_node, style).unwrap();

    // is_text specific shaping:
    if is_text {
        let needs_measure = is_new_taffy_node || match document.nodes.get(node_id) {
            Some(crate::dom::Node::Text(t)) => t.layout_dirty,
            _ => false,
        };

        if needs_measure {
            let mut max_intrinsic_width: f32 = 0.0;
            let mut min_intrinsic_width: f32 = 0.0;
            
            if let Some(buffer) = buffer_cache.get(&node_id) {
                if let Some(crate::dom::Node::Text(text_node)) = document.nodes.get(node_id) {
                    for run in buffer.layout_runs() {
                        max_intrinsic_width = max_intrinsic_width.max(run.line_w);
                        
                        let mut current_word_width = 0.0;
                        for glyph in run.glyphs {
                            let is_whitespace = text_node.text.get(glyph.start..glyph.end)
                                .map(|s| s.chars().any(|c| c.is_whitespace()))
                                .unwrap_or(false);
                            if is_whitespace {
                                min_intrinsic_width = min_intrinsic_width.max(current_word_width);
                                current_word_width = 0.0;
                            } else {
                                current_word_width += glyph.w;
                            }
                        }
                        min_intrinsic_width = min_intrinsic_width.max(current_word_width);
                    }
                }
            }
            document
                .taffy_tree
                .set_node_context(t_node, Some(crate::dom::TextMeasureContext { 
                    node_id, 
                    font_size,
                    max_intrinsic_width,
                    min_intrinsic_width,
                }))
                .unwrap();
        }
    }

    if !is_text {
        let start_idx = scratchpad.len();
        let mut child_id = document.first_child_of(node_id);
        while let Some(c) = child_id {
            let t_child = build_taffy_node(document, c, vw, vh, buffer_cache, scratchpad);
            scratchpad.push(t_child);
            child_id = document.next_sibling_of(c);
        }
        
        // Only update children if the node is new or the document is structurally dirty.
        // This avoids violent allocator thrashing inside Taffy's edge arrays on every frame.
        if is_new_taffy_node || document.dirty {
            let children_slice = &scratchpad[start_idx..];
            document
                .taffy_tree
                .set_children(t_node, children_slice)
                .unwrap();
        }
        
        scratchpad.truncate(start_idx);
    }

    t_node
}

#[inline]
fn parse_dimension(
    val: &crate::dom::StyleValue,
    vw: f32,
    vh: f32,
    font_size: f32,
) -> Option<Dimension> {
    match val {
        crate::dom::StyleValue::Auto => Some(Dimension::auto()),
        crate::dom::StyleValue::LengthPx(num) => Some(Dimension::length(*num)),
        crate::dom::StyleValue::Percent(p) => Some(Dimension::percent(*p / 100.0)),
        crate::dom::StyleValue::ViewportWidth(num) => Some(Dimension::length((num / 100.0) * vw)),
        crate::dom::StyleValue::ViewportHeight(num) => Some(Dimension::length((num / 100.0) * vh)),
        crate::dom::StyleValue::Em(num) => Some(Dimension::length(num * font_size)),
        crate::dom::StyleValue::Rem(num) => Some(Dimension::length(num * font_size)),
        _ => None,
    }
}

#[inline]
fn parse_length_percentage_auto(
    val: &crate::dom::StyleValue,
    vw: f32,
    vh: f32,
    font_size: f32,
) -> Option<taffy::style::LengthPercentageAuto> {
    match val {
        crate::dom::StyleValue::Auto => Some(taffy::style::LengthPercentageAuto::auto()),
        crate::dom::StyleValue::LengthPx(num) => {
            Some(taffy::style::LengthPercentageAuto::length(*num))
        }
        crate::dom::StyleValue::Percent(p) => {
            Some(taffy::style::LengthPercentageAuto::percent(*p / 100.0))
        }
        crate::dom::StyleValue::ViewportWidth(num) => Some(
            taffy::style::LengthPercentageAuto::length((num / 100.0) * vw),
        ),
        crate::dom::StyleValue::ViewportHeight(num) => Some(
            taffy::style::LengthPercentageAuto::length((num / 100.0) * vh),
        ),
        crate::dom::StyleValue::Em(num) => {
            Some(taffy::style::LengthPercentageAuto::length(num * font_size))
        }
        crate::dom::StyleValue::Rem(num) => {
            Some(taffy::style::LengthPercentageAuto::length(num * font_size))
        }
        _ => None,
    }
}

#[inline]
fn parse_length_percentage(
    val: &crate::dom::StyleValue,
    vw: f32,
    vh: f32,
    font_size: f32,
) -> Option<taffy::style::LengthPercentage> {
    match val {
        crate::dom::StyleValue::LengthPx(num) => Some(taffy::style::LengthPercentage::length(*num)),
        crate::dom::StyleValue::Percent(p) => {
            Some(taffy::style::LengthPercentage::percent(*p / 100.0))
        }
        crate::dom::StyleValue::ViewportWidth(num) => {
            Some(taffy::style::LengthPercentage::length((num / 100.0) * vw))
        }
        crate::dom::StyleValue::ViewportHeight(num) => {
            Some(taffy::style::LengthPercentage::length((num / 100.0) * vh))
        }
        crate::dom::StyleValue::Em(num) => {
            Some(taffy::style::LengthPercentage::length(num * font_size))
        }
        crate::dom::StyleValue::Rem(num) => {
            Some(taffy::style::LengthPercentage::length(num * font_size))
        }
        _ => None,
    }
}
