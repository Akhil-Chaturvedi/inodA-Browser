//! Layout computation module.
//!
//! Walks the arena DOM and builds a parallel `TaffyTree<TextMeasureContext>`. Before
//! running the solver, orphaned `cosmic-text::Buffer` entries (for nodes removed
//! from the DOM since the last frame) are evicted from the caller-owned buffer cache.
//! A pre-pass then creates `Buffer` objects for all current text nodes, performing
//! HarfBuzz shaping once per node.
//!
//! The Taffy measure closure calls `buffer.set_size()` to adjust the width
//! constraint, then counts `layout_runs()` for height. Repeated measure probes
//! at the same definite width reuse `TextMeasureContext::last_line_count` to
//! skip redundant work. Layout properties are read directly from `node.computed`.
//!
//! All traversals (`prepare_text_buffers`, `build_taffy_node`, `finalize_text_measurements`)
//! are iterative and stack-based to avoid stack overflow on deep DOM trees.
//!
//! `build_taffy_node` performs work conditionally:
//! - **Structural Updates**: Taffy node children are only updated via
//!   `set_children` if a node is new or the `document.dirty` flag is set.
//! - **Text Measurement**: Intrinsic width calculation and shaping are
//!   only re-run if a node is new or its `layout_dirty` flag is set.
//!
//! `<img>` elements use intrinsic sizing from HTML `width`/`height` attributes
//! and Taffy's `aspect_ratio` property.
//!
//! Supported dimension units: px, %, vw, vh, em, rem, auto.
//! `rem` resolves against `Document.root_font_size`; `em` resolves against the
//! element's own `font_size`. Supported display modes: flex, grid, block, none.
//! Note: inline and inline-block are normalized to block.
//! Box model properties mapped: margin-*, padding-*, border-*-width.

use std::{cell::UnsafeCell, collections::HashMap};

use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, Wrap};
use taffy::{
    TaffyTree,
    prelude::*,
    style::{Dimension, Style},
};

/// Width tolerance for treating two Taffy measure probes as identical.
const MEASURE_WIDTH_EPSILON: f32 = 1e-3;

// TextMeasureContext moved to crate::dom

