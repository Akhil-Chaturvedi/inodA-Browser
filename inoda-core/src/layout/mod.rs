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
//! properties (`width`, `height`, `margin`, `padding`, `border-width`, `display`,
//! `flex-direction`) are read directly from `node.computed` (a `ComputedStyle`
//! embedded in each arena `ElementData` or `TextData`).
//!
//! Supported dimension units: px, %, vw, vh, em, rem, auto.
//! Supported display modes: flex, grid, block, none.
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

    let root_taffy_node =
        build_taffy_node(document, document.root_id, viewport_width, viewport_height);

    let tree = &mut document.taffy_tree;
    let available_space = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::Definite(viewport_height),
    };

    let font_system = RefCell::new(font_system);
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

            let mut sys = font_system.borrow_mut();
            let mut b_cache = buffer_cache_cell.borrow_mut();

            let buffer = b_cache.get_mut(&ctx.node_id).unwrap();

            buffer.set_size(
                &mut sys,
                Some(width_constraint.max(1.0)),
                Some(f32::INFINITY),
            );

            buffer.shape_until_scroll(&mut sys, false);

            let mut lines_count = 0;
            let mut max_width: f32 = 0.0;
            for run in buffer.layout_runs() {
                max_width = max_width.max(run.line_w);
                lines_count += 1;
            }

            if lines_count == 0 {
                lines_count = 1;
            }

            let width = max_width.min(width_constraint.max(1.0));
            let line_height = (ctx.font_size * 1.2).max(1.0);
            let height = (lines_count as f32) * line_height;

            taffy::geometry::Size { width, height }
        },
    )
    .unwrap();

    // We walk the tree one last time to enforce exact text heights for empty nodes
    finalize_text_measurements(
        &document.taffy_tree,
        root_taffy_node,
        font_system.into_inner(),
        buffer_cache,
    );

    (root_taffy_node, std::mem::take(buffer_cache))
}

fn prepare_text_buffers(
    document: &crate::dom::Document,
    node_id: crate::dom::NodeId,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    if let Some(crate::dom::Node::Text(data)) = document.nodes.get(node_id) {
        let font_size = data.computed.font_size;

        let line_height = (font_size * 1.2).max(1.0);
        buffer_cache.entry(node_id).or_insert_with(|| {
            let mut b = Buffer::new(font_system, Metrics::new(font_size, line_height));
            b.set_wrap(font_system, Wrap::WordOrGlyph);
            b.set_text(font_system, &data.text, Attrs::new(), Shaping::Advanced);
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
) -> taffy::NodeId {
    let mut style = Style::DEFAULT;

    let (computed, is_text) = match document.nodes.get(node_id) {
        Some(crate::dom::Node::Element(data)) => (&data.computed, false),
        Some(crate::dom::Node::Text(data)) => (&data.computed, true),
        _ => {
            // Root node or fallback
            let exists = if let Some(crate::dom::Node::Root(d)) = document.nodes.get(node_id) {
                d.taffy_node
            } else {
                None
            };

            return if let Some(t_node) = exists {
                document.taffy_tree.set_style(t_node, style).unwrap();
                t_node
            } else {
                let t_node = document.taffy_tree.new_leaf(style).unwrap();
                if let Some(crate::dom::Node::Root(d)) = document.nodes.get_mut(node_id) {
                    d.taffy_node = Some(t_node);
                }
                t_node
            };
        }
    };

    let font_size = computed.font_size;

    match &*computed.display {
        "flex" => style.display = Display::Flex,
        "grid" => style.display = Display::Grid,
        "none" => style.display = Display::None,
        "block" => style.display = Display::Block,
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

    let exists = match document.nodes.get(node_id) {
        Some(crate::dom::Node::Element(d)) => d.taffy_node,
        Some(crate::dom::Node::Text(d)) => d.taffy_node,
        _ => None,
    };

    let t_node = if let Some(existing_t_node) = exists {
        document
            .taffy_tree
            .set_style(existing_t_node, style)
            .unwrap();
        existing_t_node
    } else {
        let new_t_node = if is_text {
            document
                .taffy_tree
                .new_leaf_with_context(style, crate::dom::TextMeasureContext { node_id, font_size })
                .unwrap()
        } else {
            document.taffy_tree.new_leaf(style).unwrap()
        };
        if let Some(node) = document.nodes.get_mut(node_id) {
            match node {
                crate::dom::Node::Element(d) => d.taffy_node = Some(new_t_node),
                crate::dom::Node::Text(d) => d.taffy_node = Some(new_t_node),
                _ => {}
            }
        }
        new_t_node
    };

    if !is_text {
        let mut taffy_children = Vec::new();
        let mut child_id = document.first_child_of(node_id);
        while let Some(c) = child_id {
            taffy_children.push(build_taffy_node(document, c, vw, vh));
            child_id = document.next_sibling_of(c);
        }
        document
            .taffy_tree
            .set_children(t_node, &taffy_children)
            .unwrap();
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
