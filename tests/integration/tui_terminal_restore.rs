//! The terminal restore path, in a test binary of its own: it arms and
//! disarms process-wide flags, which a test running in parallel — or a panic
//! hook another test installed — would otherwise disarm in between.

/// Bracketed paste or mouse reporting left on makes a shell see every later
/// paste wrapped in `200~` / `201~` and every click as escape codes: once
/// switched on, the restore path must switch them off, and only once.
///
/// Mouse reporting stays armed for the session even after the operator
/// switched it off with a key: that change goes through the screen's writer,
/// which may not have flushed it when the process ends, so the restore path
/// switches it off whenever it was armed.
#[test]
fn restore_switches_bracketed_paste_and_mouse_reporting_off_once_they_were_switched_on() {
    const PASTE_ON: &str = "\x1b[?2004h";
    const PASTE_OFF: &str = "\x1b[?2004l";
    const BUTTONS_ON: &str = "\x1b[?1000h";
    const SGR_ON: &str = "\x1b[?1006h";
    const BUTTONS_OFF: &str = "\x1b[?1000l";

    let mut entered = Vec::new();
    narsor::tui::arm_bracketed_paste(&mut entered);
    narsor::tui::arm_mouse_capture(&mut entered);
    let entered = String::from_utf8_lossy(&entered).into_owned();
    assert!(entered.contains(PASTE_ON), "{entered:?}");
    assert!(entered.contains(BUTTONS_ON), "{entered:?}");
    assert!(entered.contains(SGR_ON), "{entered:?}");
    assert!(
        !entered.contains("\x1b[?1003h") && !entered.contains("\x1b[?1002h"),
        "no motion or drag reporting: {entered:?}"
    );

    let mut restored = Vec::new();
    narsor::tui::restore_terminal_to(&mut restored);
    let restored = String::from_utf8_lossy(&restored).into_owned();
    assert!(restored.contains(PASTE_OFF), "{restored:?}");
    assert!(restored.contains(BUTTONS_OFF), "{restored:?}");

    let mut again = Vec::new();
    narsor::tui::restore_terminal_to(&mut again);
    let again = String::from_utf8_lossy(&again).into_owned();
    assert!(
        !again.contains(PASTE_OFF) && !again.contains(BUTTONS_OFF),
        "a second restore has nothing left to switch off: {again:?}"
    );
}
