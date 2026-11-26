use std::cell::RefCell;
use std::rc::Rc;

use igorio_core::editable::{
    AccessibilityNode, DEFAULT_ANCESTOR_LEVELS, DEFAULT_SEARCH_DEPTH, editable_self_or_ancestor,
    editable_target, editable_target_with, is_text_input,
};
use igorio_core::geom::{Point, Rect};

/// In-memory accessibility tree.
#[derive(Default)]
struct Node {
    role: Option<String>,
    subrole: Option<String>,
    frame: Option<Rect>,
    /// Strong on purpose: trees are built inline and must stay alive (tests only).
    parent_node: RefCell<Option<Rc<Node>>>,
    kids: Vec<Rc<Node>>,
    has_editable_ancestor_attribute: bool,
    is_settable_text_value: bool,
}

impl Node {
    fn new(role: Option<&str>) -> Self {
        Node {
            role: role.map(str::to_string),
            ..Node::default()
        }
    }

    fn with_subrole(mut self, subrole: &str) -> Self {
        self.subrole = Some(subrole.to_string());
        self
    }

    fn with_frame(mut self, frame: Rect) -> Self {
        self.frame = Some(frame);
        self
    }

    fn with_editable_ancestor(mut self) -> Self {
        self.has_editable_ancestor_attribute = true;
        self
    }

    fn with_settable_text(mut self) -> Self {
        self.is_settable_text_value = true;
        self
    }

    fn with_kids(mut self, kids: Vec<Rc<Node>>) -> Self {
        self.kids = kids;
        self
    }

    fn into_rc(self) -> Rc<Node> {
        let node = Rc::new(self);
        for kid in &node.kids {
            *kid.parent_node.borrow_mut() = Some(Rc::clone(&node));
        }
        node
    }
}

impl AccessibilityNode for Node {
    fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    fn subrole(&self) -> Option<&str> {
        self.subrole.as_deref()
    }

    fn frame(&self) -> Option<Rect> {
        self.frame
    }

    fn parent(&self) -> Option<Rc<Node>> {
        self.parent_node.borrow().as_ref().cloned()
    }

    fn children(&self) -> Vec<Rc<Node>> {
        self.kids.clone()
    }

    fn has_editable_ancestor_attribute(&self) -> bool {
        self.has_editable_ancestor_attribute
    }

    fn is_settable_text_value(&self) -> bool {
        self.is_settable_text_value
    }
}

#[test]
fn test_native_roles() {
    for role in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
        assert!(is_text_input(&Node::new(Some(role))), "{role}");
    }
    assert!(!is_text_input(&Node::new(Some("AXButton"))));
    assert!(!is_text_input(&Node::new(Some("AXStaticText"))));
}

#[test]
fn test_secure_field_is_not_a_paste_target() {
    assert!(!is_text_input(
        &Node::new(Some("AXTextField")).with_subrole("AXSecureTextField")
    ));
}

#[test]
fn test_chromium_editable_ancestor_attribute() {
    assert!(is_text_input(
        &Node::new(Some("AXGroup")).with_editable_ancestor()
    ));
}

#[test]
fn test_custom_text_view_with_generic_role() {
    // SwiftUI/Catalyst/canvas editors: a group whose value is settable text.
    assert!(is_text_input(
        &Node::new(Some("AXGroup")).with_settable_text()
    ));
    assert!(is_text_input(&Node::new(None).with_settable_text()));
}

#[test]
fn test_settable_non_text_controls_are_not_text() {
    assert!(!is_text_input(
        &Node::new(Some("AXSlider")).with_settable_text()
    ));
    assert!(!is_text_input(
        &Node::new(Some("AXCheckBox")).with_settable_text()
    ));
}

#[test]
fn test_static_text_inside_text_area_resolves_to_ancestor() {
    let text = Node::new(Some("AXStaticText")).into_rc();
    let group = Node::new(Some("AXGroup"))
        .with_kids(vec![Rc::clone(&text)])
        .into_rc();
    let area = Node::new(Some("AXTextArea"))
        .with_kids(vec![group])
        .into_rc();
    let _window = Node::new(Some("AXWindow"))
        .with_kids(vec![Rc::clone(&area)])
        .into_rc();
    let found = editable_self_or_ancestor(&text);
    assert!(found.is_some_and(|found| Rc::ptr_eq(&found, &area)));
}

#[test]
fn test_ancestor_walk_stops_at_web_area() {
    let text = Node::new(Some("AXStaticText")).into_rc();
    let group = Node::new(Some("AXGroup"))
        .with_kids(vec![Rc::clone(&text)])
        .into_rc();
    let web_area = Node::new(Some("AXWebArea")).with_kids(vec![group]).into_rc();
    let _area = Node::new(Some("AXTextArea"))
        .with_kids(vec![web_area])
        .into_rc();
    assert!(editable_self_or_ancestor(&text).is_none());
}

