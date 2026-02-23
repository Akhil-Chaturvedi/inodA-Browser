//! JavaScript execution module.
//!
//! Embeds QuickJS via `rquickjs`. Exposes a subset of the Web API:
//! - `console.log`, `console.warn`, `console.error` (print to stdout)
//! - `document.getElementById`, `document.querySelector` (return tag name strings, not DOM objects)
//! - `document.createElement`, `document.appendChild` (mutate the arena DOM)
//! - `document.addEventListener` (logs registration, does not dispatch events)
//! - `setTimeout` (cooperative timer queue via `pump()`)
//!
//! The Document is held behind `Arc<Mutex<>>` for shared access from JS closures.
//! Timer callbacks are stored in a JS-side registry and dispatched by the host
//! via `JsEngine::pump()`.

use rquickjs::{Context, Runtime, Persistent};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use crate::dom::Document;

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
    pub document: Arc<Mutex<Document>>,
    /// Monotonically increasing timer ID counter.
    next_timer_id: Arc<Mutex<u32>>,
    /// List of pending timers waiting to fire.
    pending_timers: Arc<Mutex<Vec<PendingTimer>>>,
}

impl JsEngine {
    pub fn new(document: Document) -> Self {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        
        let engine = JsEngine {
            runtime,
            context,
            document: Arc::new(Mutex::new(document)),
            next_timer_id: Arc::new(Mutex::new(1)),
            pending_timers: Arc::new(Mutex::new(Vec::new())),
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

            // --- console object ---
            let console_obj = rquickjs::Object::new(ctx.clone()).unwrap();

            let log_func = rquickjs::Function::new(ctx.clone(), |msg: String| {
                println!("[JS console.log] {}", msg);
            }).unwrap();
            console_obj.set("log", log_func).unwrap();

            let warn_func = rquickjs::Function::new(ctx.clone(), |msg: String| {
                println!("[JS console.warn] {}", msg);
            }).unwrap();
            console_obj.set("warn", warn_func).unwrap();

            let error_func = rquickjs::Function::new(ctx.clone(), |msg: String| {
                println!("[JS console.error] {}", msg);
            }).unwrap();
            console_obj.set("error", error_func).unwrap();

            globals.set("console", console_obj).unwrap();

            // --- document object ---
            let document_obj = rquickjs::Object::new(ctx.clone()).unwrap();

            let get_by_id_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |id: String| -> Option<String> {
                    let doc = doc_ref.lock().unwrap();
                    for (_, node) in doc.nodes.iter() {
                        if let crate::dom::Node::Element(data) = node {
                            if data.attributes.iter().find(|(k, _)| k == "id").map(|(_, v)| v) == Some(&id) {
                                return Some(data.tag_name.clone());
                            }
                        }
                    }
                    None
                }
            }).unwrap();

            document_obj.set("getElementById", get_by_id_func).unwrap();

            let query_selector_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |selector: String| -> Option<String> {
                    let doc = doc_ref.lock().unwrap();
                    for (_, node) in doc.nodes.iter() {
                        if let crate::dom::Node::Element(data) = node {
                            let is_match = 
                               (selector.starts_with('.') && data.attributes.iter().find(|(k, _)| k == "class").map(|(_, v)| format!(".{}", v)) == Some(selector.clone())) ||
                               (selector.starts_with('#') && data.attributes.iter().find(|(k, _)| k == "id").map(|(_, v)| format!("#{}", v)) == Some(selector.clone())) ||
                               (data.tag_name == selector);
                               
                            if is_match {
                                return Some(data.tag_name.clone());
                            }
                        }
                    }
                    None
                }
            }).unwrap();
            
            document_obj.set("querySelector", query_selector_func).unwrap();

            let add_event_listener_func = rquickjs::Function::new(ctx.clone(), move |event: String, _cb: rquickjs::Function| {
                println!("[JS addEventListener] Registered event: {}", event);
            }).unwrap();

            document_obj.set("addEventListener", add_event_listener_func).unwrap();

            let create_element_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |tag_name: String| -> String {
                    let mut doc = doc_ref.lock().unwrap();
                    let node = crate::dom::Node::Element(crate::dom::ElementData {
                        tag_name,
                        attributes: Vec::new(),
                        children: Vec::new(),
                    });
                    let index = doc.add_node(node);
                    let (idx, generation) = index.into_raw_parts();
                    format!("{},{}", idx, generation)
                }
            }).unwrap();
            document_obj.set("createElement", create_element_func).unwrap();

            let append_child_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |parent_handle: String, child_handle: String| {
                    let parse_handle = |s: &str| -> Option<generational_arena::Index> {
                        let parts: Vec<&str> = s.split(',').collect();
                        if parts.len() == 2 {
                            let idx = parts[0].parse::<usize>().ok()?;
                            let generation = parts[1].parse::<u64>().ok()?;
                            Some(generational_arena::Index::from_raw_parts(idx, generation))
                        } else {
                            None
                        }
                    };
                    if let (Some(parent_id), Some(child_id)) = (parse_handle(&parent_handle), parse_handle(&child_handle)) {
                        let mut doc = doc_ref.lock().unwrap();
                        doc.append_child(parent_id, child_id);
                    }
                }
            }).unwrap();
            document_obj.set("appendChild", append_child_func).unwrap();

            globals.set("document", document_obj).unwrap();

            // --- setTimeout with Persistent<Function> storage ---
            let set_timeout_func = rquickjs::Function::new(ctx.clone(), {
                let timer_id_counter = timer_id_counter.clone();
                let pending_timers = pending_timers.clone();
                move |cb: Persistent<rquickjs::Function<'static>>, delay: i32| -> i32 {
                    let mut id_counter = timer_id_counter.lock().unwrap();
                    let timer_id = *id_counter;
                    *id_counter += 1;

                    let fire_at = Instant::now() + std::time::Duration::from_millis(delay.max(0) as u64);
                    let mut timers = pending_timers.lock().unwrap();
                    timers.push(PendingTimer { id: timer_id, fire_at, callback: cb });

                    timer_id as i32
                }
            }).unwrap();

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
            let mut timers = self.pending_timers.lock().unwrap();
            let (ready, remaining): (Vec<_>, Vec<_>) = timers.drain(..).partition(|t| t.fire_at <= now);
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
        let timers = self.pending_timers.lock().unwrap();
        !timers.is_empty()
    }

    /// Evaluates a JavaScript string and returns any string result or errors
    pub fn execute_script(&self, script: &str) -> String {
        self.context.with(|ctx| {
            match ctx.eval::<rquickjs::Value, _>(script) {
                Ok(result) => {
                    if let Ok(s) = result.get::<rquickjs::String>() {
                        s.to_string().unwrap_or_else(|_| "Error formatting string".to_string())
                    } else if let Ok(i) = result.get::<i32>() {
                        i.to_string()
                    } else if let Ok(f) = result.get::<f64>() {
                        f.to_string()
                    } else if result.is_undefined() {
                        "undefined".to_string()
                    } else if result.is_null() {
                        "null".to_string()
                    } else {
                        "[Object/Unsupported]".to_string()
                    }
                },
                Err(e) => format!("JS Error: {:?}", e),
            }
        })
    }
}
