use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use igorio_core::geom::{CardId, Size};
use igorio_core::pile::HasCardId;

use crate::image::Image;

/// One card on the pile: a screenshot, or a screen recording (then `image` is its poster
/// frame and `movie` the file it stands for). From `HeldScreenshot.swift`. Cheap to clone.
#[derive(Debug, Clone)]
pub struct HeldCard {
    pub id: CardId,
    pub image: Arc<Image>,
    /// Physical pixels per logical pixel of the monitor it was captured on (1.0 at 96 DPI).
    pub scale: f64,
    pub captured_at: SystemTime,
    /// Already saved to disk, so letting go of it loses nothing.
    pub kept: bool,
    /// Small tilt in degrees so stacked cards read as separate sheets.
    pub tilt: f64,
    /// A recording: the saved video file. Delivering the card delivers this file.
    pub movie: Option<PathBuf>,
    /// How long the recording runs, in seconds, for the card's badge.
    pub duration: Option<f64>,
}

impl HeldCard {
    pub fn is_movie(&self) -> bool {
        self.movie.is_some()
    }

    /// Size in logical pixels.
    pub fn logical_size(&self) -> Size {
        Size::new(self.image.width as f64 / self.scale, self.image.height as f64 / self.scale)
    }
}

impl HasCardId for HeldCard {
    fn card_id(&self) -> CardId {
        self.id
    }
}
