//! Layout computation module.
//!
//! Converts a `StyledNode` tree into a `TaffyTree` and runs the Flexbox/Grid
//! layout algorithm. Text nodes are measured with a fixed-width heuristic
//! (8px per character, 18px height) -- not real text shaping.
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

/// Computes the visual layout bounds of all elements in the styled tree based on the provided viewport constraints.
pub fn compute_layout(document: &crate::dom::Document, styled_node: &StyledNode, viewport_width: f32, viewport_height: f32) -> (TaffyTree<String>, NodeId) {
    let mut tree: TaffyTree<String> = TaffyTree::new();
    
    // Recursive bridge to insert the styled node tree into Taffy
    let root_taffy_node = build_taffy_node(&mut tree, document, styled_node, viewport_width, viewport_height);

    // Run the massive computation algorithm from flexbox/grid layout constraints
    let available_space = Size {
        width: AvailableSpace::Definite(viewport_width),
        height: AvailableSpace::Definite(viewport_height),
    };
    tree.compute_layout_with_measure(
        root_taffy_node,
        available_space,
        |_known_dimensions, _available_space, _node_id, context: Option<&mut String>, _style| {
            if let Some(text) = context {
                let char_count = text.trim().len() as f32;
                taffy::geometry::Size {
                    width: char_count * 8.0,
                    height: 18.0,
                }
            } else {
                taffy::geometry::Size::ZERO
            }
        },
    ).unwrap();

    // After this finishes, you can obtain spatial constraints by querying the tree via:
    // tree.layout(node_id).unwrap() // Returns a `taffy::Layout` struct (x, y, width, height)
    (tree, root_taffy_node)
}

fn build_taffy_node(tree: &mut TaffyTree<String>, document: &crate::dom::Document, styled_node: &StyledNode, vw: f32, vh: f32) -> NodeId {
    // Phase 1: convert text-based CSS string values on this StyledNode into
    // Strongly-typed Taffy CSS enums
    let mut style = Style::DEFAULT;
    
    // Resolve font size for em calculations (defaulting to 16.0 px)
    let font_size = styled_node.specified_values.iter()
        .find(|(k, _)| k == "font-size")
        .and_then(|(_, v)| v.trim_end_matches("px").parse::<f32>().ok())
        .unwrap_or(16.0);

    // Apply Display (flex, grid, block, none)
    if let Some((_, display_val)) = styled_node.specified_values.iter().find(|(k, _)| k == "display") {
        match display_val.as_str() {
            "flex" => style.display = Display::Flex,
            "grid" => style.display = Display::Grid,
            "none" => style.display = Display::None,
            "block" => style.display = Display::Block,
            "inline" | "inline-block" => {
                // Approximate inline by wrapping or laying out horizontally if parent is flex
                // Taffy doesn't have native "inline" text flow yet, but wait Block can be used
            }
            _ => {}
        }
    }

    if let Some((_, dir_val)) = styled_node.specified_values.iter().find(|(k, _)| k == "flex-direction") {
        match dir_val.as_str() {
            "row" => style.flex_direction = FlexDirection::Row,
            "column" => style.flex_direction = FlexDirection::Column,
            _ => {}
        }
    }

    // Default 'block' elements stacking top-to-bottom if no direction was specified
    if styled_node.specified_values.iter().find(|(k, _)| k == "display").map(|(_, s)| s.as_str()) != Some("flex") {
        if styled_node.specified_values.iter().find(|(k, _)| k == "flex-direction").is_none() {
            style.flex_direction = FlexDirection::Column;
        }
    }

    // Apply Dimensions (width, height)
    if let Some((_, width_val)) = styled_node.specified_values.iter().find(|(k, _)| k == "width") {
        if let Some(dim) = parse_dimension(width_val, vw, vh, font_size) {
            style.size.width = dim;
        }
    }

    if let Some((_, height_val)) = styled_node.specified_values.iter().find(|(k, _)| k == "height") {
        if let Some(dim) = parse_dimension(height_val, vw, vh, font_size) {
            style.size.height = dim;
        }
    }
    
    // Add additional properties here like margins, padding, aligning etc.

    let mut is_text = false;
    let mut text_content = String::new();
    if let Some(node) = document.nodes.get(styled_node.node_id) {
        if let crate::dom::Node::Text(txt) = node {
            is_text = true;
            text_content = txt.clone();
        }
    }

    // Phase 2: Recursively build Taffy nodes for all children
    if is_text {
        // Create a leaf node with context (the text)
        let text_clone = text_content.clone();
        tree.new_leaf_with_context(style, text_clone).unwrap()
    } else {
        let mut taffy_children = Vec::new();
        for child in &styled_node.children {
            let child_taffy_id = build_taffy_node(tree, document, child, vw, vh);
            taffy_children.push(child_taffy_id);
        }

        // Phase 3: Insert into the tree and return ID
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
