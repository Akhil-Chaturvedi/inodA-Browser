//! Rendering module.
//!
//! Walks the Taffy layout tree alongside the `StyledNode` tree and issues
//! femtovg draw calls for backgrounds, borders, and text. Requires the host
//! application to provide an OpenGL context and register fonts before text
//! will render.
//!
//! Color parsing: 5 named colors (red, green, blue, black, white) and
//! 6-digit hex (#rrggbb). No rgb(), rgba(), hsl(), or alpha support.

use crate::dom::StyledNode;
use femtovg::{Color, Paint, Path, Canvas, renderer::Renderer};
use taffy::TaffyTree;

pub fn draw_layout_tree<T: Renderer>(
    canvas: &mut Canvas<T>,
    document: &crate::dom::Document,
    layout_tree: &TaffyTree,
    styled_node: &StyledNode,
    layout_node_id: taffy::NodeId,
    offset_x: f32,
    offset_y: f32,
) {
    if let Ok(layout) = layout_tree.layout(layout_node_id) {
        let abs_x = offset_x + layout.location.x;
        let abs_y = offset_y + layout.location.y;

        // Draw Background
        if let Some((_, bg_color_str)) = styled_node.specified_values.iter().find(|(k, _)| k == "background-color") {
            if let Some(color) = parse_color(bg_color_str) {
                let mut path = Path::new();
                path.rect(abs_x, abs_y, layout.size.width, layout.size.height);
                
                let paint = Paint::color(color);
                canvas.fill_path(&path, &paint);
            }
        }

        // Draw Border
        if let Some((_, border_color_str)) = styled_node.specified_values.iter().find(|(k, _)| k == "border-color") {
             if let Some(color) = parse_color(border_color_str) {
                let mut path = Path::new();
                path.rect(abs_x, abs_y, layout.size.width, layout.size.height);
                
                let mut paint = Paint::color(color);
                paint.set_line_width(1.0); // Simplified default border width
                canvas.stroke_path(&path, &paint);
             }
        }

        // Draw Text
        let mut is_text = false;
        let mut text_content = String::new();
        if let Some(node) = document.nodes.get(styled_node.node_id) {
            if let crate::dom::Node::Text(txt) = node {
                is_text = true;
                text_content = txt.clone();
            }
        }

        if is_text {
            let mut paint = Paint::color(Color::rgb(0, 0, 0)); // Default black
            if let Some((_, color_str)) = styled_node.specified_values.iter().find(|(k, _)| k == "color") {
                 if let Some(color) = parse_color(color_str) {
                      paint = Paint::color(color);
                 }
            }
            
            paint.set_font_size(16.0);
            if let Some((_, size_str)) = styled_node.specified_values.iter().find(|(k, _)| k == "font-size") {
                if let Ok(size) = size_str.trim_end_matches("px").parse::<f32>() {
                     paint.set_font_size(size);
                }
            }
            // Render text. In an actual viewport app, we would load fonts into Canvas first to provide FontId.
            // But this will silently skip or render default if context exists.
            let _ = canvas.fill_text(abs_x, abs_y + paint.font_size(), &text_content, &paint);
        }

        // Recursively draw children
        // Taffy layout children map 1:1 to StyledNode children in our current bridge architecture
        if let Ok(children) = layout_tree.children(layout_node_id) {
            for (i, child_layout_id) in children.into_iter().enumerate() {
                 if let Some(child_style) = styled_node.children.get(i) {
                     draw_layout_tree(canvas, document, layout_tree, child_style, child_layout_id, abs_x, abs_y);
                 }
            }
        }
    }
}

// Basic CSS color string to femtovg::Color parser
fn parse_color(val: &str) -> Option<Color> {
    match val.trim() {
        "red" => Some(Color::rgb(255, 0, 0)),
        "green" => Some(Color::rgb(0, 255, 0)),
        "blue" => Some(Color::rgb(0, 0, 255)),
        "black" => Some(Color::rgb(0, 0, 0)),
        "white" => Some(Color::rgb(255, 255, 255)),
        // Simple hex fallback
        hex if hex.starts_with('#') && hex.len() == 7 => {
            let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
            let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
            let b = u8::from_str_radix(&hex[5..7], 16).ok()?;
            Some(Color::rgb(r, g, b))
        }
        _ => None,
    }
}
