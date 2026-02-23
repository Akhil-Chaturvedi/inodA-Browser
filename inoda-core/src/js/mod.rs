//! JavaScript execution module.
//!
//! Embeds QuickJS via `rquickjs`. Exposes a subset of the Web API:
//! - `console.log`, `console.warn`, `console.error` (print to stdout)
//! - `document.getElementById`, `document.querySelector` (return tag name strings, not DOM objects)
//! - `document.createElement`, `document.appendChild` (mutate the arena DOM)
//! - `document.addEventListener` (logs registration, does not dispatch events)
//! - `setTimeout` (executes callback synchronously, no event loop)
//!
//! The Document is held behind `Arc<Mutex<>>` for shared access from JS closures.

use rquickjs::{Context, Runtime};
use std::sync::{Arc, Mutex};
use crate::dom::Document;

/// Wrapper around the QuickJS Runtime and Context.
pub struct JsEngine {
    #[allow(dead_code)] // Stored to keep the QuickJS Context alive
    runtime: Runtime,
    context: Context,
    // Store a thread-safe reference to the current DOM document
    // so that injected JS functions can manipulate it.
    pub document: Arc<Mutex<Document>>,
}

impl JsEngine {
    pub fn new(document: Document) -> Self {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        
        let engine = JsEngine {
            runtime,
            context,
            document: Arc::new(Mutex::new(document)),
        };

        engine.init_web_api();
        engine
    }

    /// Exposes Rust functions to the JavaScript global object
    fn init_web_api(&self) {
        let doc_ref = self.document.clone();
        
        self.context.with(|ctx| {
            let globals = ctx.globals();

            // Ensure the console object exists
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

            // Ensure the document object exists wrapper
            let document_obj = rquickjs::Object::new(ctx.clone()).unwrap();

            let get_by_id_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |id: String| -> Option<String> {
                    let doc = doc_ref.lock().unwrap();
                    for node in &doc.nodes {
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
                    for node in &doc.nodes {
                        if let crate::dom::Node::Element(data) = node {
                            // Import simple selector logic or duplicate basic check
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
                // Warning: In a real browser, this goes into a DOM event registration array attached to the targeted NodeId.
                // For this engine scaffold, we acknowledge the event registration globally.
                println!("[JS addEventListener] Registered event: {}", event);
            }).unwrap();

            document_obj.set("addEventListener", add_event_listener_func).unwrap();

            let create_element_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |tag_name: String| -> i32 {
                    let mut doc = doc_ref.lock().unwrap();
                    let node = crate::dom::Node::Element(crate::dom::ElementData {
                        tag_name,
                        attributes: Vec::new(),
                        children: Vec::new(),
                    });
                    doc.add_node(node) as i32 // Return the pointer ID to JS
                }
            }).unwrap();
            document_obj.set("createElement", create_element_func).unwrap();

            let append_child_func = rquickjs::Function::new(ctx.clone(), {
                let doc_ref = doc_ref.clone();
                move |parent_id: i32, child_id: i32| {
                    let mut doc = doc_ref.lock().unwrap();
                    doc.append_child(parent_id as usize, child_id as usize);
                }
            }).unwrap();
            document_obj.set("appendChild", append_child_func).unwrap();

            globals.set("document", document_obj).unwrap();

            // SetTimeout Polyfill (synchronous immediate call for now as a scaffold)
            let set_timeout_func = rquickjs::Function::new(ctx.clone(), move |cb: rquickjs::Function, _delay: i32| {
                // Warning: In a real browser, this goes into a Tokio event loop.
                // For this scaffold, we just execute the callback synchronously.
                let _ = cb.call::<(), ()>(());
            }).unwrap();

            globals.set("setTimeout", set_timeout_func).unwrap();
        });
    }

    /// Evaluates a JavaScript string and returns any string result or errors
    pub fn execute_script(&self, script: &str) -> String {
        self.context.with(|ctx| {
            match ctx.eval::<rquickjs::Value, _>(script) {
                Ok(result) => {
                    // Try to safely coerce any JS return value to a Rust String
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
