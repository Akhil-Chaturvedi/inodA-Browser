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
css::compute_styles()       -- iterative stack-based traversal (no recursion),
  |                            resolves specificity against StyleSheet hash-map buckets,
  |                            evaluates combinators by walking parent pointers,
  |                            deduplicates ComputedStyle pointers via StyleCache,
  |                            assigns Rc<ComputedStyle> to each ElementData/TextData
  |
v
layout::compute_layout()    -- prepares text buffers in a pre-pass,
  |                            calculates max/min intrinsic widths,
  |                            builds a TaffyTree from the arena DOM,
  |                            performs accurate re-wrapping via buffer.set_size(),
  |                            resolves dimensions (px, %, vw, vh, em, rem, auto),
  |                            runs Taffy flexbox/grid solver -> positioned Layout tree
  v
render::draw_layout_tree()  -- walks Taffy layout tree alongside the arena DOM,
                               reads ComputedStyle directly from each node,
                               issues draw calls: fill_rect (bg), stroke_rect (border),
                               draw_glyphs (text content)
```

JavaScript execution is separate from this pipeline. DOM nodes exposed to JS carry a `js_handles` reference count. QuickJS wrapper objects are tracked by a `FinalizationRegistry`; when GC'd, they decrement the `js_handles` count for the corresponding Rust arena node. Detached nodes are only wiped from the arena when no JS handles remain.

JS mutations set `document.dirty = true`. The host application is responsible for checking `dirty` and re-running `compute_styles`, `compute_layout`, and `draw_layout_tree` after JS mutations. Timer callbacks registered via `setTimeout` or `setInterval` fire only when the host calls `JsEngine::pump()`. `pump()` executes pending JavaScript jobs (microtasks/promises) asynchronously to sweep `FinalizationRegistry` callbacks. Every 60 ticks, `document.collect_garbage()` is called to clear the batched deletion queue and reclaim memory from detached, unreferenced nodes.

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
    style_cache: HashMap<ComputedStyle, Rc<ComputedStyle>>, // style sharing pool
}

Node = Element(ElementData) | Text(TextData) | Root(RootData)
NodeId = generational_arena::Index  // type alias

ElementData {
    tag_name: LocalName,                             // Standard(DefaultAtom) for HTML tags, Custom(String) for custom elements
    attributes: Vec<(String, String)>,
    classes: String,                                 // flat space-separated String; never interned to avoid global atom pool growth
    cached_inline_styles: Option<Vec<(PropertyName, StyleValue)>>, // O(1) style lookup
    parent:       Option<NodeId>,
    first_child:  Option<NodeId>,
    last_child:   Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    computed: Rc<ComputedStyle>,      // shared via StyleCache
    js_handles: usize,                // reference count for JS engine
    layout_dirty: bool,               // triggers text buffer re-shaping
}

TextData {
    text: String,
    parent:       Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    computed: Rc<ComputedStyle>,      // shared via StyleCache
    js_handles: usize,
    layout_dirty: bool,
}

RootData {
    first_child: Option<NodeId>,
    last_child:  Option<NodeId>,
    js_handles:  usize,
}
```

Generational indices prevent ABA problems. The DOM tree is wired as an intrusive linked list; mutations do not allocate child vectors. Node deletion uses an iterative queue rather than recursion to avoid stack overflow on deep trees.

`LocalName` separates standard HTML tags (interned as `DefaultAtom`) from custom element names (heap-allocated `String`). This prevents unbounded growth of the global `DefaultAtom` intern pool when arbitrary custom element names are created from JavaScript.

`ElementData::classes` stores class tokens in a single space-separated `String`. CSS class names are uncontrolled user input (modern frameworks generate randomized names like `css-1x8g9u`); interning them as `DefaultAtom` would cause the global intern pool to grow unboundedly and never shrink. ID values and attribute keys are also stored as `String` for the same reason, preventing OOM attacks from unbounded attribute labels. Unrecognized keywords in `StyleValue` are whitelisted to prevent arbitrary string leakage into the atom pool. A hard limit of `MAX_ATTRIBUTES = 32` is enforced during parsing and mutation.

### ComputedStyle (dom/mod.rs)

```
ComputedStyle {
    display:       DefaultAtom,        // "block", "flex", "grid", "none"
    flex_direction: DefaultAtom,       // "row", "column"
    width:         StyleValue,
    height:        StyleValue,
    margin:        [StyleValue; 4],    // top, right, bottom, left (Inline for cache locality)
    padding:       [StyleValue; 4],
    border_width:  [StyleValue; 4],
    bg_color:      Option<(u8, u8, u8)>,
    border_color:  Option<(u8, u8, u8)>,
    font_size:     f32,                // absolute pixels, resolved during cascade
    color:         (u8, u8, u8),
}
```

`ComputedStyle` is shared between nodes using `Rc<ComputedStyle>` and a document-level `StyleCache`. During style resolution, identical computed properties are deduplicated to point to the same heap allocation, significantly reducing the memory footprint for complex pages. There is no intermediate styled-node tree that gets built and dropped per frame.

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

