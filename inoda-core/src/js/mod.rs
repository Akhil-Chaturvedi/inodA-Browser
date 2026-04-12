//! JavaScript execution module.
//!
//! Embeds QuickJS via `rquickjs`. Exposes a subset of the Web API:
//! - `console.log`, `console.warn`, `console.error` (print to stdout)
//! - `document.getElementById`, `document.querySelector` (return native `NodeHandle` objects)
//! - `document.createElement`, `document.appendChild` (mutate the arena DOM)
//! - `element.addEventListener` (registers callbacks; dispatched via `JsEngine::dispatch_event`)
//! - `setTimeout`, `setInterval` (cooperative timer queue via `pump()`)
//!
//! DOM handles are exposed to JavaScript as native `NodeHandle` class instances
//! wrapping a `generational_arena::Index`. Methods include:
//! - `handle.tagName` (lazy lookup in arena, no redundant string storage)
//! - `handle.getAttribute(key)`
//! - `handle.setAttribute(key, value)`
//! - `handle.removeChild(child)`
//!
//! Each `NodeHandle` carries a `__nodeKey` property: a two-element JS array
//! `[u32 index, u64 generation]`. JavaScript object identity (`===`) is enforced
//! via a `_wrapNode` WeakRef cache in the JS environment. Two `FinalizationRegistry`
//! instances pair every `js_handles += 1` with a GC callback: `__nodeRegistry`
//! clears the WeakRef map entry for canonical wrappers; `__ephemeralRegistry`
//! handles duplicate raw wrappers discarded on cache hits. Both call
//! `_garbageCollectNodeRaw` (mapped to `try_cleanup_node` in Rust) to decrement
//! the handle count. Detached nodes are cleared from the arena by the batched
//! `collect_garbage()` sweep.
//!
//! The Document is held behind `Rc<RefCell<Document>>` for single-threaded access.
//! All JS operations are synchronous and serialized through this lock.
//!
//! Initialization is fallible: use [`JsEngine::try_new`] and handle [`JsEngineError`].
//! Every `js_handles += 1` from a raw DOM getter must be paired with exactly one
//! `FinalizationRegistry` callback (`__nodeRegistry` for canonical wrappers,
//! `__ephemeralRegistry` for discarded duplicate wrappers on `_wrapNode` cache hits).

use crate::dom::{Document, NodeId};
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::This;
use rquickjs::{Context, Persistent, Runtime};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

/// Errors from QuickJS runtime setup, Web API registration, dispatch, or script evaluation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum JsEngineError {
    RuntimeInit(String),
    ContextInit(String),
    WebApiInit(String),
    DispatchFailed(String),
    ScriptEval(String),
}

impl std::fmt::Display for JsEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsEngineError::RuntimeInit(s) => write!(f, "QuickJS runtime init failed: {s}"),
            JsEngineError::ContextInit(s) => write!(f, "QuickJS context init failed: {s}"),
            JsEngineError::WebApiInit(s) => write!(f, "Web API registration failed: {s}"),
            JsEngineError::DispatchFailed(s) => write!(f, "Event dispatch failed: {s}"),
            JsEngineError::ScriptEval(s) => write!(f, "Script evaluation failed: {s}"),
        }
    }
}

impl std::error::Error for JsEngineError {}

fn js_try<T>(r: rquickjs::Result<T>, ctx: &'static str) -> Result<T, JsEngineError> {
    r.map_err(|e| JsEngineError::WebApiInit(format!("{ctx}: {e:?}")))
}

// ---------------------------------------------------------------------------
// NodeHandle: an opaque JS class wrapping a generational_arena::Index.
// ---------------------------------------------------------------------------

/// A handle to a native DOM node exposed to JavaScript.
/// The `NodeHandle` caches the NodeId structurally explicitly bypassing Drop.
/// Actual memory lifecycles are resolved exclusively via explicit `removeChild()`
/// bindings and JS QuickJS `FinalizationRegistry` background mappings.
#[rquickjs::class]
#[derive(Clone, Debug)]
pub struct NodeHandle {
    pub index: u32,
    pub generation: u64,
}

