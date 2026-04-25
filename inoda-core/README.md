# inoda-core

A minimal browser engine library written in Rust. It parses HTML into an arena-based DOM, applies CSS styles with specificity and inheritance, computes Flexbox/Grid layout via Taffy, renders through an abstract backend trait, and runs a subset of the Web API in an embedded QuickJS JavaScript runtime.

This is a library crate. It does not include a window, event loop, or GPU context -- those belong to the host application binary that depends on `inoda-core`.

## Status

Early development. The engine can parse simple pages, apply stylesheets, lay out content with Flexbox/Grid, and run basic JavaScript that reads and mutates the DOM. Significant gaps remain (see [Limitations](#limitations)).

## Dependencies

| Crate                | Version | Purpose                                           |
|----------------------|---------|---------------------------------------------------|
| `html5gum`           | 0.5     | Streaming HTML tokenizer                          |
| `cssparser`          | 0.36    | Mozilla CSS tokenizer (same one Servo uses)       |
| `cosmic-text`        | 0.12    | Font shaping and wrapped text measurement         |
| `taffy`              | 0.9     | Flexbox and CSS Grid layout algorithm             |
| `rquickjs`           | 0.11    | QuickJS JavaScript engine bindings                |
| `generational-arena` | 0.2     | Generational index arena for the DOM              |
| `string_cache`       | 0.9     | Atom string interning for HTML tag names          |
| `phf`                | 0.11    | Compile-time HTML tag-name set for `LocalName`    |
| `criterion`          | 0.5     | (dev) Benchmark harness for cascade / layout / JS |

## Module overview

```
src/
  lib.rs -- crate root, re-exports modules, integration tests
  dom/mod.rs -- generational arena DOM: Document, Node, ElementData, TextData, ComputedStyle, TextComputedStyle
  html/mod.rs -- html5gum token loop, streams HTML into the arena
  css/mod.rs -- CSS parser, specificity, cascade, inheritance, shorthand expansion
  layout/mod.rs -- arena DOM -> TaffyTree builder, text buffer pre-population, dimension parsing
  render/mod.rs -- Taffy layout + arena DOM -> renderer backend draw calls
  js/mod.rs -- QuickJS runtime with document.*, console.*, cooperative timers
```

### dom

`generational_arena::Arena<Node>` indexed by `generational_arena::Index` (aliased as `NodeId`). Nodes are `Element(ElementData)`, `Text(TextData)`, or `Root(RootData)`. The tree is wired as an intrusive linked list: each node stores `first_child`, `last_child`, `next_sibling`, `prev_sibling`, and `parent` pointers directly, giving O(1) traversal and mutation without allocating child vectors.

Tag names are stored as `LocalName`, which is either `Standard(DefaultAtom)` for known HTML elements (interned, pointer-equality comparison) or `Custom(String)` for custom element names. Known tags are resolved with a compile-time `phf` set (callers must pass ASCII-lowercase names, as the tokenizer and `createElement` already do). This prevents unbounded growth of the global intern pool from arbitrary names passed through `document.createElement`.

Attribute keys and values are stored as `String`. To prevent OOM attacks from unbounded attribute names, interning into the global `DefaultAtom` pool is intentionally avoided for attributes. For security, limits are enforced: `MAX_ATTRIBUTES` (32) per element during parsing and `setAttribute`, `MAX_ATTRIBUTE_VALUE_LEN` (16KB) per value. This ensures that memory consumption scales linearly with the DOM size and is fully reclaimed upon node destruction. IDs are also stored as `String` and indexed in an $O(1)$ `id_map`.

`ComputedStyle` is stored directly inside `ElementData` for optimal L1 cache locality. It uses local enums (`DisplayKeyword`, `FlexDirectionKeyword`, `AlignItemsKeyword`, `JustifyContentKeyword`, `FlexWrapKeyword`) rather than Taffy-native types. `TextData` uses a lightweight `TextComputedStyle` struct containing only `font_size` and `color`, since text nodes do not have box layout properties. Both styles are populated once by `css::compute_styles()` during the cascade; layout and rendering read from these resolved fields without scanning style tuples. Storage is inline to eliminate the CPU overhead of deep-hashing style objects for deduplication.

`Document` fields:
- `nodes: Arena<Node>` -- the arena
- `stylesheet: StyleSheet` -- persistent, merged in-place as `<style>` tags are parsed
- `id_map: HashMap<String, NodeId>` -- O(1) `getElementById` lookup
- `styles_dirty: bool` -- tracks if `<style>` tags were added or removed, triggering a clean stylesheet rebuild. (Individual nodes also bear `styles_dirty` markers to facilitate granular incremental Subtree Invalidation algorithms instead of massive global recalculations).
- `dead_nodes: Vec<NodeId>` -- iterative deletion queue used by `remove_node` and batched by `collect_garbage()`.

Node deletion is iterative (queue-based) to avoid stack overflow on deeply nested trees.

### html

Streams `html5gum` tokens into the arena in a single pass. This is a tokenizer-driven builder with local tag-closing rules — it is **not** a WHATWG HTML tree builder, so complex parsing edge cases will not match full browsers. Byte slices are validated with `std::str::from_utf8` directly, avoiding intermediate `String` allocations.

Content inside `<script>` and `<style>` is accumulated as raw text via an `inside_raw_tag` state variable. The matching closing tag exits this state. Text from `<style>` elements is parsed immediately into `document.stylesheet` via `css::append_stylesheet()`. If an `EndTag` does not match the `current_parent`, the parser walks up the ancestor chain to find a match and reconciles the tree state.

### css

- Parses CSS text into a `StyleSheet` containing pre-parsed `ComplexSelector` ASTs.
- Property values are parsed into typed `StyleValue` enums (`LengthPx`, `Percent`, `ViewportWidth`, `ViewportHeight`, `Em`, `Rem`, `Color`, `Keyword`, `Number`, `Auto`, `None`) during the cascade. Layout and rendering operate on these enum variants, not strings.
- Property names in `Declaration` use `PropertyName`, a strongly-typed enum (`Display`, `Width`, `MarginTop`, `FontSize`, etc.). `PropertyName::from_str` returns `Option<PropertyName>`; unrecognized property names return `None` and are discarded during the cascade. Layout-critical keyword values (e.g. `flex`, `column`, `stretch`) resolve to local enums (`DisplayKeyword`, `FlexDirectionKeyword`, etc.) in `ComputedStyle` during cascade, eliminating string matching in the layout engine. This makes property matching and application an integer comparison rather than a string deref and prevents unrecognized properties from silently corrupting the style tree.
- Specificity is computed as `(id_count, class_count, tag_count)` at parse time and stored on each `ComplexSelector`.
- Rules are stored in `HashMap<String, Vec<IndexedRule>>` buckets keyed by class and ID (plain `String`), and `HashMap<DefaultAtom, Vec<IndexedRule>>` keyed by tag (bounded set of known tag names; interning is safe here). Class and ID keys are not interned because they are uncontrolled user input. Each rule is indexed in **one** bucket only (ID, else first class on the subject compound, else tag, else universal); see the `StyleSheet` doc comment in `css/mod.rs` for why multi-class selectors are fragile at index time.
- `compute_styles()` performs an iterative stack-based traversal of the arena DOM, evaluating combinators (`>`, space, `+`, `~`) by walking arena parent and sibling pointers. Attribute selectors (`[attr]`, `[attr=value]`) are matched against `ElementData::attributes`. The cascade uses `data.classes.split_whitespace()` iteration alongside a stack-allocated rule bucket gathering via `SmallVec<[&[IndexedRule]; 8]>`. The traversal utilizes short-circuit optimizations via `ancestor_attr_changed` flags to leapfrog un-mutated DOM nodes (Incremental Rendering). It populates `ComputedStyle` on each node by matching against pre-parsed rules and resolving inheritance.
- Inherits `color`, `font-family`, `font-size`, `font-weight`, `line-height`, `text-align`, `visibility` from parent. Values are copied directly from the parent's resolved style to avoid redundant allocations.
- `font-size` expressed as `Em` multiplies against the parent's resolved `font_size`. `Rem` resolves against `Document.root_font_size` (defaults to 16px, configurable by the host). Both are resolved during the cascade; the result stored in `computed.font_size` is always absolute pixels.
- Expands `margin`, `padding` shorthands (1/2/3/4-value) and maps `background` to `background-color`.
- Inline `style=""` attributes are parsed via `cssparser`'s `DeclarationParser` trait (`InlineStyleParser`). `margin` and `padding` shorthands are expanded to their four longhand properties at parse time. `background` is mapped to `background-color`. Unrecognized properties are discarded. Inline declarations are applied after stylesheet rules (highest priority).
- `document.stylesheet` is persistent; `append_stylesheet()` dynamically merges rules from new `<style>` tags into the existing AST without a full re-parse. Rebuilds only occur if nodes are removed or styles are explicitly cleared.

### layout

Walks the arena DOM and builds a parallel `TaffyTree<TextMeasureContext>`. `prepare_text_buffers` performs HarfBuzz shaping in a pre-pass to calculate `max_intrinsic_width` and `min_intrinsic_width`. The buffer cache is caller-owned and persists across frames.

To ensure high performance in embedded HMIs, the layout engine performs work conditionally:
- **Structural Updates**: Taffy node children are only updated via `set_children` if a node is new or the `document.dirty` flag is set. This avoids expensive allocator thrashing in Taffy's edge arrays on every frame.
- **Text Measurement**: Intrinsic width calculation and shaping are only re-run if a node is new or its `layout_dirty` flag is set (e.g. after a text content change via JS).

The Taffy measure closure invokes `buffer.set_size()` and counts `layout_runs()` during the solver loop. `TextMeasureContext` stores the last definite width and line count so repeated measure probes at the same width skip redundant `set_size` and counting. Final shaping at resolved widths is performed by `finalize_text_measurements`.

Layout properties are read from `computed` fields on each arena node.

Supported CSS properties mapped to Taffy:
- `display`: flex, grid, block, none
- `flex-direction`: row, column
- `width`, `height` with units: `px`, `%`, `vw`, `vh`, `em`, `rem`, `auto`
- `margin-*`, `padding-*`, `border-*-width` (including `auto` for margins)
- `align-items`, `justify-content`, `flex-wrap`, `flex-grow`, `flex-shrink`
- `row-gap`, `column-gap`
- `min-width`, `max-width`, `min-height`, `max-height`
- `<img>` intrinsic sizing via `width`/`height` HTML attributes and Taffy `aspect_ratio`

Non-flex elements default to `flex-direction: column` to approximate block stacking.

Properties not wired: `position`, `overflow`, `z-index`, `float`.

### render

Iteratively walks the Taffy layout tree alongside the arena DOM using an explicit stack to avoid overflow on deep trees. Issues backend draw calls:
- Background rectangles (`background-color`)
- Border strokes (`border-color`)
- Text: calls `draw_glyphs` once per `LayoutRun` from `buffer.layout_runs()`, passing `run.glyphs` (a `&[LayoutGlyph]` slice borrowed directly from the pre-shaped buffer) and `abs_y + run.line_y` as the vertical position. No intermediate `Vec` is allocated in the render loop.

Draw properties are read directly from `ComputedStyle` fields on each arena node. There is no intermediate draw cache or separate text layout struct.

The `RendererBackend` trait requires `fill_rect`, `stroke_rect`, `draw_glyphs`, and `draw_image` (default no-op). `draw_glyphs` accepts pre-shaped geometric glyph slices; it does not receive the `FontSystem`, ensuring that hosts can implement hardware-accelerated drawing without a CPU-side shaping dependency. `draw_image` receives screen coordinates, dimensions, and the `src` URL; the host is responsible for decoding and blitting pixel data.

Color values use RGBA 4-channel tuples `(u8, u8, u8, u8)`. Parsing supports named colors (`red`, `green`, `blue`, `black`, `white`, `transparent`), 3/4/6/8-digit hex (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`), `rgb()`, `rgba()`, `hsl()`, and `hsla()` functional notation.

### js

Embeds QuickJS via `rquickjs`. `JsEngine` holds `Document` behind `Rc<RefCell<Document>>`. QuickJS is single-threaded; all DOM access is serialized through the `RefCell`.

Use `JsEngine::try_new(document) -> Result<JsEngine, JsEngineError>` so runtime/context/Web API registration failures return to the host instead of panicking. `execute_script` and `dispatch_event` return `Result` for evaluation and dispatch errors.

Exposed globals:
- `console.log(msg)`, `console.warn(msg)`, `console.error(msg)` -- print to stdout
- `document.getElementById(id)` -- returns a cached `NodeHandle` or null
- `document.querySelector(selector)` -- tag, class, and ID selectors only. Uses an $O(1)$ fast-path for exact `#id` selectors and falls back to an iterative traversal for class/tag queries. Returns a cached `NodeHandle` or null.
- `document.createElement(tagName)` -- creates a detached element in the arena, returns a cached `NodeHandle`
- `document.appendChild(parent, child)` -- appends child node, sets `document.dirty = true`
- `document.addEventListener(event, callback)` -- registers a callback on the document
- `element.addEventListener(event, callback)` -- registers a callback on a specific element
- `setTimeout(callback, delay)` -- registers a one-shot cooperative timer; returns a timer ID
- `setInterval(callback, delay)` -- registers a repeating cooperative timer; returns a timer ID
- `clearTimeout(id)`, `clearInterval(id)` -- cancels a pending timer by ID

`NodeHandle` class methods:
- `handle.tagName` -- returns the tag name string via a lazy lookup in the arena prototype getter. No redundant string storage on the handle.
- `handle.getAttribute(key)` -- returns value or null
- `handle.setAttribute(key, value)` -- updates or inserts attribute, sets `document.dirty = true`
- `handle.removeChild(child)` -- detaches child from parent, sets `document.dirty = true`

JavaScript object identity (`===`) is enforced via a `_wrapNode` WeakRef cache in the JS environment. Rust getters for traversals (e.g. `parentNode`, `firstChild`) are patched onto the `NodeHandle` prototype using closures that proxy through this cache. `__nodeRegistry` and `__ephemeralRegistry` are both `FinalizationRegistry` instances: the former removes the WeakRef map entry and calls `_garbageCollectNodeRaw` when a canonical wrapper is collected; the latter only calls `_garbageCollectNodeRaw` when an ephemeral duplicate raw wrapper from a cache hit is collected, so each `js_handles += 1` from raw getters is paired with exactly one GC-side decrement. `_garbageCollectNodeRaw` maps to `try_cleanup_node` in Rust. Nodes are queued in `dead_nodes` and permanently removed from the arena by `collect_garbage()` once they are both detached and unreferenced.

`NodeHandle` does not implement `Drop`. Nodes created via JavaScript persist in the arena until explicitly removed via `removeChild()`. This prevents QuickJS GC from invalidating arena slots for nodes that are still attached to the tree.

Timer callbacks are stored as `rquickjs::Persistent<Function>`. Pending timers are in a `BinaryHeap` sorted by `fire_at`. To prevent memory drift from cancelled timers, the heap is compacted when it expands beyond 128 items. Cancelled timer IDs are tracked in a `HashSet<u32>`; `pump()` skips popped timers whose IDs appear in the set. When an interval timer fires, a new `PendingTimer` is pushed with the next scheduled time. Rescheduled interval timers are collected into a separate local `Vec` before being pushed back to the heap; this prevents `setInterval(cb, 0)` from re-appearing at the top of the heap within the same `pump()` call and locking the loop.

`JsEngine::pump()` executes pending JavaScript jobs (microtasks/promises) until the queue is empty before returning control to the host event loop. Every 60 ticks, `document.collect_garbage()` is called to clear the batched deletion queue.

## Building

```
cargo build
```

## Benchmarking (Criterion)

```
cargo bench
cargo bench --bench cascade
cargo bench --bench layout_measure
cargo bench --bench js_roundtrip
```

Reports are written under `target/criterion/`. CI can use `cargo bench --no-run` to ensure bench targets compile without executing them.

## Testing

```
cargo test
```

Tests in `lib.rs` cover HTML parsing, CSS combinator matching, inline style shorthand expansion, unrecognized property discarding, display normalization, JS bridge round-trips, iterative DOM node deletion, whitespace text node preservation, JS infinite loop interruption, and attribute value security limits.

## Limitations

This list is not exhaustive. The engine is a working skeleton, not a production browser.

- No networking, resource loading, or URL resolution. The host fetches resources via the `ResourceLoader` trait.
- No `<video>`, `<canvas>`, `<iframe>`, or form elements. `<img>` has layout support (intrinsic sizing); decoding is the host's responsibility.
- Inline formatting context is incomplete (no baseline alignment or float interaction).
- Font loading and fallback are backend-specific and must be provided by the host.
- `display: inline` and `inline-block` are parsed but treated identically to block.
- Layout properties `position`, `overflow`, `z-index`, `float` are not wired to Taffy.
- No `@media`, `@import`, `@keyframes`, CSS variables, or `calc()`.
- Selector matching supports `>` (child), space (descendant), `+` (next-sibling), `~` (subsequent-sibling) combinators and `[attr]`/`[attr=value]` attribute selectors, but not `:pseudo-class()` with arguments.
- Event dispatching uses flat hit-testing on layout geometry; there is no DOM event bubbling or capture phase.
- `setTimeout` and `setInterval` fire only when the host calls `pump()`. There is no background thread.
- The host is responsible for detecting `document.dirty` and re-running the style/layout/render pipeline after JS mutations.

## License

See repository root for license information.
