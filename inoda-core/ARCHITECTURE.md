# Architecture

This document describes the data flow and module boundaries inside `inoda-core`.

## Pipeline

```
HTML string
  |
  v
html::parse_html()          -- html5gum tokenizer -> token loop -> arena DOM (Document)
  |                            also extracts <style> tag contents
  v
css::parse_stylesheet()     -- cssparser tokenizer -> StyleSheet (HashMap<DefaultAtom, Vec<IndexedRule>>)
css::compute_styles()       -- walks DOM, resolves specificity against hash-map buckets via
  |                            right-to-left combinator backtracking,
  |                            inherits properties via Rc::clone, expands shorthands -> StyledNode tree
  v
layout::compute_layout()    -- pre-populates cosmic-text buffers for all text nodes,
  |                            converts StyledNode tree to TaffyTree, resolves
  |                            dimensions (px, %, vw, vh, em, rem, auto),
  |                            runs Taffy flexbox/grid solver -> positioned Layout tree
  v
render::draw_layout_tree()  -- walks Layout + StyledNode in parallel,
                               issues renderer backend draw calls: fill_rect (bg),
                               stroke_rect (border), draw_glyphs (text content)
```

JavaScript execution happens outside this pipeline. The host application creates a `JsEngine`, passing in the `Document`. JS code can read and mutate the DOM via `Rc<RefCell<Document>>`. QuickJS is single-threaded; access is serialized. Returned node handles preserve `===` identity via a `WeakRef`-based `__nodeCache` on the JS side, with a `FinalizationRegistry` that deletes stale map keys when QuickJS GC runs. There is currently no mechanism to trigger re-style or re-layout from JS mutations. Timer callbacks registered via `setTimeout` fire only when the host calls `JsEngine::pump()`.

## Data structures

### Document (dom/mod.rs)

```
Document {
    nodes: Arena<Node>,             // generational_arena::Arena, indexed by Index
    root_id: generational_arena::Index,
    style_texts: Vec<String>,       // raw CSS from <style> tags
    id_map: HashMap<String, NodeId> // O(1) getElementById lookup
}

Node = Element(ElementData) | Text(TextData) | Root(RootData)
NodeId = generational_arena::Index  // type alias

ElementData {
    tag_name: string_cache::DefaultAtom,   // interned
    attributes: Vec<(string_cache::DefaultAtom, String)>,
    classes: Vec<string_cache::DefaultAtom>,
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>
}

TextData {
    text: String,
    parent: Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>
}

RootData {
    first_child: Option<NodeId>,
    last_child: Option<NodeId>
}
```

Generational indices provide O(1) insertion and deletion without index invalidation or ABA problems. The DOM tree is wired as an intrusive linked list (`first_child`, `next_sibling`, etc.) which allows mutations without allocating child vectors. Node deletion uses an iterative queue rather than recursion to avoid stack overflow on deeply nested trees.

### StyledNode (dom/mod.rs)

```
StyledNode {
    node_id: NodeId,
    specified_values: Rc<Vec<(DefaultAtom, StyleValue)>>,  // typed property values
    children: Vec<StyledNode>
}
```

This is a tree (not arena). Each node owns its children. It exists only during layout computation and rendering, then gets dropped. When a node has no CSS declarations of its own, it receives a clone of its parent's `Rc`. When a node does have declarations, the parent's styles are filtered to only inheritable properties (e.g., `color`, `font-size`) before merging with the node's own declarations. Non-inheritable properties like `width` or `margin` are not passed to children.

### StyleSheet (css/mod.rs)

```
StyleSheet {
    by_id: HashMap<DefaultAtom, Vec<IndexedRule>>,
    by_class: HashMap<DefaultAtom, Vec<IndexedRule>>,
    by_tag: HashMap<DefaultAtom, Vec<IndexedRule>>,
    universal: Vec<IndexedRule>
}
IndexedRule { selector: ComplexSelector, declarations: Rc<Vec<Declaration>>, rule_index: usize }
ComplexSelector { last: CompoundSelector, ancestors: Vec<(Combinator, CompoundSelector)>, specificity: (u32, u32, u32) }
Combinator = Descendant | Child
Declaration { name: DefaultAtom, value: StyleValue }
```

