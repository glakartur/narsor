//! The usage limits of the harnesses' subscriptions (U7, R9, R15).
//!
//! The interface reads `GET /control/v1/usage` when it starts and then every
//! [`USAGE_PERIOD`], on a clock of its own: not the schedule of the task list
//! ([`super::super::refresh::RefreshSchedule`] has a backoff and can be
//! switched off), so `NARSOR_TUI_REFRESH_INTERVAL=none` does not stop it. The
//! next read is due a period after the previous one finished, whatever it
//! brought.
//!
//! Only the last answer that came is kept. An answer replaces the state; a
//! failure clears it — a limit the interface cannot confirm is not shown. A
//! failure is not a failure of the connection: the header and the bar stay as
//! they are. The answer closes nothing and moves no focus.
//!
//! The read is a job of [`super::background`] under [`JobKey::Usage`]; it
//! does not animate the loading marker, since nothing on the screen waits for
//! it.
//!
//! # On the bar (U8, R10–R16)
//!
//! [`bar_spans`] writes the state for the right of the bottom bar:
//!
//! ```text
//! claude 5h 1% ↻4h10m · 7d 90% ↻2d3h | codex 5h 12% ↻3h0m
//! ```
//!
//! A harness goes by its short name ([`narsor_protocol::harness`]), its limits
//! follow apart by ` · `, harnesses apart by ` | `, in the order the server
//! sent. A limit near its end is in [`Role::Attention`], a rejected one in
//! [`Role::Problem`]; without colour both carry a `!` before the window, so
//! the sign survives a cut of the end. Where the room is short, the last
//! harness goes first, whole; one harness still too long is cut at the end,
//! span by span so every visible part keeps its look, and closed with `…`.
//! Text from the server goes through [`one_line`].

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use narsor_protocol::controller::{
    HarnessUsageResponse, HarnessUsageView, UsageLimit, UsageStatus,
};
use narsor_protocol::harness;
use ratatui::text::Span;

use crate::api::ClientError;
use crate::usage_text::{percent, time_to_reset};

use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::background::{JobKey, Outcome};
use super::TaskApp;

/// How long after a read finished the next one is due.
pub const USAGE_PERIOD: Duration = Duration::from_secs(15);

impl TaskApp {
    /// The usage limits as last read: `None` before the first answer, after a
    /// failed read, and when the server holds no fresh report.
    pub fn harness_usage(&self) -> Option<&HarnessUsageResponse> {
        self.harness_usage.as_ref()
    }

    /// Takes `usage` in as an answer of the server would: what a test of the
    /// drawing sets. A response without harnesses is no state.
    pub fn set_harness_usage(&mut self, usage: Option<HarnessUsageResponse>) {
        self.harness_usage = usage.filter(|usage| !usage.harnesses.is_empty());
    }

    /// When the next read of the usage limits is due; `None` while one runs.
    pub fn next_usage_due(&self) -> Option<Instant> {
        if self.is_reading_usage() {
            return None;
        }
        Some(self.next_usage_at)
    }

    /// Whether the usage limits are being read.
    pub fn is_reading_usage(&self) -> bool {
        self.background.is_running(&JobKey::Usage)
    }

    /// Starts the read of the usage limits when it is due at `now` and none
    /// runs. Returns whether one was started.
    pub fn start_usage_if_due(&mut self, now: Instant) -> bool {
        if now < self.next_usage_at {
            return false;
        }
        let client = self.client.clone();
        self.background.start(JobKey::Usage, async move {
            let read = client.control_usage().await;
            Outcome::new(move |app: &mut TaskApp| app.on_usage_read(read, Instant::now()))
        })
    }

    /// The read is back at `now`.
    fn on_usage_read(&mut self, read: Result<HarnessUsageResponse, ClientError>, now: Instant) {
        self.set_harness_usage(read.ok());
        self.next_usage_at = now + USAGE_PERIOD;
    }
}

/// Between two limits of one harness.
const LIMIT_SEPARATOR: &str = " · ";

/// Between two harnesses.
const HARNESS_SEPARATOR: &str = " | ";

/// What closes a text cut short.
const ELLIPSIS: &str = "…";

