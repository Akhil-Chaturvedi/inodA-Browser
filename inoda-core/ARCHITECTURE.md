# Architecture

This document describes the data flow and module boundaries inside `inoda-core`.

## Pipeline

```
HTML string
  |
  v
html::parse_html()          -- html5gum token loop -> arena DOM (Document)
  |                            <style> tag mutations set document.styles_dirty = true
  v
css::compute_styles()       -- rebuilds document.stylesheet if styles_dirty,
  |                            walks the arena DOM recursively,
  |                            resolves specificity against StyleSheet hash-map buckets,
  |                            evaluates combinators by walking parent pointers,
  |                            populates ComputedStyle in-place on each ElementData/TextData
  v
layout::compute_layout()    -- prepares text buffers in a pre-pass,
  |                            calculates max/min intrinsic widths,
  |                            builds a TaffyTree from the arena DOM,
  |                            uses O(1) metrics for measure estimations,
  |                            resolves dimensions (px, %, vw, vh, em, rem, auto),
  |                            runs Taffy flexbox/grid solver -> positioned Layout tree
  v
render::draw_layout_tree()  -- walks Taffy layout tree alongside the arena DOM,
                               reads ComputedStyle directly from each node,
                               issues draw calls: fill_rect (bg), stroke_rect (border),
                               draw_glyphs (text content)
```

JavaScript execution is separate from this pipeline. DOM nodes exposed to JS carry a `js_handles` reference count. QuickJS wrapper objects are tracked by a `FinalizationRegistry`; when GC'd, they decrement the `js_handles` count for the corresponding Rust arena node. Detached nodes are only wiped from the arena when no JS handles remain.

JS mutations set `document.dirty = true`. The host application is responsible for checking `dirty` and re-running `compute_styles`, `compute_layout`, and `draw_layout_tree` after JS mutations. Timer callbacks registered via `setTimeout` or `setInterval` fire only when the host calls `JsEngine::pump()`. Every 60 ticks, `runtime.run_gc()` is called to process the `FinalizationRegistry` and release unreferenced nodes.

## Data structures

### Document (dom/mod.rs)

```
Document {
    nodes: Arena<Node>,              // generational_arena::Arena, indexed by NodeId
    root_id: NodeId,
    stylesheet: StyleSheet,          // persistent, updated when <style> tags are parsed
    id_map: HashMap<String, NodeId>, // O(1) getElementById lookup
    dead_nodes: Vec<NodeId>,         // iterative deletion queue for remove_node
    dirty: bool,                     // set true by JS DOM mutations, cleared by host after re-render
    styles_dirty: bool,              // set true when <style> tags change, triggers rebuild
}

Node = Element(ElementData) | Text(TextData) | Root(RootData)
NodeId = generational_arena::Index  // type alias

ElementData {
    tag_name: LocalName,                             // Standard(DefaultAtom) for HTML tags, Custom(String) for custom elements
    attributes: Vec<(string_cache::DefaultAtom, String)>,
    classes: Vec<String>,                            // heap-allocated Strings; never interned to avoid global atom pool growth
    parent:       Option<NodeId>,
    first_child:  Option<NodeId>,
    last_child:   Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    computed: ComputedStyle,          // populated by css::compute_styles()
    js_handles: usize,                // reference count for JS engine
}

TextData {
    text: String,
    parent:       Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    computed: ComputedStyle,
    js_handles: usize,
}

RootData {
    first_child: Option<NodeId>,
    last_child:  Option<NodeId>,
    js_handles:  usize,
}
```

Generational indices prevent ABA problems. The DOM tree is wired as an intrusive linked list; mutations do not allocate child vectors. Node deletion uses an iterative queue rather than recursion to avoid stack overflow on deep trees.

`LocalName` separates standard HTML tags (interned as `DefaultAtom`) from custom element names (heap-allocated `String`). This prevents unbounded growth of the global `DefaultAtom` intern pool when arbitrary custom element names are created from JavaScript.

