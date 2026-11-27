//! Pure decision logic, translated from `fairy-cursor/sources/core`.
//! No Windows API and no dependencies. Coordinates are f64 with a top-left origin.

pub mod geom;

pub mod capture_watchdog;
pub mod drop_policy;
pub mod editable;
pub mod freehand;
pub mod ink;
pub mod paste_policy;
pub mod pile;
pub mod shake;
pub mod viewport;