#[test]
fn test_placeholder_over_composer_resolves_to_sibling_text_view() {
    // The ChatGPT-style empty composer: "Ask anything" is a static text drawn on top of the
    // text view, so hit-testing returns the label.
    let composer_frame = Rect::new(400.0, 900.0, 700.0, 60.0);
    let placeholder = Node::new(Some("AXStaticText"))
        .with_frame(Rect::new(412.0, 915.0, 120.0, 20.0))
        .into_rc();
    let text_view = Node::new(Some("AXTextArea"))
        .with_frame(composer_frame)
        .into_rc();
    let attach = Node::new(Some("AXButton"))
        .with_frame(Rect::new(360.0, 910.0, 30.0, 30.0))
        .into_rc();
    let send = Node::new(Some("AXButton"))
        .with_frame(Rect::new(1110.0, 910.0, 30.0, 30.0))
        .into_rc();
    let composer = Node::new(Some("AXGroup"))
        .with_frame(composer_frame)
        .with_kids(vec![Rc::clone(&text_view), Rc::clone(&placeholder)])
        .into_rc();
    let group = Node::new(Some("AXGroup"))
        .with_frame(Rect::new(350.0, 890.0, 800.0, 80.0))
        .with_kids(vec![attach, composer, send])
        .into_rc();
    let _window = Node::new(Some("AXWindow"))
        .with_kids(vec![group])
        .into_rc();
    let found = editable_target(Point::new(450.0, 925.0), &placeholder);
    assert!(found.is_some_and(|found| Rc::ptr_eq(&found, &text_view)));
}

#[test]
fn test_scroll_view_padding_around_text_view_resolves() {
    let text_view = Node::new(Some("AXTextArea"))
        .with_frame(Rect::new(100.0, 100.0, 300.0, 40.0))
        .into_rc();
    let scroll = Node::new(Some("AXScrollArea"))
        .with_frame(Rect::new(90.0, 90.0, 320.0, 60.0))
        .with_kids(vec![Rc::clone(&text_view)])
        .into_rc();
    let group = Node::new(Some("AXGroup"))
        .with_kids(vec![Rc::clone(&scroll)])
        .into_rc();
    let _window = Node::new(Some("AXWindow"))
        .with_kids(vec![group])
        .into_rc();
    // Click lands on the scroll area's inset, outside the text view: the text view does not
    // contain the point, so nothing is found by geometry…
    assert!(editable_target(Point::new(95.0, 95.0), &scroll).is_none());
    // …but inside it, it is.
    assert!(
        editable_target(Point::new(120.0, 110.0), &scroll)
            .is_some_and(|found| Rc::ptr_eq(&found, &text_view))
    );
}

#[test]
fn test_button_next_to_composer_does_not_resolve() {
    let text_view = Node::new(Some("AXTextArea"))
        .with_frame(Rect::new(400.0, 900.0, 700.0, 60.0))
        .into_rc();
    let send = Node::new(Some("AXButton"))
        .with_frame(Rect::new(1110.0, 910.0, 30.0, 30.0))
        .into_rc();
    let group = Node::new(Some("AXGroup"))
        .with_kids(vec![text_view, Rc::clone(&send)])
        .into_rc();
    let _window = Node::new(Some("AXWindow"))
        .with_kids(vec![group])
        .into_rc();
    assert!(editable_target(Point::new(1120.0, 920.0), &send).is_none());
}

#[test]
fn test_search_does_not_cross_the_window() {
    let field = Node::new(Some("AXTextField"))
        .with_frame(Rect::new(0.0, 0.0, 2000.0, 2000.0))
        .into_rc();
    let label = Node::new(Some("AXStaticText"))
        .with_frame(Rect::new(10.0, 10.0, 10.0, 10.0))
        .into_rc();
    let first_window = Node::new(Some("AXWindow"))
        .with_kids(vec![field])
        .into_rc();
    let second_window = Node::new(Some("AXWindow"))
        .with_kids(vec![Rc::clone(&label)])
        .into_rc();
    let _app = Node::new(Some("AXApplication"))
        .with_kids(vec![first_window, second_window])
        .into_rc();
    assert!(editable_target(Point::new(15.0, 15.0), &label).is_none());
}

#[test]
fn test_node_budget_bounds_the_search() {
    let label = Node::new(Some("AXStaticText"))
        .with_frame(Rect::new(0.0, 0.0, 10.0, 10.0))
        .into_rc();
    let mut siblings: Vec<Rc<Node>> = (0..500)
        .map(|_| {
            Node::new(Some("AXButton"))
                .with_frame(Rect::new(0.0, 0.0, 50.0, 50.0))
                .into_rc()
        })
        .collect();
    let field = Node::new(Some("AXTextArea"))
        .with_frame(Rect::new(0.0, 0.0, 50.0, 50.0))
        .into_rc();
    siblings.push(field);
    siblings.push(Rc::clone(&label));
    let group = Node::new(Some("AXGroup")).with_kids(siblings).into_rc();
    let _window = Node::new(Some("AXWindow"))
        .with_kids(vec![group])
        .into_rc();
    assert!(
        editable_target_with(
            Point::new(5.0, 5.0),
            &label,
            DEFAULT_ANCESTOR_LEVELS,
            DEFAULT_SEARCH_DEPTH,
            80,
        )
        .is_none()
    );
}

#[test]
fn test_cycle_safe_ancestor_walk() {
    // Depth-bounded even on a very deep tree.
    let leaf = Node::new(Some("AXGroup")).into_rc();
    let mut node = Rc::clone(&leaf);
    for _ in 0..50 {
        node = Node::new(Some("AXGroup")).with_kids(vec![node]).into_rc();
    }
    assert!(editable_self_or_ancestor(&leaf).is_none());
}
