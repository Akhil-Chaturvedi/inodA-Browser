//! inoda-core: a minimal browser engine library.
//!
//! Parses HTML into an arena-based DOM with O(1) parent traversing, applies
//! CSS with specificity and combinator support, computes Flexbox/Grid layout
//! via Taffy, renders to a femtovg canvas, and exposes a native object-based
//! DOM API through an embedded QuickJS runtime.
//!
//! The engine leverages string interning for tag names and CSS property names
//! to minimize memory allocations in resource-constrained environments.
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

        // Test exposed Rust DOM API -- getElementById returns a NodeHandle with tagName
        let result2 = engine.execute_script("document.getElementById('test-id').tagName");
        assert_eq!(result2, "p");
        
        // Test querySelector -- also returns a NodeHandle with tagName
        let result3 = engine.execute_script("document.querySelector('#test-id').tagName");
        assert_eq!(result3, "p");

        // Test NodeHandle getAttribute / setAttribute
        let _ = engine.execute_script("var p = document.getElementById('test-id'); p.setAttribute('class', 'greeting');");
        let result4 = engine.execute_script("document.getElementById('test-id').getAttribute('class')");
        assert_eq!(result4, "greeting");

        // Test NodeHandle removeChild -- verify detachment from parent
        {
            let doc = engine.document.borrow();
            let body_children_before = doc.nodes.iter()
                .filter_map(|(_, n)| if let crate::dom::Node::Element(d) = n { Some(d) } else { None })
                .find(|d| &*d.tag_name == "body")
                .map(|d| d.children.len())
                .unwrap_or(0);
            drop(doc);
            
            let _ = engine.execute_script("var body2 = document.querySelector('body'); var p2 = document.getElementById('test-id'); body2.removeChild(p2);");
            
            let doc = engine.document.borrow();
            let body_children_after = doc.nodes.iter()
                .filter_map(|(_, n)| if let crate::dom::Node::Element(d) = n { Some(d) } else { None })
                .find(|d| &*d.tag_name == "body")
                .map(|d| d.children.len())
                .unwrap_or(0);
            assert_eq!(body_children_before - 1, body_children_after, "removeChild should detach child from parent");
        }
        
        let result6 = engine.execute_script("console.log('Logging works!')");
        assert_eq!(result6, "undefined"); // console.log returns undefined

        println!("Javascript execution completed successfully.");
    }

    fn find_styled_node<'a>(node: &'a crate::dom::StyledNode, doc: &crate::dom::Document, name: &str) -> Option<&'a crate::dom::StyledNode> {
        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node.node_id) {
            if &*data.tag_name == name {
                return Some(node);
            }
        }
        for child in &node.children {
            if let Some(found) = find_styled_node(child, doc, name) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn test_css_combinators() {
        let text = "<html><body><div class=\"parent\"><p><span>Text</span></p></div></body></html>";
        let doc = html::parse_html(text);
        
        let stylesheet = css::parse_stylesheet(".parent span { color: red; } .parent > span { color: blue; } p > span { font-weight: bold; }");
        let styled_tree = css::compute_styles(&doc, &stylesheet);
        
        let span = find_styled_node(&styled_tree, &doc, "span").expect("Span node should exist");
        
        // .parent span matches (Descendant) => color: red
        // .parent > span does NOT match (Child) => hasn't overwritten red with blue
        // p > span matches (Child) => font-weight: bold
        
        assert!(span.specified_values.iter().any(|(k, v)| &**k == "color" && v == "red"), "Descendant combinator failed");
        assert!(!span.specified_values.iter().any(|(k, v)| &**k == "color" && v == "blue"), "Child combinator incorrectly matched descendant");
        assert!(span.specified_values.iter().any(|(k, v)| &**k == "font-weight" && v == "bold"), "Child combinator failed");
    }
}