pub fn compute_layout(
    document: &mut crate::dom::Document,

    viewport_width: f32,
    viewport_height: f32,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) -> taffy::NodeId {
    // Evict cached text buffers for nodes that have been removed from the DOM
    buffer_cache.retain(|node_id, _| document.nodes.contains(*node_id));

    prepare_text_buffers(document, document.root_id, font_system, buffer_cache);

    let root_font_size = document.root_font_size;
    let mut scratchpad = Vec::new();
    let root_taffy_node = build_taffy_node(
        document,
        document.root_id,
        viewport_width,
        viewport_height,
        root_font_size,
        buffer_cache,
        &mut scratchpad,
    );

    let tree = &mut document.taffy_tree;
    let available_space = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::Definite(viewport_height),
    };

    // Use UnsafeCell instead of RefCell for the measure hot path.
    // Safety: compute_layout_with_measure executes the closure synchronously
    // and sequentially. No aliasing mutations occur — font_system and
    // buffer_cache are only accessed through these cells within the closure,
    // and the closure is not re-entrant.
    let font_system_cell = UnsafeCell::new(font_system);
    let buffer_cache_cell = UnsafeCell::new(buffer_cache);

    {
        // SAFETY: The measure closure is called synchronously by Taffy's solver.
        // No other code accesses font_system or buffer_cache while the solver runs.
        let fs = unsafe { &mut *font_system_cell.get() };
        let bc = unsafe { &mut *buffer_cache_cell.get() };

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

                if let Some(buffer) = bc.get_mut(&ctx.node_id) {
                    let line_height = (ctx.font_size * 1.2).max(1.0);
                    if let Some(last_w) = ctx.last_measure_width {
                        if (last_w - width_constraint).abs() <= MEASURE_WIDTH_EPSILON {
                            let width = ctx.max_intrinsic_width.min(width_constraint);
                            let height =
                                (ctx.last_line_count * line_height).max(line_height);
                            return taffy::geometry::Size { width, height };
                        }
                    }

                    buffer.set_size(fs, Some(width_constraint), None);
                    let line_count = buffer.layout_runs().count() as f32;
                    ctx.last_measure_width = Some(width_constraint);
                    ctx.last_line_count = line_count;

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

    // Recover owned values from the UnsafeCells
    let font_system = font_system_cell.into_inner();
    let buffer_cache = buffer_cache_cell.into_inner();

    // We walk the tree one last time to enforce exact text heights for empty nodes
    finalize_text_measurements(
        &document.taffy_tree,
        root_taffy_node,
        font_system,
        buffer_cache,
    );

    root_taffy_node
}

/// Iterative pre-pass: creates `Buffer` objects for all text nodes in the DOM.
fn prepare_text_buffers(
    document: &mut crate::dom::Document,
    root_id: crate::dom::NodeId,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    let mut stack = vec![root_id];
    while let Some(node_id) = stack.pop() {
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

        // Push children in reverse order so first child is processed first
        let mut children = Vec::new();
        let mut child_id = document.first_child_of(node_id);
        while let Some(c) = child_id {
            children.push(c);
            child_id = document.next_sibling_of(c);
        }
        for c in children.into_iter().rev() {
            stack.push(c);
        }
    }
}

/// Iterative post-pass: reshapes text buffers at their final resolved widths.
///
/// Compares the resolved layout width against `ctx.last_measure_width` to avoid
/// unnecessary reshaping. Only reshapes if the width differs beyond MEASURE_WIDTH_EPSILON.
/// This replaces the previous `layout_dirty` flag check, which was already cleared by
/// `prepare_text_buffers` before this function runs.
fn finalize_text_measurements(
    tree: &TaffyTree<crate::dom::TextMeasureContext>,
    root_taffy_node: taffy::NodeId,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    const MEASURE_WIDTH_EPSILON: f32 = 0.5;

    let mut stack = vec![root_taffy_node];
    while let Some(taffy_node) = stack.pop() {
        if let Some(ctx) = tree.get_node_context(taffy_node) {
            if let Ok(layout) = tree.layout(taffy_node) {
                let resolved_width = layout.size.width;

                // Only reshape if width differs from last measure by more than epsilon
                let needs_reshape = ctx.last_measure_width
                    .map(|last_w| (last_w - resolved_width).abs() > MEASURE_WIDTH_EPSILON)
                    .unwrap_or(true);

                if needs_reshape {
                    if let Some(buffer) = buffer_cache.get_mut(&ctx.node_id) {
                        buffer.set_size(
                            font_system,
                            Some(resolved_width.max(1.0)),
                            Some(f32::INFINITY),
                        );
                        buffer.shape_until_scroll(font_system, false);
                    }
                }
            }
        }

        if let Ok(children) = tree.children(taffy_node) {
            // Push in reverse so first child is processed first
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        }
    }
}

/// Iterative DOM walk that builds Taffy nodes bottom-up.
///
/// Uses a two-phase approach:
/// 1. **Walk-down**: DFS traversal collecting DOM nodes in post-order (children
///    before parents) using an explicit stack with visited flags.
/// 2. **Build-up**: Process the post-order list — each node's children are
///    guaranteed to already have Taffy node IDs assigned.
fn build_taffy_node(
    document: &mut crate::dom::Document,
    root_id: crate::dom::NodeId,
    vw: f32,
    vh: f32,
    root_font_size: f32,
    buffer_cache: &HashMap<crate::dom::NodeId, Buffer>,
    scratchpad: &mut Vec<taffy::NodeId>,
) -> taffy::NodeId {
    // Phase 1: Collect DOM nodes in post-order (children before parents).
    // Stack entries: (node_id, visited). When visited=false, we push the node
    // again with visited=true, then push its children with visited=false.
    // When visited=true, we add the node to the post-order list.
    let mut dfs_stack = vec![(root_id, false)];
    let mut post_order = Vec::new();

    while let Some((nid, visited)) = dfs_stack.pop() {
        if visited {
            post_order.push(nid);
            continue;
        }

        dfs_stack.push((nid, true));

        // Push children in reverse order so first child ends up on top
        let is_text = matches!(document.nodes.get(nid), Some(crate::dom::Node::Text(_)));
        if !is_text {
            let mut children = Vec::new();
            let mut child_id = document.first_child_of(nid);
            while let Some(c) = child_id {
                children.push(c);
                child_id = document.next_sibling_of(c);
            }
            for c in children.into_iter().rev() {
                dfs_stack.push((c, false));
            }
        }
    }

    // Phase 2: Process nodes in post-order. Children are guaranteed to have
    // their Taffy nodes already created and stored on the arena node.
    // We use a secondary scratchpad to collect child Taffy node IDs for set_children.
    let mut child_taffy_buf: Vec<taffy::NodeId> = Vec::new();

    for node_id in post_order {
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
        // Text nodes use TextComputedStyle (only font_size + color); elements use
        // the full ComputedStyle.  We branch early so text nodes skip the 36-field
        // style resolution that doesn't apply to leaf measure nodes.
        let font_size;
        let mut style = Style::default();
    
        if is_text {
            // Text nodes: only need font_size for the measure closure context.
            font_size = match document.nodes.get(node_id) {
                Some(crate::dom::Node::Text(d)) => d.computed.font_size,
                _ => 16.0,
            };
            // Text nodes are leaf measure nodes — Style::DEFAULT is sufficient.
        } else {
            let computed_fallback = crate::dom::ComputedStyle::default();
            let computed = match document.nodes.get(node_id) {
                Some(crate::dom::Node::Element(d)) => &d.computed,
                _ => &computed_fallback,
            };
    
            font_size = computed.font_size;

        style.display = match computed.display {
            crate::dom::DisplayKeyword::Flex => taffy::style::Display::Flex,
            crate::dom::DisplayKeyword::Grid => taffy::style::Display::Grid,
            crate::dom::DisplayKeyword::None => taffy::style::Display::None,
            _ => taffy::style::Display::Block,
        };
        style.flex_direction = match computed.flex_direction {
            crate::dom::FlexDirectionKeyword::Column => taffy::style::FlexDirection::Column,
            _ => taffy::style::FlexDirection::Row,
        };
        style.align_items = Some(match computed.align_items {
            crate::dom::AlignItemsKeyword::FlexEnd => taffy::style::AlignItems::FlexEnd,
            crate::dom::AlignItemsKeyword::Center => taffy::style::AlignItems::Center,
            crate::dom::AlignItemsKeyword::Baseline => taffy::style::AlignItems::Baseline,
            crate::dom::AlignItemsKeyword::FlexStart => taffy::style::AlignItems::FlexStart,
            _ => taffy::style::AlignItems::Stretch,
        });
        style.justify_content = Some(match computed.justify_content {
            crate::dom::JustifyContentKeyword::FlexEnd => taffy::style::JustifyContent::FlexEnd,
            crate::dom::JustifyContentKeyword::Center => taffy::style::JustifyContent::Center,
            crate::dom::JustifyContentKeyword::SpaceBetween => taffy::style::JustifyContent::SpaceBetween,
            crate::dom::JustifyContentKeyword::SpaceAround => taffy::style::JustifyContent::SpaceAround,
            crate::dom::JustifyContentKeyword::SpaceEvenly => taffy::style::JustifyContent::SpaceEvenly,
            _ => taffy::style::JustifyContent::FlexStart,
        });
        style.flex_wrap = match computed.flex_wrap {
            crate::dom::FlexWrapKeyword::Wrap => taffy::style::FlexWrap::Wrap,
            crate::dom::FlexWrapKeyword::WrapReverse => taffy::style::FlexWrap::WrapReverse,
            _ => taffy::style::FlexWrap::NoWrap,
        };

        let is_flex = computed.display == crate::dom::DisplayKeyword::Flex;
        let has_flex_dir = computed.flex_direction == crate::dom::FlexDirectionKeyword::Row || computed.flex_direction == crate::dom::FlexDirectionKeyword::Column;

        if !is_flex && !has_flex_dir {
            style.flex_direction = taffy::style::FlexDirection::Column;
        }

        style.flex_grow = computed.flex_grow;
        style.flex_shrink = computed.flex_shrink;

        if let Some(dim) = parse_length_percentage(&computed.row_gap, vw, vh, font_size, root_font_size) {
            style.gap.height = dim;
        }
        if let Some(dim) = parse_length_percentage(&computed.column_gap, vw, vh, font_size, root_font_size) {
            style.gap.width = dim;
        }

        if let Some(dim) = parse_dimension(&computed.min_width, vw, vh, font_size, root_font_size) {
            style.min_size.width = dim;
        }
        if let Some(dim) = parse_dimension(&computed.max_width, vw, vh, font_size, root_font_size) {
            style.max_size.width = dim;
        }
        if let Some(dim) = parse_dimension(&computed.min_height, vw, vh, font_size, root_font_size) {
            style.min_size.height = dim;
        }
        if let Some(dim) = parse_dimension(&computed.max_height, vw, vh, font_size, root_font_size) {
            style.max_size.height = dim;
        }

        if let Some(dim) = parse_dimension(&computed.width, vw, vh, font_size, root_font_size) {
            style.size.width = dim;
        }

        if let Some(dim) = parse_dimension(&computed.height, vw, vh, font_size, root_font_size) {
            style.size.height = dim;
        }

        if let Some(dim) = parse_length_percentage_auto(&computed.margin[0], vw, vh, font_size, root_font_size) {
            style.margin.top = dim;
        }
        if let Some(dim) = parse_length_percentage_auto(&computed.margin[1], vw, vh, font_size, root_font_size) {
            style.margin.right = dim;
        }
        if let Some(dim) = parse_length_percentage_auto(&computed.margin[2], vw, vh, font_size, root_font_size) {
            style.margin.bottom = dim;
        }
        if let Some(dim) = parse_length_percentage_auto(&computed.margin[3], vw, vh, font_size, root_font_size) {
            style.margin.left = dim;
        }

        if let Some(dim) = parse_length_percentage(&computed.padding[0], vw, vh, font_size, root_font_size) {
            style.padding.top = dim;
        }
        if let Some(dim) = parse_length_percentage(&computed.padding[1], vw, vh, font_size, root_font_size) {
            style.padding.right = dim;
        }
        if let Some(dim) = parse_length_percentage(&computed.padding[2], vw, vh, font_size, root_font_size) {
            style.padding.bottom = dim;
        }
        if let Some(dim) = parse_length_percentage(&computed.padding[3], vw, vh, font_size, root_font_size) {
            style.padding.left = dim;
        }

        if let Some(dim) = parse_length_percentage(&computed.border_width[0], vw, vh, font_size, root_font_size) {
            style.border.top = dim;
        }
        if let Some(dim) = parse_length_percentage(&computed.border_width[1], vw, vh, font_size, root_font_size) {
            style.border.right = dim;
        }
        if let Some(dim) = parse_length_percentage(&computed.border_width[2], vw, vh, font_size, root_font_size) {
            style.border.bottom = dim;
        }
        if let Some(dim) = parse_length_percentage(&computed.border_width[3], vw, vh, font_size, root_font_size) {
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
        } // end of element else block
    
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
                        last_measure_width: None,
                        last_line_count: 0.0,
                    }))
                    .unwrap();
            }
        }

        if !is_text {
            // Collect child Taffy node IDs — children are already processed (post-order).
            child_taffy_buf.clear();
            let mut child_id = document.first_child_of(node_id);
            while let Some(c) = child_id {
                let child_taffy = match document.nodes.get(c) {
                    Some(crate::dom::Node::Element(d)) => d.taffy_node,
                    Some(crate::dom::Node::Text(d)) => d.taffy_node,
                    Some(crate::dom::Node::Root(d)) => d.taffy_node,
                    None => None,
                };
                if let Some(ct) = child_taffy {
                    child_taffy_buf.push(ct);
                }
                child_id = document.next_sibling_of(c);
            }

            // Only update children if the node is new or the document is structurally dirty.
            // This avoids violent allocator thrashing inside Taffy's edge arrays on every frame.
            if is_new_taffy_node || document.dirty {
                document
                    .taffy_tree
                    .set_children(t_node, &child_taffy_buf)
                    .unwrap();
            }
        }

        scratchpad.push(t_node);
    }

    // The root Taffy node is the last one pushed in post-order
    *scratchpad.last().unwrap()
}

