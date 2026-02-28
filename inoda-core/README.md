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

Streams `html5gum` tokens into the arena in a single pass. Each token is converted using `std::str::from_utf8` directly on the byte slices, avoiding intermediate `String` allocations. Implicit tag auto-closing rules are applied during the `StartTag` handler (e.g., a `<li>` closes an open `<li>` sibling, a `<p>` closes an open `<p>`). Extracts raw CSS text from `<style>` elements and stores it on `Document::style_texts`. Whitespace-only text nodes are preserved for inline spacing.

### css

- Parses CSS text into a `StyleSheet` containing pre-parsed `ComplexSelector` ASTs.
- Property values are parsed into typed `StyleValue` enums (`LengthPx`, `Percent`, `Color`, `Keyword`, `Number`, `Auto`) during the cascade, so downstream consumers do not parse strings at runtime.
- Computes specificity as `(id_count, class_count, tag_count)` at parse time.
- Rules are distributed into `HashMap<DefaultAtom, Vec<IndexedRule>>` buckets keyed by tag, class, and ID. During style resolution, matching buckets are merged in a single O(N) pass using a k-way pointer walk over the pre-sorted slices, without allocating a temporary merged vector.
- Supports tag, class, ID, compound, and complex combinators (`>`, ` `).
- Inherits `color`, `font-family`, `font-size`, `font-weight`, `line-height`, `text-align`, `visibility` from parent to child. Inherited style vectors are shared via `Rc` to avoid redundant cloning.
- Expands `margin`, `padding` shorthands (1/2/4-value syntax) and maps `background` to `background-color`.
- Inline `style=""` attributes are parsed via `cssparser`'s `DeclarationParser` trait.

### layout

Walks the `StyledNode` tree and builds a parallel `TaffyTree<TextMeasureContext>`. Before running Taffy's layout solver, a pre-pass traverses the DOM and creates `cosmic-text::Buffer` objects for every text node, performing HarfBuzz shaping once. The Taffy measure closure then only calls `buffer.set_size()` to adjust the width constraint on the already-shaped buffer, avoiding repeated shaping work.

Supported CSS properties mapped to Taffy:
- `display`: flex, grid, block, none (inline/inline-block are recognized but not yet laid out differently)
- `flex-direction`: row, column
- `width`, `height` with units: `px`, `%`, `vw`, `vh`, `em`, `rem`, `auto`

Non-flex elements default to `flex-direction: column` to approximate block stacking.

Properties not yet wired: `margin-*`, `padding-*`, `border-*`, `align-items`, `justify-content`, `gap`, `flex-wrap`, `flex-grow`, `flex-shrink`, `min-*`, `max-*`, `position`, `overflow`.

### render

Recursively walks the Taffy layout tree alongside the `StyledNode` tree and issues backend draw calls:
- Background rectangles (`background-color`)
- Border strokes (`border-color`, always 1px, no per-side control)
- Text via `RendererBackend::draw_text()` using inherited `color` and `font-size`

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

The `__nodeCache` uses a `Map` of `WeakRef` objects so that cached node wrappers do not prevent QuickJS garbage collection. When a `WeakRef` is dereferenced and found dead, the wrapper is re-created.

Timer callbacks are stored as `rquickjs::Persistent<Function>` to survive context boundaries. The host application should call `pump()` on its event loop tick to dispatch expired timers.

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
- Layout properties `margin`, `padding`, `border-width`, `position`, `overflow`, `z-index`, `float` are not wired to Taffy.
- Color parsing is limited to 5 named colors and 6-digit hex.
- No `@media`, `@import`, `@keyframes`, CSS variables, or `calc()`.
- Selector matching supports combinators (`>`, ` `) but not `+`, `~`, attribute selectors, or pseudo-classes with arguments.
- `addEventListener` logs the event name but never dispatches events.
- `setTimeout` fires callbacks only when the host calls `pump()`. There is no background thread or async runtime.
- Viewport units (`vw`, `vh`) are resolved to absolute pixels at tree construction time. Resizing the window requires rebuilding the layout tree.

## License

See repository root for license information.
