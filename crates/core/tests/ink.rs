//! Ink strokes and the keys the ink answers to.
//!
//! The helpers below flatten a stroke's quadratic path into a polygon, to get its bounding box
//! and to test whether a point is inside the filled shape.

use igorio_core::freehand::PathCmd;
use igorio_core::geom::{Point, Rect};
use igorio_core::ink::{Ink, InkKey, InkTool, Stroke};

/// Segments per quadratic curve used to flatten the path.
const CURVE_STEPS: usize = 16;

fn quad_at(a: Point, ctrl: Point, to: Point, t: f64) -> Point {
    let u = 1.0 - t;
    Point::new(
        u * u * a.x + 2.0 * u * t * ctrl.x + t * t * to.x,
        u * u * a.y + 2.0 * u * t * ctrl.y + t * t * to.y,
    )
}

fn flatten(cmds: &[PathCmd]) -> Vec<Point> {
    let mut polygon = Vec::new();
    let mut current = Point::ZERO;
    for cmd in cmds {
        match *cmd {
            PathCmd::MoveTo(p) | PathCmd::LineTo(p) => {
                current = p;
                polygon.push(p);
            }
            PathCmd::QuadTo { ctrl, to } => {
                for i in 1..=CURVE_STEPS {
                    polygon.push(quad_at(current, ctrl, to, i as f64 / CURVE_STEPS as f64));
                }
                current = to;
            }
            PathCmd::Close => {}
        }
    }
    polygon
}

