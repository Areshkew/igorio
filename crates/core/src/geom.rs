//! Shared value types: points, sizes, vectors, rectangles and card ids. Seconds and
//! lengths are plain `f64`.

/// Identifies one card on the pile; the app hands them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CardId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vector {
    pub dx: f64,
    pub dy: f64,
}

impl Vector {
    pub const fn new(dx: f64, dy: f64) -> Self {
        Self { dx, dy }
    }
}

/// Origin is the top-left corner; `width` and `height` are not negative.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    pub fn min_x(&self) -> f64 {
        self.x
    }

    pub fn max_x(&self) -> f64 {
        self.x + self.width
    }

    pub fn min_y(&self) -> f64 {
        self.y
    }

    pub fn max_y(&self) -> f64 {
        self.y + self.height
    }

    pub fn mid_x(&self) -> f64 {
        self.x + self.width / 2.0
    }

    pub fn mid_y(&self) -> f64 {
        self.y + self.height / 2.0
    }

    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// The min edges are inside, the max edges are not.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x() && p.x < self.max_x() && p.y >= self.min_y() && p.y < self.max_y()
    }

    /// Positive values shrink, negative values grow.
    pub fn inset_by(&self, dx: f64, dy: f64) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.width - 2.0 * dx, self.height - 2.0 * dy)
    }

    /// `None` when the rectangles do not overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.min_x().max(other.min_x());
        let y = self.min_y().max(other.min_y());
        let w = self.max_x().min(other.max_x()) - x;
        let h = self.max_y().min(other.max_y()) - y;
        if w < 0.0 || h < 0.0 { None } else { Some(Rect::new(x, y, w, h)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_excludes_max_edges() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(!r.contains(Point::new(10.0, 5.0)));
    }

    #[test]
    fn negative_inset_grows() {
        let r = Rect::new(10.0, 10.0, 10.0, 10.0).inset_by(-6.0, -6.0);
        assert_eq!(r, Rect::new(4.0, 4.0, 22.0, 22.0));
    }

    #[test]
    fn intersection_of_apart_rects_is_none() {
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        assert_eq!(a.intersection(&Rect::new(6.0, 0.0, 5.0, 5.0)), None);
        assert_eq!(a.intersection(&Rect::new(3.0, 3.0, 5.0, 5.0)), Some(Rect::new(3.0, 3.0, 2.0, 2.0)));
    }
}