#[rquickjs::methods]
impl NodeHandle {
    #[qjs(get, rename = "__nodeKey")]
    pub fn node_key<'js>(&self, ctx: rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Array<'js>> {
        let arr = rquickjs::Array::new(ctx)?;
        arr.set(0, self.index)?;
        arr.set(1, self.generation)?;
        Ok(arr)
    }
}

impl NodeHandle {
    pub fn from_node_id(id: NodeId) -> Self {
        let (index, generation) = id.into_raw_parts();
        NodeHandle {
            index: index as u32,
            generation,
        }
    }

    pub fn to_node_id(&self) -> NodeId {
        NodeId::from_raw_parts(self.index as usize, self.generation)
    }
}

impl<'js> Trace<'js> for NodeHandle {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {
        // No JS values to trace; NodeHandle only contains plain integers and a weak ref.
    }
}

// NodeHandle intentionally omits a `Drop` trait implementation.
// Nodes created by JavaScript live in the Rust Arena until explicitly removed via `removeChild()`.
// Automatically calling `remove_node` on drop causes ABA memory corruption if a JS wrapper
// goes out of scope while the node is still attached or referenced elsewhere.
unsafe impl<'js> rquickjs::JsLifetime<'js> for NodeHandle {
    type Changed<'to> = NodeHandle;
}

// NodeHandleWithTag is removed as NodeHandle now includes tagName and node_key getters.

// ---------------------------------------------------------------------------
// Timer queue
// ---------------------------------------------------------------------------

use std::cmp::Ordering;

/// A pending timer entry storing a persistent JS callback.
struct PendingTimer {
    id: u32,
    fire_at: Instant,
    callback: Persistent<rquickjs::Function<'static>>,
    is_interval: bool,
    delay_ms: u64,
}

impl PartialEq for PendingTimer {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for PendingTimer {}

impl PartialOrd for PendingTimer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingTimer {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap behavior based on fire_at
        other
            .fire_at
            .cmp(&self.fire_at)
            .then_with(|| other.id.cmp(&self.id))
    }
}

/// Wrapper around the QuickJS Runtime and Context.
pub struct JsEngine {
    #[allow(dead_code)]
    runtime: Runtime,
    context: Context,
    pub document: Rc<RefCell<Document>>,
    /// Monotonically increasing timer ID counter.
    next_timer_id: Rc<Cell<u32>>,
    /// Min-Heap of pending timers waiting to fire.
    pending_timers: Rc<RefCell<std::collections::BinaryHeap<PendingTimer>>>,
    /// Track active timers natively preventing runaway intervals.
    active_timers: Rc<RefCell<std::collections::HashSet<u32>>>,
    /// Track iterations for deterministic QuickJS garbage collection.
    pump_ticks: Rc<Cell<u32>>,
    /// Track start time of the current JS execution block to prevent infinite loops.
    last_start_time: Rc<Cell<Option<Instant>>>,
}

impl JsEngine {
    /// Dispatches a DOM event to listeners registered on the hit node. Failures are
    /// returned so the host can log or ignore them; they are non-fatal for the engine.
    pub fn dispatch_event(&self, x: f32, y: f32, event_type: &str) -> Result<(), JsEngineError> {
        let hit = {
            let doc = self.document.borrow();
            doc.hit_test(x, y)
        };
        if let Some(node_id) = hit {
            self.last_start_time.set(Some(Instant::now()));
            let event_type_str = event_type.to_string();
            let res = self.context.with(|ctx| -> Result<(), JsEngineError> {
                let globals = ctx.globals();
                let doc_obj = js_try(globals.get::<_, rquickjs::Object>("document"), "document")?;
                let dispatch_func =
                    js_try(doc_obj.get::<_, rquickjs::Function>("_triggerEvent"), "_triggerEvent")?;
                let (idx, generation) = node_id.into_raw_parts();
                let arr = js_try(rquickjs::Array::new(ctx.clone()), "Array::new")?;
                js_try(arr.set(0, idx as u64), "event key idx")?;
                js_try(arr.set(1, generation), "event key gen")?;
                js_try(
                    dispatch_func.call::<_, ()>((arr, event_type_str)),
                    "_triggerEvent call",
                )?;
                Ok(())
            });
            self.last_start_time.set(None);
            res?;
        }
        Ok(())
    }

