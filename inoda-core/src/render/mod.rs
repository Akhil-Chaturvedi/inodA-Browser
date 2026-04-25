//! Rendering module.
//!
//! Iteratively walks the Taffy layout tree alongside the arena DOM using an
//! explicit stack to prevent overflow on deep DOM trees. Issues draw commands
//! to an abstract renderer backend. Text is rendered via pre-shaped
//! `cosmic_text::LayoutGlyph` iterators rather than raw strings.
//! Draw properties (`bg_color`, `border_color`, `font_size`, `color`) are
//! read directly from `ComputedStyle` embedded in each arena node.
//! `inoda-core` does not depend on any graphics APIs; platform binaries
//! implement the `RendererBackend` trait using their own raster target.
//! The renderer is decoupled from the shaping system, receiving pre-shaped
//! glyph slices to allow for hardware-accelerated backends without
//! CPU-side font-shaping dependencies.

use cosmic_text::Buffer;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
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
    fn draw_image(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _url: &str) {}
}

pub fn draw_layout_tree<R: RendererBackend>(
    renderer: &mut R,
    document: &crate::dom::Document,
    layout_tree: &taffy::TaffyTree,
    root_node_id: crate::dom::NodeId,
    root_layout_node_id: taffy::NodeId,
    root_offset_x: f32,
    root_offset_y: f32,
    buffer_cache: &HashMap<crate::dom::NodeId, Buffer>,
) {
    // Reusable scratch buffer for collecting child tuples — avoids a
    // per-element `Vec::new()` allocation on every iteration (Item 6).
    let mut children_buf: Vec<(crate::dom::NodeId, taffy::NodeId, f32, f32)> = Vec::new();
    let mut stack = vec![(root_node_id, root_layout_node_id, root_offset_x, root_offset_y)];

    while let Some((node_id, layout_node_id, offset_x, offset_y)) = stack.pop() {
        if let Ok(layout) = layout_tree.layout(layout_node_id) {
            let abs_x = offset_x + layout.location.x;
            let abs_y = offset_y + layout.location.y;

            match document.nodes.get(node_id) {
                Some(crate::dom::Node::Element(data)) => {
                    if let Some((r, g, b, a)) = data.computed.bg_color {
                        renderer.fill_rect(
                            abs_x,
                            abs_y,
                            layout.size.width,
                            layout.size.height,
                            Color { r, g, b, a },
                        );
                    }

                    if let Some((r, g, b, a)) = data.computed.border_color {
                        renderer.stroke_rect(
                            abs_x,
                            abs_y,
                            layout.size.width,
                            layout.size.height,
                            1.0,
                            Color { r, g, b, a },
                        );
                    }

                    if &*data.tag_name == "img" {
                        if let Some((_, src)) = data.attributes.iter().find(|(k, _)| k == "src") {
                            renderer.draw_image(abs_x, abs_y, layout.size.width, layout.size.height, src);
                        }
                    }

                    // Collect children into the reusable scratch buffer
                    children_buf.clear();
                    let mut dom_child_id = document.first_child_of(node_id);
                    while let Some(c) = dom_child_id {
                        let t_node = match document.nodes.get(c) {
                            Some(crate::dom::Node::Element(d)) => d.taffy_node,
                            Some(crate::dom::Node::Text(d)) => d.taffy_node,
                            Some(crate::dom::Node::Root(d)) => d.taffy_node,
                            _ => None,
                        };

                        if let Some(tn) = t_node {
                            children_buf.push((c, tn, abs_x, abs_y));
                        }
                        dom_child_id = document.next_sibling_of(c);
                    }

                    // Push in reverse order so that the first child is popped first
                    for child in children_buf.iter().rev() {
                        stack.push(*child);
                    }
                }
                Some(crate::dom::Node::Text(data)) => {
                    let buffer = buffer_cache.get(&node_id).unwrap();
                    let color = Color {
                        r: data.computed.color.0,
                        g: data.computed.color.1,
                        b: data.computed.color.2,
                        a: data.computed.color.3,
                    };

                    for run in buffer.layout_runs() {
                        renderer.draw_glyphs(
                            abs_x,
                            abs_y + run.line_y,
                            run.glyphs,
                            data.computed.font_size,
                            color,
                        );
                    }
                }
                _ => continue,
            }
        }
    }
}
