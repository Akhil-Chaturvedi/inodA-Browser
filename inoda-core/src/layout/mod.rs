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

use crate::dom::StyledNode;
use taffy::{
    prelude::*,
    style::{Dimension, Style},
    TaffyTree,
};

#[derive(Debug, Clone, Copy)]
pub struct TextMeasureContext {
    pub node_id: crate::dom::NodeId,
    pub font_size: f32,
}

pub fn compute_layout(document: &crate::dom::Document, styled_node: &StyledNode, viewport_width: f32, viewport_height: f32) -> (TaffyTree<TextMeasureContext>, NodeId) {
    let mut tree: TaffyTree<TextMeasureContext> = TaffyTree::new();
    let root_taffy_node = build_taffy_node(&mut tree, document, styled_node, viewport_width, viewport_height);

    let available_space = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::Definite(viewport_height),
    };

    tree.compute_layout_with_measure(
        root_taffy_node,
        available_space,
        |_known_dimensions, available_space, _node_id, context: Option<&mut TextMeasureContext>, _style| {
            let Some(ctx) = context else {
                return taffy::geometry::Size::ZERO;
            };

            let width_constraint = match available_space.width {
                AvailableSpace::Definite(w) if w.is_finite() && w > 0.0 => w,
                _ => viewport_width.max(1.0),
            };

            let text = match document.nodes.get(ctx.node_id) {
                Some(crate::dom::Node::Text(txt)) => txt.text.trim(),
                _ => "",
            };

            let char_width = (ctx.font_size * 0.55).max(1.0);
            let text_width = (text.chars().count() as f32) * char_width;
            let lines = if text.is_empty() {
                1.0
            } else {
                (text_width / width_constraint).ceil().max(1.0)
            };

            taffy::geometry::Size {
                width: text_width.min(width_constraint),
                height: lines * ctx.font_size * 1.2,
            }
        },
    ).unwrap();

    (tree, root_taffy_node)
}

fn build_taffy_node(tree: &mut TaffyTree<TextMeasureContext>, document: &crate::dom::Document, styled_node: &StyledNode, vw: f32, vh: f32) -> NodeId {
    let mut style = Style::DEFAULT;

    let font_size = styled_node.specified_values.iter()
        .find(|(k, _)| &**k == "font-size")
        .and_then(|(_, v)| v.trim_end_matches("px").parse::<f32>().ok())
        .unwrap_or(16.0);

    if let Some((_, display_val)) = styled_node.specified_values.iter().find(|(k, _)| &**k == "display") {
        match display_val.as_str() {
            "flex" => style.display = Display::Flex,
            "grid" => style.display = Display::Grid,
            "none" => style.display = Display::None,
            "block" => style.display = Display::Block,
            "inline" | "inline-block" => {}
            _ => {}
        }
    }

    if let Some((_, dir_val)) = styled_node.specified_values.iter().find(|(k, _)| &**k == "flex-direction") {
        match dir_val.as_str() {
            "row" => style.flex_direction = FlexDirection::Row,
            "column" => style.flex_direction = FlexDirection::Column,
            _ => {}
        }
    }

    if styled_node.specified_values.iter().find(|(k, _)| &**k == "display").map(|(_, s)| s.as_str()) != Some("flex") {
        if styled_node.specified_values.iter().find(|(k, _)| &**k == "flex-direction").is_none() {
            style.flex_direction = FlexDirection::Column;
        }
    }

    if let Some((_, width_val)) = styled_node.specified_values.iter().find(|(k, _)| &**k == "width") {
        if let Some(dim) = parse_dimension(width_val, vw, vh, font_size) {
            style.size.width = dim;
        }
    }

    if let Some((_, height_val)) = styled_node.specified_values.iter().find(|(k, _)| &**k == "height") {
        if let Some(dim) = parse_dimension(height_val, vw, vh, font_size) {
            style.size.height = dim;
        }
    }

    if matches!(document.nodes.get(styled_node.node_id), Some(crate::dom::Node::Text(_))) {
        tree.new_leaf_with_context(style, TextMeasureContext {
            node_id: styled_node.node_id,
            font_size,
        }).unwrap()
    } else {
        let taffy_children = styled_node.children.iter()
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
    } else if val.ends_with("%") {
        if let Ok(num) = val.trim_end_matches("%").parse::<f32>() {
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
