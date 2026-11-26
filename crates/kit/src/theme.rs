//! Visual tokens from `Theme.swift`. Colours are straight RGBA, 0 to 255.

pub type Rgba = [u8; 4];

/// rgb(28, 38, 58): the base for hairlines and shadows. Use with an alpha.
pub const SLATE: [u8; 3] = [28, 38, 58];
pub const TEXT: Rgba = [0x2a, 0x33, 0x46, 255];
pub const MUTED: Rgba = [0x5a, 0x64, 0x78, 255];
pub const ACCENT: Rgba = [0x3b, 0x82, 0xf6, 255];
/// Slate at 8%.
pub const HAIRLINE: Rgba = [28, 38, 58, 20];
/// Slate at 14%.
pub const HAIRLINE_STRONG: Rgba = [28, 38, 58, 36];

pub const RADIUS: f64 = 14.0;
pub const RADIUS_SM: f64 = 8.0;

pub const FONT_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Barlow-Regular.ttf");
pub const FONT_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/Barlow-Medium.ttf");
pub const FONT_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Barlow-SemiBold.ttf");
