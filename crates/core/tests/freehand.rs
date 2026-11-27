//! Edge cases of the freehand stroke outline.

use igorio_core::freehand::{outline, path, Options, PathCmd};
use igorio_core::geom::Point;

fn command_points(cmds: &[PathCmd]) -> Vec<Point> {
    let mut points = Vec::new();
    for cmd in cmds {
        match *cmd {
            PathCmd::MoveTo(p) | PathCmd::LineTo(p) => points.push(p),
            PathCmd::QuadTo { ctrl, to } => {
                points.push(ctrl);
                points.push(to);
            }
            PathCmd::Close => {}
        }
    }
    points
}

fn assert_all_finite(points: &[Point]) {
    for p in points {
        assert!(p.x.is_finite() && p.y.is_finite(), "coordinate not finite: {p:?}");
    }
}

#[test]
fn empty_input_gives_an_empty_outline_and_path() {
    let options = Options::new(8.5);
    let polygon = outline(&[], &options, true);
    assert!(polygon.is_empty());
    assert!(path(&polygon).is_empty());
}

#[test]
fn one_point_gives_a_round_dot() {
    let options = Options::new(8.5);
    let polygon = outline(&[Point::new(100.0, 100.0)], &options, true);
    assert!(!polygon.is_empty());
    assert_all_finite(&polygon);

    let cmds = path(&polygon);
    assert!(!cmds.is_empty());
    assert_all_finite(&command_points(&cmds));
}

#[test]
fn straight_line_starts_with_move_to_and_has_finite_coordinates() {
    let options = Options::new(8.5);
    let points = [Point::new(0.0, 50.0), Point::new(30.0, 50.0), Point::new(60.0, 50.0)];
    let polygon = outline(&points, &options, true);
    assert!(!polygon.is_empty());

    let cmds = path(&polygon);
    assert!(!cmds.is_empty());
    assert!(matches!(cmds[0], PathCmd::MoveTo(_)), "first command is not MoveTo: {:?}", cmds[0]);
    assert!(matches!(cmds.last(), Some(PathCmd::Close)), "path does not close");
    assert_all_finite(&command_points(&cmds));
}

#[test]
fn same_input_twice_gives_equal_output() {
    let options = Options::new(8.5);
    let points = [Point::new(0.0, 50.0), Point::new(30.0, 50.0), Point::new(60.0, 55.0)];
    let first = path(&outline(&points, &options, true));
    let second = path(&outline(&points, &options, true));
    assert_eq!(first, second);
}
