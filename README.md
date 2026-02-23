# inodA Browser

A lightweight web browser built for embedded and resource-constrained devices. The project aims to render basic web content (HTML, CSS, JavaScript) with minimal memory and CPU overhead, targeting hardware that cannot run Chromium or Firefox.

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
- 2D rendering (via femtovg, requires an OpenGL context from the host)
- JavaScript execution (embedded QuickJS with a subset of the Web API)

See [inoda-core/README.md](inoda-core/README.md) for module details, API surface, build instructions, and the full limitations list.

See [inoda-core/ARCHITECTURE.md](inoda-core/ARCHITECTURE.md) for data flow, data structures, and design decisions.

## Current state

The engine can parse simple HTML pages with embedded CSS, compute Flexbox layout, and render backgrounds/borders/text to a canvas. A basic JavaScript bridge provides `console.log`, `document.getElementById`, `document.querySelector`, `document.createElement`, `document.appendChild`, and a cooperative `setTimeout` with a host-driven timer queue.

This is early-stage. There is no networking, no resource loading, no image support, and no inline text flow. The host application (not included in this repository yet) must provide a window, OpenGL context, event loop, and font registration.

## Building

Requires Rust 1.70 or later.

```
cd inoda-core
cargo build
cargo test
```

## License

See repository root for license information.
