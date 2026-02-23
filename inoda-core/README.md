# inoda-core

A minimal browser engine library written in Rust. It parses HTML into a DOM tree, applies CSS styles with specificity and inheritance, computes Flexbox/Grid layout via Taffy, renders to a `femtovg` canvas, and exposes a subset of the Web API to an embedded QuickJS JavaScript runtime.

This is a library crate. It does not include a window, event loop, or GPU context -- those belong to the host application binary that depends on `inoda-core`.

## Status

Early development. The engine covers enough of the web platform to parse simple pages and lay them out, but large gaps remain (see [Limitations](#limitations)).

## Dependencies

| Crate              | Version | Purpose                                     |
|--------------------|---------|---------------------------------------------|
| `html5ever`        | 0.38    | Spec-compliant HTML tokenizer and parser    |
| `markup5ever`      | 0.38    | Shared types for html5ever (QualName, etc.) |
| `cssparser`        | 0.36    | Mozilla CSS tokenizer (same one Servo uses) |
| `taffy`            | 0.9     | Flexbox and CSS Grid layout algorithm       |
| `femtovg`          | 0.20    | 2D vector graphics (OpenGL-backed)          |
| `rquickjs`         | 0.11    | QuickJS JavaScript engine bindings          |
| `generational-arena`| 0.2    | Generational index arena for the DOM        |

No other runtime dependencies.

## Module overview

```
src/
  lib.rs        -- crate root, re-exports modules, integration tests
  dom/mod.rs    -- generational arena DOM: Document, Node, ElementData, StyledNode
  html/mod.rs   -- TreeSink implementation, streams HTML directly into arena
  css/mod.rs    -- CSS parser, specificity, inheritance, shorthand expansion, inline style parsing
  layout/mod.rs -- StyledNode -> Taffy tree builder, dimension parsing
  render/mod.rs -- Taffy layout -> femtovg draw calls (backgrounds, borders, text)
  js/mod.rs     -- QuickJS runtime with document.*, console.*, cooperative setTimeout
```

### dom

`generational_arena::Arena<Node>` indexed by `generational_arena::Index` (`NodeId`). No `Rc`, no `RefCell`. Nodes are either `Element(ElementData)`, `Text(String)`, or `Root(Vec<NodeId>)`. Attributes are stored as `Vec<(String, String)>`. Computed styles live in a separate `StyledNode` tree that references arena indices.

Generational indices allow O(1) insertion and deletion without invalidating other handles. Previous versions used a flat `Vec<Node>` indexed by `usize`, which leaked memory on deletion.

### html

Implements `html5ever::TreeSink` directly on a `DocumentBuilder` wrapper. HTML tokens stream into the generational arena in a single allocation pass. There is no intermediate `RcDom` tree. Extracts raw CSS text from `<style>` elements and stores it on `Document::style_texts`. Whitespace-only text nodes are discarded.

### css

- Parses CSS text into `StyleSheet -> Vec<StyleRule> -> Vec<Declaration>`.
- Supports tag, class, ID, and compound selectors (`div.card`, `h1, h2`).
- Computes specificity as `(id_count, class_count, tag_count)` and applies rules in specificity order, preserving source order for ties.
- Inherits `color`, `font-family`, `font-size`, `font-weight`, `line-height`, `text-align`, `visibility` from parent to child.
- Expands `margin`, `padding` shorthands (1/2/4-value syntax) and maps `background` to `background-color`.
- Inline `style=""` attributes are parsed natively via `cssparser`'s `DeclarationParser` trait. Previous versions used a `format!("dummy {{ ... }}")` workaround to wrap inline styles in a fake rule block.

### layout

Walks the `StyledNode` tree and builds a parallel `TaffyTree<String>`. Text nodes become leaf nodes measured by a character-count heuristic (8px per character, 18px line height) -- this is a placeholder, not real text shaping.

Supported CSS properties mapped to Taffy:
- `display`: flex, grid, block, none (inline/inline-block are recognized but not yet laid out differently)
- `flex-direction`: row, column
- `width`, `height` with units: `px`, `%`, `vw`, `vh`, `em`, `rem`, `auto`

Non-flex elements default to `flex-direction: column` to approximate block stacking.

Properties not yet wired: `margin-*`, `padding-*`, `border-*`, `align-items`, `justify-content`, `gap`, `flex-wrap`, `flex-grow`, `flex-shrink`, `min-*`, `max-*`, `position`, `overflow`.

### render

Recursively walks the Taffy layout tree alongside the `StyledNode` tree and issues `femtovg` draw calls:
- Background rectangles (`background-color`)
- Border strokes (`border-color`, always 1px, no per-side control)
- Text via `canvas.fill_text()` using inherited `color` and `font-size`

Color parsing handles the named colors `red`, `green`, `blue`, `black`, `white` and 6-digit hex (`#rrggbb`). No `rgb()`, `rgba()`, `hsl()`, shorthand hex, or alpha support.

Font loading is not handled here. The host application must register fonts with the `femtovg::Canvas` before text will render.

### js

Embeds QuickJS via `rquickjs`. The `JsEngine` takes ownership of a `Document` behind `Arc<Mutex<>>` so closures registered into JS can read and mutate the DOM.

Exposed globals:
- `console.log(msg)`, `console.warn(msg)`, `console.error(msg)` -- print to stdout
- `document.getElementById(id)` -- returns element tag name as a string, or null
- `document.querySelector(selector)` -- basic tag/class/id matching, returns tag name as a string, or null
- `document.createElement(tagName)` -- inserts an Element into the arena, returns its generational index as a `"idx,generation"` string handle
- `document.appendChild(parentHandle, childHandle)` -- appends a child node using string handles
- `document.addEventListener(event, callback)` -- logs the registration, does not fire events
- `setTimeout(callback, delay)` -- registers the callback in a cooperative timer queue; the host must call `JsEngine::pump()` to fire expired timers

None of these return actual DOM element objects. `getElementById` and `querySelector` return the tag name as a string. This is a scaffold, not a conformant DOM API.

Timer callbacks are stored as `rquickjs::Persistent<Function>` so they survive the QuickJS context lifetime boundary. The host application should call `pump()` on its event loop tick to dispatch expired timers.

## Building

```
cargo build
```

## Testing

```
cargo test
```

There are two integration tests in `lib.rs`:
1. `test_parse_simple_html` -- parses HTML with embedded `<style>`, builds a styled tree, computes layout, and prints the Taffy debug tree. Does not test rendering (no GL context in CI).
2. `test_javascript_bridge` -- parses HTML, creates a `JsEngine`, evaluates arithmetic, `document.getElementById`, `document.querySelector`, and `console.log`.

## Limitations

This list is not exhaustive. The engine is a working skeleton, not a production browser.

- No networking, resource loading, or URL resolution.
- No `<img>`, `<video>`, `<canvas>`, `<iframe>`, or form elements.
- No inline text flow or line wrapping. Text measurement is a fixed-width character-count estimate.
- No font loading or font fallback. The host must register fonts with femtovg.
- `display: inline` and `inline-block` are parsed but laid out identically to block.
- Layout properties `margin`, `padding`, `border-width`, `position`, `overflow`, `z-index`, `float` are not wired to Taffy.
- Color parsing is limited to 5 named colors and 6-digit hex.
- No `@media`, `@import`, `@keyframes`, CSS variables, or `calc()`.
- Selector matching is homegrown string manipulation. It handles tag, class, ID, and compound selectors but breaks on complex combinators (`>`, `+`, `~`), attribute selectors, and pseudo-classes with arguments.
- `addEventListener` logs the event name but never dispatches events.
- `document.getElementById` and `document.querySelector` return tag name strings, not DOM node objects.
- `femtovg` requires an OpenGL context. The render module cannot be used in headless/software-only environments without swapping the backend.
- `setTimeout` fires callbacks only when the host calls `pump()`. There is no background thread or async runtime.
- Viewport units (`vw`, `vh`) are resolved to absolute pixels at tree construction time. Resizing the window requires rebuilding the layout tree.

## License

See repository root for license information.
