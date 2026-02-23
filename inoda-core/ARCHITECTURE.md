# Architecture

This document describes the data flow and module boundaries inside `inoda-core`.

## Pipeline

```
HTML string
  |
  v
html::parse_html()          -- html5ever tokenizer -> TreeSink -> arena DOM (Document)
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

JavaScript execution happens outside this pipeline. The host application creates a `JsEngine`, passing in the `Document`. JS code can read and mutate the DOM via `Arc<Mutex<Document>>`, but there is currently no mechanism to trigger re-style or re-layout from JS mutations. Timer callbacks registered via `setTimeout` fire only when the host calls `JsEngine::pump()`.

## Data structures

### Document (dom/mod.rs)

```
Document {
    nodes: Arena<Node>,             // generational_arena::Arena, indexed by Index
    root_id: generational_arena::Index,
    style_texts: Vec<String>        // raw CSS from <style> tags
}

Node = Element(ElementData) | Text(String) | Root(Vec<NodeId>)
NodeId = generational_arena::Index  // type alias

ElementData {
    tag_name: String,
    attributes: Vec<(String, String)>,
    children: Vec<NodeId>
}
```

Generational indices provide O(1) insertion and deletion without index invalidation or ABA problems. Removed nodes do not leave dangling references. Previous versions used a flat `Vec<Node>` indexed by `usize`, which could not safely delete nodes.

### StyledNode (dom/mod.rs)

```
StyledNode {
    node_id: NodeId,                            // generational_arena::Index
    specified_values: Vec<(String, String)>,     // computed CSS key-value pairs
    children: Vec<StyledNode>                   // mirrors DOM children
}
```

This is a tree (not arena). Each node owns its children. It exists only during layout computation and rendering, then gets dropped.

### StyleSheet (css/mod.rs)

```
StyleSheet { rules: Vec<StyleRule> }
StyleRule  { selectors: String, declarations: Vec<Declaration> }
Declaration { name: String, value: String }
```

Selectors are stored as a raw string (e.g., `"div.card, .header"`). Comma-separated selectors are split at match time. There is no parsed selector AST. Inline `style` attributes are parsed using `cssparser`'s `DeclarationParser` trait directly, without wrapping in a fake rule block.

### PendingTimer (js/mod.rs)

```
PendingTimer {
    id: u32,
    fire_at: Instant,
    callback: Persistent<Function<'static>>     // rquickjs::Persistent
}
```

Timer callbacks are stored as `rquickjs::Persistent<Function>` which safely holds a JS function reference outside the QuickJS context lifetime. They are restored and invoked inside `JsEngine::pump()`.

## Specificity

Selectors are scored as `(id_count, class_count, tag_count)`. Matched rules are sorted by this tuple. Equal-specificity rules preserve source order. Inline `style` attributes always win because they are applied after all stylesheet rules.

## Text measurement

Text nodes are inserted into Taffy as leaf nodes with a `String` context. During `compute_layout_with_measure`, the measure function estimates width as `char_count * 8.0` pixels and height as `18.0` pixels. This is a fixed-width monospace approximation. It does not account for font metrics, kerning, proportional widths, or line wrapping.

## Thread safety

`JsEngine` holds the `Document` inside `Arc<Mutex<Document>>`. Each JS-exposed function clones the `Arc` and locks the `Mutex` to access the DOM. This means JS execution is single-threaded (QuickJS itself is single-threaded) and DOM access is serialized. Timer state (`next_timer_id`, `pending_timers`) is also behind `Arc<Mutex<>>`.

## HTML parsing

The HTML module implements `html5ever::TreeSink` directly on a `DocumentBuilder` wrapper. This allows the parser to stream tokens into the generational arena in a single pass, without constructing an intermediate `RcDom` tree. The `DocumentBuilder` uses `RefCell` for interior mutability since `TreeSink` methods take `&self`.