`ElementData::classes` stores each class token as a plain `String`. CSS class names are uncontrolled user input (modern frameworks generate randomized names like `css-1x8g9u`); interning them as `DefaultAtom` would cause the global intern pool to grow unboundedly and never shrink. ID values are also stored as `String` in `id_map` and are looked up by `&str`. Attribute keys for recognized HTML attributes (e.g. `id`, `class`, `style`) are still interned as `DefaultAtom` since the set of valid attribute names is bounded.

### ComputedStyle (dom/mod.rs)

```
ComputedStyle {
    display:       DefaultAtom,        // "block", "flex", "grid", "none"
    flex_direction: DefaultAtom,       // "row", "column"
    width:         StyleValue,
    height:        StyleValue,
    margin:        [StyleValue; 4],    // top, right, bottom, left
    padding:       [StyleValue; 4],
    border_width:  [StyleValue; 4],
    bg_color:      Option<(u8, u8, u8)>,
    border_color:  Option<(u8, u8, u8)>,
    font_size:     f32,                // absolute pixels, resolved during cascade
    color:         (u8, u8, u8),
}
```

`ComputedStyle` is stored directly inside `ElementData` and `TextData`. It is populated once during `css::compute_styles()` and read by both `layout::compute_layout()` and `render::draw_layout_tree()`. There is no intermediate styled-node tree that gets built and dropped per frame.

`StyleValue` is:

```
StyleValue = LengthPx(f32) | Percent(f32) | ViewportWidth(f32) | ViewportHeight(f32)
           | Em(f32) | Rem(f32) | Number(f32) | Keyword(DefaultAtom)
           | Color(u8, u8, u8) | Auto | None
```

`Em` and `Rem` are stored as-is in most properties and resolved to absolute pixels in `layout/mod.rs` using the element's `font_size`. For `font-size` itself, `Em` is resolved during the cascade by multiplying against the parent element's `font_size`; `Rem` always uses 16px as the root baseline.

### StyleSheet (css/mod.rs)

```
StyleSheet {
    by_id:    HashMap<String, Vec<IndexedRule>>,
    by_class: HashMap<String, Vec<IndexedRule>>,
    by_tag:   HashMap<DefaultAtom, Vec<IndexedRule>>,
    universal: Vec<IndexedRule>,
    next_rule_index: usize,
}

IndexedRule   { selector: ComplexSelector, declarations: Rc<Vec<Declaration>>, rule_index: usize }
ComplexSelector { last: CompoundSelector, ancestors: Vec<(Combinator, CompoundSelector)>, specificity: (u32, u32, u32) }
Combinator    = Descendant | Child
Declaration   { name: PropertyName, value: StyleValue }
```

`PropertyName` is a strongly-typed enum covering all CSS properties the engine recognizes (`Display`, `Width`, `MarginTop`, `Color`, `FontSize`, etc.) with an `Other(u64)` variant for unrecognized names. It replaces `DefaultAtom` as the key in `Declaration`, eliminating string-deref comparisons from the cascade hot path. Property matching during `compute_styles` is a direct enum equality check.

Selectors are pre-parsed into ASTs at stylesheet creation time. Specificity is computed once. Rules are distributed into hash-map buckets based on their right-most simple selector. During style resolution, matching buckets are merged via a k-way pointer walk over pre-sorted slices.

`StyleSheet` is stored persistently on `Document`. When HTML parsing encounters a `<style>` tag, `css::append_stylesheet()` merges the new rules into `document.stylesheet` in-place. This replaces the previous approach of collecting raw CSS text strings for batch parsing.

### PendingTimer (js/mod.rs)

```
PendingTimer {
    id:          u32,
    fire_at:     Instant,
    callback:    Persistent<Function<'static>>,  // rquickjs::Persistent
    is_interval: bool,
    delay_ms:    u64,
}
```

Timers are stored in a `std::collections::BinaryHeap` ordered by `fire_at` (min-heap via reversed `Ord`). Cancelled timer IDs are tracked in a `HashSet<u32>`; `pump()` skips any popped timer whose ID is in the set. When an interval timer fires, `pump()` reschedules it by pushing a new `PendingTimer` with `fire_at = now + delay_ms`. The `BinaryHeap` does not support in-place cancellation -- the cancel set approach avoids rebuilding the heap on `clearTimeout`.

