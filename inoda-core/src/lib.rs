//! inoda-core: a minimal browser engine library.
//!
//! pointers for O(1) traversal. Applies CSS with specificity-based matching
//! and combinator support, computes Flexbox/Grid layout via Taffy, renders
//! through an abstract backend trait, and exposes a DOM API through an
//! embedded QuickJS runtime.
//!
//! Attribute keys and values are stored as `String` to ensure OOM safety and
//! deterministic memory reclamation. For security, a limit of 32 attributes
//! per element is enforced. CSS property resolution uses a fixed-size array
//! mapping for O(1) performance, with computed styles stored inline to
//! prioritize L1 cache locality.
//!
//! This crate is a library. The host application must provide a window,
//! event loop, and graphics backend.

pub mod css;
pub mod dom;
pub mod html;
pub mod js;
pub mod layout;
pub mod render;

pub trait ResourceLoader {
    fn fetch(&self, url: &str) -> Vec<u8>;
    fn fetch_image(&self, _url: &str) -> Option<(u32, u32, Vec<u8>)> { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_html() {
        let text = "<html><head><style>.container { display: flex; flex-direction: row; width: 100px; height: 50px; } .box { width: 50%; height: 100%; }</style></head><body><p class=\"container\"><span class=\"box\"></span><span class=\"box\"></span></p></body></html>";
        let mut doc = html::parse_html(text);

        let stylesheet = css::parse_stylesheet(
            ".container { display: flex; flex-direction: row; width: 100px; height: 50px; background-color: #222222; } .box { width: 50%; height: 100%; border-color: red; }",
        );
        css::compute_styles(&mut doc, &stylesheet);

        let mut font_system = cosmic_text::FontSystem::new();
        let mut buffer_cache = std::collections::HashMap::new();
        let (root_node, _text_cache) =
            layout::compute_layout(&mut doc, 320.0, 240.0, &mut font_system, &mut buffer_cache);
        taffy::print_tree(&doc.taffy_tree, root_node);

        // Test Renderer Bridge Compile
        // For testing the bridge algorithm itself without a concrete backend, we just verify
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
        let _ = engine.execute_script(
            "var p = document.getElementById('test-id'); p.setAttribute('class', 'greeting');",
        );
        let result4 =
            engine.execute_script("document.getElementById('test-id').getAttribute('class')");
        assert_eq!(result4, "greeting");

        // Test NodeHandle removeChild -- verify detachment from parent
        {
            let doc = engine.document.borrow();
            let body_children_before = doc
                .nodes
                .iter()
                .filter_map(|(_, n)| {
                    if let crate::dom::Node::Element(d) = n {
                        Some(d)
                    } else {
                        None
                    }
                })
                .find(|d| &*d.tag_name == "body")
                .map(|d| {
                    let mut count = 0;
                    let mut child = d.first_child;
                    while let Some(c) = child {
                        count += 1;
                        child = doc.next_sibling_of(c);
                    }
                    count
                })
                .unwrap_or(0);
            drop(doc);

            let _ = engine.execute_script("var body2 = document.querySelector('body'); var p2 = document.getElementById('test-id'); body2.removeChild(p2);");

            let doc = engine.document.borrow();
            let body_children_after = doc
                .nodes
                .iter()
                .filter_map(|(_, n)| {
                    if let crate::dom::Node::Element(d) = n {
                        Some(d)
                    } else {
                        None
                    }
                })
                .find(|d| &*d.tag_name == "body")
                .map(|d| {
                    let mut count = 0;
                    let mut child = d.first_child;
                    while let Some(c) = child {
                        count += 1;
                        child = doc.next_sibling_of(c);
                    }
                    count
                })
                .unwrap_or(0);
            assert_eq!(
                body_children_before - 1,
                body_children_after,
                "removeChild should detach child from parent"
            );
        }

        let result6 = engine.execute_script("console.log('Logging works!')");
        assert_eq!(result6, "undefined"); // console.log returns undefined

        println!("Javascript execution completed successfully.");
    }

    fn find_node(doc: &crate::dom::Document, name: &str) -> Option<crate::dom::NodeId> {
        doc.nodes.iter().find_map(|(id, node)| {
            if let crate::dom::Node::Element(data) = node {
                if &*data.tag_name == name {
                    return Some(id);
                }
            }
            None
        })
    }

    #[test]
    fn test_remove_node_removes_descendants() {
        let mut doc = dom::Document::new();

        let parent = doc.add_node(dom::Node::Element(dom::ElementData::new(
            dom::LocalName::Standard(string_cache::DefaultAtom::from("div")),
        )));

        let child = doc.add_node(dom::Node::Element(dom::ElementData::new(
            dom::LocalName::Standard(string_cache::DefaultAtom::from("span")),
        )));

        let grandchild = doc.add_node(dom::Node::Text(dom::TextData::new("hello".to_string())));

        doc.append_child(doc.root_id, parent);
        doc.append_child(parent, child);
        doc.append_child(child, grandchild);

        doc.remove_node(parent);

        assert!(doc.nodes.get(parent).is_none());
        assert!(doc.nodes.get(child).is_none());
        assert!(doc.nodes.get(grandchild).is_none());
    }

    #[test]
    fn test_html_keeps_inline_whitespace_text_nodes() {
        let doc = html::parse_html("<div><span>A</span> <span>B</span></div>");

        let div_id = doc
            .nodes
            .iter()
            .find_map(|(id, n)| {
                if let dom::Node::Element(d) = n {
                    if &*d.tag_name == "div" {
                        return Some(id);
                    }
                }
                None
            })
            .expect("div should exist");

        let mut has_whitespace_text = false;
        let mut child = doc.first_child_of(div_id);
        while let Some(c) = child {
            if matches!(doc.nodes.get(c), Some(dom::Node::Text(t)) if t.text.trim().is_empty()) {
                has_whitespace_text = true;
                break;
            }
            child = doc.next_sibling_of(c);
        }

        assert!(
            has_whitespace_text,
            "Whitespace text node between inline elements should be preserved"
        );
    }

    #[test]
    fn test_css_combinators() {
        let text = "<html><body><div class=\"parent\"><p><span>Text</span></p></div></body></html>";
        let mut doc = html::parse_html(text);

        let stylesheet = css::parse_stylesheet(
            ".parent span { color: red; } .parent > span { color: blue; } p > span { font-weight: bold; }",
        );
        css::compute_styles(&mut doc, &stylesheet);

        let span_id = find_node(&doc, "span").expect("Span node should exist");
        let span_computed = match doc.nodes.get(span_id).unwrap() {
            crate::dom::Node::Element(d) => &d.computed,
            crate::dom::Node::Text(_) => panic!("Expected element"),
            crate::dom::Node::Root(_) => panic!("Expected element"),
        };

        // .parent span matches (Descendant) => color: red
        // .parent > span does NOT match (Child) => hasn't overwritten red with blue
        // p > span matches (Child) => font-weight: bold

        assert_eq!(
            span_computed.color,
            (255, 0, 0, 255),
            "Descendant combinator failed"
        );

        // Since ComputedStyle currently tracks color manually, let's just make sure
        // the color from the child combinator didn't cascade since it shouldn't match.
        assert_ne!(
            span_computed.color,
            (0, 0, 255, 255),
            "Child combinator incorrectly matched descendant"
        );
    }

    #[test]
    fn test_attribute_value_length_cap() {
        use crate::dom::MAX_ATTRIBUTE_VALUE_LEN;
        
        // 1. Test HTML parser truncation
        let mut large_val = String::with_capacity(MAX_ATTRIBUTE_VALUE_LEN + 100);
        for _ in 0..(MAX_ATTRIBUTE_VALUE_LEN + 100) { large_val.push('a'); }
        
        let html = format!("<div class='{}'></div>", large_val);
        let doc = html::parse_html(&html);
        let div_id = find_node(&doc, "div").unwrap();
        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(div_id) {
            assert_eq!(data.classes.len(), MAX_ATTRIBUTE_VALUE_LEN);
            assert!(data.classes.chars().all(|c| c == 'a'));
        }

        // 2. Test JS bridge truncation
        let engine = js::JsEngine::new(doc);
        engine.execute_script(&format!("
            let div = document.querySelector('div');
            let large = 'b'.repeat({}); 
            div.setAttribute('data-test', large);
        ", 20000));

        let doc_final = engine.document.borrow();
        if let Some(crate::dom::Node::Element(data)) = doc_final.nodes.get(div_id) {
            for (k, v) in &data.attributes {
                if k == "data-test" {
                    assert_eq!(v.len(), MAX_ATTRIBUTE_VALUE_LEN);
                    assert!(v.chars().all(|c| c == 'b'));
                }
            }
        }
    }

    #[test]
    fn test_js_infinite_loop_interruption() {
        let doc = html::parse_html("<div></div>");
        let engine = js::JsEngine::new(doc);
        
        // This should trigger the 500ms interrupt handler
        let res = engine.execute_script("while(true) {}");
        
        assert!(
            res.contains("interrupted") || res.contains("JS Error"), 
            "Infinite loop should be interrupted. Got: {}", res
        );
    }

    #[test]
    fn test_unrecognized_property_discard() {
        let mut doc = html::parse_html("<div style='width: 100px; position: absolute; margin: 10px;'></div>");
        // Apply styles (including inline)
        let stylesheet = css::StyleSheet::default();
        css::compute_styles(&mut doc, &stylesheet);
        
        let node_id = doc.first_child_of(doc.root_id).unwrap();
        
        if let Some(crate::dom::Node::Element(data)) = doc.nodes.get(node_id) {
            // width should be 100px
            assert_eq!(data.computed.width, crate::dom::StyleValue::LengthPx(100.0));
            // margin should be 10px
            assert_eq!(data.computed.margin[0], crate::dom::StyleValue::LengthPx(10.0));
            
            // Unrecognized 'position' should not be in cached_inline_styles
            if let Some(inline) = &data.cached_inline_styles {
                for (name, _) in inline {
                    assert_ne!(format!("{:?}", name), "LineHeight", "Should not have collided with LineHeight fallback");
                }
            }
        } else {
            panic!("Node not found");
        }
    }

    #[test]
    fn test_inline_display_normalization() {
        let mut doc = html::parse_html("<span style='display: inline'></span>");
        let stylesheet = css::StyleSheet::default();
        css::compute_styles(&mut doc, &stylesheet);
        
        let mut font_system = cosmic_text::FontSystem::new();
        let mut buffer_cache = std::collections::HashMap::new();
        
        let (root_taffy_node, _) = layout::compute_layout(&mut doc, 800.0, 600.0, &mut font_system, &mut buffer_cache);
        
        println!("Taffy Tree for doc.root_id:");
        taffy::print_tree(&doc.taffy_tree, root_taffy_node);
        
        let children = doc.taffy_tree.children(root_taffy_node).unwrap();
        assert!(!children.is_empty(), "Root should have children in Taffy");
        let span_taffy = children[0];
        let style = doc.taffy_tree.style(span_taffy).unwrap();
        
        // Should be normalized to Block
        assert_eq!(style.display, taffy::style::Display::Block);
    }
}
