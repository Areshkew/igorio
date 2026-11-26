//! The ink on screen during a recording: which tool is in hand and the strokes drawn with it.
//! Pure; `InkOverlay` in the recording capability draws it and feeds it the mouse.

use crate::freehand::{self, Options, PathCmd};
use crate::geom::Point;

/// What the cursor draws with while recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InkTool {
    Pen,
    Highlighter,
}

impl InkTool {
    /// `CaseIterable`: every tool, in declaration order.
    pub const ALL: [InkTool; 2] = [InkTool::Pen, InkTool::Highlighter];

    /// The String raw value, unchanged from Swift.
    pub fn as_str(self) -> &'static str {
        match self {
            InkTool::Pen => "pen",
            InkTool::Highlighter => "highlighter",
        }
    }

    /// `init(rawValue:)`.
    pub fn from_raw_value(value: &str) -> Option<InkTool> {
        match value {
            "pen" => Some(InkTool::Pen),
            "highlighter" => Some(InkTool::Highlighter),
            _ => None,
        }
    }
}

/// The keys the ink answers to while recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InkKey {
    /// ⌘⇧7: take the pen, or put it down if it's already in hand.
    Pen,
    /// ⌘⇧8: take the highlighter, or put it down if it's already in hand.
    Highlighter,
    /// esc: back to the normal cursor.
    Done,
    /// ⌘Z: take back the last stroke.
    Undo,
    /// ⌫: wipe the screen, keep drawing.
    Clear,
}

/// One stroke drawn with a tool.
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    tool: InkTool,
    points: Vec<Point>,
}

impl Stroke {
    pub(crate) fn new(tool: InkTool, points: Vec<Point>) -> Self {
        Self { tool, points }
    }

    pub fn tool(&self) -> InkTool {
        self.tool
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }
}

/// The ink on screen during a recording: which tool is in hand and the strokes drawn with it.
/// Pure; `InkOverlay` in the recording capability draws it and feeds it the mouse.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ink {
    /// `None`: the normal cursor (clicks go to the apps underneath).
    tool: Option<InkTool>,
    strokes: Vec<Stroke>,
    /// The stroke being drawn right now (mouse is down).
    current: Option<Stroke>,
}

impl Ink {
    /// Points closer than this to the previous one add nothing but weight.
    const MIN_STEP: f64 = 1.0;

    pub fn new() -> Self {
        Self::default()
    }

    /// The tool in hand; `None` is the normal cursor.
    pub fn tool(&self) -> Option<InkTool> {
        self.tool
    }

    /// The finished strokes, oldest first.
    pub fn strokes(&self) -> &[Stroke] {
        &self.strokes
    }

    /// The stroke being drawn right now, if the mouse is down.
    pub fn current(&self) -> Option<&Stroke> {
        self.current.as_ref()
    }

    pub fn is_drawing(&self) -> bool {
        self.tool.is_some()
    }

    /// Returns whether the key did something; keys that didn't are left for the app underneath.
    pub fn press(&mut self, key: InkKey) -> bool {
        match key {
            InkKey::Pen | InkKey::Highlighter => {
                let wanted = if key == InkKey::Pen { InkTool::Pen } else { InkTool::Highlighter };
                if self.tool == Some(wanted) {
                    self.put_down();
                } else {
                    self.end_stroke();
                    self.tool = Some(wanted);
                }
                true
            }
            InkKey::Done => {
                if !self.is_drawing() {
                    return false;
                }
                self.put_down();
                true
            }
            InkKey::Undo => {
                if !self.is_drawing() {
                    return false;
                }
                if self.current.is_some() {
                    self.current = None;
                } else if !self.strokes.is_empty() {
                    self.strokes.pop();
                }
                true
            }
            InkKey::Clear => {
                if !self.is_drawing() {
                    return false;
                }
                self.current = None;
                self.strokes.clear();
                true
            }
        }
    }

    pub fn begin_stroke(&mut self, point: Point) {
        let Some(tool) = self.tool else {
            return;
        };
        self.end_stroke();
        self.current = Some(Stroke::new(tool, vec![point]));
    }

    pub fn extend_stroke(&mut self, point: Point) {
        let Some(stroke) = self.current.as_mut() else {
            return;
        };
        let Some(last) = stroke.points.last().copied() else {
            return;
        };
        let step = (point.x - last.x).hypot(point.y - last.y);
        if step.is_nan() || step < Self::MIN_STEP {
            return;
        }
        stroke.points.push(point);
    }

    pub fn end_stroke(&mut self) {
        let Some(stroke) = self.current.take() else {
            return;
        };
        self.strokes.push(stroke);
    }

    /// Back to the normal cursor. The ink goes with it: it was drawn for that moment.
    pub fn put_down(&mut self) {
        self.tool = None;
        self.current = None;
        self.strokes.clear();
    }

    /// How each tool's ink behaves (see `freehand`).
    pub fn options(tool: InkTool) -> Options {
        match tool {
            InkTool::Pen => {
                // Excalidraw's freedraw settings (its "bold" width): the line swells when you slow
                // down and thins when you flick, with a soft ease into full pressure.
                Options {
                    size: 8.5,
                    thinning: 0.6,
                    smoothing: 0.5,
                    streamline: 0.5,
                    easing: |pressure| (pressure * std::f64::consts::PI / 2.0).sin(),
                }
            }
            InkTool::Highlighter => {
                // A marker keeps its width; it only gets the smoothing.
                Options {
                    size: 22.0,
                    thinning: 0.0,
                    smoothing: 0.5,
                    streamline: 0.6,
                    easing: |pressure| pressure,
                }
            }
        }
    }

    /// The filled shape of a stroke. `complete`: the pen has lifted, so it reaches the last point.
    pub fn shape(stroke: &Stroke, complete: bool) -> Vec<PathCmd> {
        freehand::path(&freehand::outline(&stroke.points, &Ink::options(stroke.tool), complete))
    }
}