## Cascade and inheritance

`css::compute_styles()` walks the arena DOM recursively. For each element node it:

1. Looks up matching rules from `document.stylesheet` buckets (by ID, class, tag, universal).
2. Merges matched rules using a k-way specificity-ordered pointer walk.
3. Applies inline `style` attribute declarations last (highest priority).
4. Builds a new `Vec<(PropertyName, StyleValue)>` of matched declarations.
5. Inherits inheritable properties (`color`, `font-size`, `font-family`, `font-weight`, `line-height`, `text-align`, `visibility`) from the parent via `Rc::clone` when no new inheritable values apply.
6. Populates `ComputedStyle` on the node via direct enum matching on `PropertyName`.
7. Recurses into children, passing the updated inheritable-style vector.

Combinator evaluation (`>` child, space descendant) walks arena parent pointers rather than maintaining a separate ancestor stack.

## JavaScript bridge

`JsEngine` holds the `Document` inside `Rc<RefCell<Document>>`. DOM-mutating JS functions call `doc.dirty = true` after making changes.

Each DOM node exposed to JS carries a `__nodeKey` property: a two-element array `[u32 index, u64 generation]`. The `__nodeCache` Map is keyed by a `BigInt` value computed as `BigInt(index) | (BigInt(generation) << 32n)`. This avoids string allocation for cache keys. A `FinalizationRegistry` receives the integer array when QuickJS garbage-collects a wrapper; it recomputes the BigInt key for cache removal and calls `_garbageCollectNodeRaw` with the raw integer array. `_garbageCollectNodeRaw` reconstructs the `NodeId` directly from the two integers and removes the arena entry only if the node is not attached to the DOM tree.

`JsEngine` also maintains a `pump_ticks` counter. Every 60 calls to `pump()`, `runtime.run_gc()` is called synchronously. This forces QuickJS to sweep `FinalizationRegistry` callbacks that it would otherwise defer indefinitely, preventing the arena from accumulating detached nodes until the host runs out of memory.

`NodeHandle` does not implement `Drop`. Nodes created by JavaScript persist in the arena until explicitly removed via `removeChild()`. This prevents QuickJS garbage collection from triggering arena mutations on nodes that are still part of the tree.

## Text measurement

The text render loop calls `buffer.layout_runs()` and invokes `draw_glyphs` once per `LayoutRun`, passing `run.glyphs` (a `&[LayoutGlyph]` slice borrowed directly from the buffer) and `run.line_y` as the vertical offset. No intermediate `Vec` is allocated during the render pass.

During `compute_layout_with_measure`, the measure closure calls `buffer.set_size()` and `buffer.shape_until_scroll()` to re-wrap text at the available width. After the layout solver finishes, `finalize_text_measurements` reshapes each buffer at its final resolved width. The renderer reads `LayoutGlyph` iterators directly from `buffer.layout_runs()`.

## HTML parsing

The HTML module iterates `html5gum` tokens in a loop. `StartTag` tokens create `ElementData` nodes and append them under `current_parent`. Before appending, the parser checks whether the new tag should implicitly close an open ancestor (e.g., `<div>` closing an open `<p>`). The walk stops at block-level boundary tags (`div`, `body`, `td`, `th`, `table`). `EndTag` tokens walk `current_parent` back up to the matching ancestor. Byte slices are validated as UTF-8 directly without allocating through `String::from_utf8_lossy`.

Content inside `<script>` and `<style>` is accumulated as raw text. The matching closing tag exits the raw state. `<style>` content is parsed immediately into `document.stylesheet` via `css::append_stylesheet()`.

## Thread safety

`JsEngine` uses `Rc<RefCell<Document>>`. QuickJS and `rquickjs` are single-threaded by design. `JsEngine` is not `Send`. All DOM access from JS callbacks is serialized through the `RefCell`. There are no mutexes or atomic operations in the engine core.
