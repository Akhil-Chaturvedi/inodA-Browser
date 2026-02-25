# inodA Browser

An experimental web browser engine for resource-constrained embedded systems. The engine is designed to parse and render basic HTML, CSS, and JavaScript with minimal memory and CPU overhead, serving as a lightweight alternative to Chromium or Firefox for specialized hardware.

## Repository structure

```
inodA-Browser/
  inoda-core/      -- the browser engine library (Rust crate)
  .gitignore
```

### inoda-core

The engine library. It handles:

- HTML parsing (spec-compliant via html5ever, streamed directly into a generational arena DOM)
- CSS parsing and style computation (specificity, inheritance, shorthand expansion)
- Flexbox/Grid layout (via the Taffy crate)
- 2D rendering (via an abstract backend trait implemented by the host)
- JavaScript execution (embedded QuickJS with a subset of the Web API)

See [inoda-core/README.md](inoda-core/README.md) for module details, API surface, build instructions, and the full limitations list.

See [inoda-core/ARCHITECTURE.md](inoda-core/ARCHITECTURE.md) for data flow, data structures, and design decisions.

## Current state

The engine parses HTML pages with embedded CSS, builds an arena-based DOM with O(1) parent traversing, and implements CSS selectors (tag, class, ID, compound, and complex combinators). It resolves Flexbox/Grid layout and renders backgrounds, borders, and text to a canvas. A JavaScript bridge provides a native `NodeHandle` class for DOM manipulation (`getElementById`, `querySelector`, `createElement`, `setAttribute`, `getAttribute`, `removeChild`) and a cooperative `setTimeout`.

The engine uses string interning (via `string_cache`) for tag names and CSS properties to minimize memory overhead. There is no networking, resource loading, or image support. The host application must provide a window or surface, an event loop, and a renderer backend implementation.

## Building

Requires Rust 1.70 or later.

```
cd inoda-core
cargo build
cargo test
```

## License

See repository root for license information.
