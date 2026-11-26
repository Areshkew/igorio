use windows::Win32::UI::Accessibility::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Bounds {
    fn contains(self, (x, y): (i32, i32)) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

#[derive(Debug, Default)]
pub struct Node {
    pub control: Option<UIA_CONTROLTYPE_ID>,
    pub localized_control: String,
    pub class: String,
    pub name: String,
    pub framework: String,
    pub focusable: Option<bool>,
    pub focused: Option<bool>,
    pub password: Option<bool>,
    pub bounds: Option<Bounds>,
    pub value: Option<bool>,
    pub text: Option<bool>,
    pub text_edit: Option<bool>,
    pub value_read_only: Option<bool>,
    pub text_read_only: Option<bool>,
    pub pid: Option<u32>,
}

impl Node {
    #[allow(non_upper_case_globals)] // Use the Windows SDK's constants directly.
    fn editable(&self) -> bool {
        if self.password == Some(true)
            || self.value_read_only == Some(true)
            || self.text_read_only == Some(true)
        {
            return false;
        }
        let text_control = matches!(
            self.control,
            Some(UIA_EditControlTypeId | UIA_ComboBoxControlTypeId | UIA_DocumentControlTypeId)
        );
        self.text_edit == Some(true)
            || (self.text == Some(true) && self.text_read_only == Some(false))
            || (self.value_read_only == Some(false) && text_control)
            || (self.control == Some(UIA_EditControlTypeId)
                && self.value != Some(true)
                && self.focusable == Some(true))
    }

    fn contains(&self, point: (i32, i32)) -> bool {
        self.bounds.is_some_and(|bounds| bounds.contains(point))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Editable,
    Terminal,
    ExplorerFolderBackground,
    Desktop,
    CanvasOrUnknown,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Editable => "EDITABLE",
            Self::Terminal => "TERMINAL",
            Self::ExplorerFolderBackground => "EXPLORER_FOLDER_BACKGROUND",
            Self::Desktop => "DESKTOP",
            Self::CanvasOrUnknown => "CANVAS_OR_UNKNOWN",
        }
    }
}

#[derive(Clone, Copy)]
pub enum ShellView {
    Folder,
    Desktop,
}

#[derive(Default)]
pub struct Facts<'a> {
    pub point: (i32, i32),
    pub chain: &'a [Node],
    pub focus: Option<&'a Node>,
    pub hit_exe: Option<&'a str>,
    pub window_exe: Option<&'a str>,
    pub shell_view: Option<ShellView>,
}

