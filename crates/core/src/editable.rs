//! The bits of an accessibility element the editable-text heuristics need. The app implements
//! `AccessibilityNode` for `AXUIElement`; tests use an in-memory tree.

use std::rc::Rc;

use crate::geom::{Point, Rect};

/// The bits of an accessibility element the editable-text heuristics need. The app conforms
/// `AXUIElement` to it; tests use an in-memory tree.
///
/// `parent` and `children` hand out `Rc`s so a node can be returned and compared by identity.
pub trait AccessibilityNode: Sized {
    fn role(&self) -> Option<&str>;
    fn subrole(&self) -> Option<&str>;
    /// Global CG points (top-left origin), None when the element has no geometry.
    fn frame(&self) -> Option<Rect>;
    fn parent(&self) -> Option<Rc<Self>>;
    fn children(&self) -> Vec<Rc<Self>>;
    /// Chromium marks every node inside an editable region with `AXEditableAncestor`.
    fn has_editable_ancestor_attribute(&self) -> bool;
    /// `AXValue` is settable and the element has a text selection (`AXSelectedTextRange`):
    /// how custom text views (SwiftUI, Catalyst, canvas editors) look when their role is a
    /// generic group.
    fn is_settable_text_value(&self) -> bool;
}

pub const EDITABLE_ROLES: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"];
const BOUNDARY_ROLES: &[&str] = &["AXWindow", "AXApplication", "AXWebArea", "AXSheet"];
/// Roles that can hold a settable value without being text.
const NON_TEXT_VALUE_ROLES: &[&str] = &[
    "AXSlider",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXIncrementor",
    "AXColorWell",
    "AXDisclosureTriangle",
    "AXScrollBar",
    "AXValueIndicator",
    "AXStepper",
    "AXSwitch",
];

/// Default for `max_depth` in [`editable_self_or_ancestor`].
pub const DEFAULT_MAX_DEPTH: usize = 12;
/// Default for `ancestor_levels` in [`editable_target`].
pub const DEFAULT_ANCESTOR_LEVELS: usize = 3;
/// Default for `search_depth` in [`editable_target`].
pub const DEFAULT_SEARCH_DEPTH: usize = 3;
/// Default for `node_budget` in [`editable_target`].
pub const DEFAULT_NODE_BUDGET: usize = 80;

/// Does this element itself take typed text?
pub fn is_text_input<N: AccessibilityNode>(node: &N) -> bool {
    if node.role().is_some_and(|role| EDITABLE_ROLES.contains(&role)) {
        // Secure fields take typing but never images; a paste would only be refused.
        return node.subrole() != Some("AXSecureTextField");
    }
    if node.has_editable_ancestor_attribute() {
        return true;
    }
    if node.is_settable_text_value() && !NON_TEXT_VALUE_ROLES.contains(&node.role().unwrap_or("")) {
        return true;
    }
    false
}

/// The element or one of its ancestors (up to the window / web area) takes typed text.
pub fn editable_self_or_ancestor<N: AccessibilityNode>(node: &Rc<N>) -> Option<Rc<N>> {
    editable_self_or_ancestor_with_max_depth(node, DEFAULT_MAX_DEPTH)
}

/// `editable_self_or_ancestor` with the walk bound given explicitly.
pub fn editable_self_or_ancestor_with_max_depth<N: AccessibilityNode>(
    node: &Rc<N>,
    max_depth: usize,
) -> Option<Rc<N>> {
    let mut current = Some(Rc::clone(node));
    let mut depth = 0;
    while let Some(n) = current {
        if depth > max_depth {
            break;
        }
        if is_text_input(n.as_ref()) {
            return Some(n);
        }
        if n.role().is_some_and(|role| BOUNDARY_ROLES.contains(&role)) {
            return None;
        }
        current = n.parent();
        depth += 1;
    }
    None
}

/// An editable element at `point` near the hit element: the hit element's own editable
/// ancestor, or an editable sibling/cousin whose frame contains the point. The second case is
/// the empty chat composer, whose placeholder label ("Ask anything") sits on top of the
/// text view, so hit-testing returns the label instead of the input.
pub fn editable_target<N: AccessibilityNode>(at: Point, hit: &Rc<N>) -> Option<Rc<N>> {
    editable_target_with(
        at,
        hit,
        DEFAULT_ANCESTOR_LEVELS,
        DEFAULT_SEARCH_DEPTH,
        DEFAULT_NODE_BUDGET,
    )
}

/// `editable_target` with the walk bounds given explicitly.
pub fn editable_target_with<N: AccessibilityNode>(
    at: Point,
    hit: &Rc<N>,
    ancestor_levels: usize,
    search_depth: usize,
    node_budget: usize,
) -> Option<Rc<N>> {
    if let Some(direct) = editable_self_or_ancestor(hit) {
        return Some(direct);
    }
    let mut budget = node_budget;
    let mut ancestor = hit.parent();
    for _ in 0..ancestor_levels {
        let Some(a) = ancestor else {
            break;
        };
        if a.role().is_some_and(|role| BOUNDARY_ROLES.contains(&role)) {
            break;
        }
        if let Some(found) = search_descendants(&a, at, search_depth, &mut budget) {
            return Some(found);
        }
        if budget == 0 {
            break;
        }
        ancestor = a.parent();
    }
    None
}

fn search_descendants<N: AccessibilityNode>(
    node: &Rc<N>,
    point: Point,
    depth: usize,
    budget: &mut usize,
) -> Option<Rc<N>> {
    if depth == 0 {
        return None;
    }
    for child in node.children() {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let Some(frame) = child.frame() else {
            continue;
        };
        if !frame.inset_by(-2.0, -2.0).contains(point) {
            continue;
        }
        if is_text_input(child.as_ref()) {
            return Some(child);
        }
        if let Some(found) = search_descendants(&child, point, depth - 1, budget) {
            return Some(found);
        }
    }
    None
}