#[inline]
fn parse_dimension(
    val: &crate::dom::StyleValue,
    vw: f32,
    vh: f32,
    font_size: f32,
    root_font_size: f32,
) -> Option<Dimension> {
    match val {
        crate::dom::StyleValue::Auto => Some(Dimension::auto()),
        crate::dom::StyleValue::LengthPx(num) => Some(Dimension::length(*num)),
        crate::dom::StyleValue::Percent(p) => Some(Dimension::percent(*p / 100.0)),
        crate::dom::StyleValue::ViewportWidth(num) => Some(Dimension::length((num / 100.0) * vw)),
        crate::dom::StyleValue::ViewportHeight(num) => Some(Dimension::length((num / 100.0) * vh)),
        crate::dom::StyleValue::Em(num) => Some(Dimension::length(num * font_size)),
        crate::dom::StyleValue::Rem(num) => Some(Dimension::length(num * root_font_size)),
        _ => None,
    }
}

#[inline]
fn parse_length_percentage_auto(
    val: &crate::dom::StyleValue,
    vw: f32,
    vh: f32,
    font_size: f32,
    root_font_size: f32,
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
            Some(taffy::style::LengthPercentageAuto::length(num * root_font_size))
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
    root_font_size: f32,
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
            Some(taffy::style::LengthPercentage::length(num * root_font_size))
        }
        _ => None,
    }
}
