#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    PrintScreen,
    AltPrintScreen,
    WinPrintScreen,
    WinShiftS,
}

#[derive(Default)]
pub struct Signals {
    pub clipboard_read: bool,
    pub image: bool,
    pub file_transfer: bool,
    pub html: bool,
    pub text: bool,
    pub capture_owner: bool,
    pub recent_overlay: bool,
    pub fresh_shortcut: Option<Shortcut>,
    pub instant_shortcut: Option<Shortcut>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Screenshot(&'static str),
    Other(&'static str),
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Screenshot(_) => "SCREENSHOT",
            Self::Other(_) => "OTHER",
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::Screenshot(reason) | Self::Other(reason) => reason,
        }
    }
}

pub fn classify(signals: Signals) -> Verdict {
    use Verdict::{Other, Screenshot};
    if !signals.clipboard_read {
        return Other("clipboard unavailable");
    }
    if signals.file_transfer {
        return Other("file-transfer format");
    }
    if !signals.image {
        return Other("no raster image format");
    }
    if signals.html || signals.text {
        return Other("image accompanied by HTML or text");
    }
    if signals.recent_overlay {
        return Screenshot("image after a snipping overlay");
    }
    if signals.capture_owner && signals.fresh_shortcut.is_some() {
        return Screenshot("image from a capture process after a capture shortcut");
    }
    match signals.instant_shortcut {
        Some(Shortcut::PrintScreen) => Screenshot("image immediately after PrtScn"),
        Some(Shortcut::AltPrintScreen) => Screenshot("image immediately after Alt+PrtScn"),
        Some(Shortcut::WinPrintScreen) => Screenshot("image immediately after Win+PrtScn"),
        _ => Other("image without recent capture evidence"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Signals {
        Signals {
            clipboard_read: true,
            image: true,
            ..Default::default()
        }
    }

    #[test]
    fn snip_after_overlay_even_without_shortcut_or_owner() {
        assert!(matches!(
            classify(Signals {
                recent_overlay: true,
                ..image()
            }),
            Verdict::Screenshot(_)
        ));
    }

    #[test]
    fn snip_with_capture_owner_and_shortcut_but_no_visible_overlay() {
        assert!(matches!(
            classify(Signals {
                capture_owner: true,
                fresh_shortcut: Some(Shortcut::WinShiftS),
                ..image()
            }),
            Verdict::Screenshot(_)
        ));
    }

    #[test]
    fn direct_print_screen_variants() {
        for shortcut in [
            Shortcut::PrintScreen,
            Shortcut::AltPrintScreen,
            Shortcut::WinPrintScreen,
        ] {
            assert!(matches!(
                classify(Signals {
                    instant_shortcut: Some(shortcut),
                    ..image()
                }),
                Verdict::Screenshot(_)
            ));
        }
    }

    #[test]
    fn browser_image_with_html_overrides_recent_capture_activity() {
        assert_eq!(
            classify(Signals {
                html: true,
                recent_overlay: true,
                ..image()
            }),
            Verdict::Other("image accompanied by HTML or text")
        );
    }

    #[test]
    fn explorer_file_copy_with_an_image_preview() {
        assert_eq!(
            classify(Signals {
                file_transfer: true,
                ..image()
            }),
            Verdict::Other("file-transfer format")
        );
    }

    #[test]
    fn text_copy() {
        assert_eq!(
            classify(Signals {
                clipboard_read: true,
                text: true,
                ..Default::default()
            }),
            Verdict::Other("no raster image format")
        );
    }

    #[test]
    fn editor_image_or_browser_copy_without_html() {
        assert_eq!(
            classify(image()),
            Verdict::Other("image without recent capture evidence")
        );
    }

    #[test]
    fn win_shift_s_without_capture_evidence_does_not_classify_a_later_copy() {
        assert_eq!(
            classify(Signals {
                fresh_shortcut: Some(Shortcut::WinShiftS),
                instant_shortcut: Some(Shortcut::WinShiftS),
                ..image()
            }),
            Verdict::Other("image without recent capture evidence")
        );
    }

    #[test]
    fn copying_from_the_snipping_tool_without_capture_context() {
        assert_eq!(
            classify(Signals {
                capture_owner: true,
                ..image()
            }),
            Verdict::Other("image without recent capture evidence")
        );
    }
}
