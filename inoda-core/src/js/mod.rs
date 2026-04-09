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
//! via a `_wrapNode` WeakRef cache in the JS environment. A `FinalizationRegistry`
//! receives this raw integer array when a wrapper is GC'd and calls the native
//! Rust `_garbageCollectNodeRaw`, which decrements the handle count. Detached
//! nodes are cleared from the arena by the batched `collect_garbage()` sweep.
//!
//! The Document is held behind `Rc<RefCell<Document>>` for single-threaded access.
//! All JS operations are synchronous and serialized through this lock.

use crate::dom::{Document, NodeId};
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::This;
use rquickjs::{Context, Persistent, Runtime};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

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
}

impl JsEngine {
    pub fn dispatch_event(&self, x: f32, y: f32, event_type: &str) {
        let hit = {
            let doc = self.document.borrow();
            doc.hit_test(x, y)
        };
        if let Some(node_id) = hit {
            let event_type_str = event_type.to_string();
            let _ = self.context.with(|ctx| {
                let globals = ctx.globals();
                if let Ok(doc_obj) = globals.get::<_, rquickjs::Object>("document") {
                    if let Ok(dispatch_func) = doc_obj.get::<_, rquickjs::Function>("_triggerEvent") {
                        let (idx, generation) = node_id.into_raw_parts();
                        let arr = rquickjs::Array::new(ctx.clone()).unwrap();
                        arr.set(0, idx as u64).unwrap();
                        arr.set(1, generation).unwrap();
                        let _ = dispatch_func.call::<_, ()>((arr, event_type_str));
                    }
                }
            });
        }
    }
    pub fn new(document: Document) -> Self {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();

        let engine = JsEngine {
            runtime,
            context,
            document: Rc::new(RefCell::new(document)),
            next_timer_id: Rc::new(Cell::new(1)),
            pending_timers: Rc::new(RefCell::new(std::collections::BinaryHeap::new())),
            active_timers: Rc::new(RefCell::new(std::collections::HashSet::new())),
            pump_ticks: Rc::new(Cell::new(0)),
        };

        engine.init_web_api();
        engine
    }

    /// Exposes Rust functions to the JavaScript global object
    fn init_web_api(&self) {
        let doc_ref = self.document.clone();
        let timer_id_counter = self.next_timer_id.clone();
        let pending_timers = self.pending_timers.clone();
        let active_timers = self.active_timers.clone();

        self.context.with(|ctx| {
            let globals = ctx.globals();

            // Register the NodeHandle class prototype
            rquickjs::Class::<NodeHandle>::define(&globals).unwrap();
            let proto = rquickjs::Class::<NodeHandle>::prototype(&ctx)
                .unwrap()
                .unwrap();

            let tag_name_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |This(this): This<rquickjs::Class<'_, NodeHandle>>| -> String {
                    let doc = doc_ref.borrow();
                    let node_id = this.borrow().to_node_id();
                    match doc.nodes.get(node_id) {
                        Some(crate::dom::Node::Element(data)) => data.tag_name.to_string(),
                        _ => String::new(),
                    }
                }
            })
            .unwrap();
            proto.set("_tagNameRaw", tag_name_func).unwrap();

            let get_attr_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();
            proto.set("getAttribute", get_attr_func).unwrap();