/// The usage limits of `usage` at `now`, in at most `width` columns: whole
/// harnesses from the last while more than one is left, then the end of the
/// text with `…`.
pub fn bar_spans(
    usage: &HarnessUsageResponse,
    now: DateTime<Utc>,
    styles: &Styles,
    width: usize,
) -> Vec<Span<'static>> {
    let mut segments: Vec<_> = usage
        .harnesses
        .iter()
        .map(|view| harness_segment(view, now, styles))
        .collect();
    while segments.len() > 1 && joined_width(&segments) > width {
        segments.pop();
    }
    cut(join(segments), width)
}

/// One harness: its short name, then its limits.
fn harness_segment(
    view: &HarnessUsageView,
    now: DateTime<Utc>,
    styles: &Styles,
) -> Vec<Span<'static>> {
    let name = harness::find(&view.harness).map_or_else(
        || one_line(&view.harness),
        |info| info.short_name.to_string(),
    );
    let mut spans = vec![Span::raw(name)];
    for (index, limit) in view.limits.iter().enumerate() {
        spans.push(Span::raw(if index == 0 { " " } else { LIMIT_SEPARATOR }));
        spans.push(limit_span(limit, now, styles));
    }
    spans
}

/// One limit: `[!]window percent% ↻time`, in the role of its status.
fn limit_span(limit: &UsageLimit, now: DateTime<Utc>, styles: &Styles) -> Span<'static> {
    let role = match limit.status {
        Some(UsageStatus::AllowedWarning) => Some(Role::Attention),
        Some(UsageStatus::Rejected) => Some(Role::Problem),
        Some(UsageStatus::Allowed | UsageStatus::Unrecognized) | None => None,
    };
    let mark = if role.is_some() && styles.no_color() {
        "!"
    } else {
        ""
    };
    let text = format!(
        "{mark}{} {}% ↻{}",
        one_line(&limit.window),
        percent(limit.utilization),
        time_to_reset(now, limit.resets_at),
    );
    match role {
        Some(role) => Span::styled(text, styles.of(role)),
        None => Span::raw(text),
    }
}

fn width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

/// The columns of `segments` joined by [`HARNESS_SEPARATOR`].
fn joined_width(segments: &[Vec<Span<'static>>]) -> usize {
    let separators = segments.len().saturating_sub(1) * HARNESS_SEPARATOR.chars().count();
    segments.iter().map(|spans| width(spans)).sum::<usize>() + separators
}

fn join(segments: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut joined = Vec::new();
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            joined.push(Span::raw(HARNESS_SEPARATOR));
        }
        joined.extend(segment);
    }
    joined
}

