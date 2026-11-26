use igorio_core::drop_policy::{
    Action, DoubleClick, Spot, confirm_after_mouse_up, confirm_canvas_drop, decide, is_canvas,
    is_drop_spot, is_known_canvas_app, is_page_drawn_document,
};
use igorio_core::geom::{Point, Rect};

const PAGE: Rect = Rect::new(0.0, 160.0, 1800.0, 1009.0);

// MARK: DropPolicy

#[test]
fn test_nothing_held_ignores() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: false,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Ignore
    );
}

#[test]
fn test_hidden_for_capture_ignores() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            hidden_for_capture: true,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Ignore
    );
}

#[test]
fn test_busy_ignores() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            busy: true,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Ignore
    );
}

#[test]
fn test_triple_click_ignored() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            click_count: 3,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Ignore
    );
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            click_count: 1,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Ignore
    );
}

#[test]
fn test_own_note_ignored() {
    // Notes take the pile through their own double-click handler; the router must not also
    // paste into them (double delivery, ⌘V into ourselves).
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            target_is_own_process: true,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Ignore
    );
}

#[test]
fn test_finder_drop_spot_saves() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            is_finder: true,
            finder_drop_spot: true,
            ..Default::default()
        }),
        Action::SaveToFinder
    );
}

#[test]
fn test_finder_item_or_search_field_ignored() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            is_finder: true,
            finder_drop_spot: false,
            ..Default::default()
        }),
        Action::Ignore
    );
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            is_finder: true,
            finder_drop_spot: false,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Ignore
    );
}

#[test]
fn test_editable_pastes() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            editable_under_cursor: true,
            ..Default::default()
        }),
        Action::Paste
    );
}

#[test]
fn test_unrecognised_element_in_any_app_checks_focus() {
    // Previously only Chromium apps got a second look; native composers (ChatGPT, Messages
    // under a placeholder) were dropped on the floor.
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            ..Default::default()
        }),
        Action::CheckFocusAfterMouseUp
    );
}

// MARK: Confirmation after mouse-up

#[test]
fn test_focused_field_containing_click_confirms() {
    assert!(confirm_after_mouse_up(
        Point::new(500.0, 900.0),
        false,
        true,
        Some(Rect::new(300.0, 860.0, 600.0, 80.0)),
        false
    ));
}

#[test]
fn test_focused_field_elsewhere_does_not_confirm() {
    // Double-click on a toolbar while a search field elsewhere has focus.
    assert!(!confirm_after_mouse_up(
        Point::new(500.0, 40.0),
        false,
        true,
        Some(Rect::new(300.0, 860.0, 600.0, 80.0)),
        false
    ));
}

#[test]
fn test_focused_field_edge_slack() {
    assert!(confirm_after_mouse_up(
        Point::new(297.0, 862.0),
        false,
        true,
        Some(Rect::new(300.0, 860.0, 600.0, 80.0)),
        false
    ));
    assert!(!confirm_after_mouse_up(
        Point::new(280.0, 862.0),
        false,
        true,
        Some(Rect::new(300.0, 860.0, 600.0, 80.0)),
        false
    ));
}

#[test]
fn test_focused_non_editable_does_not_confirm() {
    assert!(!confirm_after_mouse_up(
        Point::ZERO,
        false,
        false,
        Some(Rect::new(-10.0, -10.0, 20.0, 20.0)),
        false
    ));
}

#[test]
fn test_focused_without_frame_does_not_confirm() {
    assert!(!confirm_after_mouse_up(
        Point::ZERO,
        false,
        true,
        None,
        false
    ));
}

#[test]
fn test_late_hit_or_nearby_confirms() {
    assert!(confirm_after_mouse_up(
        Point::ZERO,
        true,
        false,
        None,
        false
    ));
    assert!(confirm_after_mouse_up(
        Point::ZERO,
        false,
        false,
        None,
        true
    ));
}

// MARK: Finder

#[test]
fn test_finder_background_roles() {
    for role in [
        "AXScrollArea",
        "AXGroup",
        "AXList",
        "AXOutline",
        "AXTable",
        "AXBrowser",
        "AXGrid",
    ] {
        assert!(is_drop_spot(Some(role), &[]), "{role}");
    }
}

