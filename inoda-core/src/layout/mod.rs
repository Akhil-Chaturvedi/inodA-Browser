//! Layout computation module.
//!
//! Converts a `StyledNode` tree into a `TaffyTree` and runs the Flexbox/Grid
//! layout algorithm. Before running the solver, a pre-pass creates
//! `cosmic-text::Buffer` objects for all text nodes so that HarfBuzz shaping
//! runs once. The Taffy measure closure calls `buffer.set_size()` followed by
//! `buffer.shape_until_scroll()` to re-wrap text at the new width constraint.
//!
//! Supported dimension units: px, %, vw, vh, em, rem, auto.
//! Supported display modes: flex, grid, block, none.
//! Box model properties mapped: margin-*, padding-*, border-*-width.

use std::{cell::RefCell, collections::HashMap};

use crate::dom::StyledNode;
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, Wrap};
use taffy::{
    TaffyTree,
    prelude::*,
    style::{Dimension, Style},
};

#[derive(Debug, Clone, Copy)]
pub struct TextMeasureContext {
    pub node_id: crate::dom::NodeId,
    pub font_size: f32,
}

pub fn compute_layout(
    document: &crate::dom::Document,
    styled_node: &StyledNode,
    viewport_width: f32,
    viewport_height: f32,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) -> (TaffyTree<TextMeasureContext>, NodeId, HashMap<crate::dom::NodeId, Buffer>) {
    let mut tree: TaffyTree<TextMeasureContext> = TaffyTree::new();

    prepare_text_buffers(document, styled_node, font_system, buffer_cache);

    let root_taffy_node = build_taffy_node(
        &mut tree,
        document,
        styled_node, 
        viewport_width,
        viewport_height,
    );

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
         context: Option<&mut TextMeasureContext>,
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
        &tree,
        root_taffy_node,
        font_system.into_inner(),
        buffer_cache,
    );

    (tree, root_taffy_node, std::mem::take(buffer_cache))
}

fn prepare_text_buffers(
    document: &crate::dom::Document,
    styled_node: &StyledNode,
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    if let Some(crate::dom::Node::Text(txt)) = document.nodes.get(styled_node.node_id) {
        let font_size = styled_node
            .local
            .iter()
            .find(|(k, _)| &**k == "font-size")
            .or_else(|| {
                styled_node
                    .inherited
                    .as_ref()
                    .and_then(|i| i.iter().find(|(k, _)| &**k == "font-size"))
            })
            .and_then(|(_, v)| match v {
                crate::dom::StyleValue::LengthPx(num) => Some(num.clone()),
                crate::dom::StyleValue::Number(num) => Some(num.clone()),
                _ => None,
            })
            .unwrap_or(16.0);

        let line_height = (font_size * 1.2).max(1.0);
        let _buffer = buffer_cache.entry(styled_node.node_id).or_insert_with(|| {
            let mut b = Buffer::new(font_system, Metrics::new(font_size, line_height));
            b.set_wrap(font_system, Wrap::WordOrGlyph);
            b.set_text(font_system, &txt.text, Attrs::new(), Shaping::Advanced);
            b
        });
    }

    for child in &styled_node.children {
        prepare_text_buffers(document, child, font_system, buffer_cache);
    }
}

