# inoda-core

A minimal browser engine library written in Rust. It parses HTML into an arena-based DOM, applies CSS styles with specificity and inheritance, computes Flexbox/Grid layout via Taffy, renders through an abstract backend trait, and runs a subset of the Web API in an embedded QuickJS JavaScript runtime.

This is a library crate. It does not include a window, event loop, or GPU context -- those belong to the host application binary that depends on `inoda-core`.

## Status

Early development. The engine can parse simple pages, apply stylesheets, lay out content with Flexbox/Grid, and run basic JavaScript that reads and mutates the DOM. Significant gaps remain (see [Limitations](#limitations)).

## Dependencies

| Crate                | Version | Purpose                                        |
|----------------------|---------|------------------------------------------------|
| `html5gum`           | 0.5     | Streaming HTML tokenizer                       |
| `cssparser`          | 0.36    | Mozilla CSS tokenizer (same one Servo uses)    |
| `cosmic-text`        | 0.12    | Font shaping and wrapped text measurement      |
| `taffy`              | 0.9     | Flexbox and CSS Grid layout algorithm          |
| `rquickjs`           | 0.11    | QuickJS JavaScript engine bindings             |
| `generational-arena` | 0.2     | Generational index arena for the DOM           |
| `string_cache`       | 0.9     | Atom string interning for HTML tag names       |

## Module overview

```
src/
  lib.rs        -- crate root, re-exports modules, integration tests
  dom/mod.rs    -- generational arena DOM: Document, Node, ElementData, ComputedStyle
  html/mod.rs   -- html5gum token loop, streams HTML into the arena
  css/mod.rs    -- CSS parser, specificity, cascade, inheritance, shorthand expansion
  layout/mod.rs -- arena DOM -> TaffyTree builder, text buffer pre-population, dimension parsing
  render/mod.rs -- Taffy layout + arena DOM -> renderer backend draw calls
  js/mod.rs     -- QuickJS runtime with document.*, console.*, cooperative timers
```

### dom

`generational_arena::Arena<Node>` indexed by `generational_arena::Index` (aliased as `NodeId`). Nodes are `Element(ElementData)`, `Text(TextData)`, or `Root(RootData)`. The tree is wired as an intrusive linked list: each node stores `first_child`, `last_child`, `next_sibling`, `prev_sibling`, and `parent` pointers directly, giving O(1) traversal and mutation without allocating child vectors.

Tag names are stored as `LocalName`, which is either `Standard(DefaultAtom)` for known HTML elements (interned, pointer-equality comparison) or `Custom(String)` for custom element names. This prevents unbounded growth of the global intern pool from arbitrary names passed through `document.createElement`.

Attribute keys and values are stored as `String`. To prevent OOM attacks from unbounded attribute names, interning into the global `DefaultAtom` pool is intentionally avoided for attributes. This ensures that memory consumption scales linearly with the DOM size and is fully reclaimed upon node destruction. IDs are also stored as `String` and indexed in an $O(1)$ `id_map`.

`ComputedStyle` is stored directly on `ElementData` and `TextData` as a contiguous inline struct (no heap pointer chasing), populated once during `css::compute_styles()`. Layout and rendering read from `computed` fields without scanning style tuples.

`Document` fields:
- `nodes: Arena<Node>` -- the arena
- `stylesheet: StyleSheet` -- persistent, merged in-place as `<style>` tags are parsed
- `id_map: HashMap<String, NodeId>` -- O(1) `getElementById` lookup
- `styles_dirty: bool` -- tracks if `<style>` tags were added or removed, triggering a clean stylesheet rebuild.
- `dead_nodes: Vec<NodeId>` -- iterative deletion queue used by `remove_node` and batched by `collect_garbage()`.

Node deletion is iterative (queue-based) to avoid stack overflow on deeply nested trees.

### html

Streams `html5gum` tokens into the arena in a single pass. Byte slices are validated with `std::str::from_utf8` directly, avoiding intermediate `String` allocations.

Implicit tag auto-closing walks up the ancestor chain from `current_parent`. For example, a `<div>` token will first close an open `<p>`, but the walk stops at block-level boundary tags (`div`, `body`, `td`, `th`, `table`) to prevent over-closing. `EndTag` tokens walk `current_parent` back to the matching ancestor.

Content inside `<script>` and `<style>` is accumulated as raw text via an `inside_raw_tag` state variable. The matching closing tag exits this state. Text from `<style>` elements is parsed immediately into `document.stylesheet` via `css::append_stylesheet()`.

### css

- Parses CSS text into a `StyleSheet` containing pre-parsed `ComplexSelector` ASTs.
- Property values are parsed into typed `StyleValue` enums (`LengthPx`, `Percent`, `ViewportWidth`, `ViewportHeight`, `Em`, `Rem`, `Color`, `Keyword`, `Number`, `Auto`, `None`) during the cascade. Layout and rendering operate on these enum variants, not strings.
- Property names in `Declaration` use `PropertyName`, a strongly-typed enum (`Display`, `Width`, `MarginTop`, `FontSize`, etc.) with an `Other(u64)` fallback. This makes property matching during cascade an integer comparison rather than a string deref.
- Specificity is computed as `(id_count, class_count, tag_count)` at parse time and stored on each `ComplexSelector`.
- Rules are stored in `HashMap<String, Vec<IndexedRule>>` buckets keyed by class and ID (plain `String`), and `HashMap<DefaultAtom, Vec<IndexedRule>>` keyed by tag (bounded set of known tag names; interning is safe here). Class and ID keys are not interned because they are uncontrolled user input.
- `compute_styles()` walks the arena DOM recursively, evaluates combinators (`>`, space) by walking arena parent pointers, populates `ComputedStyle` on each node via direct `PropertyName` enum matching.
- Inherits `color`, `font-family`, `font-size`, `font-weight`, `line-height`, `text-align`, `visibility` from parent. Only inheritable properties are passed to children; non-inheritable properties like `width` or `margin` are filtered before recursing. Values are copied directly from the parent's resolved style to avoid redundant allocations.
- `font-size` expressed as `Em` multiplies against the parent's resolved `font_size`. `Rem` always uses 16px as root baseline. Both are resolved during the cascade; the result stored in `computed.font_size` is always absolute pixels.
- Expands `margin`, `padding` shorthands (1/2/3/4-value) and maps `background` to `background-color`.
- Inline `style=""` attributes are parsed via `cssparser`'s `DeclarationParser` trait and applied after stylesheet rules.
- `document.stylesheet` is persistent but invalidates via `rebuild_styles()` when the DOM is mutated. Only rules from currently attached `<style>` tags are preserved, preventing memory leaks from removed nodes.

### layout

Walks the arena DOM and builds a parallel `TaffyTree<TextMeasureContext>`. `prepare_text_buffers` performs HarfBuzz shaping in a pre-pass to calculate `max_intrinsic_width` and `min_intrinsic_width`. The buffer cache is caller-owned and persists across frames.

The Taffy measure closure invokes `buffer.set_size()` and re-calculates the line count during the solver loop. This ensures accurate Flexbox/Grid height resolution across different width constraints while still benefiting from once-per-allocation HarfBuzz shaping in the pre-pass. Final shaping at resolved widths is performed by `finalize_text_measurements`.

Layout properties are read from `computed` fields on each arena node.

Supported CSS properties mapped to Taffy:
- `display`: flex, grid, block, none
- `flex-direction`: row, column
- `width`, `height` with units: `px`, `%`, `vw`, `vh`, `em`, `rem`, `auto`
- `margin-*`, `padding-*`, `border-*-width` (including `auto` for margins)

Non-flex elements default to `flex-direction: column` to approximate block stacking.

Properties not wired: `align-items`, `justify-content`, `gap`, `flex-wrap`, `flex-grow`, `flex-shrink`, `min-*`, `max-*`, `position`, `overflow`.

### render

Recursively walks the Taffy layout tree alongside the arena DOM and issues backend draw calls:
- Background rectangles (`background-color`)
- Border strokes (`border-color`)
- Text: calls `draw_glyphs` once per `LayoutRun` from `buffer.layout_runs()`, passing `run.glyphs` (a `&[LayoutGlyph]` slice borrowed directly from the pre-shaped buffer) and `abs_y + run.line_y` as the vertical position. No intermediate `Vec` is allocated in the render loop.

Draw properties are read directly from `ComputedStyle` fields on each arena node. There is no intermediate draw cache or separate text layout struct.

The `RendererBackend` trait requires `fill_rect`, `stroke_rect`, and `draw_glyphs`. `draw_glyphs` accepts a `&mut cosmic_text::FontSystem` to allow the host to physically rasterize the provided glyph offsets.

Color parsing handles the named colors `red`, `green`, `blue`, `black`, `white` and 6-digit hex (`#rrggbb`). No `rgb()`, `rgba()`, `hsl()`, shorthand hex, or alpha.

### js

Embeds QuickJS via `rquickjs`. `JsEngine` holds `Document` behind `Rc<RefCell<Document>>`. QuickJS is single-threaded; all DOM access is serialized through the `RefCell`.

Exposed globals:
- `console.log(msg)`, `console.warn(msg)`, `console.error(msg)` -- print to stdout
- `document.getElementById(id)` -- returns a cached `NodeHandle` or null
- `document.querySelector(selector)` -- tag, class, and ID selectors only. Uses an $O(1)$ fast-path for exact `#id` selectors and falls back to a recursive traversal for class/tag queries. Returns a cached `NodeHandle` or null.
- `document.createElement(tagName)` -- creates a detached element in the arena, returns a cached `NodeHandle`
- `document.appendChild(parent, child)` -- appends child node, sets `document.dirty = true`
- `document.addEventListener(event, callback)` -- records registration; does not dispatch events
- `setTimeout(callback, delay)` -- registers a one-shot cooperative timer; returns a timer ID
- `setInterval(callback, delay)` -- registers a repeating cooperative timer; returns a timer ID
- `clearTimeout(id)`, `clearInterval(id)` -- cancels a pending timer by ID

`NodeHandle` class methods:
- `handle.tagName` -- returns the tag name string
- `handle.getAttribute(key)` -- returns value or null
- `handle.setAttribute(key, value)` -- updates or inserts attribute, sets `document.dirty = true`
- `handle.removeChild(child)` -- detaches child from parent, sets `document.dirty = true`

JavaScript object identity (`===`) is enforced via a `_wrapNode` WeakRef cache in the JS environment. Rust getters for traversals (e.g. `parentNode`, `firstChild`) are patched onto the `NodeHandle` prototype using closures that proxy through this cache. A `FinalizationRegistry` receives the raw `[index, generation]` integer array when QuickJS GC collects a wrapper; it invokes `_garbageCollectNodeRaw` (mapped to `try_cleanup_node` in Rust) to decrement the node's handle count. Nodes are queued in `dead_nodes` and permanently removed from the arena by `collect_garbage()` once they are both detached and unreferenced.

`NodeHandle` does not implement `Drop`. Nodes created via JavaScript persist in the arena until explicitly removed via `removeChild()`. This prevents QuickJS GC from invalidating arena slots for nodes that are still attached to the tree.

`NodeHandle` does not implement `Drop`. Nodes created via JavaScript persist in the arena until explicitly removed via `removeChild()`. This prevents QuickJS GC from invalidating arena slots for nodes that are still attached to the tree.

Timer callbacks are stored as `rquickjs::Persistent<Function>`. Pending timers are in a `BinaryHeap` sorted by `fire_at`. Cancelled timer IDs are tracked in a `HashSet<u32>`; `pump()` skips popped timers whose IDs appear in the set. When an interval timer fires, a new `PendingTimer` is pushed with the next scheduled time. Rescheduled interval timers are collected into a separate local `Vec` before being pushed back to the heap; this prevents `setInterval(cb, 0)` from re-appearing at the top of the heap within the same `pump()` call and locking the loop.

`JsEngine::pump()` executes pending JavaScript jobs (microtasks/promises) until the queue is empty before returning control to the host event loop. Every 60 ticks, `document.collect_garbage()` is called to clear the batched deletion queue and process the `FinalizationRegistry`. This ensures deterministic memory reclamation without blocking the main thread for expensive GC synchronous sweeps. The `_wrapNode` bridge also handles manual refcount correction when QuickJS merges new handles into its identity cache.

## Building

```
cargo build
```

## Testing

```
cargo test
```

Tests in `lib.rs` cover HTML parsing, CSS combinator matching, JS bridge round-trips, iterative DOM node deletion, and whitespace text node preservation.

## Limitations

This list is not exhaustive. The engine is a working skeleton, not a production browser.

- No networking, resource loading, or URL resolution.
- No `<img>`, `<video>`, `<canvas>`, `<iframe>`, or form elements.
- Inline formatting context is incomplete (no baseline alignment or float interaction).
- Font loading and fallback are backend-specific and must be provided by the host.
- `display: inline` and `inline-block` are parsed but treated identically to block.
- Layout properties `position`, `overflow`, `z-index`, `float`, `align-items`, `justify-content`, `gap`, `flex-grow`, `flex-shrink`, `min-*`, `max-*` are not wired to Taffy.
- Color parsing is limited to 5 named colors and 6-digit hex.
- No `@media`, `@import`, `@keyframes`, CSS variables, or `calc()`.
- Selector matching supports `>` (child) and space (descendant) combinators, but not `+`, `~`, attribute selectors, or `:pseudo-class()` with arguments.
- `addEventListener` records the registration but never dispatches any events.
- `setTimeout` and `setInterval` fire only when the host calls `pump()`. There is no background thread.
- The host is responsible for detecting `document.dirty` and re-running the style/layout/render pipeline after JS mutations.
- `rem` unit resolution uses a fixed 16px root baseline; there is no `<html>` element font-size negotiation.

## License

See repository root for license information.