/// `spans` in at most `width` columns: as they are when they fit, otherwise
/// cut at the end with `…` in the look of the span it cuts.
fn cut(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if self::width(&spans) <= width {
        return spans;
    }
    let mut room = width.saturating_sub(1);
    let mut kept = Vec::new();
    for span in spans {
        let length = span.content.chars().count();
        if length <= room {
            room -= length;
            kept.push(span);
            continue;
        }
        if width > 0 {
            if room > 0 {
                let head: String = span.content.chars().take(room).collect();
                kept.push(Span::styled(head, span.style));
            }
            kept.push(Span::styled(ELLIPSIS, span.style));
        }
        break;
    }
    kept
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use chrono::{TimeZone, Utc};
    use narsor_protocol::controller::{HarnessUsageView, UsageLimit};

    use super::*;
    use crate::api::ApiClient;

    fn app() -> TaskApp {
        let client = ApiClient::new(SocketAddr::from(([127, 0, 0, 1], 9)), "token");
        TaskApp::new(client, "local", None)
    }

    fn claude() -> HarnessUsageResponse {
        let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        HarnessUsageResponse {
            harnesses: vec![HarnessUsageView {
                harness: "claude-code".into(),
                observed_at: at,
                received_at: at,
                limits: vec![UsageLimit {
                    window: "5h".into(),
                    utilization: 0.4,
                    resets_at: at,
                    status: None,
                }],
            }],
        }
    }

    #[test]
    fn an_answer_replaces_the_state_and_a_failure_clears_it() {
        let mut app = app();
        let now = Instant::now();
        app.on_usage_read(Ok(claude()), now);
        assert_eq!(app.harness_usage(), Some(&claude()));
        app.on_usage_read(Err(ClientError::Server("boom".into())), now);
        assert_eq!(app.harness_usage(), None);
    }

    #[test]
    fn an_empty_answer_forgets_the_harness_shown_before() {
        let mut app = app();
        app.set_harness_usage(Some(claude()));
        app.on_usage_read(Ok(HarnessUsageResponse::default()), Instant::now());
        assert_eq!(app.harness_usage(), None);
    }

    #[test]
    fn the_next_read_is_due_a_period_after_the_last_finished() {
        let mut app = app();
        let finished = Instant::now();
        app.on_usage_read(Ok(claude()), finished);
        assert_eq!(app.next_usage_due(), Some(finished + USAGE_PERIOD));
    }

    #[tokio::test]
    async fn the_first_read_is_due_at_once_and_does_not_animate_the_marker() {
        let mut app = app();
        assert!(app.start_usage_if_due(Instant::now()));
        assert!(app.is_reading_usage());
        assert!(!app.is_loading());
        assert_eq!(app.next_usage_due(), None);
        assert!(
            !app.start_usage_if_due(Instant::now()),
            "one read at a time"
        );
    }

    mod bar {
        use chrono::{DateTime, Duration, TimeZone, Utc};
        use narsor_protocol::controller::{
            HarnessUsageResponse, HarnessUsageView, UsageLimit, UsageStatus,
        };
        use ratatui::text::Span;

        use super::super::bar_spans;
        use crate::tui::style::{Role, Styles};

        fn now() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 10, 31, 14, 2, 0).unwrap()
        }

        fn limit(
            window: &str,
            utilization: f64,
            in_: Duration,
            status: Option<UsageStatus>,
        ) -> UsageLimit {
            UsageLimit {
                window: window.into(),
                utilization,
                resets_at: now() + in_,
                status,
            }
        }

        fn harness(id: &str, limits: Vec<UsageLimit>) -> HarnessUsageView {
            HarnessUsageView {
                harness: id.into(),
                observed_at: now(),
                received_at: now(),
                limits,
            }
        }

        /// The state of AE1: `5h` at 1%, `7d` at 90% and near its end.
        fn claude(status_7d: UsageStatus) -> HarnessUsageView {
            harness(
                "claude-code",
                vec![
                    limit(
                        "5h",
                        0.01,
                        Duration::hours(4) + Duration::minutes(10),
                        Some(UsageStatus::Allowed),
                    ),
                    limit(
                        "7d",
                        0.9,
                        Duration::days(2) + Duration::hours(3),
                        Some(status_7d),
                    ),
                ],
            )
        }

        fn codex() -> HarnessUsageView {
            harness("codex", vec![limit("5h", 0.12, Duration::hours(3), None)])
        }

        fn state(harnesses: Vec<HarnessUsageView>) -> HarnessUsageResponse {
            HarnessUsageResponse { harnesses }
        }

        fn text(spans: &[Span<'_>]) -> String {
            spans.iter().map(|span| span.content.as_ref()).collect()
        }

        /// The span holding `needle`.
        fn span_of<'a>(spans: &'a [Span<'a>], needle: &str) -> &'a Span<'a> {
            spans
                .iter()
                .find(|span| span.content.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} is not in {:?}", text(spans)))
        }

        #[test]
        fn a_harness_reads_its_short_name_and_its_limits() {
            let spans = bar_spans(
                &state(vec![claude(UsageStatus::AllowedWarning)]),
                now(),
                &Styles::new(false),
                200,
            );
            assert_eq!(text(&spans), "claude 5h 1% ↻4h10m · 7d 90% ↻2d3h");
        }

        #[test]
        fn a_warning_is_in_attention_and_a_rejection_in_problem() {
            let styles = Styles::new(false);
            let warned = bar_spans(
                &state(vec![claude(UsageStatus::AllowedWarning)]),
                now(),
                &styles,
                200,
            );
            assert_eq!(span_of(&warned, "7d").style, styles.of(Role::Attention));
            assert_eq!(span_of(&warned, "5h").style, Default::default());
            let rejected = bar_spans(
                &state(vec![claude(UsageStatus::Rejected)]),
                now(),
                &styles,
                200,
            );
            assert_eq!(span_of(&rejected, "7d").style, styles.of(Role::Problem));
        }

        #[test]
        fn without_colour_a_warning_and_a_rejection_carry_a_sign() {
            let styles = Styles::new(true);
            for status in [UsageStatus::AllowedWarning, UsageStatus::Rejected] {
                let spans = bar_spans(&state(vec![claude(status)]), now(), &styles, 200);
                assert_eq!(text(&spans), "claude 5h 1% ↻4h10m · !7d 90% ↻2d3h");
            }
        }

        #[test]
        fn in_colour_there_is_no_sign() {
            let spans = bar_spans(
                &state(vec![claude(UsageStatus::Rejected)]),
                now(),
                &Styles::new(false),
                200,
            );
            assert!(!text(&spans).contains('!'), "{:?}", text(&spans));
        }

        #[test]
        fn harnesses_stand_apart_in_the_order_given() {
            let spans = bar_spans(
                &state(vec![claude(UsageStatus::Allowed), codex()]),
                now(),
                &Styles::new(false),
                200,
            );
            assert_eq!(
                text(&spans),
                "claude 5h 1% ↻4h10m · 7d 90% ↻2d3h | codex 5h 12% ↻3h0m"
            );
        }

        #[test]
        fn the_last_harness_goes_whole_when_the_room_is_short() {
            let first = "claude 5h 1% ↻4h10m · 7d 90% ↻2d3h";
            let width = first.chars().count() + 5;
            let spans = bar_spans(
                &state(vec![claude(UsageStatus::Allowed), codex()]),
                now(),
                &Styles::new(false),
                width,
            );
            assert_eq!(text(&spans), first);
        }

        #[test]
        fn one_harness_longer_than_the_room_is_cut_with_an_ellipsis() {
            let spans = bar_spans(
                &state(vec![claude(UsageStatus::Allowed), codex()]),
                now(),
                &Styles::new(false),
                25,
            );
            let shown = text(&spans);
            assert_eq!(shown, "claude 5h 1% ↻4h10m · 7d…");
            assert_eq!(shown.chars().count(), 25);
        }

        #[test]
        fn a_cut_limit_keeps_its_colour() {
            let styles = Styles::new(false);
            let spans = bar_spans(
                &state(vec![claude(UsageStatus::AllowedWarning)]),
                now(),
                &styles,
                26,
            );
            assert_eq!(text(&spans), "claude 5h 1% ↻4h10m · 7d …");
            assert_eq!(span_of(&spans, "7d").style, styles.of(Role::Attention));
            assert_eq!(span_of(&spans, "…").style, styles.of(Role::Attention));
        }

        #[test]
        fn a_cut_keeps_the_sign_at_the_start_of_a_limit() {
            let spans = bar_spans(
                &state(vec![claude(UsageStatus::Rejected)]),
                now(),
                &Styles::new(true),
                26,
            );
            assert_eq!(text(&spans), "claude 5h 1% ↻4h10m · !7d…");
        }

        #[test]
        fn no_room_is_no_text_and_one_column_is_an_ellipsis() {
            let usage = state(vec![claude(UsageStatus::Allowed)]);
            let styles = Styles::new(false);
            assert!(bar_spans(&usage, now(), &styles, 0).is_empty());
            assert_eq!(text(&bar_spans(&usage, now(), &styles, 1)), "…");
        }

        #[test]
        fn every_width_fits() {
            let usage = state(vec![claude(UsageStatus::AllowedWarning), codex()]);
            for width in 0..80 {
                let spans = bar_spans(&usage, now(), &Styles::new(false), width);
                assert!(text(&spans).chars().count() <= width, "{width}");
            }
        }

        #[test]
        fn a_reset_already_past_reads_zero_minutes() {
            let usage = state(vec![harness(
                "claude-code",
                vec![limit("5h", 0.5, -Duration::minutes(3), None)],
            )]);
            let spans = bar_spans(&usage, now(), &Styles::new(false), 200);
            assert_eq!(text(&spans), "claude 5h 50% ↻0m");
        }

        #[test]
        fn the_percent_is_rounded() {
            for (utilization, shown) in [(0.005, "1%"), (0.004, "0%"), (1.0, "100%")] {
                let usage = state(vec![harness(
                    "claude-code",
                    vec![limit("5h", utilization, Duration::minutes(45), None)],
                )]);
                let spans = bar_spans(&usage, now(), &Styles::new(false), 200);
                assert_eq!(text(&spans), format!("claude 5h {shown} ↻45m"));
            }
        }

        #[test]
        fn an_unknown_harness_and_a_window_are_put_on_one_safe_line() {
            let usage = state(vec![harness(
                "odd\u{1b}[31mone\nx",
                vec![limit("5\u{7}h", 0.5, Duration::minutes(45), None)],
            )]);
            let shown = text(&bar_spans(&usage, now(), &Styles::new(false), 200));
            assert!(!shown.chars().any(char::is_control), "{shown:?}");
            assert!(shown.starts_with("odd"), "{shown:?}");
        }
    }
}
