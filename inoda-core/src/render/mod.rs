//! Rendering module.
//!
//! Walks the Taffy layout tree alongside the arena DOM and issues draw commands
//! to an abstract renderer backend. Text is rendered via pre-shaped
//! `cosmic_text::LayoutGlyph` iterators rather than raw strings.
//! Draw properties (`bg_color`, `border_color`, `font_size`, `color`) are
//! read directly from `ComputedStyle` embedded in each arena node.
//! `inoda-core` does not depend on any graphics APIs; platform binaries
//! implement the `RendererBackend` trait using their own raster target.

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
    fn draw_glyphs(
        &mut self,
        x: f32,
        y: f32,
        glyphs: &[cosmic_text::LayoutGlyph],
        size: f32,
        color: Color,
    );
}

pub fn draw_layout_tree<R: RendererBackend>(
    renderer: &mut R,
    document: &crate::dom::Document,
    layout_tree: &taffy::TaffyTree,
    node_id: crate::dom::NodeId,
    layout_node_id: taffy::NodeId,
    offset_x: f32,
    offset_y: f32,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
) {
    let computed = match document.nodes.get(node_id) {
        Some(crate::dom::Node::Element(data)) => &data.computed,
        Some(crate::dom::Node::Text(data)) => &data.computed,
        _ => return,
    };

    if let Ok(layout) = layout_tree.layout(layout_node_id) {
        let abs_x = offset_x + layout.location.x;
        let abs_y = offset_y + layout.location.y;

        if let Some((r, g, b)) = computed.bg_color {
            renderer.fill_rect(
                abs_x,
                abs_y,
                layout.size.width,
                layout.size.height,
                Color { r, g, b },
            );
        }

        if let Some((r, g, b)) = computed.border_color {
            renderer.stroke_rect(
                abs_x,
                abs_y,
                layout.size.width,
                layout.size.height,
                1.0,
                Color { r, g, b },
            );
        }

        if let crate::dom::Node::Text(_) = document.nodes.get(node_id).unwrap() {
            let buffer = buffer_cache.get_mut(&node_id).unwrap();
            let color = Color {
                r: computed.color.0,
                g: computed.color.1,
                b: computed.color.2,
            };

            for run in buffer.layout_runs() {
                renderer.draw_glyphs(
                    abs_x,
                    abs_y + run.line_y,
                    run.glyphs,
                    computed.font_size,
                    color,
                );
            }
        } else if let Ok(children) = layout_tree.children(layout_node_id) {
            let mut dom_child_id = document.first_child_of(node_id);
            for taffy_child in children {
                if let Some(c) = dom_child_id {
                    draw_layout_tree(
                        renderer,
                        document,
                        layout_tree,
                        c,
                        taffy_child,
                        abs_x,
                        abs_y,
                        buffer_cache,
                    );
                    dom_child_id = document.next_sibling_of(c);
                }
            }
        }
    }
}
