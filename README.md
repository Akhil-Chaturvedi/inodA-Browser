# inodA Browser

An experimental web browser engine for resource-constrained embedded systems. The engine is deliberately restricted to parse and render elementary HTML, CSS, and JavaScript with static memory bounds and bounded CPU overhead, serving as an inflexible but ultra-lightweight alternative to Chromium or Firefox for specialized hardware.

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

The engine parses HTML elements via synchronous blocking sequences, allocating tokens natively into an intrusive linked list arena-based DOM. This structure supports precise $O(1)$ parent traversals and $O(1)$ constant insertions/removals. The CSS layout engine requires `cosmic-text` glyph rendering mapped internally against $O(1)$ specific selectors (tag, class, ID). Combinators traverse recursively natively up the document structure. The JS engine is solely single-threaded and synchronously blocked, exposing a `NodeHandle` mapping structure allowing manual Javascript logic.

Memory pointers map statically back to interning (via `string_cache`), strictly pooling native DOM elements to identical cache points. There is zero networking, asset streaming, image loading, or complex iframe nesting built in. The host system strictly requires independent application definitions spanning Event Loops and graphical Canvas bindings.

## Building

Requires Rust 1.70 or later.

```
cd inoda-core
cargo build
cargo test
```

## License

See repository root for license information.
