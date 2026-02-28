//! Rendering module.
//!
//! Walks the Taffy layout tree alongside the `StyledNode` tree and issues
//! draw commands to an abstract renderer backend. Text is rendered via
//! pre-shaped `cosmic_text::LayoutGlyph` arrays rather than raw strings.
//! `inoda-core` does not depend on OpenGL APIs; platform binaries can
//! implement the `RendererBackend` trait using tiny-skia, LVGL, or any
//! other raster target.

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
    pub glyphs: Vec<cosmic_text::LayoutGlyph>,
}

pub trait RendererBackend {
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color);
    fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32, line_width: f32, color: Color);
    fn draw_glyphs(&mut self, x: f32, y: f32, glyphs: &[cosmic_text::LayoutGlyph], size: f32, color: Color);

    fn draw_text_layout(&mut self, lines: &[TextDrawLine], size: f32, color: Color) {
        for line in lines {
            self.draw_glyphs(line.x, line.baseline_y, &line.glyphs, size, color);
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

        if let Some((_, bg_color_val)) = styled_node
            .specified_values
            .iter()
            .find(|(k, _)| &**k == "background-color")
        {
            if let crate::dom::StyleValue::Color(r, g, b) = bg_color_val {
                renderer.fill_rect(abs_x, abs_y, layout.size.width, layout.size.height, Color { r: *r, g: *g, b: *b });
            }
        }

        if let Some((_, border_color_val)) = styled_node
            .specified_values
            .iter()
            .find(|(k, _)| &**k == "border-color")
        {
            if let crate::dom::StyleValue::Color(r, g, b) = border_color_val {
                renderer.stroke_rect(
                    abs_x,
                    abs_y,
                    layout.size.width,
                    layout.size.height,
                    1.0,
                    Color { r: *r, g: *g, b: *b },
                );
            }
        }

        if let Some(crate::dom::Node::Text(_txt)) = document.nodes.get(styled_node.node_id) {
            let mut color = Color { r: 0, g: 0, b: 0 };
            if let Some((_, color_val)) = styled_node
                .specified_values
                .iter()
                .find(|(k, _)| &**k == "color")
            {
                if let crate::dom::StyleValue::Color(r, g, b) = color_val {
                    color = Color { r: *r, g: *g, b: *b };
                }
            }

            let mut font_size = 16.0;
            if let Some((_, size_val)) = styled_node
                .specified_values
                .iter()
                .find(|(k, _)| &**k == "font-size")
            {
                match size_val {
                    crate::dom::StyleValue::LengthPx(num) => font_size = *num,
                    crate::dom::StyleValue::Number(num) => font_size = *num,
                    _ => {}
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
                        glyphs: line.glyphs.clone(),
                    })
                    .collect::<Vec<_>>();
                renderer.draw_text_layout(&lines, font_size, color);
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


