//! What a whole task used, as one line: its cost and its tokens in and out,
//! and the part of them the subagents used.
//!
//! The numbers are those the harness reported on each timeline position, added
//! up as they are: `in` and `out` are the `input` and `output` of
//! [`TokenCounts`], and the cache counts are neither added to them nor read.

use narsor_protocol::api::{Position, PositionUsage, TokenCounts};

const THOUSAND: u64 = 1_000;
const MILLION: u64 = 1_000_000;
/// Below this a count still rounds to a thousand, not to a million.
const MILLION_ROUNDED: u64 = MILLION - THOUSAND / 2;
const UNKNOWN: &str = "—";

/// The usage of all positions of a task, a part no position reported being
/// `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TaskUsage {
    pub total: Option<TokenCounts>,
    pub subagents: Option<TokenCounts>,
    pub subagent_count: Option<u32>,
    pub cost_usd: Option<f64>,
}

/// The sum of the usage of `positions`; `None` when no position reported any.
pub fn sum(positions: &[Position]) -> Option<TaskUsage> {
    let mut sum: Option<TaskUsage> = None;
    for usage in positions.iter().filter_map(|position| position.usage) {
        let so_far = sum.unwrap_or_default();
        sum = Some(TaskUsage {
            total: add(so_far.total, usage.total, TokenCounts::plus),
            subagents: add(so_far.subagents, usage.subagents, TokenCounts::plus),
            subagent_count: add(
                so_far.subagent_count,
                usage.subagent_count,
                u32::saturating_add,
            ),
            cost_usd: add(so_far.cost_usd, usage.cost_usd, |a, b| a + b),
        });
    }
    sum.filter(|usage| {
        usage.total.is_some() || usage.subagents.is_some() || usage.cost_usd.is_some()
    })
}

fn add<T: Copy>(a: Option<T>, b: Option<T>, plus: impl Fn(T, T) -> T) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(plus(a, b)),
        (a, b) => a.or(b),
    }
}

/// `$1.20 · in 120k out 8k`, with `· w tym subagenci in 40k out 3k (2)` when
/// `with_subagents` and the subagents used anything. A part that is unknown
/// reads `—`.
pub fn line(usage: &TaskUsage, with_subagents: bool) -> String {
    let cost = usage
        .cost_usd
        .map_or_else(|| UNKNOWN.to_string(), |cost| format!("${cost:.2}"));
    let mut text = format!("{cost} · {}", tokens(usage.total));
    let subagents = usage
        .subagents
        .filter(|counts| counts.input + counts.output > 0);
    if let (true, Some(counts)) = (with_subagents, subagents) {
        text.push_str(&format!(" · w tym subagenci {}", tokens(Some(counts))));
        if let Some(count) = usage.subagent_count.filter(|count| *count > 0) {
            text.push_str(&format!(" ({count})"));
        }
    }
    text
}

/// What one step used, as the cells of a table row: each one is empty when
/// the harness did not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepCells {
    pub cost: String,
    pub input: String,
    pub output: String,
    pub subagent_count: String,
    pub subagent_input: String,
    pub subagent_output: String,
}

impl StepCells {
    /// Whether the step has anything to show on the main part of its row.
    pub fn has_main(&self) -> bool {
        !(self.cost.is_empty() && self.input.is_empty())
    }

    /// Whether the step has subagents to show.
    pub fn has_subagents(&self) -> bool {
        !self.subagent_input.is_empty()
    }
}

/// The cells of the usage of one step; all empty for a step without a report.
/// Subagents that used nothing, or were not reported, are empty.
pub fn step_cells(usage: Option<&PositionUsage>) -> StepCells {
    let Some(usage) = usage else {
        return StepCells::default();
    };
    let mut cells = StepCells {
        cost: usage
            .cost_usd
            .map_or_else(String::new, |cost| format!("${cost:.2}")),
        ..StepCells::default()
    };
    if let Some(counts) = usage.total {
        cells.input = format!("↓{}", scaled(counts.input));
        cells.output = format!("↑{}", scaled(counts.output));
    }
    if let Some(counts) = usage
        .subagents
        .filter(|counts| counts.input + counts.output > 0)
    {
        cells.subagent_input = format!("↓{}", scaled(counts.input));
        cells.subagent_output = format!("↑{}", scaled(counts.output));
        if let Some(count) = usage.subagent_count.filter(|count| *count > 0) {
            cells.subagent_count = format!("{count}×");
        }
    }
    cells
}

