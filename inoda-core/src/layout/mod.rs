//! Layout computation module.
//!
//! Converts a `StyledNode` tree into a `TaffyTree` and runs the Flexbox/Grid
//! layout algorithm.
//!
//! Text nodes are measured against the available width so long text can wrap
//! into multiple lines. Taffy delegates `cosmic-text` measurements strictly matching
//! a persistent hoisted `TextLayoutCache` avoiding CPU loop lockups over re-evaluating shapes.
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
    font_system: &mut FontSystem,
    buffer_cache: &mut HashMap<crate::dom::NodeId, Buffer>,
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

    let font_system = RefCell::new(font_system);
    let measured_text_nodes: RefCell<TextLayoutCache> = RefCell::new(HashMap::new());
    let buffer_cache_cell = RefCell::new(buffer_cache);

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
            
            let buffer = b_cache.entry(ctx.node_id).or_insert_with(|| {
                let text = match document.nodes.get(ctx.node_id) {
                    Some(crate::dom::Node::Text(txt)) => txt.text.as_str(),
                    _ => "",
                };
                let line_height = (ctx.font_size * 1.2).max(1.0);
                let mut b = Buffer::new(&mut sys, Metrics::new(ctx.font_size, line_height));
                b.set_wrap(&mut sys, Wrap::WordOrGlyph);
                b.set_text(&mut sys, text, Attrs::new(), Shaping::Advanced);
                b
            });

            buffer.set_size(
                &mut sys,
                Some(width_constraint.max(1.0)),
                Some(f32::INFINITY),
            );
            buffer.shape_until_scroll(&mut sys, false);

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
            let line_height = (ctx.font_size * 1.2).max(1.0);
            let height = (lines.len() as f32) * line_height;

            let layout = TextNodeLayout {
                lines,
                line_height,
                width,
                height,
            };

            measured_text_nodes.borrow_mut().insert(ctx.node_id, layout);
            
            taffy::geometry::Size { width, height }
        },
    )
    .unwrap();

    (tree, root_taffy_node, measured_text_nodes.into_inner())
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
        .and_then(|(_, v)| match v {
            crate::dom::StyleValue::LengthPx(num) => Some(*num),
            crate::dom::StyleValue::Number(num) => Some(*num),
            _ => None,
        })
        .unwrap_or(16.0);

    if let Some((_, display_val)) = styled_node
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "display")
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
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "flex-direction")
    {
        if let crate::dom::StyleValue::Keyword(kw) = dir_val {
            match &**kw {
                "row" => style.flex_direction = FlexDirection::Row,
                "column" => style.flex_direction = FlexDirection::Column,
                _ => {}
            }
        }
    }

    if styled_node
        .specified_values
        .iter()
        .find(|(k, _)| &**k == "display")
        .map(|(_, s)| s)
        != Some(&crate::dom::StyleValue::Keyword(string_cache::DefaultAtom::from("flex")))
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
fn parse_dimension(val: &crate::dom::StyleValue, vw: f32, vh: f32, font_size: f32) -> Option<Dimension> {
    match val {
        crate::dom::StyleValue::Auto => Some(Dimension::auto()),
        crate::dom::StyleValue::LengthPx(num) => Some(Dimension::length(*num)),
        crate::dom::StyleValue::Percent(p) => Some(Dimension::percent(*p / 100.0)),
        crate::dom::StyleValue::Keyword(kw) => {
             let s = &**kw;
             if s.ends_with("vw") {
                 let num = s.trim_end_matches("vw").parse::<f32>().ok()?;
                 return Some(Dimension::length((num / 100.0) * vw));
             }
             if s.ends_with("vh") {
                 let num = s.trim_end_matches("vh").parse::<f32>().ok()?;
                 return Some(Dimension::length((num / 100.0) * vh));
             }
             if s.ends_with("em") || s.ends_with("rem") {
                 let num = s.trim_end_matches("rem").trim_end_matches("em").parse::<f32>().ok()?;
                 return Some(Dimension::length(num * font_size));
             }
             None
        }
        _ => None,
    }
}
