//! Rendering module.
//!
//! Walks the Taffy layout tree alongside the `StyledNode` tree and issues
//! draw commands to an abstract renderer backend. `inoda-core` does not
//! depend on OpenGL APIs; platform binaries can implement this trait using
//! tiny-skia, LVGL, or any other raster target.

use crate::{dom::StyledNode, layout::TextLayoutCache};
use taffy::TaffyTree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone)]
pub struct TextDrawLine {
    pub x: f32,
    pub baseline_y: f32,
    pub text: String,
}

pub trait RendererBackend {
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color);
    fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32, line_width: f32, color: Color);
    fn draw_text(&mut self, x: f32, y: f32, text: &str, size: f32, color: Color);

    fn draw_text_layout(&mut self, lines: &[TextDrawLine], size: f32, color: Color) {
        for line in lines {
            self.draw_text(line.x, line.baseline_y, &line.text, size, color);
        }
    }
}

pub fn draw_layout_tree<R: RendererBackend>(
    renderer: &mut R,
    document: &crate::dom::Document,
    layout_tree: &TaffyTree,
    styled_node: &StyledNode,
    layout_node_id: taffy::NodeId,
    offset_x: f32,
    offset_y: f32,
    text_layouts: Option<&TextLayoutCache>,
) {
    if let Ok(layout) = layout_tree.layout(layout_node_id) {
        let abs_x = offset_x + layout.location.x;
        let abs_y = offset_y + layout.location.y;

        if let Some((_, bg_color_str)) = styled_node
            .specified_values
            .iter()
            .find(|(k, _)| &**k == "background-color")
        {
            if let Some(color) = parse_color(bg_color_str) {
                renderer.fill_rect(abs_x, abs_y, layout.size.width, layout.size.height, color);
            }
        }

        if let Some((_, border_color_str)) = styled_node
            .specified_values
            .iter()
            .find(|(k, _)| &**k == "border-color")
        {
            if let Some(color) = parse_color(border_color_str) {
                renderer.stroke_rect(
                    abs_x,
                    abs_y,
                    layout.size.width,
                    layout.size.height,
                    1.0,
                    color,
                );
            }
        }

        if let Some(crate::dom::Node::Text(txt)) = document.nodes.get(styled_node.node_id) {
            let mut color = Color { r: 0, g: 0, b: 0 };
            if let Some((_, color_str)) = styled_node
                .specified_values
                .iter()
                .find(|(k, _)| &**k == "color")
            {
                if let Some(parsed) = parse_color(color_str) {
                    color = parsed;
                }
            }

            let mut font_size = 16.0;
            if let Some((_, size_str)) = styled_node
                .specified_values
                .iter()
                .find(|(k, _)| &**k == "font-size")
            {
                if let Ok(size) = size_str.trim_end_matches("px").parse::<f32>() {
                    font_size = size;
                }
            }

            if let Some(cache) = text_layouts.and_then(|m| m.get(&styled_node.node_id)) {
                let lines = cache
                    .lines
                    .iter()
                    .enumerate()
                    .map(|(line_index, line)| TextDrawLine {
                        x: abs_x,
                        baseline_y: abs_y + (line_index as f32 * cache.line_height) + font_size,
                        text: line.text.clone(),
                    })
                    .collect::<Vec<_>>();
                renderer.draw_text_layout(&lines, font_size, color);
            } else {
                renderer.draw_text(abs_x, abs_y + font_size, &txt.text, font_size, color);
            }
        }

        if let Ok(children) = layout_tree.children(layout_node_id) {
            for (i, child_layout_id) in children.into_iter().enumerate() {
                if let Some(child_style) = styled_node.children.get(i) {
                    draw_layout_tree(
                        renderer,
                        document,
                        layout_tree,
                        child_style,
                        child_layout_id,
                        abs_x,
                        abs_y,
                        text_layouts,
                    );
                }
            }
        }
    }
}

fn parse_color(val: &str) -> Option<Color> {
    match val.trim() {
        "red" => Some(Color { r: 255, g: 0, b: 0 }),
        "green" => Some(Color { r: 0, g: 255, b: 0 }),
        "blue" => Some(Color { r: 0, g: 0, b: 255 }),
        "black" => Some(Color { r: 0, g: 0, b: 0 }),
        "white" => Some(Color {
            r: 255,
            g: 255,
            b: 255,
        }),
        hex if hex.starts_with('#') && hex.len() == 7 => {
            let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
            let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
            let b = u8::from_str_radix(&hex[5..7], 16).ok()?;
            Some(Color { r, g, b })
        }
        _ => None,
    }
}