#[allow(non_upper_case_globals)] // Use the Windows SDK's constants directly.
pub fn verdict(facts: &Facts<'_>) -> (Verdict, &'static str) {
    use Verdict::*;

    let terminals = [
        "WindowsTerminal.exe",
        "conhost.exe",
        "OpenConsole.exe",
        "wezterm-gui.exe",
        "alacritty.exe",
    ];
    if [facts.hit_exe, facts.window_exe]
        .into_iter()
        .flatten()
        .any(|exe| terminals.iter().any(|name| name.eq_ignore_ascii_case(exe)))
    {
        return (Terminal, "terminal executable");
    }

    let Some(hit) = facts.chain.first() else {
        return (CanvasOrUnknown, "no UI Automation hit");
    };

    if let Some(shell) = facts.shell_view {
        let background_role = matches!(
            hit.control,
            Some(
                UIA_ListControlTypeId
                    | UIA_DataGridControlTypeId
                    | UIA_TableControlTypeId
                    | UIA_PaneControlTypeId
                    | UIA_GroupControlTypeId
                    | UIA_CustomControlTypeId
            )
        );
        let inside_item_or_chrome = facts.chain.iter().any(|node| {
            matches!(
                node.control,
                Some(
                    UIA_ListItemControlTypeId
                        | UIA_DataItemControlTypeId
                        | UIA_TreeItemControlTypeId
                        | UIA_HeaderControlTypeId
                        | UIA_HeaderItemControlTypeId
                        | UIA_ButtonControlTypeId
                        | UIA_ToolBarControlTypeId
                        | UIA_MenuControlTypeId
                        | UIA_MenuBarControlTypeId
                        | UIA_TabControlTypeId
                )
            )
        });
        if background_role && !inside_item_or_chrome {
            return match shell {
                ShellView::Folder => (ExplorerFolderBackground, "shell view container"),
                ShellView::Desktop => (Desktop, "desktop view container"),
            };
        }
        return (
            CanvasOrUnknown,
            "shell item, chrome, or unrecognized view element",
        );
    }

    for (depth, node) in facts.chain.iter().enumerate() {
        if node.password == Some(true) {
            return (CanvasOrUnknown, "password control");
        }
        if node.editable() {
            return if depth == 0 {
                (Editable, "writable text hit")
            } else {
                (Editable, "writable text ancestor")
            };
        }
        if matches!(
            node.control,
            Some(UIA_WindowControlTypeId | UIA_DocumentControlTypeId)
        ) {
            break;
        }
    }

    if facts.focus.is_some_and(|focus| {
        hit.pid.is_some()
            && hit.pid == focus.pid
            && focus.focused == Some(true)
            && focus.editable()
            && focus.contains(facts.point)
    }) {
        return (Editable, "same-process focused text control contains click");
    }
    (CanvasOrUnknown, "no writable text evidence")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(control: UIA_CONTROLTYPE_ID) -> Node {
        Node {
            control: Some(control),
            ..Node::default()
        }
    }

    fn edit() -> Node {
        Node {
            value: Some(true),
            value_read_only: Some(false),
            ..node(UIA_EditControlTypeId)
        }
    }

    #[test]
    fn edit_ancestor_before_document_boundary() {
        let chain = [
            node(UIA_TextControlTypeId),
            edit(),
            node(UIA_DocumentControlTypeId),
        ];
        assert_eq!(
            verdict(&Facts {
                chain: &chain,
                ..Facts::default()
            })
            .0,
            Verdict::Editable
        );
        let chain = [
            node(UIA_TextControlTypeId),
            node(UIA_DocumentControlTypeId),
            edit(),
        ];
        assert_eq!(
            verdict(&Facts {
                chain: &chain,
                ..Facts::default()
            })
            .0,
            Verdict::CanvasOrUnknown
        );
    }

    #[test]
    fn read_only_password_and_non_text_values_are_ignored() {
        for hit in [
            Node {
                value_read_only: Some(true),
                ..edit()
            },
            Node {
                password: Some(true),
                ..edit()
            },
            Node {
                value_read_only: Some(false),
                ..node(UIA_SliderControlTypeId)
            },
            Node {
                text: Some(true),
                ..node(UIA_DocumentControlTypeId)
            },
        ] {
            assert_eq!(
                verdict(&Facts {
                    chain: &[hit],
                    ..Facts::default()
                })
                .0,
                Verdict::CanvasOrUnknown
            );
        }
    }

    #[test]
    fn writable_document_and_text_edit_provider() {
        for hit in [
            Node {
                text: Some(true),
                text_read_only: Some(false),
                ..node(UIA_DocumentControlTypeId)
            },
            Node {
                text_edit: Some(true),
                ..node(UIA_CustomControlTypeId)
            },
        ] {
            assert_eq!(
                verdict(&Facts {
                    chain: &[hit],
                    ..Facts::default()
                })
                .0,
                Verdict::Editable
            );
        }
    }

    #[test]
    fn focus_fallback_requires_click_inside_same_process() {
        let chain = [Node {
            pid: Some(10),
            ..node(UIA_TextControlTypeId)
        }];
        let mut focus = edit();
        focus.pid = Some(10);
        focus.focused = Some(true);
        focus.bounds = Some(Bounds {
            left: 2,
            top: 2,
            right: 8,
            bottom: 8,
        });
        for (point, expected) in [
            ((4, 4), Verdict::Editable),
            ((12, 4), Verdict::CanvasOrUnknown),
        ] {
            assert_eq!(
                verdict(&Facts {
                    chain: &chain,
                    focus: Some(&focus),
                    point,
                    ..Facts::default()
                })
                .0,
                expected
            );
        }
        focus.pid = Some(11);
        assert_eq!(
            verdict(&Facts {
                chain: &chain,
                focus: Some(&focus),
                point: (4, 4),
                ..Facts::default()
            })
            .0,
            Verdict::CanvasOrUnknown
        );
    }

    #[test]
    fn terminals_win_even_without_a_hit() {
        for exe in [
            "windowsterminal.EXE",
            "conhost.exe",
            "OpenConsole.exe",
            "wezterm-gui.exe",
            "alacritty.exe",
        ] {
            assert_eq!(
                verdict(&Facts {
                    window_exe: Some(exe),
                    ..Facts::default()
                })
                .0,
                Verdict::Terminal
            );
        }
        assert_eq!(
            verdict(&Facts {
                hit_exe: Some("WindowsTerminal.exe"),
                chain: &[edit()],
                ..Facts::default()
            })
            .0,
            Verdict::Terminal
        );
    }

    #[test]
    fn shell_background_excludes_items_and_chrome() {
        for (view, expected) in [
            (ShellView::Folder, Verdict::ExplorerFolderBackground),
            (ShellView::Desktop, Verdict::Desktop),
        ] {
            let shell_view = Some(view);
            let chain = [node(UIA_ListControlTypeId)];
            assert_eq!(
                verdict(&Facts {
                    chain: &chain,
                    shell_view,
                    ..Facts::default()
                })
                .0,
                expected
            );
            let chain = [
                node(UIA_GroupControlTypeId),
                node(UIA_ListItemControlTypeId),
                node(UIA_ListControlTypeId),
            ];
            assert_eq!(
                verdict(&Facts {
                    chain: &chain,
                    shell_view,
                    ..Facts::default()
                })
                .0,
                Verdict::CanvasOrUnknown
            );
            let chain = [node(UIA_PaneControlTypeId), node(UIA_ToolBarControlTypeId)];
            assert_eq!(
                verdict(&Facts {
                    chain: &chain,
                    shell_view,
                    ..Facts::default()
                })
                .0,
                Verdict::CanvasOrUnknown
            );
        }
    }
}
