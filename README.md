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
- CSS parsing and style computation (specificity, inheritance, shorthand expansion, cascade)
- Flexbox/Grid layout (via the Taffy crate)
- 2D rendering (via an abstract backend trait implemented by the host)
- JavaScript execution (embedded QuickJS with a subset of the Web API)

See [inoda-core/README.md](inoda-core/README.md) for module details, API surface, build instructions, and the full limitations list.

See [inoda-core/ARCHITECTURE.md](inoda-core/ARCHITECTURE.md) for data flow, data structures, and design decisions.

## Current state

HTML is tokenized via `html5gum` into an intrusive linked-list arena DOM (`generational_arena`). Each node stores parent, child, and sibling pointers directly for O(1) traversal and mutation. Tag names for standard HTML elements are interned as `DefaultAtom`; custom element names use a heap-allocated `String` variant (`LocalName`) to avoid exhausting the global intern pool.

CSS selectors are pre-parsed into ASTs and distributed into hash-map buckets keyed by tag (`DefaultAtom`), class (`String`), and ID (`String`) for sublinear lookup. Class and ID bucket keys are not interned because they are uncontrolled user input; frameworks like Tailwind and CSS-in-JS generate thousands of randomized class names per session, and interning them as `DefaultAtom` would grow the global pool permanently. Property names in parsed `Declaration` objects use a typed `PropertyName` enum covering all supported properties, making cascade property matching an integer comparison rather than a string deref. Combinators (`>`, space) are evaluated by walking arena parent pointers. Property values are parsed into typed `StyleValue` enums during cascade.

`ElementData::classes` stores class tokens as plain `String` values for the same reason. Only attribute keys for recognized HTML attributes (e.g. `id`, `class`, `style`) are interned as `DefaultAtom` since the set of valid attribute keys is bounded.

`ComputedStyle` is stored directly in each arena node's `ElementData` and `TextData`, populated once by `css::compute_styles()`. Layout and rendering read from these fields directly, frame-to-frame, without building any intermediate style tree. `document.stylesheet` is persistent and merged in-place as `<style>` tags are encountered during parsing. Font-size values expressed as `em` or `rem` are resolved during the style cascade using the parent element's resolved pixel size.

Text measurement uses `cosmic-text` for HarfBuzz-based shaping. Text is shaped once in a pre-pass to calculate intrinsic metrics (max/min width). Taffy's layout solver uses these metrics for $O(1)$ size estimations, deferring final re-wrapping until the layout is resolved to prevent performance bottlenecks. Buffer cache for nodes removed from the DOM is evicted at the start of each layout call.

The JS engine is single-threaded via `Rc<RefCell<Document>>`. Each DOM node carries a `js_handles` reference counter and a `__nodeKey` identity. Reference counting prevents double-frees and ensures detached nodes remain in the arena as long as JavaScript holds a handle. A `FinalizationRegistry` decrements these handles when JS objects are garbage-collected, triggering a safe wipe of the arena entry once it is both detached and unreferenced. JS DOM mutations set `document.dirty = true`; the host application is responsible for re-running the style, layout, and render pipeline. `setTimeout`, `setInterval`, `clearTimeout`, and `clearInterval` are exposed; timers fire only when the host calls `JsEngine::pump()`. Rescheduled `setInterval` timers are staged into a separate list before being pushed back to the heap to prevent loop locking. Every 60 calls to `pump()`, `runtime.run_gc()` is called to force handle cleanup.

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
