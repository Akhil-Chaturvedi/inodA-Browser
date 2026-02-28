# inoda-core

A minimal browser engine library written in Rust. It parses HTML into a DOM tree, applies CSS styles with specificity and inheritance, computes Flexbox/Grid layout via Taffy, renders through an abstract backend trait, and exposes a subset of the Web API to an embedded QuickJS JavaScript runtime.

This is a library crate. It does not include a window, event loop, or GPU context -- those belong to the host application binary that depends on `inoda-core`.

## Status

Early development. The engine covers enough of the web platform to parse simple pages and lay them out, but large gaps remain (see [Limitations](#limitations)).

## Dependencies

| Crate                | Version | Purpose                                        |
|----------------------|---------|------------------------------------------------|
| `html5gum`           | 0.5     | Streaming HTML tokenizer                       |
| `cssparser`          | 0.36    | Mozilla CSS tokenizer (same one Servo uses)    |
| `cosmic-text`        | 0.12    | Font shaping and wrapped text measurement      |
| `taffy`              | 0.9     | Flexbox and CSS Grid layout algorithm          |
| `rquickjs`           | 0.11    | QuickJS JavaScript engine bindings             |
| `generational-arena` | 0.2     | Generational index arena for the DOM           |
| `string_cache`       | 0.9     | Atom string interning for tag names and keys   |

No other runtime dependencies.

## Module overview

```
src/
  lib.rs        -- crate root, re-exports modules, integration tests
  dom/mod.rs    -- generational arena DOM: Document, Node, ElementData, StyledNode
  html/mod.rs   -- html5gum token loop, streams HTML into the arena
  css/mod.rs    -- CSS parser, specificity, inheritance, shorthand expansion, inline style parsing
  layout/mod.rs -- StyledNode -> Taffy tree builder, text buffer pre-population, dimension parsing
  render/mod.rs -- Taffy layout -> renderer backend draw calls (backgrounds, borders, text)
  js/mod.rs     -- QuickJS runtime with document.*, console.*, cooperative setTimeout
```

### dom

`generational_arena::Arena<Node>` indexed by `generational_arena::Index` (aliased as `NodeId`). Nodes are `Element(ElementData)`, `Text(TextData)`, or `Root(RootData)`. The tree is wired as an intrusive linked list: each node stores `first_child`, `last_child`, `next_sibling`, `prev_sibling`, and `parent` pointers directly, giving O(1) traversal and mutation without allocating child vectors.

Tag names and attribute keys are interned as `string_cache::DefaultAtom`. Element classes are stored in a `Vec<DefaultAtom>` (not a `HashSet`) to reduce per-element memory overhead on constrained devices. Node deletion is iterative (queue-based) rather than recursive to avoid stack overflow on deeply nested trees with small thread stacks.

Documents maintain an `id_map: HashMap<String, NodeId>` for O(1) `getElementById` lookups.

### html

Streams `html5gum` tokens into the arena in a single pass. Each token is converted using `std::str::from_utf8` directly on the byte slices, avoiding intermediate `String` allocations. Implicit tag auto-closing walks up the ancestor chain from `current_parent` until it either finds the tag that should be closed (e.g., a `<div>` walks up past `<span>` and `<b>` to find and close an open `<p>`) or hits a block-level boundary (`div`, `body`, `td`, `th`, `table`) and stops. Content inside `<script>` and `<style>` tags is treated as raw text -- inner tokens are not parsed as HTML tags or appended to the DOM tree. CSS text from `<style>` elements is collected into `Document::style_texts`. Whitespace-only text nodes are preserved for inline spacing.

### css

- Parses CSS text into a `StyleSheet` containing pre-parsed `ComplexSelector` ASTs.
- Property values are parsed into typed `StyleValue` enums (`LengthPx`, `Percent`, `ViewportWidth`, `ViewportHeight`, `Em`, `Rem`, `Color`, `Keyword`, `Number`, `Auto`, `None`) during the cascade, so downstream consumers do not parse strings at runtime.
- Computes specificity as `(id_count, class_count, tag_count)` at parse time.
- Rules are distributed into `HashMap<DefaultAtom, Vec<IndexedRule>>` buckets keyed by tag, class, and ID. Each rule records its source index for stable ordering. During style resolution, matching buckets are merged in a single O(N) pass using a k-way pointer walk over the pre-sorted slices, breaking specificity ties by source order, without allocating a temporary merged vector.
- Supports tag, class, ID, compound, and complex combinators (`>`, ` `).
- Inherits `color`, `font-family`, `font-size`, `font-weight`, `line-height`, `text-align`, `visibility` from parent to child. Only inheritable properties are passed to children; non-inheritable properties like `width` or `margin` are filtered out before recursing. Inherited style vectors are shared via `Rc` when no new declarations apply.
- Expands `margin`, `padding` shorthands (1/2/4-value syntax) and maps `background` to `background-color`.
- Inline `style=""` attributes are parsed via `cssparser`'s `DeclarationParser` trait.

### layout

Walks the `StyledNode` tree and builds a parallel `TaffyTree<TextMeasureContext>`. A pre-pass traverses the styled DOM and creates `cosmic-text::Buffer` objects for every text node, performing HarfBuzz shaping once. The buffer cache is caller-owned and persists across frames; it is not cleared internally. The Taffy measure closure calls `buffer.set_size()` followed by `buffer.shape_until_scroll()` to re-wrap text at the new width constraint. `TextLineLayout` stores `cosmic_text::LayoutGlyph` arrays directly rather than `String` copies, avoiding heap allocations during the layout pass.

Supported CSS properties mapped to Taffy:
- `display`: flex, grid, block, none (inline/inline-block are recognized but not yet laid out differently)
- `flex-direction`: row, column
- `width`, `height` with units: `px`, `%`, `vw`, `vh`, `em`, `rem`, `auto`
- `margin-top`, `margin-right`, `margin-bottom`, `margin-left` (including `auto`)
- `padding-top`, `padding-right`, `padding-bottom`, `padding-left`
- `border-width`, `border-top-width`, `border-right-width`, `border-bottom-width`, `border-left-width`

Non-flex elements default to `flex-direction: column` to approximate block stacking.

Properties not yet wired: `align-items`, `justify-content`, `gap`, `flex-wrap`, `flex-grow`, `flex-shrink`, `min-*`, `max-*`, `position`, `overflow`.

### render

Recursively walks the Taffy layout tree alongside the `StyledNode` tree and issues backend draw calls:
- Background rectangles (`background-color`)
- Border strokes (`border-color`, always 1px, no per-side control)
- Text via `RendererBackend::draw_glyphs()` using pre-shaped `cosmic_text::LayoutGlyph` arrays, inherited `color`, and `font-size`

The `RendererBackend` trait requires implementing `fill_rect`, `stroke_rect`, and `draw_glyphs`. A default `draw_text_layout` method iterates over `TextDrawLine` entries and delegates to `draw_glyphs`.

Color parsing handles the named colors `red`, `green`, `blue`, `black`, `white` and 6-digit hex (`#rrggbb`). No `rgb()`, `rgba()`, `hsl()`, shorthand hex, or alpha support.

Font loading is backend-specific and must be provided by the host renderer implementation.

### js

Embeds QuickJS via `rquickjs`. The `JsEngine` holds the `Document` behind `Rc<RefCell<Document>>`. QuickJS is single-threaded; all DOM access is serialized through the `RefCell`.

Exposed globals:
- `console.log(msg)`, `console.warn(msg)`, `console.error(msg)` -- print to stdout
- `document.getElementById(id)` -- returns a `NodeHandle` object. Repeated calls for the same node return the same JS object (identity preserved via a `WeakRef`-based `__nodeCache`).
- `document.querySelector(selector)` -- tag, class, and ID selectors. Returns a cached `NodeHandle` or null.
- `document.createElement(tagName)` -- creates a detached element node in the arena, returns a cached `NodeHandle`.
- `document.appendChild(parent, child)` -- appends a child node
- `document.addEventListener(event, callback)` -- logs registration (scaffold; does not dispatch events)
- `setTimeout(callback, delay)` -- registers a cooperative timer; host must call `pump()`

`NodeHandle` class methods:
- `handle.tagName` -- returns the tag name string
- `handle.getAttribute(key)` -- returns value or null
- `handle.setAttribute(key, value)` -- updates or inserts attribute
- `handle.removeChild(child)` -- detaches child from parent

The `__nodeCache` uses a `Map` of `WeakRef` objects so that cached node wrappers do not prevent QuickJS garbage collection. A `FinalizationRegistry` is registered alongside each `WeakRef` entry to delete the corresponding `Map` key when QuickJS garbage-collects the wrapper object, preventing unbounded key accumulation.

`NodeHandle` does not implement `Drop`. Nodes created via JavaScript remain in the Rust arena until explicitly removed via `removeChild()`. This avoids ABA memory corruption that would occur if QuickJS garbage collection triggered arena deletions for nodes that are still attached to the DOM.

`setAttribute('id', newId)` removes the old ID from `Document::id_map` before inserting the new one. `remove_node` verifies that the `id_map` entry points to the node being removed before deleting it, preventing stale entries from corrupting lookups.

Timer callbacks are stored as `rquickjs::Persistent<Function>` to survive context boundaries. Pending timers are held in a `BinaryHeap` sorted by fire time. The host application should call `pump()` on its event loop tick; `pump()` pops expired timers from the heap without allocating temporary vectors.

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
- `display: inline` and `inline-block` are parsed but laid out identically to block.
- Layout properties `position`, `overflow`, `z-index`, `float`, `align-items`, `justify-content` are not wired to Taffy.
- Color parsing is limited to 5 named colors and 6-digit hex.
- No `@media`, `@import`, `@keyframes`, CSS variables, or `calc()`.
- Selector matching supports combinators (`>`, ` `) but not `+`, `~`, attribute selectors, or pseudo-classes with arguments.
- `addEventListener` logs the event name but never dispatches events.
- `setTimeout` fires callbacks only when the host calls `pump()`. There is no background thread or async runtime.
- Viewport units (`vw`, `vh`) are resolved to absolute pixels at tree construction time. Resizing the window requires rebuilding the layout tree.

## License

See repository root for license information.
