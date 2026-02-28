# inodA Browser

An experimental web browser engine for resource-constrained embedded systems. The engine parses a subset of HTML, CSS, and JavaScript with bounded memory usage and CPU overhead. It is not a replacement for Chromium or Firefox; it is a minimal rendering engine for specialized hardware where those browsers cannot run.

## Repository structure

```
inodA-Browser/
  inoda-core/      -- the browser engine library (Rust crate)
  .gitignore
```

### inoda-core

The engine library. It handles:

- HTML parsing (streaming tokenizer via html5gum, single-pass into a generational arena DOM)
- CSS parsing and style computation (specificity, inheritance, shorthand expansion)
- Flexbox/Grid layout (via the Taffy crate)
- 2D rendering (via an abstract backend trait implemented by the host)
- JavaScript execution (embedded QuickJS with a subset of the Web API)

See [inoda-core/README.md](inoda-core/README.md) for module details, API surface, build instructions, and the full limitations list.

See [inoda-core/ARCHITECTURE.md](inoda-core/ARCHITECTURE.md) for data flow, data structures, and design decisions.

## Current state

The engine tokenizes HTML via `html5gum` into an intrusive linked-list arena-based DOM. Nodes store parent, child, and sibling pointers directly for O(1) traversal and O(1) insertion/removal. CSS selectors are pre-parsed into an AST and distributed into hash-map buckets by tag, class, and ID for sublinear lookup. Combinators (child `>`, descendant ` `) are evaluated by walking parent pointers. Property values are parsed into typed enums (`StyleValue`) during the cascade, so the layout and render loops operate on numbers and enum variants rather than strings.

Text measurement uses `cosmic-text` for HarfBuzz-based shaping. Text buffers are pre-populated before Taffy's layout solver runs, so the measure closure only adjusts width constraints on already-shaped buffers.

The JS engine is single-threaded, exposing DOM handles as `NodeHandle` class instances backed by arena indices. A `WeakRef`-based identity cache ensures `===` equality across repeated queries for the same node.

There is no networking, asset loading, image support, or iframe handling. The host application must provide a window, event loop, and graphics backend.

## Building

Requires Rust 1.70 or later.

```
cd inoda-core
cargo build
cargo test
```

## License

See repository root for license information.