    /// Fallible constructor. Prefer this in production so OOM / init failures surface to the host.
    pub fn try_new(document: Document) -> Result<Self, JsEngineError> {
        let runtime = Runtime::new()
            .map_err(|e| JsEngineError::RuntimeInit(format!("{e:?}")))?;
        let context = Context::full(&runtime)
            .map_err(|e| JsEngineError::ContextInit(format!("{e:?}")))?;

        let last_start_time: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        {
            let last_start = last_start_time.clone();
            runtime.set_interrupt_handler(Some(Box::new(move || {
                if let Some(start) = last_start.get() {
                    if start.elapsed().as_millis() >= 500 {
                        return true; // Interrupt!
                    }
                }
                false
            })));
        }

        let engine = JsEngine {
            runtime,
            context,
            document: Rc::new(RefCell::new(document)),
            next_timer_id: Rc::new(Cell::new(1)),
            pending_timers: Rc::new(RefCell::new(std::collections::BinaryHeap::new())),
            active_timers: Rc::new(RefCell::new(std::collections::HashSet::new())),
            pump_ticks: Rc::new(Cell::new(0)),
            last_start_time,
        };

        engine.init_web_api()?;
        Ok(engine)
    }

    /// Exposes Rust functions to the JavaScript global object
    fn init_web_api(&self) -> Result<(), JsEngineError> {
        let doc_ref = self.document.clone();
        let timer_id_counter = self.next_timer_id.clone();
        let pending_timers = self.pending_timers.clone();
        let active_timers = self.active_timers.clone();

        self.context.with(|ctx| -> Result<(), JsEngineError> {
            let globals = ctx.globals();

            // Register the NodeHandle class prototype
            js_try(rquickjs::Class::<NodeHandle>::define(&globals), "Class::define NodeHandle")?;
            let proto = js_try(
                rquickjs::Class::<NodeHandle>::prototype(&ctx),
                "Class::prototype NodeHandle",
            )?
            .ok_or_else(|| {
                JsEngineError::WebApiInit("Class::prototype NodeHandle returned None".into())
            })?;

            let tag_name_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |This(this): This<rquickjs::Class<'_, NodeHandle>>| -> String {
                        let doc = doc_ref.borrow();
                        let node_id = this.borrow().to_node_id();
                        match doc.nodes.get(node_id) {
                            Some(crate::dom::Node::Element(data)) => data.tag_name.to_string(),
                            _ => String::new(),
                        }
                    }
                }),
                "Function _tagNameRaw",
            )?;
            js_try(
                proto.set("_tagNameRaw", tag_name_func),
                "proto _tagNameRaw",
            )?;

            let get_attr_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |This(this): This<rquickjs::Class<'_, NodeHandle>>,
                          attr: String|
                          -> Option<String> {
                        let doc = doc_ref.borrow();
                        let node_id = this.borrow().to_node_id();
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
                            for (k, v) in &data.attributes {
                                if k == &attr {
                                    return Some(v.clone());
                                }
                            }
                        }
                        None
                    }
                }),
                "Function getAttribute",
            )?;
            js_try(
                proto.set("getAttribute", get_attr_func),
                "proto getAttribute",
            )?;

            let set_attr_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |This(this): This<rquickjs::Class<'_, NodeHandle>>,
                          key: String,
                          mut value: String| {
                    if value.len() > crate::dom::MAX_ATTRIBUTE_VALUE_LEN {
                        value.truncate(crate::dom::MAX_ATTRIBUTE_VALUE_LEN);
                    }
                    let mut doc = doc_ref.borrow_mut();
                    let node_id = this.borrow().to_node_id();
                    let mut old_id = None;
                    let mut is_class = false;
                    let mut is_style = false;

                    if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
                        if key == "class" { is_class = true; }
                        else if key == "style" { is_style = true; }
                        else if key == "id" {
                            for (k, v) in &data.attributes {
                                if k == "id" {
                                    old_id = Some(v.clone());
                                    break;
                                }
                            }
                        }
                    }

                    if is_class {
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                            data.classes = value.clone();

                            // Also update attributes vector for consistency with getAttribute
                            let mut found = false;
                            for (k, v) in data.attributes.iter_mut() {
                                if k == "class" {
                                    *v = value.clone();
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                data.attributes.push(("class".to_string(), value.clone()));
                            }
                        }
                    } else if is_style {
                        let decls = crate::css::parse_inline_declarations(&value);
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                            data.cached_inline_styles = Some(decls.into_iter().map(|d| (d.name, d.value)).collect());

                            // Also update attributes vector
                            let mut found = false;
                            for (k, v) in data.attributes.iter_mut() {
                                if k == "style" {
                                    *v = value.clone();
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                data.attributes.push(("style".to_string(), value.clone()));
                            }
                        }
                    } else {
                        if key == "id" {
                            if let Some(oid) = old_id {
                                doc.id_map.remove(&oid);
                            }
                            doc.id_map.insert(value.clone(), node_id);
                        }

                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                            let mut found = false;
                            for (k, v) in data.attributes.iter_mut() {
                                if k == &key {
                                    *v = value.clone();
                                    found = true;
                                    break;
                                }
                            }
                            if !found && data.attributes.len() < crate::dom::MAX_ATTRIBUTES {
                                data.attributes.push((key, value));
                            }
                        }
                    }

                    if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                        data.styles_dirty = true;
                    }

                    doc.dirty = true;
                }
                }),
                "Function setAttribute",
            )?;
            js_try(
                proto.set("setAttribute", set_attr_func),
                "proto setAttribute",
            )?;

            let remove_child_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |This(this): This<rquickjs::Class<'_, NodeHandle>>,
                          child: rquickjs::Class<'_, NodeHandle>| {
                        let mut doc = doc_ref.borrow_mut();
                        let parent_id = this.borrow().to_node_id();
                        let child_id = child.borrow().to_node_id();
                        doc.remove_child(parent_id, child_id);
                    }
                }),
                "Function removeChild",
            )?;
            js_try(
                proto.set("removeChild", remove_child_func),
                "proto removeChild",
            )?;

            let parent_node_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |This(this): This<rquickjs::Class<'_, NodeHandle>>| -> Option<NodeHandle> {
                    let mut doc = doc_ref.borrow_mut();
                    let node_id = this.borrow().to_node_id();
                    if let Some(parent_id) = doc.parent_of(node_id) {
                        if let Some(node) = doc.nodes.get_mut(parent_id) {
                            match node {
                                crate::dom::Node::Element(d) => d.js_handles += 1,
                                crate::dom::Node::Text(d) => d.js_handles += 1,
                                crate::dom::Node::Root(d) => d.js_handles += 1,
                            }
                            return Some(NodeHandle::from_node_id(parent_id));
                        }
                    }
                    None
                }
                }),
                "Function _parentNodeRaw",
            )?;
            js_try(
                proto.set("_parentNodeRaw", parent_node_func),
                "proto _parentNodeRaw",
            )?;

            let first_child_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |This(this): This<rquickjs::Class<'_, NodeHandle>>| -> Option<NodeHandle> {
                    let mut doc = doc_ref.borrow_mut();
                    let node_id = this.borrow().to_node_id();
                    if let Some(child_id) = doc.first_child_of(node_id) {
                        if let Some(node) = doc.nodes.get_mut(child_id) {
                            match node {
                                crate::dom::Node::Element(d) => d.js_handles += 1,
                                crate::dom::Node::Text(d) => d.js_handles += 1,
                                crate::dom::Node::Root(d) => d.js_handles += 1,
                            }
                            return Some(NodeHandle::from_node_id(child_id));
                        }
                    }
                    None
                }
                }),
                "Function _firstChildRaw",
            )?;
            js_try(
                proto.set("_firstChildRaw", first_child_func),
                "proto _firstChildRaw",
            )?;

            let next_sibling_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |This(this): This<rquickjs::Class<'_, NodeHandle>>| -> Option<NodeHandle> {
                    let mut doc = doc_ref.borrow_mut();
                    let node_id = this.borrow().to_node_id();
                    if let Some(sibling_id) = doc.next_sibling_of(node_id) {
                        if let Some(node) = doc.nodes.get_mut(sibling_id) {
                            match node {
                                crate::dom::Node::Element(d) => d.js_handles += 1,
                                crate::dom::Node::Text(d) => d.js_handles += 1,
                                crate::dom::Node::Root(d) => d.js_handles += 1,
                            }
                            return Some(NodeHandle::from_node_id(sibling_id));
                        }
                    }
                    None
                }
                }),
                "Function _nextSiblingRaw",
            )?;
            js_try(
                proto.set("_nextSiblingRaw", next_sibling_func),
                "proto _nextSiblingRaw",
            )?;

            // --- console object ---
            let console_obj = js_try(rquickjs::Object::new(ctx.clone()), "console Object::new")?;

            let log_func = js_try(
                rquickjs::Function::new(ctx.clone(), |msg: String| {
                    println!("[JS console.log] {}", msg);
                }),
                "console.log",
            )?;
            js_try(console_obj.set("log", log_func), "console set log")?;

            let warn_func = js_try(
                rquickjs::Function::new(ctx.clone(), |msg: String| {
                    println!("[JS console.warn] {}", msg);
                }),
                "console.warn",
            )?;
            js_try(console_obj.set("warn", warn_func), "console set warn")?;

            let error_func = js_try(
                rquickjs::Function::new(ctx.clone(), |msg: String| {
                    println!("[JS console.error] {}", msg);
                }),
                "console.error",
            )?;
            js_try(console_obj.set("error", error_func), "console set error")?;

            js_try(globals.set("console", console_obj), "globals console")?;

            // --- document object ---
            let document_obj =
                js_try(rquickjs::Object::new(ctx.clone()), "document Object::new")?;

            // Native lookup helpers; wrapped below with JS-side identity cache
            let get_by_id_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |id: String| -> Option<NodeHandle> {
                        let mut doc = doc_ref.borrow_mut();
                        if let Some(&node_id) = doc.id_map.get(&id) {
                            if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                                data.js_handles += 1;
                                return Some(NodeHandle::from_node_id(node_id));
                            }
                        }
                        None
                    }
                }),
                "Function _getElementByIdRaw",
            )?;

            js_try(
                document_obj.set("_getElementByIdRaw", get_by_id_func),
                "document _getElementByIdRaw",
            )?;

            // querySelector: returns a NodeHandle JS object or null
            let query_selector_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |selector: String| -> Option<NodeHandle> {
                    let mut doc = doc_ref.borrow_mut();
                    let root_id = doc.root_id;

                    fn matches_selector(selector: &str, data: &crate::dom::ElementData) -> bool {
                        if selector.starts_with('.') {
                            let class_name = &selector[1..];
                            data.classes.split_whitespace().any(|c| c == class_name)
                        } else if selector.starts_with('#') {
                            let id_name = &selector[1..];
                            data.attributes
                                .iter()
                                .any(|(k, v)| &**k == "id" && v == id_name)
                        } else {
                            &*data.tag_name == selector
                        }
                    }

                    fn find_iterative(
                        doc: &crate::dom::Document,
                        root: crate::dom::NodeId,
                        selector: &str,
                    ) -> Option<crate::dom::NodeId> {
                        // Depth-first search using an explicit stack
                        let mut stack = vec![root];
                        while let Some(current) = stack.pop() {
                            if let Some(node) = doc.nodes.get(current) {
                                if let crate::dom::Node::Element(data) = node {
                                    if matches_selector(selector, data) {
                                        return Some(current);
                                    }
                                }
                                
                                // Push children in reverse order so first child is popped first
                                let mut children_to_push = Vec::new();
                                let mut child = doc.first_child_of(current);
                                while let Some(c) = child {
                                    children_to_push.push(c);
                                    child = doc.next_sibling_of(c);
                                }
                                for c in children_to_push.into_iter().rev() {
                                    stack.push(c);
                                }
                            }
                        }
                        None
                    }

                    if selector.starts_with('#') {
                        let id_name = &selector[1..];
                        if let Some(&node_id) = doc.id_map.get(id_name) {
                            if let Some(node) = doc.nodes.get_mut(node_id) {
                                match node {
                                    crate::dom::Node::Element(d) => d.js_handles += 1,
                                    crate::dom::Node::Text(d) => d.js_handles += 1,
                                    crate::dom::Node::Root(d) => d.js_handles += 1,
                                }
                                return Some(NodeHandle::from_node_id(node_id));
                            }
                        }
                        return None;
                    }

                    if let Some(node_id) = find_iterative(&doc, root_id, &selector) {
                        if let Some(node) = doc.nodes.get_mut(node_id) {
                            match node {
                                crate::dom::Node::Element(d) => d.js_handles += 1,
                                crate::dom::Node::Text(d) => d.js_handles += 1,
                                crate::dom::Node::Root(d) => d.js_handles += 1,
                            }
                        }
                        return Some(NodeHandle::from_node_id(node_id));
                    }
                    None
                }
                }),
                "Function _querySelectorRaw",
            )?;

            js_try(
                document_obj.set("_querySelectorRaw", query_selector_func),
                "document _querySelectorRaw",
            )?;

            // addEventListener is implemented via JS polyfill on the Prototype now

            // createElement: creates an unattached node, returns a NodeHandle JS object
            let create_element_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |tag_name: String| -> NodeHandle {
                        let mut doc = doc_ref.borrow_mut();
                        let safe_tag = tag_name.to_lowercase();
                        let local_name = crate::dom::LocalName::new(&safe_tag);

                        let mut data = crate::dom::ElementData::new(local_name.clone());
                        data.js_handles = 1; // Start with 1 as we return it to JS
                        let index = doc.add_node(crate::dom::Node::Element(data));
                        drop(doc);

                        NodeHandle::from_node_id(index)
                    }
                }),
                "Function _createElementRaw",
            )?;
            js_try(
                document_obj.set("_createElementRaw", create_element_func),
                "document _createElementRaw",
            )?;

            // appendChild: accepts two NodeHandle objects (no string parsing)
            let append_child_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |parent_cls: rquickjs::Class<'_, NodeHandle>,
                          child_cls: rquickjs::Class<'_, NodeHandle>| {
                        let parent_id = parent_cls.borrow().to_node_id();
                        let child_id = child_cls.borrow().to_node_id();
                        let mut doc = doc_ref.borrow_mut();
                        doc.append_child(parent_id, child_id);
                    }
                }),
                "Function appendChild",
            )?;
            js_try(
                document_obj.set("appendChild", append_child_func),
                "document appendChild",
            )?;

            // _garbageCollectNodeRaw: invoked natively by JS FinalizationRegistry
            let gc_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let doc_ref = doc_ref.clone();
                    move |node_key: rquickjs::Array<'_>| {
                        if let (Ok(idx), Ok(gen_val)) =
                            (node_key.get::<u32>(0), node_key.get::<u64>(1))
                        {
                            let node_id = NodeId::from_raw_parts(idx as usize, gen_val);
                            let mut doc = doc_ref.borrow_mut();
                            doc.try_cleanup_node(node_id);
                        }
                    }
                }),
                "Function _garbageCollectNodeRaw",
            )?;
            js_try(
                document_obj.set("_garbageCollectNodeRaw", gc_func),
                "document _garbageCollectNodeRaw",
            )?;

            js_try(globals.set("document", document_obj), "globals document")?;

            let _: () = js_try(
                ctx.eval(
                    r#"
                document.__nodeCache = new Map();
                document.__nodeRegistry = new FinalizationRegistry(key => {
                    let mapKey = BigInt(key[0]) | (BigInt(key[1]) << 32n);
                    document.__nodeCache.delete(mapKey);
                    document._garbageCollectNodeRaw(key);
                });
                document.__ephemeralRegistry = new FinalizationRegistry(key => {
                    document._garbageCollectNodeRaw(key);
                });

                document._wrapNode = function(rawNode) {
                    if (!rawNode) return null;
                    let keyPair = rawNode.__nodeKey; // [idx, gen]
                    let mapKey = BigInt(keyPair[0]) | (BigInt(keyPair[1]) << 32n);
                    let cachedRef = document.__nodeCache.get(mapKey);
                    if (cachedRef) {
                        let cachedObj = cachedRef.deref();
                        if (cachedObj) {
                            document.__ephemeralRegistry.register(rawNode, keyPair);
                            return cachedObj;
                        }
                    }
                    document.__nodeCache.set(mapKey, new WeakRef(rawNode));
                    document.__nodeRegistry.register(rawNode, keyPair);
                    return rawNode;
                };

                document.getElementById = function(id) {
                    return this._wrapNode(this._getElementByIdRaw(id));
                };
                document.querySelector = function(selector) {
                    return this._wrapNode(this._querySelectorRaw(selector));
                };
                document.createElement = function(tag) {
                    return this._wrapNode(this._createElementRaw(tag));
                };
                document.addEventListener = function(eventType, cb) {
                    this.__listeners = this.__listeners || {};
                    this.__listeners[eventType] = this.__listeners[eventType] || [];
                    this.__listeners[eventType].push(cb);
                };
                document._triggerEvent = function(keyPair, eventType) {
                    let mapKey = BigInt(keyPair[0]) | (BigInt(keyPair[1]) << 32n);
                    let cachedRef = document.__nodeCache.get(mapKey);
                    if (!cachedRef) return;
                    let target = cachedRef.deref();
                    if (!target) return;
                    if (target.__listeners && target.__listeners[eventType]) {
                        let event = { target, type: eventType };
                        for (let cb of target.__listeners[eventType]) cb(event);
                    }
                };
            "#,
                ),
                "ctx.eval bootstrap",
            )?;

            let patch_func: rquickjs::Function = js_try(
                ctx.eval(
                    r#"
                (function(proto) {
                    Object.defineProperty(proto, "parentNode", { get() { return document._wrapNode(this._parentNodeRaw()); } });
                    Object.defineProperty(proto, "firstChild", { get() { return document._wrapNode(this._firstChildRaw()); } });
                    Object.defineProperty(proto, "nextSibling", { get() { return document._wrapNode(this._nextSiblingRaw()); } });
                    Object.defineProperty(proto, "tagName", { get() { return this._tagNameRaw(); } });
                    proto.addEventListener = function(eventType, cb) {
                        this.__listeners = this.__listeners || {};
                        this.__listeners[eventType] = this.__listeners[eventType] || [];
                        this.__listeners[eventType].push(cb);
                    };
                })
                "#,
                ),
                "ctx.eval prototype patch",
            )?;
            js_try(
                patch_func.call::<_, ()>((proto.clone(),)),
                "patch_func.call",
            )?;

            // --- setTimeout with Persistent<Function> storage ---
            let set_timeout_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                let timer_id_counter = timer_id_counter.clone();
                let pending_timers = pending_timers.clone();
                let active_timers = active_timers.clone();
                move |cb: Persistent<rquickjs::Function<'static>>, delay: i32| -> u32 {
                    let timer_id = timer_id_counter.get();
                    timer_id_counter.set(timer_id + 1);

                    active_timers.borrow_mut().insert(timer_id);

                    let delay_ms = delay.max(0) as u64;
                    let fire_at = Instant::now() + std::time::Duration::from_millis(delay_ms);

                    pending_timers.borrow_mut().push(PendingTimer {
                        id: timer_id,
                        fire_at,
                        callback: cb,
                        is_interval: false,
                        delay_ms,
                    });

                    timer_id
                }
                }),
                "setTimeout",
            )?;

            let set_interval_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                let timer_id_counter = timer_id_counter.clone();
                let pending_timers = pending_timers.clone();
                let active_timers = active_timers.clone();
                move |cb: Persistent<rquickjs::Function<'static>>, delay: i32| -> u32 {
                    let timer_id = timer_id_counter.get();
                    timer_id_counter.set(timer_id + 1);

                    active_timers.borrow_mut().insert(timer_id);

                    let delay_ms = delay.max(0) as u64;
                    let fire_at = Instant::now() + std::time::Duration::from_millis(delay_ms);

                    pending_timers.borrow_mut().push(PendingTimer {
                        id: timer_id,
                        fire_at,
                        callback: cb,
                        is_interval: true,
                        delay_ms,
                    });

                    timer_id
                }
                }),
                "setInterval",
            )?;

            let clear_timer_func = js_try(
                rquickjs::Function::new(ctx.clone(), {
                    let active_timers = active_timers.clone();
                    move |id: u32| {
                        active_timers.borrow_mut().remove(&id);
                    }
                }),
                "clearTimer",
            )?;

            js_try(
                globals.set("setTimeout", set_timeout_func),
                "globals setTimeout",
            )?;
            js_try(
                globals.set("setInterval", set_interval_func),
                "globals setInterval",
            )?;
            js_try(
                globals.set("clearTimeout", clear_timer_func.clone()),
                "globals clearTimeout",
            )?;
            js_try(
                globals.set("clearInterval", clear_timer_func),
                "globals clearInterval",
            )?;
            Ok(())
        })
    }

    /// Pump the timer queue. Fires all expired timers whose delay has elapsed.
    /// Returns the number of timers that fired.
    /// The host application should call this on each iteration of its event loop.
    pub fn pump(&self) -> u32 {
        let now = Instant::now();
        let mut expired = Vec::new();
        let mut rescheduled = Vec::new();
        {
            let mut timers = self.pending_timers.borrow_mut();
            let mut active = self.active_timers.borrow_mut();
            
            // Periodic compaction to prevent memory drift from cancelled timers
            if timers.len() > 128 {
                let mut v = std::mem::take(&mut *timers).into_vec();
                v.retain(|t| active.contains(&t.id));
                *timers = std::collections::BinaryHeap::from(v);
            }

            while let Some(top) = timers.peek() {
                if top.fire_at <= now {
                    let timer = timers.pop().unwrap();
                    if !active.contains(&timer.id) {
                        continue;
                    }

                    if !timer.is_interval {
                        active.remove(&timer.id);
                    }

                    expired.push(timer.callback.clone());

                    if timer.is_interval {
                        rescheduled.push(PendingTimer {
                            id: timer.id,
                            fire_at: now + std::time::Duration::from_millis(timer.delay_ms),
                            callback: timer.callback,
                            is_interval: true,
                            delay_ms: timer.delay_ms,
                        });
                    }
                } else {
                    break;
                }
            }
        }

        let count = expired.len() as u32;
        for persistent_cb in expired {
            self.last_start_time.set(Some(Instant::now()));
            self.context.with(|ctx| {
                if let Ok(func) = persistent_cb.restore(&ctx) {
                    let _: Result<(), _> = func.call::<(), ()>(());
                }
            });
            self.last_start_time.set(None);
        }

        // Re-queue intervals
        if !rescheduled.is_empty() {
            let mut timers = self.pending_timers.borrow_mut();
            for t in rescheduled {
                timers.push(t);
            }
        }

        // Only sweep every 60 ticks to avoid blocking the event loop
        let ticks = self.pump_ticks.get() + 1;
        self.pump_ticks.set(ticks);
        if ticks >= 60 {
            // Document batched GC handle cleanup
            self.document.borrow_mut().collect_garbage();
            self.pump_ticks.set(0);
        }

        self.last_start_time.set(Some(Instant::now()));
        while let Ok(true) = self.runtime.execute_pending_job() {}
        self.last_start_time.set(None);

        count
    }

    /// Returns true if there are pending timers that haven't fired yet.
    pub fn has_pending_timers(&self) -> bool {
        let timers = self.pending_timers.borrow();
        !timers.is_empty()
    }

    /// Evaluates a JavaScript string. After successful [`JsEngine::try_new`], this is the
    /// primary API for running scripts; failures surface as [`JsEngineError::ScriptEval`].
    pub fn execute_script(&self, script: &str) -> Result<String, JsEngineError> {
        self.last_start_time.set(Some(Instant::now()));
        let res = self.context.with(|ctx| {
            let result = ctx
                .eval::<rquickjs::Value, _>(script)
                .map_err(|e| JsEngineError::ScriptEval(format!("{e:?}")))?;
            if let Ok(s) = result.get::<rquickjs::String>() {
                Ok(s.to_string().unwrap_or_else(|_| "Error formatting string".to_string()))
            } else if let Ok(i) = result.get::<i32>() {
                Ok(i.to_string())
            } else if let Ok(f) = result.get::<f64>() {
                Ok(f.to_string())
            } else if let Ok(b) = result.get::<bool>() {
                Ok(b.to_string())
            } else if result.is_undefined() {
                Ok("undefined".into())
            } else if result.is_null() {
                Ok("null".into())
            } else {
                Ok("[Object/Unsupported]".into())
            }
        });
        self.last_start_time.set(None);
        res
    }
}
