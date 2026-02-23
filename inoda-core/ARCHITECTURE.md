# Architecture

This document describes the data flow and module boundaries inside `inoda-core`.

## Pipeline

```
HTML string
  |
  v
html::parse_html()          -- html5ever tokenizer -> arena DOM (Document)
  |                            also extracts <style> tag contents
  v
css::parse_stylesheet()     -- cssparser tokenizer -> StyleSheet (Vec<StyleRule>)
css::compute_styles()       -- walks DOM, matches selectors, applies specificity,
  |                            inherits properties, expands shorthands -> StyledNode tree
  v
layout::compute_layout()    -- converts StyledNode tree to TaffyTree, resolves
  |                            dimensions (px, %, vw, vh, em, rem, auto),
  |                            runs Taffy flexbox/grid solver -> positioned Layout tree
  v
render::draw_layout_tree()  -- walks Layout + StyledNode in parallel,
                               issues femtovg draw calls: fill_path (bg), stroke_path
                               (border), fill_text (text content)
```

JavaScript execution happens outside this pipeline. The host application creates a `JsEngine`, passing in the `Document`. JS code can read and mutate the DOM via `Arc<Mutex<Document>>`, but there is currently no mechanism to trigger re-style or re-layout from JS mutations.

## Data structures

### Document (dom/mod.rs)

```
Document {
    nodes: Vec<Node>,       // flat arena, indexed by usize
    root_id: usize,         // always 0
    style_texts: Vec<String> // raw CSS from <style> tags
}

Node = Element(ElementData) | Text(String) | Root(Vec<NodeId>)

ElementData {
    tag_name: String,
    attributes: Vec<(String, String)>,
    children: Vec<NodeId>
}
```

Why a flat Vec instead of a tree of Rc/RefCell? Fewer allocations, better cache locality, simpler ownership for passing into closures (e.g., the JS engine).

### StyledNode (dom/mod.rs)

```
StyledNode {
    node_id: usize,
    specified_values: Vec<(String, String)>,  // computed CSS key-value pairs
    children: Vec<StyledNode>                 // mirrors DOM children
}
```

This is a tree (not arena). Each node owns its children. It exists only during layout computation and rendering, then gets dropped.

### StyleSheet (css/mod.rs)

```
StyleSheet { rules: Vec<StyleRule> }
StyleRule  { selectors: String, declarations: Vec<Declaration> }
Declaration { name: String, value: String }
```

Selectors are stored as a raw string (e.g., `"div.card, .header"`). Comma-separated selectors are split at match time. There is no parsed selector AST.

## Specificity

Selectors are scored as `(id_count, class_count, tag_count)`. Matched rules are sorted by this tuple. Equal-specificity rules preserve source order. Inline `style` attributes always win because they are applied after all stylesheet rules.

## Text measurement

Text nodes are inserted into Taffy as leaf nodes with a `String` context. During `compute_layout_with_measure`, the measure function estimates width as `char_count * 8.0` pixels and height as `18.0` pixels. This is a fixed-width monospace approximation. It does not account for font metrics, kerning, proportional widths, or line wrapping.

## Thread safety

`JsEngine` holds the `Document` inside `Arc<Mutex<Document>>`. Each JS-exposed function clones the `Arc` and locks the `Mutex` to access the DOM. This means JS execution is single-threaded (QuickJS itself is single-threaded) and DOM access is serialized.
