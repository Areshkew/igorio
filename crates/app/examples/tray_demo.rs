//! Interactive tray demo. Click the icon (left or right) to open the menu.
//! Actions print to the console. Runs until you choose Quit.

use std::cell::Cell;
use std::rc::Rc;

use igorio_app::tray::{MenuItem, Tray};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostQuitMessage, TranslateMessage,
};

const HELLO: u32 = 1;
const COPY: u32 = 2;
const TOGGLE: u32 = 3;
const DISABLED: u32 = 4;
const SUB_ONE: u32 = 5;
const SUB_TWO: u32 = 6;
const QUIT: u32 = 7;

fn main() -> windows::core::Result<()> {
    let checked = Rc::new(Cell::new(true));
    let menu_checked = checked.clone();
    let build = Box::new(move || {
        vec![
            MenuItem::Label("Igorio tray demo".into()),
            MenuItem::action(HELLO, "Say Hello"),
            MenuItem::action(COPY, "Copy & Paste (prints)"),
            MenuItem::Action {
                id: TOGGLE,
                label: "Keep Every Screenshot".into(),
                enabled: true,
                checked: menu_checked.get(),
            },
            MenuItem::Action {
                id: DISABLED,
                label: "Restore Last Screenshot".into(),
                enabled: false,
                checked: false,
            },
            MenuItem::Submenu {
                label: "Recent Notes".into(),
                items: vec![
                    MenuItem::action(SUB_ONE, "First note"),
                    MenuItem::action(SUB_TWO, "Second note"),
                ],
            },
            MenuItem::Separator,
            MenuItem::action(QUIT, "Quit Igorio Demo"),
        ]
    });
    let on_action = Box::new(move |id| match id {
        HELLO => println!("Hello"),
        COPY => println!("Copy & Paste chosen"),
        TOGGLE => {
            checked.set(!checked.get());
            println!("Keep Every Screenshot: {}", checked.get());
        }
        SUB_ONE | SUB_TWO => println!("Note {}", id - SUB_ONE + 1),
        QUIT => {
            println!("Quit");
            // SAFETY: posts WM_QUIT to this thread's queue; no pointers.
            unsafe { PostQuitMessage(0) };
        }
        other => println!("Unexpected id {other}"),
    });
    let tray = Tray::new("Igorio tray demo", build, on_action)?;
    println!("Tray icon added. Click it to open the menu; choose Quit to exit.");

    let mut msg = MSG::default();
    // SAFETY: `msg` is a live MSG; standard message loop on the thread that owns the tray.
    // GetMessageW returns -1 on error, 0 on WM_QUIT.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: `msg` was filled by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drop(tray);
    Ok(())
}