Selectors are pre-parsed into a `ComplexSelector` AST at stylesheet creation time. Specificity is calculated once during parsing. Each rule also records a `rule_index` (its position in the original stylesheet) for stable tie-breaking. Rules are distributed into hash-map buckets based on their right-most selector component. During style resolution, matching buckets are merged via a k-way pointer walk over the pre-sorted slices, breaking specificity ties by `rule_index` to preserve document source order. Combinators (`>` for child, space for descendant) are evaluated by walking parent pointers up the arena. Inline `style` attributes are parsed via `cssparser`'s `DeclarationParser` trait.

### PendingTimer (js/mod.rs)

```
PendingTimer {
    id: u32,
    fire_at: Instant,
    callback: Persistent<Function<'static>>     // rquickjs::Persistent
}
```

Timer callbacks are stored as `rquickjs::Persistent<Function>` which holds a JS function reference outside the QuickJS context lifetime. Pending timers are held in a `std::collections::BinaryHeap` ordered by `fire_at` (min-heap via reversed `Ord`). `JsEngine::pump()` pops expired entries from the heap without allocating temporary vectors.

## Specificity

Selectors are scored as `(id_count, class_count, tag_count)`. Matched rules are merged in specificity order. Equal-specificity rules from different buckets are tie-broken by `rule_index` to preserve document source order. Inline `style` attributes always win because they are applied after all stylesheet rules.

## Text measurement

Text nodes are inserted into Taffy as leaf nodes with a `TextMeasureContext`. A pre-pass traverses the styled DOM and creates a `cosmic-text::Buffer` for each text node, performing HarfBuzz shaping once. The buffer cache is caller-owned and persists across frames; it is not cleared internally. During `compute_layout_with_measure`, the measure closure retrieves the already-shaped buffer, calls `buffer.set_size()` to adjust the width constraint, then calls `buffer.shape_until_scroll()` to re-wrap the text. It then counts layout lines to determine the height.

After layout completes, `finalize_text_measurements` walks the Taffy tree and extracts the final glyph arrays (not string copies) into a `TextLayoutCache` for the renderer.

## Thread safety

`JsEngine` holds the `Document` inside `Rc<RefCell<Document>>`. QuickJS and `rquickjs` are designed for single-threaded usage. All JS-exposed functions borrow the `RefCell` to access the DOM. This avoids mutex overhead while maintaining memory safety through Rust's runtime borrow checking. `NodeHandle` does not implement `Drop`; nodes created by JavaScript persist in the arena until explicitly removed via `removeChild()`. This prevents ABA corruption from QuickJS garbage collection triggering arena deletions on still-attached nodes.

## HTML parsing

The HTML module iterates over `html5gum` tokens in a loop. Each `StartTag` token creates an `ElementData` node in the arena and appends it as a child of the current parent. Before appending, the parser walks up the ancestor chain from `current_parent` to check whether the new tag should implicitly close an ancestor. For example, when a `<div>` is encountered inside `<p><span><b>`, the walk finds the `<p>` ancestor and pops `current_parent` back to the `<p>`'s parent. The walk stops at block-level boundaries (`div`, `body`, `td`, `th`, `table`) to prevent over-closing. `EndTag` tokens pop the current parent back to its own parent. `String` tokens create `TextData` nodes. Byte slices are validated with `std::str::from_utf8` directly, without allocating through `from_utf8_lossy`.

Content inside `<script>` and `<style>` tags is tracked via an `inside_raw_tag` state variable. While inside a raw tag, `StartTag` and `EndTag` tokens for other elements are accumulated as literal text rather than parsed as DOM nodes. Only the matching closing tag (e.g., `</script>`) exits the raw state. CSS text from `<style>` elements is collected into `Document::style_texts`.
