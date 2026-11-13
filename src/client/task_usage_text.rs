//! What a whole task used, as one line: its cost and its tokens in and out,
//! and the part of them the subagents used.
//!
//! The numbers are those the harness reported on each timeline position, added
//! up as they are: `↑` and `↓` are the `input` and `output` of
//! [`TokenCounts`], and the cache counts are neither added to them nor read.
//!
//! What the conductor used is reported apart from what the agents used, and
//! stays apart: the agent's numbers never include it, and the combined
//! numbers are the sum of the two, made here.

use narsor_protocol::api::{Position, PositionUsage, TokenCounts};

/// The icon of the subagents, in the task header and in every step row.
pub const SUBAGENTS_ICON: &str = "👥";

const THOUSAND: u64 = 1_000;
const MILLION: u64 = 1_000_000;
/// Below this a count still rounds to a thousand, not to a million.
const MILLION_ROUNDED: u64 = MILLION - THOUSAND / 2;

/// The usage of all positions of a task, a part no position reported being
/// `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TaskUsage {
    pub total: Option<TokenCounts>,
    pub subagents: Option<TokenCounts>,
    pub subagent_count: Option<u32>,
    /// The tokens of the subagents together, without a split in and out.
    pub subagent_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    /// What the subagents cost, when the harness says. It never does today,
    /// so it stays `None` and the line shows no cost for them.
    pub subagent_cost_usd: Option<f64>,
    /// What the conductor used when it decided, apart from the above.
    pub conductor_total: Option<TokenCounts>,
    pub conductor_cost_usd: Option<f64>,
}

impl TaskUsage {
    /// Whether the conductor reported anything.
    pub fn has_conductor(&self) -> bool {
        self.conductor_total.is_some() || self.conductor_cost_usd.is_some()
    }

    /// The cost of the agents and the conductor together; `None` when
    /// neither reported one.
    pub fn combined_cost_usd(&self) -> Option<f64> {
        add(self.cost_usd, self.conductor_cost_usd, |a, b| a + b)
    }
}

/// How many subagents one step started: the count the harness gave, else
/// the number of subagents in its stats; `None` when neither is above zero.
/// The task header and the step rows both read the count here.
pub fn subagent_count_of(usage: &PositionUsage) -> Option<u32> {
    usage.subagent_count.filter(|count| *count > 0).or_else(|| {
        usage
            .subagent_stats
            .map(|stats| stats.spawned)
            .filter(|spawned| *spawned > 0)
    })
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
                subagent_count_of(&usage),
                u32::saturating_add,
            ),
            subagent_tokens: add(
                so_far.subagent_tokens,
                usage
                    .subagent_stats
                    .filter(|stats| stats.spawned > 0 && stats.tokens > 0)
                    .map(|stats| stats.tokens),
                u64::saturating_add,
            ),
            cost_usd: add(so_far.cost_usd, usage.cost_usd, |a, b| a + b),
            subagent_cost_usd: None,
            conductor_total: add(
                so_far.conductor_total,
                usage.conductor_total,
                TokenCounts::plus,
            ),
            conductor_cost_usd: add(
                so_far.conductor_cost_usd,
                usage.conductor_cost_usd,
                |a, b| a + b,
            ),
        });
    }
    sum.filter(|usage| {
        usage.total.is_some()
            || usage.subagents.is_some()
            || usage.subagent_count.is_some()
            || usage.subagent_tokens.is_some()
            || usage.cost_usd.is_some()
            || usage.has_conductor()
    })
}

fn add<T: Copy>(a: Option<T>, b: Option<T>, plus: impl Fn(T, T) -> T) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(plus(a, b)),
        (a, b) => a.or(b),
    }
}