/// What became of a step's subagents, e.g. `sub 11 ✓10 ✗1`: how many were
/// started and finished, then each count that is not zero (`✗` failed, `†`
/// killed, `⊘` refused). Empty when none were started.
pub fn subagent_stats_text(usage: Option<&PositionUsage>) -> String {
    let Some(stats) = usage.and_then(|usage| usage.subagent_stats) else {
        return String::new();
    };
    if stats.spawned == 0 {
        return String::new();
    }
    let mut text = format!("sub {} ✓{}", stats.spawned, stats.completed);
    for (mark, count) in [
        ("✗", stats.failed),
        ("†", stats.killed),
        ("⊘", stats.refused),
    ] {
        if count > 0 {
            text.push_str(&format!(" {mark}{count}"));
        }
    }
    text
}

fn tokens(counts: Option<TokenCounts>) -> String {
    match counts {
        Some(counts) => format!("in {} out {}", scaled(counts.input), scaled(counts.output)),
        None => UNKNOWN.to_string(),
    }
}

/// `950`, `1.5k`, `120k`, `2.4M`: the count with at most the three digits.
fn scaled(count: u64) -> String {
    if count < THOUSAND {
        count.to_string()
    } else if count < MILLION_ROUNDED {
        unit(count, THOUSAND, "k")
    } else {
        unit(count, MILLION, "M")
    }
}

