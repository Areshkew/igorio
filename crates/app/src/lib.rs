//! The app: overlay, follower, pile state, drawing, tray, settings.
//! `main.rs` only wires these modules together.

pub mod layout;
pub mod scene;

pub mod clipboard;
pub mod crash;
pub mod draw;
pub mod drop_router;
pub mod explorer;
pub mod follower;
pub mod guide;
pub mod hook;
pub mod ink_draw;
pub mod input;
pub mod notes;
pub mod overlay;
pub mod recording;
pub mod saver;
pub mod settings;
pub mod settings_window;
pub mod shortcuts;
pub mod snip;
pub mod state;
pub mod tray;
pub mod uia;
