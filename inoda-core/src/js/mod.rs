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
//! The Document is held behind `Rc<RefCell<Document>>` for single-threaded access.
//! All JS operations are synchronous and serialized through this lock.

use crate::dom::Document;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::This;
use rquickjs::{Context, Persistent, Runtime};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

// ---------------------------------------------------------------------------
// NodeHandle: an opaque JS class wrapping a generational_arena::Index.
// ---------------------------------------------------------------------------

/// A DOM node handle exposed to JavaScript as a native class.
/// Stores the raw parts of a `generational_arena::Index` to avoid
/// string serialization on every DOM operation.
pub struct NodeHandle {
    arena_index: usize,
    arena_generation: u64,
}

impl NodeHandle {
    pub fn from_node_id(id: crate::dom::NodeId) -> Self {
        let (idx, generation) = id.into_raw_parts();
        NodeHandle {
            arena_index: idx,
            arena_generation: generation,
        }
    }

    pub fn to_node_id(&self) -> crate::dom::NodeId {
        generational_arena::Index::from_raw_parts(self.arena_index, self.arena_generation)
    }
}

impl<'js> Trace<'js> for NodeHandle {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {
        // No JS values to trace; NodeHandle only contains plain integers.
    }
}

unsafe impl<'js> rquickjs::JsLifetime<'js> for NodeHandle {
    type Changed<'to> = NodeHandle;
}

impl<'js> JsClass<'js> for NodeHandle {
    const NAME: &'static str = "NodeHandle";
    type Mutable = Readable;

    fn prototype(ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<Option<rquickjs::Object<'js>>> {
        let proto = rquickjs::Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(
        _ctx: &rquickjs::Ctx<'js>,
    ) -> rquickjs::Result<Option<rquickjs::function::Constructor<'js>>> {
        // NodeHandle cannot be constructed from JS -- only from Rust.
        Ok(None)
    }
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

/// A pending timer entry storing a persistent JS callback.
struct PendingTimer {
    #[allow(dead_code)]
    id: u32,
    fire_at: Instant,
    callback: Persistent<rquickjs::Function<'static>>,
}

/// Wrapper around the QuickJS Runtime and Context.
pub struct JsEngine {
    #[allow(dead_code)]
    runtime: Runtime,
    context: Context,
    pub document: Rc<RefCell<Document>>,
    /// Monotonically increasing timer ID counter.
    next_timer_id: Rc<Cell<u32>>,
    /// List of pending timers waiting to fire.
    pending_timers: Rc<RefCell<Vec<PendingTimer>>>,
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
            pending_timers: Rc::new(RefCell::new(Vec::new())),
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
                    if let Some(crate::dom::Node::Element(data)) = doc.nodes.get_mut(node_id) {
                        let local_attr = markup5ever::LocalName::from(attr.as_str());
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
                                data.classes.insert(markup5ever::LocalName::from(c));
                            }
                        }
                    }
                    if attr == "id" {
                        doc.id_map.insert(value.clone(), node_id);
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
                            let is_match = (selector.starts_with('.')
                                && data.attributes.iter().any(|(k, v)| {
                                    &**k == "class" && format!(".{}", v) == selector
                                }))
                                || (selector.starts_with('#')
                                    && data.attributes.iter().any(|(k, v)| {
                                        &**k == "id" && format!("#{}", v) == selector
                                    }))
                                || (&*data.tag_name == selector);

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

            // createElement: returns a NodeHandle JS object
            let create_element_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |tag_name: String| -> NodeHandleWithTag {
                    let mut doc = doc_ref.borrow_mut();
                    let tag_name_clone = tag_name.clone();
                    let node = crate::dom::Node::Element(crate::dom::ElementData {
                        tag_name: markup5ever::LocalName::from(tag_name.as_str()),
                        attributes: Vec::new(),
                        classes: std::collections::HashSet::new(),
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
                        tag_name: tag_name_clone,
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

            globals.set("document", document_obj).unwrap();

            let _: () = ctx
                .eval(
                    r#"
                document.getElementById = function(id) {
                    return this._getElementByIdRaw(id);
                };
                document.querySelector = function(selector) {
                    return this._querySelectorRaw(selector);
                };
                document.createElement = function(tag) {
                    return this._createElementRaw(tag);
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
        let expired: Vec<Persistent<rquickjs::Function<'static>>>;
        {
            let mut timers = self.pending_timers.borrow_mut();
            let (ready, remaining): (Vec<_>, Vec<_>) =
                timers.drain(..).partition(|t| t.fire_at <= now);
            expired = ready.into_iter().map(|t| t.callback).collect();
            *timers = remaining;
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