/// The bounding box of the path's points, control points included.
#[track_caller]
fn bounding_box(cmds: &[PathCmd]) -> Rect {
    let polygon = flatten(cmds);
    assert!(!polygon.is_empty(), "the path has no points");
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in &polygon {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

fn is_left(a: Point, b: Point, p: Point) -> f64 {
    (b.x - a.x) * (p.y - a.y) - (p.x - a.x) * (b.y - a.y)
}

/// Whether the point is inside the filled (closed) path, by the nonzero winding rule.
#[track_caller]
fn path_contains(cmds: &[PathCmd], point: Point) -> bool {
    let polygon = flatten(cmds);
    assert!(!polygon.is_empty(), "the path has no points");
    let mut winding = 0;
    for i in 0..polygon.len() {
        let a = polygon[i];
        let b = polygon[(i + 1) % polygon.len()];
        if a.y <= point.y {
            if b.y > point.y && is_left(a, b, point) > 0.0 {
                winding += 1;
            }
        } else if b.y <= point.y && is_left(a, b, point) < 0.0 {
            winding -= 1;
        }
    }
    winding != 0
}

#[track_caller]
fn assert_close(actual: f64, expected: f64, accuracy: f64) {
    assert!(
        (actual - expected).abs() <= accuracy,
        "expected {actual} to be within {accuracy} of {expected}"
    );
}

fn draw_line(ink: &mut Ink, y: f64) {
    ink.begin_stroke(Point::new(0.0, y));
    ink.extend_stroke(Point::new(40.0, y));
    ink.extend_stroke(Point::new(80.0, y + 5.0));
    ink.end_stroke();
}

/// Builds a finished stroke the way the overlay does. `Stroke::new` is crate-private, so the
/// test takes a tool and feeds the points through `Ink`; the helper checks they all survived
/// the one-point minimum step, so `Ink::shape` sees exactly the requested points.
#[track_caller]
fn stroke(tool: InkTool, points: &[Point]) -> Stroke {
    let mut ink = Ink::new();
    ink.press(match tool {
        InkTool::Pen => InkKey::Pen,
        InkTool::Highlighter => InkKey::Highlighter,
    });
    ink.begin_stroke(points[0]);
    for point in &points[1..] {
        ink.extend_stroke(*point);
    }
    ink.end_stroke();
    let built = ink.strokes()[0].clone();
    assert_eq!(built.points(), points, "the stroke did not keep every point");
    built
}

#[test]
fn test_shortcut_takes_tool_and_pressing_again_puts_it_down() {
    let mut ink = Ink::new();
    assert!(ink.press(InkKey::Pen));
    assert_eq!(ink.tool(), Some(InkTool::Pen));
    assert!(ink.press(InkKey::Pen));
    assert_eq!(ink.tool(), None);
}

#[test]
fn test_other_shortcut_switches_tool_and_keeps_the_ink() {
    let mut ink = Ink::new();
    ink.press(InkKey::Pen);
    draw_line(&mut ink, 10.0);
    ink.press(InkKey::Highlighter);
    assert_eq!(ink.tool(), Some(InkTool::Highlighter));
    assert_eq!(ink.strokes().len(), 1);
    assert_eq!(ink.strokes()[0].tool(), InkTool::Pen);
    draw_line(&mut ink, 10.0);
    let tools: Vec<InkTool> = ink.strokes().iter().map(|stroke| stroke.tool()).collect();
    assert_eq!(tools, vec![InkTool::Pen, InkTool::Highlighter]);
}

#[test]
fn test_done_puts_tool_down_and_wipes_the_ink() {
    let mut ink = Ink::new();
    ink.press(InkKey::Highlighter);
    draw_line(&mut ink, 10.0);
    assert!(ink.press(InkKey::Done));
    assert_eq!(ink.tool(), None);
    assert!(ink.strokes().is_empty());
}

#[test]
fn test_editing_keys_are_left_alone_when_not_drawing() {
    // esc, Ctrl+Z and Backspace belong to the app underneath unless a tool is in hand.
    let mut ink = Ink::new();
    assert!(!ink.press(InkKey::Done));
    assert!(!ink.press(InkKey::Undo));
    assert!(!ink.press(InkKey::Clear));
}

#[test]
fn test_undo_takes_back_the_last_stroke() {
    let mut ink = Ink::new();
    ink.press(InkKey::Pen);
    draw_line(&mut ink, 10.0);
    draw_line(&mut ink, 50.0);
    assert!(ink.press(InkKey::Undo));
    assert_eq!(ink.strokes().len(), 1);
    assert!(ink.press(InkKey::Undo));
    assert!(ink.strokes().is_empty());
    assert!(ink.press(InkKey::Undo), "still drawing: the key is ours even with nothing to undo");
}

#[test]
fn test_clear_wipes_but_keeps_the_tool() {
    let mut ink = Ink::new();
    ink.press(InkKey::Pen);
    draw_line(&mut ink, 10.0);
    ink.press(InkKey::Clear);
    assert!(ink.strokes().is_empty());
    assert_eq!(ink.tool(), Some(InkTool::Pen));
}

#[test]
fn test_no_strokes_without_a_tool() {
    let mut ink = Ink::new();
    ink.begin_stroke(Point::ZERO);
    ink.extend_stroke(Point::new(10.0, 0.0));
    ink.end_stroke();
    assert!(ink.strokes().is_empty());
}

#[test]
fn test_tiny_moves_are_dropped() {
    let mut ink = Ink::new();
    ink.press(InkKey::Pen);
    ink.begin_stroke(Point::ZERO);
    ink.extend_stroke(Point::new(0.5, 0.5));
    ink.extend_stroke(Point::new(5.0, 0.0));
    assert_eq!(ink.current().map(|stroke| stroke.points().len()), Some(2));
}

#[test]
fn test_a_click_makes_a_round_dot() {
    let shape = Ink::shape(&stroke(InkTool::Pen, &[Point::new(100.0, 100.0)]), true);
    let bounds = bounding_box(&shape);
    assert!(!shape.is_empty());
    assert!(bounds.contains(Point::new(100.0, 100.0)));
    assert_close(bounds.width, bounds.height, 1.5);
}

#[test]
fn test_completed_stroke_covers_its_ends() {
    let points: Vec<Point> = (0..=30)
        .map(|i| Point::new(i as f64 * 6.0, 50.0 + (i as f64 / 4.0).sin() * 20.0))
        .collect();
    let shape = Ink::shape(&stroke(InkTool::Pen, &points), true);
    assert!(path_contains(&shape, points[0]));
    assert!(path_contains(&shape, *points.last().unwrap()));
    assert!(path_contains(&shape, points[15]));
}

#[test]
fn test_pen_thins_when_fast_and_highlighter_does_not() {
    // Same distance, drawn slowly (many close points) and quickly (few far apart).
    fn width(tool: InkTool, step: f64) -> f64 {
        let mut points = Vec::new();
        let mut x = 0.0;
        while x <= 300.0 {
            points.push(Point::new(x, 100.0));
            x += step;
        }
        bounding_box(&Ink::shape(&stroke(tool, &points), true)).height
    }

    assert!(width(InkTool::Pen, 2.0) > width(InkTool::Pen, 30.0) + 1.0);
    assert_close(width(InkTool::Highlighter, 2.0), width(InkTool::Highlighter, 30.0), 1.0);
}
