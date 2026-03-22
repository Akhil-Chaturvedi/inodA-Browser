//! JavaScript execution module.
//!
//! Embeds QuickJS via `rquickjs`. Exposes a subset of the Web API:
//! - `console.log`, `console.warn`, `console.error` (print to stdout)
//! - `document.getElementById`, `document.querySelector` (return native `NodeHandle` objects)
//! - `document.createElement`, `document.appendChild` (mutate the arena DOM)
//! - `document.addEventListener` (logs registration, does not dispatch events)
//! - `setTimeout` (cooperative timer queue via `pump()`)
//!
//! DOM handles are exposed to JavaScript as native `NodeHandle` class instances
//! wrapping a `generational_arena::Index`. Methods include:
//! - `handle.tagName`
//! - `handle.getAttribute(key)`
//! - `handle.setAttribute(key, value)`
//! - `handle.removeChild(child)`
//!
//! Each `NodeHandle` carries a `__nodeKey` property: a two-element JS array
//! `[u32 index, u64 generation]`. The `__nodeCache` Map is keyed by the
//! string `"index:generation"` built from that array. A `FinalizationRegistry`
//! receives the integer array when a wrapper is GC'd and calls the native Rust
//! `_garbageCollectNodeRaw`, which reconstructs the `NodeId` from the two
//! integers without string parsing and removes the node from the arena if it
//! is detached from the DOM tree.
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
    pub tag_name: String,
}

#[rquickjs::methods]
impl NodeHandle {
    #[qjs(get)]
    #[allow(non_snake_case)]
    pub fn tagName(&self) -> String {
        self.tag_name.clone()
    }

    #[qjs(get, rename = "__nodeKey")]
    pub fn node_key<'js>(&self, ctx: rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Array<'js>> {
        let arr = rquickjs::Array::new(ctx)?;
        arr.set(0, self.index)?;
        arr.set(1, self.generation)?;
        Ok(arr)
    }
}

