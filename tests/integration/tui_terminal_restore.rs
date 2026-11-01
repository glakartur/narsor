//! The terminal restore path, in a test binary of its own: it arms and
//! disarms process-wide flags, which a test running in parallel — or a panic
//! hook another test installed — would otherwise disarm in between.

/// Bracketed paste left on makes a shell see every later paste wrapped in
/// `200~` / `201~`: once switched on, the restore path must switch it off,
/// and only once.
#[test]
fn restore_switches_bracketed_paste_off_once_it_was_switched_on() {
    const ON: &str = "\x1b[?2004h";
    const OFF: &str = "\x1b[?2004l";

    let mut entered = Vec::new();
    narsor::tui::arm_bracketed_paste(&mut entered);
    assert!(String::from_utf8_lossy(&entered).contains(ON));

    let mut restored = Vec::new();
    narsor::tui::restore_terminal_to(&mut restored);
    let restored = String::from_utf8_lossy(&restored).into_owned();
    assert!(restored.contains(OFF), "{restored:?}");

    let mut again = Vec::new();
    narsor::tui::restore_terminal_to(&mut again);
    assert!(
        !String::from_utf8_lossy(&again).contains(OFF),
        "a second restore has nothing left to switch off"
    );
}