/// `razem $68.30 · ✨ $34.15 ↑304 ↓72k (👥 2 $12 ↑100 ↓20k) · ⚡ $34.15 ↑304 ↓72k`:
/// the cost of all, then each group with its cost and its tokens in (`↑`) and
/// out (`↓`): `✨` the agents (their numbers include the subagents'), `👥` the
/// subagents in brackets after them — their count first, then their tokens in
/// and out (`↑` `↓`) or together (`↕`) — and `⚡` the
/// conductor. A group or a part nobody reported is left out, so the line has
/// no placeholder; `None` when there is nothing to say. The subagents are
/// left out when not `with_subagents`.
pub fn line(usage: &TaskUsage, with_subagents: bool) -> Option<String> {
    let subagents = with_subagents.then(|| subagents_block(usage)).flatten();
    let agents = parts_of(usage.cost_usd, usage.total);
    let agents_group = match (agents.is_empty(), subagents) {
        (true, None) => None,
        (true, Some(subagents)) => Some(format!("({subagents})")),
        (false, None) => Some(format!("✨ {agents}")),
        (false, Some(subagents)) => Some(format!("✨ {agents} ({subagents})")),
    };
    let conductor = parts_of(usage.conductor_cost_usd, usage.conductor_total);
    let groups: Vec<String> = [
        usage
            .combined_cost_usd()
            .map(|cost| format!("razem {}", money(cost))),
        agents_group,
        (!conductor.is_empty()).then(|| format!("⚡ {conductor}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!groups.is_empty()).then(|| groups.join(" · "))
}

/// `👥 2 $12 ↑100 ↓20k`: the subagents' count, cost and tokens, each part
/// only when known. Tokens are `↑` `↓` when the harness split them, else `↕`
/// for their sum. A count alone is a block; subagents without a count, a cost
/// or tokens are not.
fn subagents_block(usage: &TaskUsage) -> Option<String> {
    let split = usage
        .subagents
        .filter(|counts| counts.input + counts.output > 0);
    let used = match split {
        Some(_) => parts_of(usage.subagent_cost_usd, split),
        None => [
            usage.subagent_cost_usd.map(money),
            usage
                .subagent_tokens
                .map(|tokens| format!("↕{}", scaled(tokens))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" "),
    };
    let count = usage
        .subagent_count
        .filter(|count| *count > 0)
        .map(|count| count.to_string());
    let parts: Vec<String> = count
        .into_iter()
        .chain((!used.is_empty()).then_some(used))
        .collect();
    (!parts.is_empty()).then(|| format!("{SUBAGENTS_ICON} {}", parts.join(" ")))
}

/// The cost, then the tokens, those known, separated by a space; empty when
/// neither is.
fn parts_of(cost: Option<f64>, counts: Option<TokenCounts>) -> String {
    [cost.map(money), counts.map(tokens)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

fn money(cost: f64) -> String {
    format!("${cost:.2}")
}

/// What one step used, as the cells of a table row: each one is empty when
/// the harness did not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepCells {
    pub cost: String,
    /// What the conductor cost in this step, apart from `cost`.
    pub conductor_cost: String,
    pub input: String,
    pub output: String,
    pub subagent_count: String,
    pub subagent_input: String,
    pub subagent_output: String,
    /// The tokens of the subagents together, when the harness gave no split.
    pub subagent_total: String,
}

impl StepCells {
    /// Whether the step has anything to show on the main part of its row.
    pub fn has_main(&self) -> bool {
        !(self.cost.is_empty() && self.conductor_cost.is_empty() && self.input.is_empty())
    }

    /// Whether the step has subagents to show.
    pub fn has_subagents(&self) -> bool {
        !(self.subagent_count.is_empty()
            && self.subagent_input.is_empty()
            && self.subagent_total.is_empty())
    }
}

/// The cells of the usage of one step; all empty for a step without a report.
/// Subagents come from what the harness said: their tokens in and out when it
/// split them, else their tokens together; the count is [`subagent_count_of`].
/// Subagents that used nothing and were not counted are empty.
pub fn step_cells(usage: Option<&PositionUsage>) -> StepCells {
    let Some(usage) = usage else {
        return StepCells::default();
    };
    let mut cells = StepCells {
        cost: usage
            .cost_usd
            .map_or_else(String::new, |cost| format!("${cost:.2}")),
        conductor_cost: usage
            .conductor_cost_usd
            .map_or_else(String::new, |cost| format!("dyr. ${cost:.2}")),
        ..StepCells::default()
    };
    if let Some(counts) = usage.total {
        cells.input = format!("↑{}", scaled(counts.input));
        cells.output = format!("↓{}", scaled(counts.output));
    }
    if let Some(counts) = usage
        .subagents
        .filter(|counts| counts.input + counts.output > 0)
    {
        cells.subagent_input = format!("↑{}", scaled(counts.input));
        cells.subagent_output = format!("↓{}", scaled(counts.output));
    }
    if let Some(count) = subagent_count_of(usage) {
        cells.subagent_count = format!("{SUBAGENTS_ICON} {count}");
    }
    let stats = usage
        .subagent_stats
        .filter(|stats| stats.spawned > 0 && stats.tokens > 0);
    if let (Some(stats), true) = (stats, cells.subagent_input.is_empty()) {
        cells.subagent_total = format!("↕{}", scaled(stats.tokens));
    }
    cells
}

fn tokens(counts: TokenCounts) -> String {
    format!("↑{} ↓{}", scaled(counts.input), scaled(counts.output))
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
            decision_by: None,
        }
    }

    fn used(total: (u64, u64), subagents: Option<(u64, u64)>, cost: f64) -> PositionUsage {
        PositionUsage {
            total: Some(counts(total.0, total.1)),
            subagents: subagents.map(|(input, output)| counts(input, output)),
            subagent_count: subagents.map(|_| 1),
            subagent_stats: None,
            cost_usd: Some(cost),
            ..PositionUsage::default()
        }
    }

    /// The line, which has to exist.
    fn text(usage: &TaskUsage, with_subagents: bool) -> String {
        line(usage, with_subagents).expect("a line")
    }

    fn full_usage() -> TaskUsage {
        TaskUsage {
            total: Some(counts(304, 72_000)),
            subagents: Some(counts(100, 20_000)),
            subagent_count: Some(2),
            subagent_tokens: None,
            cost_usd: Some(34.15),
            subagent_cost_usd: Some(12.0),
            conductor_total: Some(counts(304, 72_000)),
            conductor_cost_usd: Some(34.15),
        }
    }

    #[test]
    fn the_full_line_has_the_icons_and_the_subagents_in_brackets() {
        assert_eq!(
            text(&full_usage(), true),
            "razem $68.30 · ✨ $34.15 ↑304 ↓72k (👥 2 $12.00 ↑100 ↓20k) · ⚡ $34.15 ↑304 ↓72k"
        );
        assert_eq!(
            text(&full_usage(), false),
            "razem $68.30 · ✨ $34.15 ↑304 ↓72k · ⚡ $34.15 ↑304 ↓72k"
        );
    }

    /// Covers AE4: what nobody reported has no block.
    #[test]
    fn a_line_has_only_the_blocks_with_data() {
        let usage = TaskUsage {
            total: Some(counts(304, 72_000)),
            cost_usd: Some(34.15),
            ..TaskUsage::default()
        };
        assert_eq!(text(&usage, true), "razem $34.15 · ✨ $34.15 ↑304 ↓72k");
    }

    /// Covers AE5: no cost for the subagents, and no dash in its place.
    #[test]
    fn subagents_without_a_cost_show_their_count_and_tokens_only() {
        let usage = TaskUsage {
            total: Some(counts(304, 72_000)),
            subagents: Some(counts(304, 72_000)),
            subagent_count: Some(2),
            ..TaskUsage::default()
        };
        assert_eq!(text(&usage, true), "✨ ↑304 ↓72k (👥 2 ↑304 ↓72k)");
        assert!(!text(&usage, true).contains('—'));
    }

    #[test]
    fn subagents_alone_stand_in_brackets_and_a_task_with_nothing_has_no_line() {
        let only_subagents = TaskUsage {
            subagents: Some(counts(100, 20_000)),
            subagent_count: Some(2),
            ..TaskUsage::default()
        };
        assert_eq!(text(&only_subagents, true), "(👥 2 ↑100 ↓20k)");
        assert_eq!(line(&only_subagents, false), None);
        assert_eq!(line(&TaskUsage::default(), true), None);
        // Covers R10: a count alone is a block.
        let zero = TaskUsage {
            subagents: Some(counts(0, 0)),
            subagent_count: Some(2),
            ..TaskUsage::default()
        };
        assert_eq!(text(&zero, true), "(👥 2)");
        let no_count = TaskUsage {
            subagents: Some(counts(0, 0)),
            ..TaskUsage::default()
        };
        assert_eq!(line(&no_count, true), None);
    }

    #[test]
    fn subagents_without_a_count_show_their_tokens() {
        let usage = TaskUsage {
            total: Some(counts(304, 72_000)),
            subagents: Some(counts(100, 20_000)),
            ..TaskUsage::default()
        };
        assert_eq!(text(&usage, true), "✨ ↑304 ↓72k (👥 ↑100 ↓20k)");
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
            text(&usage, true),
            "razem $1.20 · ✨ $1.20 ↑120k ↓8k (👥 2 ↑40k ↓3k)"
        );
        assert_eq!(text(&usage, false), "razem $1.20 · ✨ $1.20 ↑120k ↓8k");
    }

    #[test]
    fn a_task_without_any_report_has_no_usage() {
        assert_eq!(sum(&[]), None);
        assert_eq!(sum(&[position(None)]), None);
        assert_eq!(sum(&[position(Some(PositionUsage::default()))]), None);
    }

    #[test]
    fn a_part_nobody_reported_is_left_out() {
        let only_cost = PositionUsage {
            cost_usd: Some(0.5),
            ..PositionUsage::default()
        };
        let only_tokens = PositionUsage {
            total: Some(counts(1_500, 20)),
            ..PositionUsage::default()
        };
        assert_eq!(
            text(&sum(&[position(Some(only_cost))]).unwrap(), true),
            "razem $0.50 · ✨ $0.50"
        );
        assert_eq!(
            text(&sum(&[position(Some(only_tokens))]).unwrap(), true),
            "✨ ↑1.5k ↓20"
        );
    }

    #[test]
    fn subagents_that_used_nothing_show_their_count_only() {
        let none = used((1_000, 100), None, 0.1);
        let zero = used((1_000, 100), Some((0, 0)), 0.1);
        let usage = sum(&[position(Some(none))]).unwrap();
        assert_eq!(text(&usage, true), "razem $0.10 · ✨ $0.10 ↑1k ↓100");
        let usage = sum(&[position(Some(zero))]).unwrap();
        assert_eq!(text(&usage, true), "razem $0.10 · ✨ $0.10 ↑1k ↓100 (👥 1)");
    }

    fn stats_usage(spawned: u32, tokens: u64) -> PositionUsage {
        PositionUsage {
            subagent_stats: Some(SubagentStats {
                spawned,
                tokens,
                ..SubagentStats::default()
            }),
            ..PositionUsage::default()
        }
    }

    /// Covers AE3: subagents without tokens still show their count.
    #[test]
    fn subagents_without_tokens_show_their_count_in_the_header() {
        let mut step = used((120_000, 8_000), None, 1.2);
        step.subagent_stats = Some(SubagentStats {
            spawned: 3,
            ..SubagentStats::default()
        });
        let usage = sum(&[position(Some(step))]).unwrap();
        assert_eq!(
            text(&usage, true),
            "razem $1.20 · ✨ $1.20 ↑120k ↓8k (👥 3)"
        );
        let only = sum(&[position(Some(stats_usage(3, 0)))]).unwrap();
        assert_eq!(text(&only, true), "(👥 3)");
        assert!(sum(&[position(Some(stats_usage(0, 0)))]).is_none());
    }

    /// Covers AE4 and the header of a harness that gives tokens together.
    #[test]
    fn the_tokens_of_subagents_together_are_added_up_in_the_header() {
        let usage = sum(&[
            position(Some(stats_usage(2, 50_000))),
            position(Some(stats_usage(1, 36_000))),
        ])
        .unwrap();
        assert_eq!(text(&usage, true), "(👥 3 ↕86k)");
        let copilot = sum(&[position(Some(stats_usage(2, 7_978)))]).unwrap();
        assert_eq!(text(&copilot, true), "(👥 2 ↕8k)");
    }

    /// Covers AE2: the cost, when a harness gives one, stands before the tokens.
    #[test]
    fn the_cost_of_subagents_stands_before_their_tokens() {
        let usage = TaskUsage {
            subagent_count: Some(2),
            subagent_tokens: Some(86_000),
            subagent_cost_usd: Some(0.4),
            ..TaskUsage::default()
        };
        assert_eq!(text(&usage, true), "(👥 2 $0.40 ↕86k)");
    }

    /// The success criterion: the header and the rows tell the same.
    #[test]
    fn the_header_is_the_sum_of_the_rows() {
        let steps = [stats_usage(2, 50_000), stats_usage(1, 36_000)];
        let usage = sum(&steps.map(|step| position(Some(step)))).unwrap();
        let cells: Vec<StepCells> = steps.iter().map(|step| step_cells(Some(step))).collect();
        assert_eq!(cells[0].subagent_count, "👥 2");
        assert_eq!(cells[1].subagent_total, "↕36k");
        assert_eq!(usage.subagent_count, Some(3));
        assert_eq!(usage.subagent_tokens, Some(86_000));
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
            text(&usage, false)
        };
        assert!(at(0.004).starts_with("razem $0.00 "));
        assert!(at(1.236).starts_with("razem $1.24 "));
    }

    #[test]
    fn the_cache_does_not_change_what_the_arrow_up_counts() {
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
        assert_eq!(text(&usage, true), "✨ ↑2k ↓100");
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
            ("$0.42", "↑120k", "↓8k", "👥 1", "↑40k", "↓3k")
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
            ("↑950", "↓2.4M")
        );
        assert!(cells.cost.is_empty());
        assert!(!step_cells(Some(&PositionUsage::default())).has_main());
    }

    #[test]
    fn the_subagent_stats_fill_the_count_and_the_tokens_together() {
        let stats = |spawned, tokens| PositionUsage {
            subagent_stats: Some(SubagentStats {
                spawned,
                completed: spawned,
                failed: 1,
                tokens,
                ..SubagentStats::default()
            }),
            ..PositionUsage::default()
        };
        let cells = step_cells(Some(&stats(3, 45_000)));
        assert_eq!(
            (
                cells.subagent_count.as_str(),
                cells.subagent_input.as_str(),
                cells.subagent_total.as_str()
            ),
            ("👥 3", "", "↕45k")
        );
        assert!(cells.has_subagents());
        let cells = step_cells(Some(&stats(2, 0)));
        assert_eq!(cells.subagent_count, "👥 2");
        assert!(cells.subagent_total.is_empty() && cells.has_subagents());
        assert!(!step_cells(Some(&stats(0, 0))).has_subagents());
    }

    #[test]
    fn a_split_of_the_subagent_tokens_wins_over_their_sum() {
        let mut usage = used((1_000, 100), Some((40_000, 3_000)), 0.1);
        usage.subagent_stats = Some(SubagentStats {
            spawned: 4,
            tokens: 43_000,
            ..SubagentStats::default()
        });
        // The count of the harness (1) wins over the stats (4), R11.
        let cells = step_cells(Some(&usage));
        assert_eq!(
            (
                cells.subagent_count.as_str(),
                cells.subagent_input.as_str(),
                cells.subagent_total.as_str()
            ),
            ("👥 1", "↑40k", "")
        );
    }

    #[test]
    fn subagents_that_used_nothing_are_no_cells_but_a_counted_one_shows_its_count() {
        let cells = step_cells(Some(&used((1_000, 100), None, 0.1)));
        assert!(!cells.has_subagents() && cells.subagent_count.is_empty());
        let cells = step_cells(Some(&used((1_000, 100), Some((0, 0)), 0.1)));
        assert!(cells.has_subagents() && cells.subagent_input.is_empty());
        assert_eq!(cells.subagent_count, "👥 1");
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
            ("↑2k", "↓100")
        );
    }

    #[test]
    fn conductor_use_is_summed_apart_and_combined_with_the_agents() {
        let mut first = used((80_000, 5_000), None, 0.8);
        first.conductor_cost_usd = Some(0.05);
        first.conductor_total = Some(counts(1_000, 100));
        let mut second = used((20_000, 1_000), None, 0.2);
        second.conductor_cost_usd = Some(0.25);
        second.conductor_total = Some(counts(500, 50));

        let task = sum(&[position(Some(first)), position(Some(second))]).unwrap();

        assert_eq!(task.cost_usd, Some(1.0));
        assert_eq!(task.total, Some(counts(100_000, 6_000)));
        assert_eq!(task.conductor_cost_usd, Some(0.3));
        assert_eq!(task.conductor_total, Some(counts(1_500, 150)));
        assert_eq!(task.combined_cost_usd(), Some(1.3));
    }

    #[test]
    fn a_task_with_only_conductor_use_still_has_a_sum() {
        let only_conductor = PositionUsage {
            conductor_cost_usd: Some(0.1),
            ..PositionUsage::default()
        };

        let task = sum(&[position(Some(only_conductor))]).unwrap();

        assert_eq!(task.cost_usd, None);
        assert_eq!(task.combined_cost_usd(), Some(0.1));
    }

    #[test]
    fn the_line_names_the_conductor_and_the_total_only_when_it_reported() {
        let mut usage = TaskUsage {
            cost_usd: Some(1.0),
            total: Some(counts(1_000, 100)),
            ..TaskUsage::default()
        };
        let without = text(&usage, true);
        assert!(!without.contains('⚡'));

        usage.conductor_cost_usd = Some(0.25);
        usage.conductor_total = Some(counts(9_000, 1_000));
        assert_eq!(
            text(&usage, true),
            "razem $1.25 · ✨ $1.00 ↑1k ↓100 · ⚡ $0.25 ↑9k ↓1k"
        );
    }

    #[test]
    fn the_conductors_cost_has_a_cell_of_its_own() {
        let usage = PositionUsage {
            cost_usd: Some(0.5),
            conductor_cost_usd: Some(0.05),
            ..PositionUsage::default()
        };

        let cells = step_cells(Some(&usage));

        assert_eq!(cells.cost, "$0.50");
        assert_eq!(cells.conductor_cost, "dyr. $0.05");
        assert!(step_cells(Some(&used((1, 1), None, 0.5)))
            .conductor_cost
            .is_empty());
    }

    #[test]
    fn a_position_usage_without_conductor_fields_reads_back() {
        let usage: PositionUsage =
            serde_json::from_str(r#"{"cost_usd":0.5,"total":{"input":1}}"#).unwrap();

        assert_eq!(usage.cost_usd, Some(0.5));
        assert_eq!(
            (usage.conductor_total, usage.conductor_cost_usd),
            (None, None)
        );
    }

    /// R11: the count of the harness wins; zero or none falls back to the stats.
    #[test]
    fn the_count_of_a_step_comes_from_the_harness_then_from_the_stats() {
        let with = |count: Option<u32>, spawned: Option<u32>| PositionUsage {
            subagent_count: count,
            subagent_stats: spawned.map(|spawned| SubagentStats {
                spawned,
                ..SubagentStats::default()
            }),
            ..PositionUsage::default()
        };
        for (usage, expected) in [
            (with(Some(5), Some(2)), Some(5)),
            (with(None, Some(2)), Some(2)),
            (with(Some(0), Some(2)), Some(2)),
            (with(Some(0), None), None),
            (with(None, None), None),
        ] {
            assert_eq!(subagent_count_of(&usage), expected);
            assert_eq!(
                step_cells(Some(&usage)).subagent_count,
                expected.map_or_else(String::new, |count| format!("{SUBAGENTS_ICON} {count}"))
            );
        }
    }

    /// The tokens of the harness stay: 80k in and 6k out, 2 subagents.
    #[test]
    fn a_split_with_a_count_reads_as_cells() {
        let mut usage = used((1_000, 100), Some((80_000, 6_000)), 0.1);
        usage.subagent_count = Some(2);
        let cells = step_cells(Some(&usage));
        assert_eq!(
            (
                cells.subagent_count.as_str(),
                cells.subagent_input.as_str(),
                cells.subagent_output.as_str()
            ),
            ("👥 2", "↑80k", "↓6k")
        );
    }
}