fn unit(count: u64, divisor: u64, suffix: &str) -> String {
    if count / divisor >= 10 {
        return format!("{}{suffix}", (count + divisor / 2) / divisor);
    }
    let tenths = (count * 10 + divisor / 2) / divisor;
    if tenths.is_multiple_of(10) {
        format!("{}{suffix}", tenths / 10)
    } else {
        format!("{}.{}{suffix}", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use narsor_protocol::api::{ConversationState, PositionStatus, PositionUsage, SubagentStats};

    use super::*;

    fn counts(input: u64, output: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            ..TokenCounts::default()
        }
    }

    fn position(usage: Option<PositionUsage>) -> Position {
        Position {
            label: "step #1".into(),
            state_id: "step".into(),
            visit: 1,
            action_execution_id: None,
            action_type: None,
            status: PositionStatus::Succeeded,
            started_at: None,
            ended_at: None,
            error: None,
            resumed_harness_session: None,
            harness_session_interrupted: false,
            retry_pending: false,
            conversation: ConversationState::Recorded,
            session_number: Some(1),
            attempts: Vec::new(),
            harness: None,
            model: None,
            effort: None,
            harness_session_id: None,
            usage,
        }
    }

    fn used(total: (u64, u64), subagents: Option<(u64, u64)>, cost: f64) -> PositionUsage {
        PositionUsage {
            total: Some(counts(total.0, total.1)),
            subagents: subagents.map(|(input, output)| counts(input, output)),
            subagent_count: subagents.map(|_| 1),
            subagent_stats: None,
            cost_usd: Some(cost),
        }
    }

    #[test]
    fn the_steps_of_a_task_are_added_up() {
        let positions = [
            position(Some(used((80_000, 5_000), Some((30_000, 2_000)), 0.8))),
            position(None),
            position(Some(used((40_000, 3_000), Some((10_000, 1_000)), 0.4))),
        ];
        let usage = sum(&positions).unwrap();
        assert_eq!(
            line(&usage, true),
            "$1.20 · in 120k out 8k · w tym subagenci in 40k out 3k (2)"
        );
        assert_eq!(line(&usage, false), "$1.20 · in 120k out 8k");
    }

    #[test]
    fn a_task_without_any_report_has_no_usage() {
        assert_eq!(sum(&[]), None);
        assert_eq!(sum(&[position(None)]), None);
        assert_eq!(sum(&[position(Some(PositionUsage::default()))]), None);
    }

    #[test]
    fn a_part_nobody_reported_reads_a_dash() {
        let only_cost = PositionUsage {
            cost_usd: Some(0.5),
            ..PositionUsage::default()
        };
        let only_tokens = PositionUsage {
            total: Some(counts(1_500, 20)),
            ..PositionUsage::default()
        };
        assert_eq!(
            line(&sum(&[position(Some(only_cost))]).unwrap(), true),
            "$0.50 · —"
        );
        assert_eq!(
            line(&sum(&[position(Some(only_tokens))]).unwrap(), true),
            "— · in 1.5k out 20"
        );
    }

    #[test]
    fn subagents_that_used_nothing_are_not_shown() {
        let none = used((1_000, 100), None, 0.1);
        let zero = used((1_000, 100), Some((0, 0)), 0.1);
        for usage in [none, zero] {
            let usage = sum(&[position(Some(usage))]).unwrap();
            assert_eq!(line(&usage, true), "$0.10 · in 1k out 100");
        }
    }

    #[test]
    fn counts_are_written_in_thousands_and_millions() {
        for (count, text) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (1_550, "1.6k"),
            (9_949, "9.9k"),
            (10_000, "10k"),
            (999_499, "999k"),
            (999_500, "1M"),
            (2_400_000, "2.4M"),
            (15_000_000, "15M"),
        ] {
            assert_eq!(scaled(count), text, "{count}");
        }
    }

    #[test]
    fn the_cost_is_rounded_to_cents() {
        let at = |cost| {
            let usage = sum(&[position(Some(used((1, 1), None, cost)))]).unwrap();
            line(&usage, false)
        };
        assert!(at(0.004).starts_with("$0.00 "));
        assert!(at(1.236).starts_with("$1.24 "));
    }

    #[test]
    fn the_cache_does_not_change_what_in_counts() {
        let cached = PositionUsage {
            total: Some(TokenCounts {
                input: 2_000,
                output: 100,
                cache_read: 500_000,
                cache_write: 90_000,
            }),
            ..PositionUsage::default()
        };
        let usage = sum(&[position(Some(cached))]).unwrap();
        assert_eq!(line(&usage, true), "— · in 2k out 100");
    }

    #[test]
    fn a_step_reads_as_cells() {
        let cells = step_cells(Some(&used((120_000, 8_000), Some((40_000, 3_000)), 0.42)));
        assert_eq!(
            (
                cells.cost.as_str(),
                cells.input.as_str(),
                cells.output.as_str(),
                cells.subagent_count.as_str(),
                cells.subagent_input.as_str(),
                cells.subagent_output.as_str(),
            ),
            ("$0.42", "↓120k", "↑8k", "1×", "↓40k", "↑3k")
        );
        assert!(cells.has_main() && cells.has_subagents());
    }

    #[test]
    fn what_a_step_did_not_report_is_an_empty_cell() {
        assert_eq!(step_cells(None), StepCells::default());
        let only_cost = PositionUsage {
            cost_usd: Some(0.5),
            ..PositionUsage::default()
        };
        let cells = step_cells(Some(&only_cost));
        assert_eq!(cells.cost, "$0.50");
        assert!(cells.input.is_empty() && !cells.has_subagents());
        let only_tokens = PositionUsage {
            total: Some(counts(950, 2_400_000)),
            ..PositionUsage::default()
        };
        let cells = step_cells(Some(&only_tokens));
        assert_eq!(
            (cells.input.as_str(), cells.output.as_str()),
            ("↓950", "↑2.4M")
        );
        assert!(cells.cost.is_empty());
        assert!(!step_cells(Some(&PositionUsage::default())).has_main());
    }

    #[test]
    fn the_subagent_stats_show_only_the_counts_that_are_not_zero() {
        let usage = |spawned, completed, failed, killed, refused| PositionUsage {
            subagent_stats: Some(SubagentStats {
                spawned,
                completed,
                failed,
                killed,
                refused,
            }),
            ..PositionUsage::default()
        };
        assert_eq!(
            subagent_stats_text(Some(&usage(11, 11, 0, 0, 0))),
            "sub 11 ✓11"
        );
        assert_eq!(
            subagent_stats_text(Some(&usage(5, 2, 1, 1, 1))),
            "sub 5 ✓2 ✗1 †1 ⊘1"
        );
        assert_eq!(subagent_stats_text(Some(&usage(0, 0, 0, 0, 0))), "");
        assert_eq!(subagent_stats_text(None), "");
    }

    #[test]
    fn subagents_that_used_nothing_are_no_cells() {
        for subagents in [None, Some((0, 0))] {
            let cells = step_cells(Some(&used((1_000, 100), subagents, 0.1)));
            assert!(!cells.has_subagents() && cells.subagent_count.is_empty());
        }
    }

    #[test]
    fn the_cache_does_not_change_the_cells() {
        let cached = PositionUsage {
            total: Some(TokenCounts {
                input: 2_000,
                output: 100,
                cache_read: 500_000,
                cache_write: 90_000,
            }),
            ..PositionUsage::default()
        };
        let cells = step_cells(Some(&cached));
        assert_eq!(
            (cells.input.as_str(), cells.output.as_str()),
            ("↓2k", "↑100")
        );
    }
}
