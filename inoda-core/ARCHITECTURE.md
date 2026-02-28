# Architecture

This document describes the data flow and module boundaries inside `inoda-core`.

## Pipeline

```
HTML string
  |
  v
html::parse_html()          -- html5gum tokenizer -> byte-stream iterator -> arena DOM (Document)
  |                            also extracts <style> tag contents inline
  v
css::parse_stylesheet()     -- cssparser tokenizer -> StyleSheet (HashMap<DefaultAtom, Vec<IndexedRule>>)
css::compute_styles()       -- walks DOM, resolves specificity against HashMaps via right-to-left combinator backtracking,
  |                            inherits properties via Rc::clone, expands shorthands -> StyledNode tree
  v
layout::compute_layout()    -- converts StyledNode tree to TaffyTree, resolves
  |                            dimensions (px, %, vw, vh, em, rem, auto),
  |                            runs Taffy flexbox/grid solver -> positioned Layout tree
  v
render::draw_layout_tree()  -- walks Layout + StyledNode in parallel,
                               issues renderer backend draw calls: fill_rect (bg),
                               stroke_rect (border), draw_text (text content)
```

JavaScript execution happens outside this pipeline. The host application creates a `JsEngine`, passing in the `Document`. JS code can read and mutate the DOM via `Rc<RefCell<Document>>`. QuickJS is single-threaded; access is serialized. There is currently no mechanism to trigger re-style or re-layout from JS mutations. Timer callbacks registered via `setTimeout` fire only when the host calls `JsEngine::pump()`.

## Data structures

### Document (dom/mod.rs)

```
Document {
    nodes: Arena<Node>,             // generational_arena::Arena, indexed by Index
    root_id: generational_arena::Index,
    style_texts: Vec<String>,       // raw CSS from <style> tags
    // parent pointers live on ElementData/TextData for O(1) parent lookups
}

Node = Element(ElementData) | Text(TextData) | Root(RootData)
NodeId = generational_arena::Index  // type alias

ElementData {
    tag_name: string_cache::DefaultAtom,   // interned
    attributes: Vec<(string_cache::DefaultAtom, String)>,
    classes: std::collections::HashSet<string_cache::DefaultAtom>,
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>
}

TextData {
    text: String,
    parent: Option<NodeId>
}

RootData {
    first_child: Option<NodeId>,
    last_child: Option<NodeId>
}
```

Generational indices provide O(1) insertion and deletion without index invalidation or ABA problems. Removed nodes do not leave dangling references. The DOM tree itself is wired via an intrusive linked list (`first_child`, `next_sibling`, etc.) which allows for zero-allocation mutations.

### StyledNode (dom/mod.rs)

```
StyledNode {
    node_id: NodeId,                            // generational_arena::Index
    specified_values: std::rc::Rc<Vec<(string_cache::DefaultAtom, String)>>, // shared computed CSS properties
    children: Vec<StyledNode>                   // mirrors DOM children
}
```

This is a tree (not arena). Each node owns its children. It exists only during layout computation and rendering, then gets dropped.

### StyleSheet (css/mod.rs)

```
StyleSheet {
    by_id: HashMap<DefaultAtom, Vec<IndexedRule>>,
    by_class: HashMap<DefaultAtom, Vec<IndexedRule>>,
    by_tag: HashMap<DefaultAtom, Vec<IndexedRule>>,
    universal: Vec<IndexedRule>
}
IndexedRule { selector: ComplexSelector, declarations: std::rc::Rc<Vec<Declaration>> }
ComplexSelector { last: CompoundSelector, ancestors: Vec<(Combinator, CompoundSelector)>, specificity: (u32, u32, u32) }
Combinator = Descendant | Child
Declaration { name: string_cache::DefaultAtom, value: String }
```

Selectors are pre-parsed into a `ComplexSelector` AST at stylesheet creation time. Specificity is calculated once during parsing, and rules are distributed into $O(1)$ Hash Maps (`by_tag`, `by_class`, etc.) based on their right-most matching segment. Combinators (`>` for child, space for descendant) are evaluated using recursive tree backtracking logic against `generational_arena` node pointers. Inline `style` attributes are parsed using `cssparser`'s `DeclarationParser` trait directly.

### PendingTimer (js/mod.rs)

```
PendingTimer {
    id: u32,
    fire_at: Instant,
    callback: Persistent<Function<'static>>     // rquickjs::Persistent
}
```

Timer callbacks are stored as `rquickjs::Persistent<Function>` which safely holds a JS function reference outside the QuickJS context lifetime. They are restored and invoked inside `JsEngine::pump()`.

## Specificity

Selectors are scored as `(id_count, class_count, tag_count)`. Matched rules are sorted by this tuple. Equal-specificity rules preserve source order. Inline `style` attributes always win because they are applied after all stylesheet rules.

## Text measurement

Text nodes are inserted into Taffy as leaf nodes with a measurement context. During `compute_layout_with_measure`, text uses `cosmic-text` and `fontdb` to perform actual text shaping and font metric calculation via a `TextLayoutCache`. This provides accurate glyph sizing and wrapping based on system or hosted fonts rather than approximations.

## Thread safety

`JsEngine` holds the `Document` inside `Rc<RefCell<Document>>`. QuickJS and its wrapper `rquickjs` are designed for single-threaded usage. All JS-exposed functions (e.g., in `NodeHandle`) borrow the `RefCell` to access the DOM. This model provides maximum deterministic performance for embedded environments by avoiding mutex locking arrays while ensuring memory safety through Rust's runtime borrow checking. `NodeHandle` captures a `Weak<RefCell<Document>>` so `rquickjs` Garbage Collection passes map natively to Rust `Drop` trait execution, severing detached `NodeId`s correctly from `generational_arena`.

## HTML parsing

The HTML module implements a purely custom structural byte-stream using `html5gum` token emitters. This structure entirely ditches abstract `TreeSink` logic. It maps byte slices from the emitter sequentially into `generational_arena` ID allocations natively, executing at $O(1)$ token parsing cost with precisely zero runtime heap allocations.
