//! Hand-drawn strokes that feel like ink: a Rust version of Steve Ruiz's perfect-freehand
//! (MIT, https://github.com/steveruiz/perfect-freehand), the algorithm Excalidraw draws with.
//!
//! Instead of a line of fixed width, a stroke is a filled outline whose width follows a
//! simulated pen pressure: fast movement thins the line, slow movement fills it out. The input
//! is "streamlined" (each point eases toward the pointer), which irons out hand jitter.
//!
//! Coordinates are y-down, like perfect-freehand.

use crate::geom::Point;

/// One step of a [`path`]. A path is a list of these commands, in order; the renderer fills
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathCmd {
    MoveTo(Point),
    LineTo(Point),
    QuadTo { ctrl: Point, to: Point },
    Close,
}

/// How a stroke is drawn.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Diameter of the stroke at full pressure.
    pub size: f64,
    /// How much pressure changes the width: 0 = even width, 1 = very thin when fast.
    pub thinning: f64,
    /// Softens the outline's edges (0…1).
    pub smoothing: f64,
    /// How far each point eases toward the pointer (0…1); higher = smoother, laggier.
    pub streamline: f64,
    /// Maps pressure to width.
    pub easing: fn(f64) -> f64,
}

impl Options {
    pub fn new(size: f64) -> Self {
        Self {
            size,
            thinning: 0.5,
            smoothing: 0.5,
            streamline: 0.5,
            easing: |pressure| pressure,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StrokePoint {
    point: Point,
    pressure: f64,
    vector: Point,
    distance: f64,
    running_length: f64,
}

const RATE_PRESSURE_CHANGE: f64 = 0.275;
const FIXED_PI: f64 = std::f64::consts::PI + 0.0001;

/// The outline polygon of a stroke through `points`. `complete`: the pen has lifted, so the
/// stroke runs all the way to the last point instead of trailing behind it.
pub fn outline(points: &[Point], options: &Options, complete: bool) -> Vec<Point> {
    outline_points(&stroke_points(points, options, complete), options, complete)
}

/// A smooth closed path through the outline: quadratic curves through the midpoints.
pub fn path(outline: &[Point]) -> Vec<PathCmd> {
    if outline.len() <= 2 {
        return Vec::new();
    }
    let mut cmds = vec![PathCmd::MoveTo(outline[0])];
    for i in 0..outline.len() {
        let a = outline[i];
        let b = outline[(i + 1) % outline.len()];
        cmds.push(PathCmd::QuadTo { ctrl: a, to: mid(a, b) });
    }
    cmds.push(PathCmd::Close);
    cmds
}

// MARK: - Points along the stroke

fn stroke_points(input: &[Point], options: &Options, complete: bool) -> Vec<StrokePoint> {
    if input.is_empty() {
        return Vec::new();
    }
    let t = 0.15 + (1.0 - options.streamline) * 0.85;
    let mut pts = input.to_vec();
    if pts.len() == 2 {
        // Two points make a straight line: add a few in between so it has a body.
        let first = pts[0];
        let last = pts[1];
        let mut body = Vec::with_capacity(5);
        body.push(first);
        for i in 1..=4 {
            body.push(lerp(first, last, i as f64 / 4.0));
        }
        pts = body;
    } else if pts.len() == 1 {
        pts.push(add(pts[0], Point::new(1.0, 1.0)));
    }

    let mut result = vec![StrokePoint {
        point: pts[0],
        pressure: 0.25,
        vector: Point::new(1.0, 1.0),
        distance: 0.0,
        running_length: 0.0,
    }];
    let mut reached_minimum_length = false;
    let mut running_length: f64 = 0.0;
    let mut prev = result[0];
    let max = pts.len() - 1;
    for (i, input_point) in pts.iter().enumerate().skip(1) {
        let point = if complete && i == max {
            *input_point
        } else {
            lerp(prev.point, *input_point, t)
        };
        if point == prev.point {
            continue;
        }
        let distance = dist(point, prev.point);
        running_length += distance;
        if i < max && !reached_minimum_length {
            if running_length < options.size {
                continue;
            }
            reached_minimum_length = true;
        }
        prev = StrokePoint {
            point,
            pressure: 0.5,
            vector: unit(sub(prev.point, point)),
            distance,
            running_length,
        };
        result.push(prev);
    }
    result[0].vector = if result.len() > 1 { result[1].vector } else { Point::ZERO };
    result
}

// MARK: - Outline

fn outline_points(points: &[StrokePoint], options: &Options, _complete: bool) -> Vec<Point> {
    let size = options.size;
    let Some(last_stroke) = points.last().copied() else {
        return Vec::new();
    };
    if size.is_nan() || size <= 0.0 {
        return Vec::new();
    }
    let total_length = last_stroke.running_length;
    let min_distance = (size * options.smoothing).powi(2);
    let mut left: Vec<Point> = Vec::new();
    let mut right: Vec<Point> = Vec::new();

    // Start from the pressure the first few points suggest, so the line doesn't begin blobby.
    let mut prev_pressure = points[0].pressure;
    for current in points.iter().take(10) {
        let sp = (current.distance / size).min(1.0);
        let rp = (1.0 - sp).min(1.0);
        let pressure = (prev_pressure + (rp - prev_pressure) * (sp * RATE_PRESSURE_CHANGE)).min(1.0);
        prev_pressure = (prev_pressure + pressure) / 2.0;
    }
    let mut radius = stroke_radius(size, options.thinning, last_stroke.pressure, options.easing);
    let mut first_radius: Option<f64> = None;
    let mut prev_vector = points[0].vector;
    let mut pl = points[0].point;
    let mut pr = pl;
    let mut tl = pl;
    let mut tr = pr;
    let mut prev_sharp = false;

    for i in 0..points.len() {
        let current = points[i];
        let point = current.point;
        let vector = current.vector;
        if i < points.len() - 1 && total_length - current.running_length < 3.0 {
            continue;
        }

        let mut pressure = current.pressure;
        if options.thinning != 0.0 {
            let sp = (current.distance / size).min(1.0);
            let rp = (1.0 - sp).min(1.0);
            pressure = (prev_pressure + (rp - prev_pressure) * (sp * RATE_PRESSURE_CHANGE)).min(1.0);
            radius = stroke_radius(size, options.thinning, pressure, options.easing);
        } else {
            radius = size / 2.0;
        }
        if first_radius.is_none() {
            first_radius = Some(radius);
        }
        radius = radius.max(0.01);

        let next_vector = if i < points.len() - 1 { points[i + 1].vector } else { points[i].vector };
        let next_dot = if i < points.len() - 1 { dot(vector, next_vector) } else { 1.0 };
        let prev_dot = dot(vector, prev_vector);
        let sharp = prev_dot < 0.0 && !prev_sharp;
        let next_sharp = next_dot < 0.0;

        if sharp || next_sharp {
            // A hairpin: round the corner instead of letting the outline cross itself.
            let offset = mul(perp(prev_vector), radius);
            let mut t: f64 = 0.0;
            while t <= 1.0 {
                tl = rotate(sub(point, offset), point, FIXED_PI * t);
                left.push(tl);
                tr = rotate(add(point, offset), point, FIXED_PI * -t);
                right.push(tr);
                t += 1.0 / 13.0;
            }
            pl = tl;
            pr = tr;
            if next_sharp {
                prev_sharp = true;
            }
            continue;
        }
        prev_sharp = false;

        if i == points.len() - 1 {
            let offset = mul(perp(vector), radius);
            left.push(sub(point, offset));
            right.push(add(point, offset));
            continue;
        }

        let offset = mul(perp(lerp(next_vector, vector, next_dot)), radius);
        tl = sub(point, offset);
        if i <= 1 || dist2(pl, tl) > min_distance {
            left.push(tl);
            pl = tl;
        }
        tr = add(point, offset);
        if i <= 1 || dist2(pr, tr) > min_distance {
            right.push(tr);
            pr = tr;
        }
        prev_pressure = pressure;
        prev_vector = vector;
    }

    let first_point = points[0].point;
    let last_point = if points.len() > 1 {
        last_stroke.point
    } else {
        add(points[0].point, Point::new(1.0, 1.0))
    };

    if points.len() == 1 {
        // A click: a round dot.
        let start = project(
            first_point,
            unit(perp(sub(first_point, last_point))),
            -(first_radius.unwrap_or(radius)),
        );
        let mut dot: Vec<Point> = Vec::new();
        let mut t: f64 = 1.0 / 13.0;
        while t <= 1.0 {
            dot.push(rotate(start, first_point, FIXED_PI * 2.0 * t));
            t += 1.0 / 13.0;
        }
        return dot;
    }

    let mut start_cap: Vec<Point> = Vec::new();
    if let Some(first) = right.first() {
        let mut t: f64 = 1.0 / 13.0;
        while t <= 1.0 {
            start_cap.push(rotate(*first, first_point, FIXED_PI * t));
            t += 1.0 / 13.0;
        }
    }
    let mut end_cap: Vec<Point> = Vec::new();
    let direction = perp(neg(last_stroke.vector));
    let start = project(last_point, direction, radius);
    let mut t: f64 = 1.0 / 29.0;
    while t < 1.0 {
        end_cap.push(rotate(start, last_point, FIXED_PI * 3.0 * t));
        t += 1.0 / 29.0;
    }
    let mut result = left;
    result.extend_from_slice(&end_cap);
    result.extend(right.iter().rev().copied());
    result.extend_from_slice(&start_cap);
    result
}

fn stroke_radius(size: f64, thinning: f64, pressure: f64, easing: fn(f64) -> f64) -> f64 {
    size * easing(0.5 - thinning * (0.5 - pressure))
}

// MARK: - Vectors

fn add(a: Point, b: Point) -> Point {
    Point::new(a.x + b.x, a.y + b.y)
}

fn sub(a: Point, b: Point) -> Point {
    Point::new(a.x - b.x, a.y - b.y)
}

fn mul(a: Point, n: f64) -> Point {
    Point::new(a.x * n, a.y * n)
}

fn neg(a: Point) -> Point {
    Point::new(-a.x, -a.y)
}

fn perp(a: Point) -> Point {
    Point::new(a.y, -a.x)
}

fn dot(a: Point, b: Point) -> f64 {
    a.x * b.x + a.y * b.y
}

fn mid(a: Point, b: Point) -> Point {
    Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)
}

fn lerp(a: Point, b: Point, t: f64) -> Point {
    add(a, mul(sub(b, a), t))
}

fn project(a: Point, b: Point, c: f64) -> Point {
    add(a, mul(b, c))
}

fn dist2(a: Point, b: Point) -> f64 {
    (a.x - b.x).powi(2) + (a.y - b.y).powi(2)
}

fn dist(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn unit(a: Point) -> Point {
    let length = a.x.hypot(a.y);
    if length == 0.0 { a } else { Point::new(a.x / length, a.y / length) }
}

fn rotate(a: Point, around: Point, by: f64) -> Point {
    let s = by.sin();
    let co = by.cos();
    let px = a.x - around.x;
    let py = a.y - around.y;
    Point::new(px * co - py * s + around.x, px * s + py * co + around.y)
}
