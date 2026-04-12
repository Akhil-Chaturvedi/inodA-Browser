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

- HTML parsing (html5gum tokenizer plus local DOM rules; not a WHATWG tree builder, single-pass into a generational arena DOM)
- CSS parsing and style computation (specificity, inheritance, shorthand expansion, cascade)
- Flexbox/Grid layout (via the Taffy crate)
- 2D rendering (via an abstract backend trait implemented by the host)
- JavaScript execution (embedded QuickJS with a subset of the Web API)

See [inoda-core/README.md](inoda-core/README.md) for module details, API surface, build instructions, and the full limitations list.

See [inoda-core/ARCHITECTURE.md](inoda-core/ARCHITECTURE.md) for data flow, data structures, and design decisions.

## Current state

HTML is tokenized via `html5gum` and built into an intrusive linked-list arena DOM (`generational_arena`); tree shape follows engine-local rules, not the full HTML parsing algorithm used in major browsers. Each node stores parent, child, and sibling pointers directly for O(1) traversal and mutation. Tag names for standard HTML elements are interned as `DefaultAtom`; custom element names use a heap-allocated `String` variant (`LocalName`) to avoid exhausting the global intern pool.

CSS selectors are pre-parsed into ASTs and distributed into hash-map buckets keyed by tag (`DefaultAtom`), class (`String`), and ID (`String`) for sublinear lookup. Class and ID bucket keys are not interned because they are uncontrolled user input; frameworks like Tailwind and CSS-in-JS generate thousands of randomized class names per session, and interning them as `DefaultAtom` would grow the global pool permanently. Property names in parsed `Declaration` objects use a typed `PropertyName` enum covering all supported properties, making cascade property matching an integer comparison rather than a string deref. Layout-critical keywords (e.g. `display: flex`) resolve to native Taffy enums during the cascade to eliminate per-frame string matching in the layout engine. Combinators (`>`, space, `+`, `~`) are evaluated by walking arena parent and sibling pointers. Attribute selectors (`[attr]`, `[attr=value]`) are matched against `ElementData::attributes`. Property values are parsed into typed `StyleValue` enums during cascade.

`ElementData::classes` stores class tokens in a single space-separated `String` rather than a `Vec`. The CSS cascade uses `split_whitespace()` iteration to check matches on-the-fly, and arrays rule buckets into a `SmallVec<[&[IndexedRule]; 8]>` to avoid per-element heap allocations during the cascade loops while skipping explicit lifetime gymnastics. ID values and attribute keys are also stored as `String` because they are uncontrolled user inputs; interning them as `DefaultAtom` would cause the global intern pool to grow permanently. For security, a limit of `MAX_ATTRIBUTES = 32` and `MAX_ATTRIBUTE_VALUE_LEN = 16KB` is enforced during parsing and mutation. This prevents both heap fragmentation and memory exhaustion attacks from uncontrolled input.

`ComputedStyle` is stored directly in each arena node's `ElementData` and `TextData`, populated once by `css::compute_styles()`. Layout and rendering read from these fields directly, frame-to-frame, without building any intermediate style tree. `document.stylesheet` is persistent and merged in-place as `<style>` tags are encountered during parsing. Font-size values expressed as `em` are resolved during the style cascade using the parent element's resolved pixel size. `rem` is resolved against `Document.root_font_size`, which defaults to 16px and is configurable by the host.

Text measurement uses `cosmic-text` for HarfBuzz-based shaping. Text is shaped once in a pre-pass to calculate intrinsic metrics. Taffy's layout solver invokes `buffer.set_size()` during the measure pass to re-paginate text at available widths; `TextMeasureContext` caches the last definite width and line count so repeated measure probes at the same width skip redundant work. After convergence, `finalize_text_measurements` performs final shaping at resolved dimensions. Buffer cache for nodes removed from the DOM is evicted at the start of each layout call.

The JS engine is single-threaded via `Rc<RefCell<Document>>`. Each DOM node carries a `js_handles` reference counter. JavaScript object identity (`===`) is enforced via a `_wrapNode` WeakRef cache, with traversal methods and `tagName` patched onto the prototype for efficiency. Reference counting prevents double-frees and ensures detached nodes remain in the arena as long as JavaScript holds a handle. Two `FinalizationRegistry` instances pair every `js_handles += 1` from raw DOM getters with a GC callback: `__nodeRegistry` clears the WeakRef map entry for canonical wrappers; `__ephemeralRegistry` decrements only when a duplicate raw wrapper is discarded on a cache hit. `_garbageCollectNodeRaw` maps to `try_cleanup_node` in Rust. Nodes are cleared from the arena by a batched `collect_garbage()` sweep once they are both detached and unreferenced. Construct the engine with `JsEngine::try_new` and handle `JsEngineError` from initialization, `execute_script`, and `dispatch_event`. JS DOM mutations set `document.dirty = true`; the host application is responsible for re-running the pipeline. `setTimeout`, `setInterval`, `clearTimeout`, and `clearInterval` are exposed; timers fire only when the host calls `JsEngine::pump()`. Every 60 calls to `pump()`, `document.collect_garbage()` is called to process the handle deletion queue. `pump()` also executes pending microtasks until the job queue is empty.

There is no networking, asset loading, or iframe handling. `<img>` elements have layout support (intrinsic sizing via `width`/`height` attributes and `aspect_ratio`); the host is responsible for decoding and rasterizing image data via the `ResourceLoader` and `RendererBackend` traits. The host application must provide a window, event loop, and graphics backend.

## Building

Requires Rust 1.85 or later.

```
cd inoda-core
cargo build
cargo test
```

Optional Criterion benchmarks (writes HTML reports under `target/criterion/`):

```
cargo bench
cargo bench --bench cascade
```

## License

See repository root for license information.
