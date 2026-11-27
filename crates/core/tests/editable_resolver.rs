use std::cell::{Cell, RefCell};
use std::rc::Rc;

use igorio_core::editable::{
    AccessibilityNode, DEFAULT_ANCESTOR_LEVELS, DEFAULT_SEARCH_DEPTH, editable_self_or_ancestor,
    editable_target, editable_target_with, is_text_input,
};
use igorio_core::geom::{Point, Rect};

/// In-memory accessibility tree: a cheap handle, like the app's wrapper around a UI Automation
/// element. Cloning it keeps the same identity, so the tests can compare results with `same`.
#[derive(Clone)]
struct Node(Rc<NodeData>);

#[derive(Default)]
struct NodeData {
    role: Option<String>,
    subrole: RefCell<Option<String>>,
    frame: Cell<Option<Rect>>,
    /// Strong on purpose: trees are built inline and must stay alive (tests only).
    parent_node: RefCell<Option<Node>>,
    kids: RefCell<Vec<Node>>,
    has_editable_ancestor_attribute: Cell<bool>,
    is_settable_text_value: Cell<bool>,
}

impl Node {
    fn new(role: Option<&str>) -> Self {
        Node(Rc::new(NodeData {
            role: role.map(str::to_string),
            ..NodeData::default()
        }))
    }

    fn with_subrole(self, subrole: &str) -> Self {
        *self.0.subrole.borrow_mut() = Some(subrole.to_string());
        self
    }

    fn with_frame(self, frame: Rect) -> Self {
        self.0.frame.set(Some(frame));
        self
    }

    fn with_editable_ancestor(self) -> Self {
        self.0.has_editable_ancestor_attribute.set(true);
        self
    }

    fn with_settable_text(self) -> Self {
        self.0.is_settable_text_value.set(true);
        self
    }

    fn with_kids(self, kids: Vec<Node>) -> Self {
        for kid in &kids {
            *kid.0.parent_node.borrow_mut() = Some(self.clone());
        }
        *self.0.kids.borrow_mut() = kids;
        self
    }