impl NodeHandle {
    pub fn from_node_id(id: NodeId, tag_name: String) -> Self {
        let (index, generation) = id.into_raw_parts();
        NodeHandle {
            index: index as u32,
            generation,
            tag_name,
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
    /// Track cancelled timers natively preventing runaway intervals.
    cancelled_timers: Rc<RefCell<std::collections::HashSet<u32>>>,
    /// Track iterations for deterministic QuickJS garbage collection.
    pump_ticks: Rc<Cell<u32>>,
}

impl JsEngine {
    pub fn new(document: Document) -> Self {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();

        let engine = JsEngine {
            runtime,
            context,
            document: Rc::new(RefCell::new(document)),
            next_timer_id: Rc::new(Cell::new(1)),
            pending_timers: Rc::new(RefCell::new(std::collections::BinaryHeap::new())),
            cancelled_timers: Rc::new(RefCell::new(std::collections::HashSet::new())),
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
        let cancelled_timers = self.cancelled_timers.clone();

        self.context.with(|ctx| {
            let globals = ctx.globals();

            // Register the NodeHandle class prototype
            let proto = rquickjs::Class::<NodeHandle>::prototype(&ctx)
                .unwrap()
                .unwrap();

            let get_attr_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |This(this): This<rquickjs::Class<'_, NodeHandle>>,
                      attr: String|
                      -> Option<String> {
                    let doc = doc_ref.borrow();
                    let node_id = this.borrow().to_node_id();
                    if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
                        for (k, v) in &data.attributes {
                            if &**k == attr {
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
                      attr: String,
                      value: String| {
                    let mut doc = doc_ref.borrow_mut();
                    doc.dirty = true;
                    let node_id = this.borrow().to_node_id();

                    if attr == "id" {
                        // Securely remove the old ID from the ABA mapping
                        let mut old_id_to_remove = None;
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
                            if let Some((_, old_val)) =
                                data.attributes.iter().find(|(k, _)| &**k == "id")
                            {
                                old_id_to_remove = Some(old_val.clone());
                            }
                        }
                        if let Some(old_id) = old_id_to_remove {
                            doc.id_map.remove(&old_id);
                        }

                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                            let local_attr = string_cache::DefaultAtom::from("id");
                            if let Some(pos) =
                                data.attributes.iter().position(|(k, _)| *k == local_attr)
                            {
                                data.attributes[pos].1 = value.clone();
                            } else {
                                data.attributes.push((local_attr, value.clone()));
                            }
                        }

                        doc.id_map.insert(value.clone(), node_id);
                    } else if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id)
                    {
                        let local_attr = string_cache::DefaultAtom::from(attr.as_str());
                        if let Some(pos) =
                            data.attributes.iter().position(|(k, _)| *k == local_attr)
                        {
                            data.attributes[pos].1 = value.clone();
                        } else {
                            data.attributes.push((local_attr.clone(), value.clone()));
                        }

                        if &*local_attr == "class" {
                            data.classes.clear();
                            for c in value.split_whitespace() {
                                let class_string = c.to_string();
                                if !data.classes.contains(&class_string) {
                                    data.classes.push(class_string);
                                }
                            }
                        }
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
                    let doc = doc_ref.borrow();
                    if let Some(&node_id) = doc.id_map.get(&id) {
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
                            return Some(NodeHandle::from_node_id(
                                node_id,
                                data.tag_name.to_string(),
                            ));
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
                    let doc = doc_ref.borrow();
                    for (node_id, node) in doc.nodes.iter() {
                        if let crate::dom::Node::Element(data) = node {
                            let is_match = if selector.starts_with('.') {
                                let class_name = &selector[1..];
                                data.classes.iter().any(|c| &**c == class_name)
                            } else if selector.starts_with('#') {
                                let id_name = &selector[1..];
                                data.attributes
                                    .iter()
                                    .any(|(k, v)| &**k == "id" && v == id_name)
                            } else {
                                &*data.tag_name == selector
                            };

                            if is_match {
                                return Some(NodeHandle::from_node_id(
                                    node_id,
                                    data.tag_name.to_string(),
                                ));
                            }
                        }
                    }
                    None
                }
            })
            .unwrap();

            document_obj
                .set("_querySelectorRaw", query_selector_func)
                .unwrap();

            let add_event_listener_func = rquickjs::Function::new(
                ctx.clone(),
                move |event: String, _cb: rquickjs::Function| {
                    println!("[JS addEventListener] Registered event: {}", event);
                },
            )
            .unwrap();

            document_obj
                .set("addEventListener", add_event_listener_func)
                .unwrap();

            // createElement: creates an unattached node, returns a NodeHandle JS object
            let create_element_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |tag_name: String| -> NodeHandle {
                    let mut doc = doc_ref.borrow_mut();
                    let safe_tag = tag_name.to_lowercase();
                    let local_name = crate::dom::LocalName::new(&safe_tag);

                    let node = crate::dom::Node::Element(crate::dom::ElementData {
                        tag_name: local_name.clone(),
                        attributes: Vec::new(),
                        classes: Vec::new(),
                        parent: None,
                        first_child: None,
                        last_child: None,
                        prev_sibling: None,
                        next_sibling: None,
                        computed: crate::dom::ComputedStyle::default(),
                        taffy_node: None,
                    });
                    let index = doc.add_node(node);
                    drop(doc);

                    NodeHandle::from_node_id(index, local_name.to_string())
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

                        // If the JS wrapper is garbage collected AND the node is unattached,
                        // safely wipe it from the Rust Arena freeing memory.
                        if !doc.is_attached_to_root(node_id) && node_id != doc.root_id {
                            doc.remove_node(node_id);
                        }
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
                        if (cachedObj) return cachedObj;
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
            "#,
                )
                .unwrap();

            // --- setTimeout with Persistent<Function> storage ---
            let set_timeout_func = rquickjs::Function::new(ctx.clone(), {
                let timer_id_counter = timer_id_counter.clone();
                let pending_timers = pending_timers.clone();
                move |cb: Persistent<rquickjs::Function<'static>>, delay: i32| -> u32 {
                    let timer_id = timer_id_counter.get();
                    timer_id_counter.set(timer_id + 1);

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
                move |cb: Persistent<rquickjs::Function<'static>>, delay: i32| -> u32 {
                    let timer_id = timer_id_counter.get();
                    timer_id_counter.set(timer_id + 1);

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
                let cancelled_timers = cancelled_timers.clone();
                move |id: u32| {
                    cancelled_timers.borrow_mut().insert(id);
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
            let mut cancelled = self.cancelled_timers.borrow_mut();
            while let Some(top) = timers.peek() {
                if top.fire_at <= now {
                    let timer = timers.pop().unwrap();
                    if cancelled.remove(&timer.id) {
                        continue;
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

        // Deterministic GC limit mapping
        let ticks = self.pump_ticks.get();
        if ticks > 60 {
            self.runtime.run_gc(); // Sweep abandoned closures + DOM Nodes deterministically
            self.pump_ticks.set(0);
        } else {
            self.pump_ticks.set(ticks + 1);
        }

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