#[test]
fn test_finder_items_are_not_drop_spots() {
    for role in [
        "AXImage",
        "AXTextField",
        "AXStaticText",
        "AXRow",
        "AXCell",
        "AXButton",
    ] {
        assert!(!is_drop_spot(Some(role), &[]), "{role}");
    }
    assert!(!is_drop_spot(None, &[]));
}

#[test]
fn test_title_bar_double_click_is_not_a_drop_spot() {
    // Double-clicking a title bar zooms the window; it used to dump the pile into the folder.
    assert!(!is_drop_spot(Some("AXWindow"), &[]));
}

#[test]
fn test_toolbar_group_is_not_a_drop_spot() {
    assert!(!is_drop_spot(
        Some("AXGroup"),
        &["AXToolbar".to_string(), "AXWindow".to_string()]
    ));
    assert!(is_drop_spot(
        Some("AXGroup"),
        &[
            "AXScrollArea".to_string(),
            "AXSplitGroup".to_string(),
            "AXWindow".to_string()
        ]
    ));
}

// MARK: CanvasPolicy

#[test]
fn test_web_canvas_filling_the_page_is_a_canvas() {
    // Excalidraw in Chrome: the <canvas> is an AXImage as big as the page.
    let spot = Spot {
        role: Some("AXImage".to_string()),
        ancestor_roles: vec![
            "AXGroup".to_string(),
            "AXGroup".to_string(),
            "AXWebArea".to_string(),
            "AXScrollArea".to_string(),
            "AXWindow".to_string(),
        ],
        frame: Some(PAGE),
        web_area_frame: Some(PAGE),
        url: Some("https://example.org/board".to_string()),
        bundle_id: Some("com.google.Chrome".to_string()),
    };
    assert!(is_canvas(&spot));
}

#[test]
fn test_an_ordinary_picture_in_a_page_is_not() {
    let photo = Rect::new(300.0, 300.0, 600.0, 400.0);
    let spot = Spot {
        role: Some("AXImage".to_string()),
        ancestor_roles: vec!["AXGroup".to_string(), "AXWebArea".to_string()],
        frame: Some(photo),
        web_area_frame: Some(PAGE),
        url: Some("https://news.example.com/story".to_string()),
        bundle_id: Some("com.google.Chrome".to_string()),
    };
    assert!(!is_canvas(&spot));
}

#[test]
fn test_a_picture_inside_a_link_is_not() {
    let spot = Spot {
        role: Some("AXImage".to_string()),
        ancestor_roles: vec!["AXLink".to_string(), "AXWebArea".to_string()],
        frame: Some(PAGE),
        web_area_frame: Some(PAGE),
        ..Default::default()
    };
    assert!(!is_canvas(&spot));
}

#[test]
fn test_known_canvas_sites() {
    assert!(is_known_canvas_app(
        Some("com.google.Chrome"),
        Some("https://www.figma.com/design/abc/File?node-id=1")
    ));
    assert!(is_known_canvas_app(
        Some("com.apple.Safari"),
        Some("https://docs.google.com/presentation/d/abc/edit#slide=1")
    ));
    assert!(is_known_canvas_app(
        None,
        Some("https://app.excalidraw.com/x")
    ));
    assert!(!is_known_canvas_app(
        Some("com.google.Chrome"),
        Some("https://docs.google.com/spreadsheets/d/abc")
    ));
    assert!(!is_known_canvas_app(
        Some("com.google.Chrome"),
        Some("https://notfigma.com/")
    ));
    assert!(is_known_canvas_app(Some("com.figma.Desktop"), None));
    assert!(is_known_canvas_app(Some("com.apple.iWork.Keynote"), None));
}

#[test]
fn test_google_docs_is_a_drop_target_that_keeps_its_selection_rules() {
    let url = "https://docs.google.com/document/d/abc/edit?tab=t.0";
    assert!(is_known_canvas_app(Some("com.google.Chrome"), Some(url)));
    assert!(is_page_drawn_document(Some(url)));
    assert!(is_canvas(&Spot {
        role: Some("AXGroup".to_string()),
        ancestor_roles: vec!["AXGroup".to_string(), "AXWebArea".to_string()],
        url: Some(url.to_string()),
        ..Default::default()
    }));
    assert!(!is_canvas(&Spot {
        role: Some("AXButton".to_string()),
        ancestor_roles: vec!["AXToolbar".to_string(), "AXWebArea".to_string()],
        url: Some(url.to_string()),
        ..Default::default()
    }));
    // Slides and boards have no word selection to protect.
    assert!(!is_page_drawn_document(Some(
        "https://docs.google.com/presentation/d/abc/edit"
    )));
    assert!(!is_page_drawn_document(None));
}