fn finalize_text_measurements(
    tree: &TaffyTree<TextMeasureContext>,
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
    tree: &mut TaffyTree<TextMeasureContext>,
    document: &crate::dom::Document,
    styled_node: &StyledNode,
    vw: f32,
    vh: f32,
) -> NodeId {
    let mut style = Style::DEFAULT;

    let font_size = styled_node
        .local
        .iter()
        .find(|(k, _)| &**k == "font-size")
        .or_else(|| {
            styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "font-size"))
        })
        .and_then(|(_, v)| match v {
            crate::dom::StyleValue::LengthPx(num) => Some(num.clone()),
            crate::dom::StyleValue::Number(num) => Some(num.clone()),
            _ => None,
        })
        .unwrap_or(16.0);

        if let Some((_, display_val)) = styled_node
        .local
        .iter()
        .find(|(k, _)| &**k == "display")
        .or_else(|| {
            styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "display"))
        })
    {
        if let crate::dom::StyleValue::Keyword(kw) = display_val {
            match &**kw {
                "flex" => style.display = Display::Flex,
                "grid" => style.display = Display::Grid,
                "none" => style.display = Display::None,
                "block" => style.display = Display::Block,
                "inline" | "inline-block" => {}
                _ => {}
            }
        }
    }

    if let Some((_, dir_val)) = styled_node
        .local
        .iter()
        .find(|(k, _)| &**k == "flex-direction")
        .or_else(|| {
            styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "flex-direction"))
        })
    {
        if let crate::dom::StyleValue::Keyword(kw) = dir_val {
            match &**kw {
                "row" => style.flex_direction = FlexDirection::Row,
                "column" => style.flex_direction = FlexDirection::Column,
                _ => {}
            }
        }
    }

    let is_flex = styled_node
        .local
        .iter()
        .find(|(k, _)| &**k == "display")
        .or_else(|| {
            styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "display"))
        })
        .map(|(_, s)| s)
        == Some(&crate::dom::StyleValue::Keyword(string_cache::DefaultAtom::from("flex")));

    let has_flex_dir = styled_node
        .local
        .iter()
        .find(|(k, _)| &**k == "flex-direction")
        .or_else(|| {
            styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "flex-direction"))
        })
        .is_some();

    if !is_flex && !has_flex_dir {
        style.flex_direction = FlexDirection::Column;
    }

    if let Some((_, width_val)) = styled_node
        .local
        .iter()
        .find(|(k, _)| &**k == "width")
        .or_else(|| {
            styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "width"))
        })
    {
        if let Some(dim) = parse_dimension(width_val, vw, vh, font_size) {
            style.size.width = dim;
        }
    }

    if let Some((_, height_val)) = styled_node
        .local
        .iter()
        .find(|(k, _)| &**k == "height")
        .or_else(|| {
            styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "height"))
        })
    {
        if let Some(dim) = parse_dimension(height_val, vw, vh, font_size) {
            style.size.height = dim;
        }
    }

    let empty_vec = Vec::new();
    let iter = styled_node.inherited.as_deref().unwrap_or(&empty_vec).iter().chain(styled_node.local.iter());
    for (k, v) in iter {
        match &**k {
            "margin-top" => if let Some(dim) = parse_length_percentage_auto(v, vw, vh, font_size) { style.margin.top = dim; },
            "margin-right" => if let Some(dim) = parse_length_percentage_auto(v, vw, vh, font_size) { style.margin.right = dim; },
            "margin-bottom" => if let Some(dim) = parse_length_percentage_auto(v, vw, vh, font_size) { style.margin.bottom = dim; },
            "margin-left" => if let Some(dim) = parse_length_percentage_auto(v, vw, vh, font_size) { style.margin.left = dim; },
            "padding-top" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.padding.top = dim; },
            "padding-right" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.padding.right = dim; },
            "padding-bottom" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.padding.bottom = dim; },
            "padding-left" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.padding.left = dim; },
            "border-width" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) {
                style.border.top = dim;
                style.border.right = dim;
                style.border.bottom = dim;
                style.border.left = dim;
            },
            "border-top-width" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.border.top = dim; },
            "border-right-width" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.border.right = dim; },
            "border-bottom-width" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.border.bottom = dim; },
            "border-left-width" => if let Some(dim) = parse_length_percentage(v, vw, vh, font_size) { style.border.left = dim; },
            _ => {}
        }
    }

    if matches!(
        document.nodes.get(styled_node.node_id),
        Some(crate::dom::Node::Text(_))
    ) {
        tree.new_leaf_with_context(
            style,
            TextMeasureContext {
                node_id: styled_node.node_id,
                font_size,
            },
        )
        .unwrap()
    } else {
        let taffy_children = styled_node
            .children
            .iter()
            .map(|child| build_taffy_node(tree, document, child, vw, vh))
            .collect::<Vec<_>>();

        tree.new_with_children(style, &taffy_children).unwrap()
    }
}

#[inline]
fn parse_dimension(val: &crate::dom::StyleValue, vw: f32, vh: f32, font_size: f32) -> Option<Dimension> {
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
fn parse_length_percentage_auto(val: &crate::dom::StyleValue, vw: f32, vh: f32, font_size: f32) -> Option<taffy::style::LengthPercentageAuto> {
    match val {
        crate::dom::StyleValue::Auto => Some(taffy::style::LengthPercentageAuto::auto()),
        crate::dom::StyleValue::LengthPx(num) => Some(taffy::style::LengthPercentageAuto::length(*num)),
        crate::dom::StyleValue::Percent(p) => Some(taffy::style::LengthPercentageAuto::percent(*p / 100.0)),
        crate::dom::StyleValue::ViewportWidth(num) => Some(taffy::style::LengthPercentageAuto::length((num / 100.0) * vw)),
        crate::dom::StyleValue::ViewportHeight(num) => Some(taffy::style::LengthPercentageAuto::length((num / 100.0) * vh)),
        crate::dom::StyleValue::Em(num) => Some(taffy::style::LengthPercentageAuto::length(num * font_size)),
        crate::dom::StyleValue::Rem(num) => Some(taffy::style::LengthPercentageAuto::length(num * font_size)),
        _ => None,
    }
}

#[inline]
fn parse_length_percentage(val: &crate::dom::StyleValue, vw: f32, vh: f32, font_size: f32) -> Option<taffy::style::LengthPercentage> {
    match val {
        crate::dom::StyleValue::LengthPx(num) => Some(taffy::style::LengthPercentage::length(*num)),
        crate::dom::StyleValue::Percent(p) => Some(taffy::style::LengthPercentage::percent(*p / 100.0)),
        crate::dom::StyleValue::ViewportWidth(num) => Some(taffy::style::LengthPercentage::length((num / 100.0) * vw)),
        crate::dom::StyleValue::ViewportHeight(num) => Some(taffy::style::LengthPercentage::length((num / 100.0) * vh)),
        crate::dom::StyleValue::Em(num) => Some(taffy::style::LengthPercentage::length(num * font_size)),
        crate::dom::StyleValue::Rem(num) => Some(taffy::style::LengthPercentage::length(num * font_size)),
        _ => None,
    }
}
