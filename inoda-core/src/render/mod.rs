//! Rendering module.
//!
//! Walks the Taffy layout tree alongside the `StyledNode` tree and issues
//! draw commands to an abstract renderer backend. Text is rendered via
//! pre-shaped `cosmic_text::LayoutGlyph` arrays rather than raw strings.
//! `inoda-core` does not depend on OpenGL APIs; platform binaries can
//! implement the `RendererBackend` trait using tiny-skia, LVGL, or any
//! other raster target.

use crate::dom::StyledNode;
use taffy::TaffyTree;
use cosmic_text::Buffer;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub trait RendererBackend {
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color);
    fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32, line_width: f32, color: Color);
    fn draw_glyphs(&mut self, x: f32, y: f32, glyphs: &[cosmic_text::LayoutGlyph], size: f32, color: Color);

}

pub fn draw_layout_tree<R: RendererBackend>(
    renderer: &mut R,
    document: &crate::dom::Document,
    layout_tree: &TaffyTree,
    styled_node: &StyledNode,
    layout_node_id: taffy::NodeId,
    offset_x: f32,
    offset_y: f32,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    if let Ok(layout) = layout_tree.layout(layout_node_id) {
        let abs_x = offset_x + layout.location.x;
        let abs_y = offset_y + layout.location.y;

        if let Some((_, bg_color_val)) = styled_node
            .local
            .iter()
            .chain(styled_node.inherited.as_deref().unwrap_or(&vec![]).iter())
            .find(|(k, _)| &**k == "background-color")
        {
            if let crate::dom::StyleValue::Color(r, g, b) = bg_color_val {
                renderer.fill_rect(abs_x, abs_y, layout.size.width, layout.size.height, Color { r: r.clone(), g: g.clone(), b: b.clone() });
            }
        }

        if let Some((_, border_color_val)) = styled_node
            .local
            .iter()
            .chain(styled_node.inherited.as_deref().unwrap_or(&vec![]).iter())
            .find(|(k, _)| &**k == "border-color")
        {
            if let crate::dom::StyleValue::Color(r, g, b) = border_color_val {
                renderer.stroke_rect(
                    abs_x,
                    abs_y,
                    layout.size.width,
                    layout.size.height,
                    1.0,
                    Color { r: r.clone(), g: g.clone(), b: b.clone() },
                );
            }
        }

        if let crate::dom::Node::Text(_) = document.nodes.get(styled_node.node_id).unwrap() {
            if let Some(mut font_size) = styled_node
                .inherited
                .as_ref()
                .and_then(|i| i.iter().find(|(k, _)| &**k == "font-size"))
                .or_else(|| {
                    styled_node
                        .local
                        .iter()
                        .find(|(k, _)| &**k == "font-size")
                })
                .map(|(_, v)| match v {
                    crate::dom::StyleValue::LengthPx(px) => *px,
                    _ => 16.0,
                })
            {
                if font_size == 0.0 {
                    font_size = 16.0;
                }

                let text_color = styled_node
                    .inherited
                    .as_ref()
                    .and_then(|i| i.iter().find(|(k, _)| &**k == "color"))
                    .or_else(|| {
                        styled_node
                            .local
                            .iter()
                            .find(|(k, _)| &**k == "color")
                    })
                    .map(|(_, v)| match v {
                        crate::dom::StyleValue::Color(r, g, b) => Color { r: *r, g: *g, b: *b },
                        _ => Color { r: 0, g: 0, b: 0 },
                    })
                    .unwrap_or(Color { r: 0, g: 0, b: 0 });

                let line_height = (font_size * 1.2).max(1.0);

                if let Some(buffer) = buffer_cache.get(&styled_node.node_id) {
                    for (line_index, run) in buffer.layout_runs().enumerate() {
                        let baseline_y = abs_y + (line_index as f32 * line_height) + font_size;
                        renderer.draw_glyphs(abs_x, baseline_y, run.glyphs, font_size, text_color);
                    }
                }
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
                        buffer_cache,
                    );
                }
            }
        }
    }
}
