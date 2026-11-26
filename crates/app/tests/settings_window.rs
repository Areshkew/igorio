//! The hotkey service as the settings window drives it. Registers no hotkeys.

use std::thread;
use std::time::{Duration, Instant};

use igorio_app::settings_window::ShortcutService;
use igorio_app::shortcuts::{Action, Hotkeys, Registration, RegistrationReport, Shortcuts};

#[test]
fn tagged_reports_come_back_in_order_behind_pause_and_resume() {
    let hotkeys = Hotkeys::start(&Shortcuts::none()).expect("hotkey thread");
    let service: &dyn ShortcutService = &hotkeys;
    service.set_paused(true);
    assert!(service.report_after_commands(7));
    service.apply(&Shortcuts::none());
    service.set_paused(false);
    assert!(service.report_after_commands(8));
    let mut reports: Vec<RegistrationReport> = Vec::new();
    let until = Instant::now() + Duration::from_secs(2);
    while reports.len() < 2 && Instant::now() < until {
        reports.extend(service.reports());
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(reports.iter().map(|report| report.tag).collect::<Vec<_>>(), [7, 8]);
    for report in &reports {
        assert_eq!(report.entries.len(), Action::ALL.len());
        for (action, status) in &report.entries {
            let expected =
                if action.available() { Registration::NotSet } else { Registration::Unavailable };
            assert_eq!(*status, expected, "{action:?}");
        }
    }
    assert!(service.reports().is_empty());
    assert!(hotkeys.shutdown());
}
