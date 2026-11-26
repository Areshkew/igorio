//! What a double-click while carrying screenshots should do. Pure: the caller gathers the facts
//! (from the cursor event and accessibility) and acts on the answer.

use crate::geom::{Point, Rect};

/// The facts a double-click while carrying screenshots is decided from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoubleClick {
    pub is_holding: bool,
    /// The system screenshot UI is up (the fairy is hidden).
    pub hidden_for_capture: bool,
    /// A delivery is already in progress.
    pub busy: bool,
    pub click_count: i32,
    /// The element under the cursor belongs to Fairy Cursor itself (a note): notes handle
    /// their own double-click.
    pub target_is_own_process: bool,
    pub is_finder: bool,
    pub finder_drop_spot: bool,
    /// The element under the cursor (or its editable ancestor) takes typed text.
    pub editable_under_cursor: bool,
    /// A terminal that did not answer the accessibility lookup in time (busy printing): its
    /// window is one big text view, so paste rather than drop the gesture.
    pub silent_terminal: bool,
}

impl Default for DoubleClick {
    fn default() -> Self {
        Self {
            is_holding: false,
            hidden_for_capture: false,
            busy: false,
            click_count: 2,
            target_is_own_process: false,
            is_finder: false,
            finder_drop_spot: false,
            editable_under_cursor: false,
            silent_terminal: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Ignore,
    SaveToFinder,
    Paste,
    /// Not recognisably a text input yet: after the mouse-up, ask the app what got focus
    /// (custom composers, placeholder overlays, web views that expose inputs late).
    CheckFocusAfterMouseUp,
}

pub fn decide(c: &DoubleClick) -> Action {
    if !(c.is_holding
        && !c.hidden_for_capture
        && !c.busy
        && c.click_count == 2
        && !c.target_is_own_process)
    {
        return Action::Ignore;
    }
    if c.is_finder {
        return if c.finder_drop_spot {
            Action::SaveToFinder
        } else {
            Action::Ignore
        };
    }
    if c.editable_under_cursor || c.silent_terminal {
        Action::Paste
    } else {
        Action::CheckFocusAfterMouseUp
    }
}

/// After the mouse-up, no text input was found but the spot looks like a canvas (a slide, a
/// Figma board): paste there unless typing is visibly going somewhere else, in which case
/// the ⌘V would land in that field instead of on the canvas.
pub fn confirm_canvas_drop(
    point: Point,
    canvas_spot: bool,
    known_canvas_app: bool,
    focused_editable: bool,
    focused_frame: Option<Rect>,
) -> bool {
    let _ = point; // kept for symmetry with the caller; the geometry is the focused field's
    if !canvas_spot {
        return false;
    }
    // Canvas apps keep an invisible text field focused to catch typing; trust them.
    if known_canvas_app || !focused_editable {
        return true;
    }
    let Some(frame) = focused_frame else {
        return true;
    };
    !is_visible_field(&frame)
}

/// After the mouse-up: is there a text input to paste into at `point`?
/// - hit_editable: the element now under the cursor is editable (web inputs that appeared late).
/// - focused: the app's focused element, if editable, and its frame (global CG points).
/// - nearby_editable: an editable element found next to the hit element whose frame contains
///   the point (a composer hidden under its placeholder label).
pub fn confirm_after_mouse_up(
    point: Point,
    hit_editable: bool,
    focused_editable: bool,
    focused_frame: Option<Rect>,
    nearby_editable: bool,
) -> bool {
    if hit_editable || nearby_editable {
        return true;
    }
    // Focus alone is not enough: a double-click on a toolbar must not paste into a text
    // field elsewhere in the window. The click has to be inside the focused field.
    let inside_focused = focused_frame.is_some_and(|f| f.inset_by(-6.0, -6.0).contains(point));
    focused_editable && inside_focused
}

/// Canvases: slides, whiteboards and design tools take a pasted image anywhere on their surface,
/// but have no text input to recognise. Pure: the caller reads the accessibility facts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Spot {
    pub role: Option<String>,
    /// Nearest first, up to the window.
    pub ancestor_roles: Vec<String>,
    pub frame: Option<Rect>,
    /// The web page the element is in (None for native views).
    pub web_area_frame: Option<Rect>,
    pub url: Option<String>,
    pub bundle_id: Option<String>,
}

/// Controls and lists: a double-click there means something else (open, rename, zoom).
pub const CONTROL_ROLES: &[&str] = &[
    "AXButton",
    "AXLink",
    "AXMenuItem",
    "AXMenuBarItem",
    "AXMenuButton",
    "AXPopUpButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXSlider",
    "AXScrollBar",
    "AXTabGroup",
    "AXToolbar",
    "AXDisclosureTriangle",
    "AXIncrementor",
    "AXColorWell",
    "AXCell",
    "AXRow",
    "AXColumn",
    "AXHeading",
    "AXStaticText",
    "AXWindow",
];
pub const CONTROL_ANCESTOR_ROLES: &[&str] = &[
    "AXToolbar",
    "AXMenuBar",
    "AXMenu",
    "AXTabGroup",
    "AXButton",
    "AXLink",
    "AXPopover",
    "AXSheet",
    "AXOutline",
    "AXTable",
    "AXList",
    "AXRow",
    "AXCell",
];
/// What a canvas surface looks like in a known canvas app.
pub const SURFACE_ROLES: &[&str] = &[
    "AXLayoutArea",
    "AXScrollArea",
    "AXGroup",
    "AXImage",
    "AXUnknown",
    "AXCanvas",
    "AXWebArea",
    "AXSplitGroup",
];

pub const CANVAS_BUNDLE_PREFIXES: &[&str] = &[
    "com.figma.",
    "com.apple.iWork.Keynote",
    "com.apple.iWork.Pages",
    "com.apple.Keynote",
    "com.apple.Pages",
    "com.microsoft.Powerpoint",
    "com.apple.freeform",
    "com.electron.realtimeboard",
    "com.miro.",
    "com.canva.",
    "com.bohemiancoding.sketch",
    "com.adobe.illustrator",
    "com.adobe.Photoshop",
    "com.seriflabs.",
    "com.pixelmatorteam.",
    "com.lukilabs.lukiapp",
    "com.tldraw.",
    "com.pitch.",
    "com.omnigroup.OmniGraffle",
];
pub const CANVAS_HOSTS: &[&str] = &[
    "figma.com",
    "miro.com",
    "canva.com",
    "excalidraw.com",
    "tldraw.com",
    "whimsical.com",
    "lucid.app",
    "app.diagrams.net",
    "pitch.com",
    "jamboard.google.com",
    "framer.com",
    "penpot.app",
];
/// Google's editors draw the page themselves and catch typing in a hidden field.
pub const CANVAS_URL_PREFIXES: &[&str] =
    &["docs.google.com/presentation", "docs.google.com/drawings"];
/// Text documents drawn the same way. They are found like a canvas but pasted into like a
/// text field: the double-click selected a word, so the selection is collapsed first and
/// the image lands after that word instead of replacing it.
pub const DOCUMENT_URL_PREFIXES: &[&str] = &["docs.google.com/document"];

pub fn is_page_drawn_document(url: Option<&str>) -> bool {
    let Some(key) = url.and_then(host_and_path) else {
        return false;
    };
    starts_with_any(&key, DOCUMENT_URL_PREFIXES)
}

pub fn is_known_canvas_app(bundle_id: Option<&str>, url: Option<&str>) -> bool {
    if bundle_id.is_some_and(|bundle| starts_with_any(bundle, CANVAS_BUNDLE_PREFIXES)) {
        return true;
    }
    let Some(key) = url.and_then(host_and_path) else {
        return false;
    };
    let host = key
        .split('/')
        .find(|piece| !piece.is_empty())
        .unwrap_or(key.as_str());
    if CANVAS_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    {
        return true;
    }
    starts_with_any(&key, CANVAS_URL_PREFIXES) || starts_with_any(&key, DOCUMENT_URL_PREFIXES)
}

/// "https://www.figma.com/design/x?y" → "figma.com/design/x".
fn host_and_path(url: &str) -> Option<String> {
    let scheme = url.find("://")?;
    let mut rest = &url[scheme + 3..];
    if let Some(cut) = rest.find(['?', '#']) {
        rest = &rest[..cut];
    }
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_lowercase())
    }
}

