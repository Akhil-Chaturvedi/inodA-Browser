# inoda-core

A minimal browser engine library written in Rust. It parses HTML into a DOM tree, applies CSS styles with specificity and inheritance, computes Flexbox/Grid layout via Taffy, renders through an abstract backend trait, and exposes a subset of the Web API to an embedded QuickJS JavaScript runtime.

This is a library crate. It does not include a window, event loop, or GPU context -- those belong to the host application binary that depends on `inoda-core`.

## Status

Early development. The engine covers enough of the web platform to parse simple pages and lay them out, but large gaps remain (see [Limitations](#limitations)).

## Dependencies

| Crate              | Version | Purpose                                     |
|--------------------|---------|---------------------------------------------|
| `html5ever`        | 0.38    | Spec-compliant HTML tokenizer and parser    |
| `markup5ever`      | 0.38    | Shared types for html5ever (LocalName, etc.) |
| `cssparser`        | 0.36    | Mozilla CSS tokenizer (same one Servo uses) |
| `cosmic-text`      | 0.12    | Font shaping + wrapped text measurement      |
| `taffy`            | 0.9     | Flexbox and CSS Grid layout algorithm       |
| `rquickjs`         | 0.11    | QuickJS JavaScript engine bindings          |
| `generational-arena`| 0.2    | Generational index arena for the DOM        |
| `string_cache`     | 0.9     | Atom string interning (DefaultAtom, etc.)    |

No other runtime dependencies.

## Module overview

```
src/
  lib.rs        -- crate root, re-exports modules, integration tests
  dom/mod.rs    -- generational arena DOM: Document, Node, ElementData, StyledNode
  html/mod.rs   -- TreeSink implementation, streams HTML directly into arena
  css/mod.rs    -- CSS parser, specificity, inheritance, shorthand expansion, inline style parsing
  layout/mod.rs -- StyledNode -> Taffy tree builder, dimension parsing
  render/mod.rs -- Taffy layout -> renderer backend draw calls (backgrounds, borders, text)
  js/mod.rs     -- QuickJS runtime with document.*, console.*, cooperative setTimeout
```

### dom

`generational_arena::Arena<Node>` indexed by `generational_arena::Index` (`NodeId`). No `Rc`, no `RefCell`. Nodes are either `Element(ElementData)`, `Text(TextData)`, or `Root(RootData)`. Each element/text node stores its `parent: Option<NodeId>` directly for $O(1)$ parent lookups. `ElementData.tag_name` and attribute keys use `markup5ever::LocalName` for string interning.

Generational indices allow $O(1)$ insertion and deletion without invalidating other handles. Previous versions used a flat `Vec<Node>` indexed by `usize`, which leaked memory on deletion and could not safely track parents.

### html

Implements `html5ever::TreeSink` directly on a `DocumentBuilder` wrapper. HTML tokens stream into the generational arena in a single allocation pass. There is no intermediate `RcDom` tree. Extracts raw CSS text from `<style>` elements and stores it on `Document::style_texts`. Whitespace text nodes are preserved so inline spacing semantics are not lost.

### css

- Parses CSS text into a `StyleSheet` containing pre-parsed `ComplexSelector` ASTs.
- Supports tag, class, ID, compound, and complex combinators (`>`, ` `).
- Computes specificity as `(id_count, class_count, tag_count)` at parse time.
- Inherits `color`, `font-family`, `font-size`, `font-weight`, `line-height`, `text-align`, `visibility` from parent to child.
- Expands `margin`, `padding` shorthands (1/2/4-value syntax) and maps `background` to `background-color`.
- Uses `string_cache::DefaultAtom` for property names to minimize memory overhead during style resolution.
- Inline `style=""` attributes are parsed natively via `cssparser`'s `DeclarationParser` trait.

### layout

Walks the `StyledNode` tree and builds a parallel `TaffyTree<TextMeasureContext>`. Text nodes become leaf nodes measured by a fixed-width approximation (`char_width ~= font_size * 0.55`) with whitespace-aware wrapping and long-token fallback.

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

Font loading is backend-specific and is handled by the host renderer implementation.

### js

Embeds QuickJS via `rquickjs`. The `JsEngine` holds the `Document` behind `Rc<RefCell<Document>>`. QuickJS is single-threaded; all DOM access is serialized through the `RefCell`.

Exposed globals:
- `console.log(msg)`, `console.warn(msg)`, `console.error(msg)` -- print to stdout
- `document.getElementById(id)` -- returns a native `NodeHandle` object, or null
- `document.querySelector(selector)` -- supports complex combinators, returns a `NodeHandle`, or null
- `document.createElement(tagName)` -- inserts an Element into the arena, returns a `NodeHandle`
- `document.appendChild(parent, child)` -- appends a child node using native object handles
- `document.addEventListener(event, callback)` -- logs registration (scaffold)
- `setTimeout(callback, delay)` -- registers a cooperative timer; host must call `pump()`

`NodeHandle` class methods:
- `handle.tagName` -- returns the tag name string
- `handle.getAttribute(key)` -- returns value or null
- `handle.setAttribute(key, value)` -- updates or inserts attribute
- `handle.removeChild(child)` -- detaches child from node

Timer callbacks are stored as `rquickjs::Persistent<Function>` to survive context boundaries. The host application should call `pump()` on its event loop tick to dispatch expired timers.

## Building

```
cargo build
```

## Testing

```
cargo test
```

Key tests in `lib.rs` cover parsing/layout, JS bridge behavior, CSS combinator matching, recursive DOM node deletion, and whitespace text preservation.

## Limitations

This list is not exhaustive. The engine is a working skeleton, not a production browser.

- No networking, resource loading, or URL resolution.
- No `<img>`, `<video>`, `<canvas>`, `<iframe>`, or form elements.
- Inline formatting context is still incomplete. Text measurement uses a fixed-width approximation with whitespace-aware wrapping.
- Font loading and fallback are backend-specific and must be provided by the host.
- `display: inline` and `inline-block` are parsed but laid out identically to block.
- Layout properties `margin`, `padding`, `border-width`, `position`, `overflow`, `z-index`, `float` are not wired to Taffy.
- Color parsing is limited to 5 named colors and 6-digit hex.
- No `@media`, `@import`, `@keyframes`, CSS variables, or `calc()`.
- Selector matching supports combinators (`>`, ` `) but not `+`, `~`, attribute selectors, or pseudo-classes with arguments.
- `addEventListener` logs the event name but never dispatches events.
- Text measurement uses a monospace approximation and space-aware wrapping; exact glyph metrics are backend-specific.
- `setTimeout` fires callbacks only when the host calls `pump()`. There is no background thread or async runtime.
- Viewport units (`vw`, `vh`) are resolved to absolute pixels at tree construction time. Resizing the window requires rebuilding the layout tree.

## License

See repository root for license information.