#[test]
fn test_slides_surface_is_a_canvas_but_its_filmstrip_and_buttons_are_not() {
    let url = "https://docs.google.com/presentation/d/abc/edit";
    assert!(is_canvas(&Spot {
        role: Some("AXGroup".to_string()),
        ancestor_roles: vec!["AXGroup".to_string(), "AXWebArea".to_string()],
        url: Some(url.to_string()),
        ..Default::default()
    }));
    assert!(!is_canvas(&Spot {
        role: Some("AXButton".to_string()),
        ancestor_roles: vec!["AXToolbar".to_string(), "AXWebArea".to_string()],
        url: Some(url.to_string()),
        ..Default::default()
    }));
    assert!(!is_canvas(&Spot {
        role: Some("AXGroup".to_string()),
        ancestor_roles: vec!["AXToolbar".to_string(), "AXWebArea".to_string()],
        url: Some(url.to_string()),
        ..Default::default()
    }));
    assert!(!is_canvas(&Spot {
        role: Some("AXStaticText".to_string()),
        ancestor_roles: vec!["AXGroup".to_string(), "AXWebArea".to_string()],
        url: Some(url.to_string()),
        ..Default::default()
    }));
}

#[test]
fn test_keynote() {
    assert!(is_canvas(&Spot {
        role: Some("AXLayoutArea".to_string()),
        ancestor_roles: vec![
            "AXScrollArea".to_string(),
            "AXSplitGroup".to_string(),
            "AXWindow".to_string()
        ],
        bundle_id: Some("com.apple.iWork.Keynote".to_string()),
        ..Default::default()
    }));
    // The slide navigator is an outline: a double-click there is not a drop.
    assert!(!is_canvas(&Spot {
        role: Some("AXGroup".to_string()),
        ancestor_roles: vec![
            "AXRow".to_string(),
            "AXOutline".to_string(),
            "AXWindow".to_string()
        ],
        bundle_id: Some("com.apple.iWork.Keynote".to_string()),
        ..Default::default()
    }));
    assert!(!is_canvas(&Spot {
        role: Some("AXWindow".to_string()),
        bundle_id: Some("com.apple.iWork.Keynote".to_string()),
        ..Default::default()
    }));
}

#[test]
fn test_a_plain_window_background_is_not_a_canvas() {
    assert!(!is_canvas(&Spot {
        role: Some("AXGroup".to_string()),
        ancestor_roles: vec!["AXWindow".to_string()],
        bundle_id: Some("com.apple.mail".to_string()),
        ..Default::default()
    }));
    assert!(!is_canvas(&Spot {
        role: None,
        ..Default::default()
    }));
}

#[test]
fn test_canvas_drop_needs_typing_not_to_be_going_elsewhere() {
    let point = Point::new(900.0, 600.0);
    let composer = Rect::new(400.0, 950.0, 800.0, 60.0);
    assert!(confirm_canvas_drop(point, true, false, false, None));
    assert!(
        !confirm_canvas_drop(point, true, false, true, Some(composer)),
        "a visible focused field would receive the ⌘V instead"
    );
    assert!(
        confirm_canvas_drop(
            point,
            true,
            false,
            true,
            Some(Rect::new(-10000.0, 0.0, 1.0, 1.0))
        ),
        "the hidden field canvas apps use to catch typing"
    );
    assert!(confirm_canvas_drop(
        point,
        true,
        true,
        true,
        Some(composer)
    ));
    assert!(!confirm_canvas_drop(point, false, true, false, None));
}

#[test]
fn test_a_busy_terminal_still_takes_the_paste() {
    assert_eq!(
        decide(&DoubleClick {
            is_holding: true,
            silent_terminal: true,
            ..Default::default()
        }),
        Action::Paste
    );
    assert_eq!(
        decide(&DoubleClick {
            is_holding: false,
            silent_terminal: true,
            ..Default::default()
        }),
        Action::Ignore
    );
}
