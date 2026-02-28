//! inoda-core: a minimal browser engine library.
//!
//! Parses HTML into an intrusive linked list arena-based DOM with O(1) parent traversing 
//! and zero-allocation mutations. Applies $O(1)$ CSS matching with specificity and combinator 
//! support, computes Flexbox/Grid layout via Taffy, renders through an abstract backend 
//! trait, and exposes a native object-based DOM API through an embedded QuickJS runtime.
//!
//! The engine leverages string interning for tag names and CSS property names
//! to minimize memory allocations in resource-constrained environments.
//!
//! This crate is a library. The host application must provide a window,
//! event loop, and graphics backend implementation.

pub mod css;
pub mod dom;
pub mod html;
pub mod js;
pub mod layout;
pub mod render;

pub trait ResourceLoader {
    fn fetch(&self, url: &str) -> Vec<u8>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_html() {
        let text = "<html><head><style>.container { display: flex; flex-direction: row; width: 100px; height: 50px; } .box { width: 50%; height: 100%; }</style></head><body><p class=\"container\"><span class=\"box\"></span><span class=\"box\"></span></p></body></html>";
        let doc = html::parse_html(text);

        let stylesheet = css::parse_stylesheet(
            ".container { display: flex; flex-direction: row; width: 100px; height: 50px; background-color: #222222; } .box { width: 50%; height: 100%; border-color: red; }",
        );
        let styled_tree = css::compute_styles(&doc, &stylesheet);

        let mut font_system = cosmic_text::FontSystem::new();
        let (layout_tree, root_node, _text_cache) = layout::compute_layout(&doc, &styled_tree, 320.0, 240.0, &mut font_system);
        taffy::print_tree(&layout_tree, root_node);

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

    fn find_styled_node<'a>(
        node: &'a crate::dom::StyledNode,
        doc: &crate::dom::Document,
        name: &str,
    ) -> Option<&'a crate::dom::StyledNode> {
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
    fn test_remove_node_removes_descendants() {
        let mut doc = dom::Document::new();

        let parent = doc.add_node(dom::Node::Element(dom::ElementData {
            tag_name: string_cache::DefaultAtom::from("div"),
            attributes: Vec::new(),
            classes: std::collections::HashSet::new(),
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
        }));

        let child = doc.add_node(dom::Node::Element(dom::ElementData {
            tag_name: string_cache::DefaultAtom::from("span"),
            attributes: Vec::new(),
            classes: std::collections::HashSet::new(),
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
        }));

        let grandchild = doc.add_node(dom::Node::Text(dom::TextData {
            text: "hello".to_string(),
            parent: None,
            prev_sibling: None,
            next_sibling: None,
        }));

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
        let doc = html::parse_html(text);

        let stylesheet = css::parse_stylesheet(
            ".parent span { color: red; } .parent > span { color: blue; } p > span { font-weight: bold; }",
        );
        let styled_tree = css::compute_styles(&doc, &stylesheet);

        let span = find_styled_node(&styled_tree, &doc, "span").expect("Span node should exist");

        // .parent span matches (Descendant) => color: red
        // .parent > span does NOT match (Child) => hasn't overwritten red with blue
        // p > span matches (Child) => font-weight: bold

        assert!(
            span.specified_values
                .iter()
                .any(|(k, v)| &**k == "color" && v == "red"),
            "Descendant combinator failed"
        );
        assert!(
            !span
                .specified_values
                .iter()
                .any(|(k, v)| &**k == "color" && v == "blue"),
            "Child combinator incorrectly matched descendant"
        );
        assert!(
            span.specified_values
                .iter()
                .any(|(k, v)| &**k == "font-weight" && v == "bold"),
            "Child combinator failed"
        );
    }
}
