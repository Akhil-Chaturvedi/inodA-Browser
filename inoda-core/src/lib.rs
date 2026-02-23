//! inoda-core: a minimal browser engine library.
//!
//! Parses HTML into an arena-based DOM, applies CSS with specificity and
//! inheritance, computes Flexbox/Grid layout via Taffy, renders to a femtovg
//! canvas, and exposes a subset of the Web API through an embedded QuickJS
//! runtime.
//!
//! This crate is a library. The host application must provide a window,
//! OpenGL context, event loop, and font registration.

pub mod dom;
pub mod html;
pub mod css;
pub mod layout;
pub mod render;
pub mod js;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_html() {
        let text = "<html><head><style>.container { display: flex; flex-direction: row; width: 100px; height: 50px; } .box { width: 50%; height: 100%; }</style></head><body><p class=\"container\"><span class=\"box\"></span><span class=\"box\"></span></p></body></html>";
        let doc = html::parse_html(text);
        
        let stylesheet = css::parse_stylesheet(".container { display: flex; flex-direction: row; width: 100px; height: 50px; background-color: #222222; } .box { width: 50%; height: 100%; border-color: red; }");
        let styled_tree = css::compute_styles(&doc, &stylesheet);
        
        let (layout_tree, root_node) = layout::compute_layout(&doc, &styled_tree, 320.0, 240.0);
        taffy::print_tree(&layout_tree, root_node);

        // Test Renderer Bridge Compile
        // Note: actually executing femtovg requires an OpenGL/WebGL context.
        // For testing the bridge algorithm itself without a Window, we just verify
        // that the layout computation succeeded. Actual pixel rendering will be
        // done in the application binary holding the Window.

        println!("Render tree layout computation completed successfully.");
    }

    #[test]
    fn test_javascript_bridge() {
        let text = "<html><body><p id=\"test-id\">Hello inodA</p></body></html>";
        let doc = html::parse_html(text);

        // Initialize engine and transfer document ownership
        let engine = js::JsEngine::new(doc);

        // Test standard JS
        let result = engine.execute_script("1 + 1");
        assert_eq!(result, "2");

        // Test exposed Rust DOM API
        let result2 = engine.execute_script("document.getElementById('test-id')");
        assert_eq!(result2, "p"); // Should return the tag name we coded in the bridge
        
        // Test querySelector
        let result3 = engine.execute_script("document.querySelector('#test-id')");
        assert_eq!(result3, "p");
        
        let result4 = engine.execute_script("console.log('Logging works!')");
        assert_eq!(result4, "undefined"); // console.log returns undefined

        println!("Javascript execution completed successfully.");
    }
}

