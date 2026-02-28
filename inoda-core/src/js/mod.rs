//! JavaScript execution module.
//!
//! Embeds QuickJS via `rquickjs`. Exposes a subset of the Web API:
//! - `console.log`, `console.warn`, `console.error` (print to stdout)
//! - `document.getElementById`, `document.querySelector` (return native `NodeHandle` objects globally cached via `__nodeCache` to explicitly preserve `===` identity)
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

pub struct NodeHandleWithTag {
    handle: NodeHandle,
    tag_name: String,
    node_key: String,
}

impl<'js> rquickjs::IntoJs<'js> for NodeHandleWithTag {
    fn into_js(self, ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
        let cls = rquickjs::Class::instance(ctx.clone(), self.handle)?;
        cls.set("tagName", self.tag_name)?;
        cls.set("__nodeKey", self.node_key)?;
        cls.into_js(ctx)
    }
}

// ---------------------------------------------------------------------------
// Timer queue
// ---------------------------------------------------------------------------

use std::cmp::Ordering;

/// A pending timer entry storing a persistent JS callback.
struct PendingTimer {
    #[allow(dead_code)]
    id: u32,
    fire_at: Instant,
    callback: Persistent<rquickjs::Function<'static>>,
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
        other.fire_at.cmp(&self.fire_at).then_with(|| other.id.cmp(&self.id))
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
        };

        engine.init_web_api();
        engine
    }

    /// Exposes Rust functions to the JavaScript global object
    fn init_web_api(&self) {
        let doc_ref = self.document.clone();
        let timer_id_counter = self.next_timer_id.clone();
        let pending_timers = self.pending_timers.clone();

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
                    let node_id = this.borrow().to_node_id();
                    
                    if attr == "id" {
                        // Securely remove the old ID from the ABA mapping
                        let mut old_id_to_remove = None;
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
                            if let Some((_, old_val)) = data.attributes.iter().find(|(k, _)| &**k == "id") {
                                old_id_to_remove = Some(old_val.clone());
                            }
                        }
                        if let Some(old_id) = old_id_to_remove {
                            doc.id_map.remove(&old_id);
                        }
                        
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                            let local_attr = string_cache::DefaultAtom::from("id");
                            if let Some(pos) = data.attributes.iter().position(|(k, _)| *k == local_attr) {
                                data.attributes[pos].1 = value.clone();
                            } else {
                                data.attributes.push((local_attr, value.clone()));
                            }
                        }

                        doc.id_map.insert(value.clone(), node_id);
                    } else if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
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
                                let class_atom = string_cache::DefaultAtom::from(c);
                                if !data.classes.contains(&class_atom) {
                                    data.classes.push(class_atom);
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
                move |id: String| -> Option<NodeHandleWithTag> {
                    let doc = doc_ref.borrow();
                    if let Some(&node_id) = doc.id_map.get(&id) {
                        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
                            return Some(NodeHandleWithTag {
                                handle: NodeHandle::from_node_id(node_id),
                                tag_name: data.tag_name.to_string(),
                                node_key: format!(
                                    "{}:{}",
                                    node_id.into_raw_parts().0,
                                    node_id.into_raw_parts().1
                                ),
                            });
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
                move |selector: String| -> Option<NodeHandleWithTag> {
                    let doc = doc_ref.borrow();
                    for (node_id, node) in doc.nodes.iter() {
                        if let crate::dom::Node::Element(data) = node {
                            let is_match = if selector.starts_with('.') {
                                let class_name = &selector[1..];
                                data.classes.iter().any(|c| &**c == class_name)
                            } else if selector.starts_with('#') {
                                let id_name = &selector[1..];
                                data.attributes.iter().any(|(k, v)| &**k == "id" && v == id_name)
                            } else {
                                &*data.tag_name == selector
                            };

                            if is_match {
                                return Some(NodeHandleWithTag {
                                    handle: NodeHandle::from_node_id(node_id),
                                    tag_name: data.tag_name.to_string(),
                                    node_key: format!(
                                        "{}:{}",
                                        node_id.into_raw_parts().0,
                                        node_id.into_raw_parts().1
                                    ),
                                });
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
                move |tag_name: String| -> NodeHandleWithTag {
                    let mut doc = doc_ref.borrow_mut();
                    let safe_tag = tag_name.to_lowercase();
                    
                    let is_html5_tag = matches!(
                        safe_tag.as_str(),
                        "a" | "abbr" | "address" | "area" | "article" | "aside" | "audio" | "b" | "base" | "bdi" | "bdo" | "blockquote" | "body" | "br" | "button" | "canvas" | "caption" | "cite" | "code" | "col" | "colgroup" | "data" | "datalist" | "dd" | "del" | "details" | "dfn" | "dialog" | "div" | "dl" | "dt" | "em" | "embed" | "fieldset" | "figcaption" | "figure" | "footer" | "form" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "head" | "header" | "hr" | "html" | "i" | "iframe" | "img" | "input" | "ins" | "kbd" | "label" | "legend" | "li" | "link" | "main" | "map" | "mark" | "meta" | "meter" | "nav" | "noscript" | "object" | "ol" | "optgroup" | "option" | "output" | "p" | "param" | "picture" | "pre" | "progress" | "q" | "rp" | "rt" | "ruby" | "s" | "samp" | "script" | "section" | "select" | "small" | "source" | "span" | "strong" | "style" | "sub" | "summary" | "sup" | "table" | "tbody" | "td" | "template" | "textarea" | "tfoot" | "th" | "thead" | "time" | "title" | "tr" | "track" | "u" | "ul" | "var" | "video" | "wbr"
                    );

                    let atom = if is_html5_tag {
                        string_cache::DefaultAtom::from(safe_tag.as_str())
                    } else {
                        string_cache::DefaultAtom::from("div") // Fallback for invalid tags to prevent OOM
                    };

                    let node = crate::dom::Node::Element(crate::dom::ElementData {
                        tag_name: atom.clone(),
                        attributes: Vec::new(),
                        classes: Vec::new(),
                        parent: None,
                        first_child: None,
                        last_child: None,
                        prev_sibling: None,
                        next_sibling: None,
                    });
                    let index = doc.add_node(node);
                    drop(doc);

                    NodeHandleWithTag {
                        handle: NodeHandle::from_node_id(index),
                        tag_name: atom.to_string(),
                        node_key: format!(
                            "{}:{}",
                            index.into_raw_parts().0,
                            index.into_raw_parts().1
                        ),
                    }
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
                move |node_key: String| {
                    if let Some((idx_str, gen_str)) = node_key.split_once(':') {
                        if let (Ok(idx), Ok(gen_val)) = (idx_str.parse::<usize>(), gen_str.parse::<u64>()) {
                            let node_id = NodeId::from_raw_parts(idx, gen_val);
                            let mut doc = doc_ref.borrow_mut();
                            
                            // If the JS wrapper is garbage collected AND the node is unattached,
                            // safely wipe it from the Rust Arena freeing memory.
                            if !doc.is_attached_to_root(node_id) && node_id != doc.root_id {
                                doc.remove_node(node_id);
                            }
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
                    document.__nodeCache.delete(key);
                    document._garbageCollectNodeRaw(key);
                });

                document._wrapNode = function(rawNode) {
                    if (!rawNode) return null;
                    let key = rawNode.__nodeKey;
                    let cachedRef = document.__nodeCache.get(key);
                    if (cachedRef) {
                        let cachedObj = cachedRef.deref();
                        if (cachedObj) return cachedObj;
                    }
                    document.__nodeCache.set(key, new WeakRef(rawNode));
                    document.__nodeRegistry.register(rawNode, key);
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
                move |cb: Persistent<rquickjs::Function<'static>>, delay: i32| -> i32 {
                    let timer_id = timer_id_counter.get();
                    timer_id_counter.set(timer_id + 1);

                    let fire_at =
                        Instant::now() + std::time::Duration::from_millis(delay.max(0) as u64);
                    let mut timers = pending_timers.borrow_mut();
                    timers.push(PendingTimer {
                        id: timer_id,
                        fire_at,
                        callback: cb,
                    });

                    timer_id as i32
                }
            })
            .unwrap();

            globals.set("setTimeout", set_timeout_func).unwrap();
        });
    }

    /// Pump the timer queue. Fires all expired timers whose delay has elapsed.
    /// Returns the number of timers that fired.
    /// The host application should call this on each iteration of its event loop.
    pub fn pump(&self) -> u32 {
        let now = Instant::now();
        let mut expired = Vec::new();
        {
            let mut timers = self.pending_timers.borrow_mut();
            while let Some(top) = timers.peek() {
                if top.fire_at <= now {
                    let timer = timers.pop().unwrap();
                    expired.push(timer.callback);
                } else {
                    break;
                }
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