            let set_attr_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |This(this): This<rquickjs::Class<'_, NodeHandle>>,
                      key: String,
                      value: String| {
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

                    let mut needs_style_recompute = false;
                    if is_class {
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                            data.classes = value.clone();
                            needs_style_recompute = true;
                            
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
                            needs_style_recompute = true;

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

                    if needs_style_recompute {
                        doc.styles_dirty = true;
                    }
                }
            })
            .unwrap();
            proto.set("setAttribute", set_attr_func).unwrap();

            let remove_child_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |This(this): This<rquickjs::Class<'_, NodeHandle>>,
                      child: rquickjs::Class<'_, NodeHandle>| {
                    let mut doc = doc_ref.borrow_mut();
                    let parent_id = this.borrow().to_node_id();
                    let child_id = child.borrow().to_node_id();
                    doc.remove_child(parent_id, child_id);
                }
            })
            .unwrap();
            proto.set("removeChild", remove_child_func).unwrap();

            let parent_node_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();
            proto.set("_parentNodeRaw", parent_node_func).unwrap();

            let first_child_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();
            proto.set("_firstChildRaw", first_child_func).unwrap();

            let next_sibling_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();
            proto.set("_nextSiblingRaw", next_sibling_func).unwrap();

            // --- console object ---
            let console_obj = rquickjs::Object::new(ctx.clone()).unwrap();

            let log_func = rquickjs::Function::new(ctx.clone(), |msg: String| {
                println!("[JS console.log] {}", msg);
            })
            .unwrap();
            console_obj.set("log", log_func).unwrap();

            let warn_func = rquickjs::Function::new(ctx.clone(), |msg: String| {
                println!("[JS console.warn] {}", msg);
            })
            .unwrap();
            console_obj.set("warn", warn_func).unwrap();

            let error_func = rquickjs::Function::new(ctx.clone(), |msg: String| {
                println!("[JS console.error] {}", msg);
            })
            .unwrap();
            console_obj.set("error", error_func).unwrap();

            globals.set("console", console_obj).unwrap();

            // --- document object ---
            let document_obj = rquickjs::Object::new(ctx.clone()).unwrap();

            // Native lookup helpers; wrapped below with JS-side identity cache
            let get_by_id_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();

            document_obj
                .set("_getElementByIdRaw", get_by_id_func)
                .unwrap();

            // querySelector: returns a NodeHandle JS object or null
            let query_selector_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();

            document_obj
                .set("_querySelectorRaw", query_selector_func)
                .unwrap();

            // addEventListener is implemented via JS polyfill on the Prototype now

            // createElement: creates an unattached node, returns a NodeHandle JS object
            let create_element_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();
            document_obj
                .set("_createElementRaw", create_element_func)
                .unwrap();

            // appendChild: accepts two NodeHandle objects (no string parsing)
            let append_child_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |parent_cls: rquickjs::Class<'_, NodeHandle>,
                      child_cls: rquickjs::Class<'_, NodeHandle>| {
                    let parent_id = parent_cls.borrow().to_node_id();
                    let child_id = child_cls.borrow().to_node_id();
                    let mut doc = doc_ref.borrow_mut();
                    doc.append_child(parent_id, child_id);
                }
            })
            .unwrap();
            document_obj.set("appendChild", append_child_func).unwrap();

            // _garbageCollectNodeRaw: invoked natively by JS FinalizationRegistry
            let gc_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |node_key: rquickjs::Array<'_>| {
                    if let (Ok(idx), Ok(gen_val)) = (node_key.get::<u32>(0), node_key.get::<u64>(1))
                    {
                        let node_id = NodeId::from_raw_parts(idx as usize, gen_val);
                        let mut doc = doc_ref.borrow_mut();
                        doc.try_cleanup_node(node_id);
                    }
                }
            })
            .unwrap();
            document_obj.set("_garbageCollectNodeRaw", gc_func).unwrap();

            globals.set("document", document_obj).unwrap();

            let _: () = ctx
                .eval(
                    r#"
                document.__nodeCache = new Map();
                document.__nodeRegistry = new FinalizationRegistry(key => {
                    let mapKey = BigInt(key[0]) | (BigInt(key[1]) << 32n);
                    document.__nodeCache.delete(mapKey);
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
                            // Leak fix: manually decrement Rust refcount for the discarded new handle
                            document._garbageCollectNodeRaw(keyPair);
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
                )
                .unwrap();

            let patch_func: rquickjs::Function = ctx.eval(
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
            ).unwrap();
            let _: () = patch_func.call((proto.clone(),)).unwrap();

            // --- setTimeout with Persistent<Function> storage ---
            let set_timeout_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();

            let set_interval_func = rquickjs::Function::new(ctx.clone(), {
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
            })
            .unwrap();

            let clear_timer_func = rquickjs::Function::new(ctx.clone(), {
                let active_timers = active_timers.clone();
                move |id: u32| {
                    active_timers.borrow_mut().remove(&id);
                }
            })
            .unwrap();

            globals.set("setTimeout", set_timeout_func).unwrap();
            globals.set("setInterval", set_interval_func).unwrap();
            globals
                .set("clearTimeout", clear_timer_func.clone())
                .unwrap();
            globals.set("clearInterval", clear_timer_func).unwrap();
        });
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

            for t in rescheduled {
                timers.push(t);
            }
        }

        let count = expired.len() as u32;
        for persistent_cb in expired {
            self.context.with(|ctx| {
                if let Ok(func) = persistent_cb.restore(&ctx) {
                    let _: Result<(), _> = func.call::<(), ()>(());
                }
            });
        }

        // Only sweep every 60 ticks to avoid blocking the event loop
        let ticks = self.pump_ticks.get() + 1;
        self.pump_ticks.set(ticks);
        if ticks >= 60 {
            // Document batched GC handle cleanup
            self.document.borrow_mut().collect_garbage();
            self.pump_ticks.set(0);
        }

        while let Ok(true) = self.runtime.execute_pending_job() {}

        count
    }

    /// Returns true if there are pending timers that haven't fired yet.
    pub fn has_pending_timers(&self) -> bool {
        let timers = self.pending_timers.borrow();
        !timers.is_empty()
    }

    /// Evaluates a JavaScript string and returns any string result or errors
    pub fn execute_script(&self, script: &str) -> String {
        self.context
            .with(|ctx| match ctx.eval::<rquickjs::Value, _>(script) {
                Ok(result) => {
                    if let Ok(s) = result.get::<rquickjs::String>() {
                        s.to_string()
                            .unwrap_or_else(|_| "Error formatting string".to_string())
                    } else if let Ok(i) = result.get::<i32>() {
                        i.to_string()
                    } else if let Ok(f) = result.get::<f64>() {
                        f.to_string()
                    } else if let Ok(b) = result.get::<bool>() {
                        b.to_string()
                    } else if result.is_undefined() {
                        "undefined".to_string()
                    } else if result.is_null() {
                        "null".to_string()
                    } else {
                        "[Object/Unsupported]".to_string()
                    }
                }
                Err(e) => format!("JS Error: {:?}", e),
            })
    }
}