fn starts_with_any(value: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().copied().any(|p| value.starts_with(p))
}

pub fn is_canvas(s: &Spot) -> bool {
    let Some(role) = s.role.as_deref() else {
        return false;
    };
    if CONTROL_ROLES.contains(&role) {
        return false;
    }
    if s.ancestor_roles
        .iter()
        .any(|a| contains_role(CONTROL_ANCESTOR_ROLES, a))
    {
        return false;
    }
    if is_known_canvas_app(s.bundle_id.as_deref(), s.url.as_deref()) {
        return SURFACE_ROLES.contains(&role);
    }
    // Anywhere else: a web page's <canvas> (browsers call it an image) that fills most of
    // the page, or a native layout area (how drawing surfaces describe themselves).
    if role == "AXLayoutArea" {
        return true;
    }
    if (role == "AXImage" || role == "AXCanvas")
        && let (Some(frame), Some(page)) = (s.frame, s.web_area_frame)
    {
        let Some(visible) = frame.intersection(&page) else {
            return false;
        };
        if !(page.width > 0.0 && page.height > 0.0) {
            return false;
        }
        return visible.width * visible.height >= 0.5 * page.width * page.height;
    }
    false
}

/// A focused field the user can see (as opposed to the tiny or off-screen one a canvas app
/// uses to catch typing).
pub fn is_visible_field(frame: &Rect) -> bool {
    frame.width >= 24.0 && frame.height >= 10.0 && frame.max_x() > 0.0 && frame.max_y() > 0.0
}

/// Roles that are the empty background of a folder view (or the desktop) rather than an item
/// (which the double-click would open). `AXWindow` is deliberately absent: a double-click on
/// a window's title bar zooms the window and must not dump the pile into the folder.
pub const CONTAINER_ROLES: &[&str] = &[
    "AXScrollArea",
    "AXGroup",
    "AXList",
    "AXOutline",
    "AXTable",
    "AXBrowser",
    "AXSplitGroup",
    "AXLayoutArea",
    "AXGrid",
];
/// Anywhere inside these is window chrome, not a folder view.
pub const CHROME_ANCESTOR_ROLES: &[&str] = &[
    "AXToolbar",
    "AXTabGroupBar",
    "AXMenuBar",
    "AXMenu",
    "AXSheet",
    "AXPopover",
];

fn contains_role(roles: &[&str], role: &str) -> bool {
    roles.contains(&role)
}

pub fn is_drop_spot(role: Option<&str>, ancestor_roles: &[String]) -> bool {
    let Some(role) = role else {
        return false;
    };
    if !CONTAINER_ROLES.contains(&role) {
        return false;
    }
    !ancestor_roles
        .iter()
        .any(|a| contains_role(CHROME_ANCESTOR_ROLES, a))
}