`PropertyName` is a strongly-typed enum covering all CSS properties the engine recognizes (`Display`, `Width`, `MarginTop`, `Color`, `FontSize`, etc.). It replaces `DefaultAtom` as the key in `Declaration`, eliminating string-deref comparisons from the cascade hot path. Property matching during `compute_styles` is a direct integer comparison using pre-computed enum variants mapped to a fixed-size `[Option<StyleValue>; 25]` array.

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

Timers are stored in a `std::collections::BinaryHeap` ordered by `fire_at` (min-heap via reversed `Ord`). Active timer IDs are tracked in an `active_timers` HashSet; `pump()` skips any popped timer whose ID is no longer in the set. When an interval timer fires, `pump()` reschedules it by pushing a new `PendingTimer` with `fire_at = now + delay_ms`. The `BinaryHeap` does not support in-place cancellation -- the active set approach avoids rebuilding the heap on `clearTimeout`.

## Cascade and inheritance

`css::compute_styles()` performs an iterative stack-based traversal of the arena DOM. For each element node it:

1. Looks up matching rules from `document.stylesheet` buckets (by ID, class, tag, universal).
2. Merges matched rules using a k-way specificity-ordered pointer walk.
3. Applies inline `style` attribute declarations last (highest priority).
4. Resolves the final property set against a fixed-size `[Option<StyleValue>; 25]` array using property bitmasks.
5. Deduplicates the resulting `ComputedStyle` against the `StyleCache` to retrieve an `Rc<ComputedStyle>`.
6. Assigns the `Rc` to the node and marks `layout_dirty = true` if the style changed.
7. Pushes children onto the traversal stack.

Inheritable properties (`color`, `font-size`, etc.) are resolved during the cascade and stored in the `ComputedStyle`. Combinator evaluation (`>` child, space descendant) walks arena parent pointers rather than maintaining a separate ancestor stack.

## JavaScript bridge

`JsEngine` holds the `Document` inside `Rc<RefCell<Document>>`. DOM-mutating JS functions call `doc.dirty = true` after making changes.

JavaScript object identity (`===`) is enforced via a `_wrapNode` WeakRef cache. Rust traversals (e.g. `parentNode`, `firstChild`) are patched onto the `NodeHandle` prototype as closures that proxy through this cache. A `FinalizationRegistry` receives the raw `[index, generation]` integer array when QuickJS garbage-collects a wrapper; it invokes `_garbageCollectNodeRaw` (mapped to `try_cleanup_node` in Rust) to decrement the handle count. To prevent the "Fat Node" memory leak, `_wrapNode` manually triggers `_garbageCollectNodeRaw` when a duplicate handle is received but discarded in favor of a cached wrapper.

`JsEngine::pump()` executes pending JavaScript jobs (microtasks/promises) asynchronously to sweep `FinalizationRegistry` callbacks. Every 60 ticks, `document.collect_garbage()` is called to clear the batched deletion queue and process the `FinalizationRegistry`. This ensures deterministic memory reclamation without blocking the main thread for expensive GC synchronous sweeps.

`NodeHandle` does not implement `Drop`. Nodes created by JavaScript persist in the arena until explicitly removed via `removeChild()`. This prevents QuickJS garbage collection from triggering arena mutations on nodes that are still part of the tree.

## Text measurement

The text render loop calls `buffer.layout_runs()` and invokes `draw_glyphs` once per `LayoutRun`, passing `run.glyphs` (a `&[LayoutGlyph]` slice borrowed directly from the buffer) and `run.line_y` as the vertical offset. No intermediate `Vec` is allocated during the render pass.

During `compute_layout_with_measure`, the measure closure calls `buffer.set_size()` and re-wraps text at the available width constraint. Accurate height resolution is achieved by querying the resulting layout runs. After the layout solver finishes, `finalize_text_measurements` reshapes each buffer at its final resolved width if necessary. The renderer reads `LayoutGlyph` iterators directly from `buffer.layout_runs()`.

## HTML parsing

The HTML module iterates `html5gum` tokens in a loop. `StartTag` tokens create `ElementData` nodes and append them under `current_parent`. Before appending, the parser checks whether the new tag should implicitly close an open ancestor (e.g., `<div>` closing an open `<p>`). The walk stops at block-level boundary tags (`div`, `body`, `td`, `th`, `table`). `EndTag` tokens walk `current_parent` back up to the matching ancestor. Byte slices are validated as UTF-8 directly without allocating through `String::from_utf8_lossy`.

Content inside `<script>` and `<style>` is accumulated as raw text. The matching closing tag exits the raw state. `<style>` content is parsed immediately into `document.stylesheet` via `css::append_stylesheet()`.

## Thread safety

`JsEngine` uses `Rc<RefCell<Document>>`. QuickJS and `rquickjs` are single-threaded by design. `JsEngine` is not `Send`. All DOM access from JS callbacks is serialized through the `RefCell`. There are no mutexes or atomic operations in the engine core.
