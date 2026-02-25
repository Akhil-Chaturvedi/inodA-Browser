//! Layout computation module.
//!
//! Converts a `StyledNode` tree into a `TaffyTree` and runs the Flexbox/Grid
//! layout algorithm.
//!
//! Text nodes are measured against the available width so long text can wrap
//! into multiple lines, allowing Taffy to receive a realistic block height.
//!
//! Supported dimension units: px, %, vw, vh, em, rem, auto.
//! Supported display modes: flex, grid, block, none.
//! Properties like margin, padding, and alignment are not yet wired.

use std::{cell::RefCell, collections::HashMap};

use crate::dom::StyledNode;
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, Wrap};
use taffy::{
    TaffyTree,
    prelude::*,
    style::{Dimension, Style},
};

pub type TextLayoutCache = HashMap<crate::dom::NodeId, TextNodeLayout>;

#[derive(Debug, Clone)]
pub struct TextLineLayout {
    pub text: String,
    pub line_width: f32,
}

#[derive(Debug, Clone)]
pub struct TextNodeLayout {
    pub lines: Vec<TextLineLayout>,
    pub line_height: f32,
    pub width: f32,
    pub height: f32,
}

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
) -> (TaffyTree<TextMeasureContext>, NodeId, TextLayoutCache) {
    let mut tree: TaffyTree<TextMeasureContext> = TaffyTree::new();
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

    let font_system = RefCell::new(FontSystem::new());
    let measured_text_nodes: RefCell<TextLayoutCache> = RefCell::new(HashMap::new());

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

            let text = match document.nodes.get(ctx.node_id) {
                Some(crate::dom::Node::Text(txt)) => txt.text.as_str(),
                _ => "",
            };

            let mut fs = font_system.borrow_mut();
            let measured = measure_text_with_cosmic(&mut fs, text, width_constraint, ctx.font_size);
            measured_text_nodes
                .borrow_mut()
                .insert(ctx.node_id, measured.clone());

            taffy::geometry::Size {
                width: measured.width,
                height: measured.height,
            }
        },
    )
    .unwrap();

    (tree, root_taffy_node, measured_text_nodes.into_inner())
}

fn measure_text_with_cosmic(
    font_system: &mut FontSystem,
    text: &str,
    width_constraint: f32,
    font_size: f32,
) -> TextNodeLayout {
    let line_height = (font_size * 1.2).max(1.0);

    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    buffer.set_wrap(font_system, Wrap::WordOrGlyph);
    buffer.set_size(
        font_system,
        Some(width_constraint.max(1.0)),
        Some(f32::INFINITY),
    );
    buffer.set_text(font_system, text, Attrs::new(), Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);

    let mut lines = Vec::new();
    let mut max_width: f32 = 0.0;
    for run in buffer.layout_runs() {
        max_width = max_width.max(run.line_w);
        lines.push(TextLineLayout {
            text: run.text.to_string(),
            line_width: run.line_w,
        });
    }

    if lines.is_empty() {
        lines.push(TextLineLayout {
            text: String::new(),
            line_width: 0.0,
        });
    }

    let width = max_width.min(width_constraint.max(1.0));
    let height = (lines.len() as f32) * line_height;

    TextNodeLayout {
        lines,
        line_height,
        width,
        height,
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
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "font-size")
        .and_then(|(_, v)| v.trim_end_matches("px").parse::<f32>().ok())
        .unwrap_or(16.0);

    if let Some((_, display_val)) = styled_node
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "display")
    {
        match display_val.as_str() {
            "flex" => style.display = Display::Flex,
            "grid" => style.display = Display::Grid,
            "none" => style.display = Display::None,
            "block" => style.display = Display::Block,
            "inline" | "inline-block" => {}
            _ => {}
        }
    }

    if let Some((_, dir_val)) = styled_node
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "flex-direction")
    {
        match dir_val.as_str() {
            "row" => style.flex_direction = FlexDirection::Row,
            "column" => style.flex_direction = FlexDirection::Column,
            _ => {}
        }
    }

    if styled_node
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "display")
        .map(|(_, s)| s.as_str())
        != Some("flex")
    {
        if styled_node
            .specified_values
            .iter()
            .find(|(k, _)| &**k == "flex-direction")
            .is_none()
        {
            style.flex_direction = FlexDirection::Column;
        }
    }

    if let Some((_, width_val)) = styled_node
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "width")
    {
        if let Some(dim) = parse_dimension(width_val, vw, vh, font_size) {
            style.size.width = dim;
        }
    }

    if let Some((_, height_val)) = styled_node
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "height")
    {
        if let Some(dim) = parse_dimension(height_val, vw, vh, font_size) {
            style.size.height = dim;
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
fn parse_dimension(val: &str, vw: f32, vh: f32, font_size: f32) -> Option<Dimension> {
    let val = val.trim();
    if val == "auto" {
        return Some(Dimension::auto());
    }

    if val.ends_with("px") {
        if let Ok(num) = val.trim_end_matches("px").parse::<f32>() {
            return Some(Dimension::length(num));
        }
    } else if val.ends_with('%') {
        if let Ok(num) = val.trim_end_matches('%').parse::<f32>() {
            return Some(Dimension::percent(num / 100.0));
        }
    } else if val.ends_with("vw") {
        if let Ok(num) = val.trim_end_matches("vw").parse::<f32>() {
            return Some(Dimension::length(vw * num / 100.0));
        }
    } else if val.ends_with("vh") {
        if let Ok(num) = val.trim_end_matches("vh").parse::<f32>() {
            return Some(Dimension::length(vh * num / 100.0));
        }
    } else if val.ends_with("rem") || val.ends_with("em") {
        let trim_str = if val.ends_with("rem") { "rem" } else { "em" };
        if let Ok(num) = val.trim_end_matches(trim_str).parse::<f32>() {
            return Some(Dimension::length(font_size * num));
        }
    }
    None
}