    /// The same element: pointer equality of the inner handle.
    fn same(&self, other: &Node) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl AccessibilityNode for Node {
    fn role(&self) -> Option<String> {
        self.0.role.clone()
    }

    fn subrole(&self) -> Option<String> {
        self.0.subrole.borrow().clone()
    }

    fn frame(&self) -> Option<Rect> {
        self.0.frame.get()
    }

    fn parent(&self) -> Option<Node> {
        self.0.parent_node.borrow().clone()
    }

    fn children(&self) -> Vec<Node> {
        self.0.kids.borrow().clone()
    }

    fn has_editable_ancestor_attribute(&self) -> bool {
        self.0.has_editable_ancestor_attribute.get()
    }

    fn is_settable_text_value(&self) -> bool {
        self.0.is_settable_text_value.get()
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
    // Custom-drawn and canvas editors: a group whose value is settable text.
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
    let text = Node::new(Some("AXStaticText"));
    let group = Node::new(Some("AXGroup")).with_kids(vec![text.clone()]);
    let area = Node::new(Some("AXTextArea")).with_kids(vec![group]);
    let _window = Node::new(Some("AXWindow")).with_kids(vec![area.clone()]);
    let found = editable_self_or_ancestor(&text);
    assert!(found.is_some_and(|found| found.same(&area)));
}

#[test]
fn test_ancestor_walk_stops_at_web_area() {
    let text = Node::new(Some("AXStaticText"));
    let group = Node::new(Some("AXGroup")).with_kids(vec![text.clone()]);
    let web_area = Node::new(Some("AXWebArea")).with_kids(vec![group]);
    let _area = Node::new(Some("AXTextArea")).with_kids(vec![web_area]);
    assert!(editable_self_or_ancestor(&text).is_none());
}

#[test]
fn test_placeholder_over_composer_resolves_to_sibling_text_view() {
    // The ChatGPT-style empty composer: "Ask anything" is a static text drawn on top of the
    // text view, so hit-testing returns the label.
    let composer_frame = Rect::new(400.0, 900.0, 700.0, 60.0);
    let placeholder =
        Node::new(Some("AXStaticText")).with_frame(Rect::new(412.0, 915.0, 120.0, 20.0));
    let text_view = Node::new(Some("AXTextArea")).with_frame(composer_frame);
    let attach = Node::new(Some("AXButton")).with_frame(Rect::new(360.0, 910.0, 30.0, 30.0));
    let send = Node::new(Some("AXButton")).with_frame(Rect::new(1110.0, 910.0, 30.0, 30.0));
    let composer = Node::new(Some("AXGroup"))
        .with_frame(composer_frame)
        .with_kids(vec![text_view.clone(), placeholder.clone()]);
    let group = Node::new(Some("AXGroup"))
        .with_frame(Rect::new(350.0, 890.0, 800.0, 80.0))
        .with_kids(vec![attach, composer, send]);
    let _window = Node::new(Some("AXWindow")).with_kids(vec![group]);
    let found = editable_target(Point::new(450.0, 925.0), &placeholder);
    assert!(found.is_some_and(|found| found.same(&text_view)));
}

#[test]
fn test_scroll_view_padding_around_text_view_resolves() {
    let text_view = Node::new(Some("AXTextArea")).with_frame(Rect::new(100.0, 100.0, 300.0, 40.0));
    let scroll = Node::new(Some("AXScrollArea"))
        .with_frame(Rect::new(90.0, 90.0, 320.0, 60.0))
        .with_kids(vec![text_view.clone()]);
    let group = Node::new(Some("AXGroup")).with_kids(vec![scroll.clone()]);
    let _window = Node::new(Some("AXWindow")).with_kids(vec![group]);
    // Click lands on the scroll area's inset, outside the text view: the text view does not
    // contain the point, so nothing is found by geometry…
    assert!(editable_target(Point::new(95.0, 95.0), &scroll).is_none());
    // …but inside it, it is.
    assert!(
        editable_target(Point::new(120.0, 110.0), &scroll)
            .is_some_and(|found| found.same(&text_view))
    );
}

#[test]
fn test_button_next_to_composer_does_not_resolve() {
    let text_view = Node::new(Some("AXTextArea")).with_frame(Rect::new(400.0, 900.0, 700.0, 60.0));
    let send = Node::new(Some("AXButton")).with_frame(Rect::new(1110.0, 910.0, 30.0, 30.0));
    let group = Node::new(Some("AXGroup")).with_kids(vec![text_view, send.clone()]);
    let _window = Node::new(Some("AXWindow")).with_kids(vec![group]);
    assert!(editable_target(Point::new(1120.0, 920.0), &send).is_none());
}

#[test]
fn test_search_does_not_cross_the_window() {
    let field = Node::new(Some("AXTextField")).with_frame(Rect::new(0.0, 0.0, 2000.0, 2000.0));
    let label = Node::new(Some("AXStaticText")).with_frame(Rect::new(10.0, 10.0, 10.0, 10.0));
    let first_window = Node::new(Some("AXWindow")).with_kids(vec![field]);
    let second_window = Node::new(Some("AXWindow")).with_kids(vec![label.clone()]);
    let _app = Node::new(Some("AXApplication")).with_kids(vec![first_window, second_window]);
    assert!(editable_target(Point::new(15.0, 15.0), &label).is_none());
}

#[test]
fn test_node_budget_bounds_the_search() {
    let label = Node::new(Some("AXStaticText")).with_frame(Rect::new(0.0, 0.0, 10.0, 10.0));
    let mut siblings: Vec<Node> = (0..500)
        .map(|_| Node::new(Some("AXButton")).with_frame(Rect::new(0.0, 0.0, 50.0, 50.0)))
        .collect();
    let field = Node::new(Some("AXTextArea")).with_frame(Rect::new(0.0, 0.0, 50.0, 50.0));
    siblings.push(field);
    siblings.push(label.clone());
    let group = Node::new(Some("AXGroup")).with_kids(siblings);
    let _window = Node::new(Some("AXWindow")).with_kids(vec![group]);
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
    let leaf = Node::new(Some("AXGroup"));
    let mut node = leaf.clone();
    for _ in 0..50 {
        node = Node::new(Some("AXGroup")).with_kids(vec![node]);
    }
    assert!(editable_self_or_ancestor(&leaf).is_none());
}
