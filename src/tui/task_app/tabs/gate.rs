//! The entry (`in`) and exit (`out`) scripts of a watched gate on *Przebieg*
//! (R20): a control-plane overlay on the runner timeline. While an entry
//! script failed its last attempt the gate's position shows `failure`; the
//! attempts of every script are lines under the gate's step, never steps of
//! their own. Without the overlay (a server from before it, a gate without
//! scripts) the runner status shows unchanged.

use ratatui::text::Line;

use narsor_protocol::api::{Position, PositionStatus};
use narsor_protocol::controller::{GateAutomationPhase, GateHookView, TaskGateAutomationView};

use crate::tui::style::{Role, Styles};

use super::super::super::scroll::one_line;

/// Longest error excerpt a line shows, in characters.
const ERROR_EXCERPT: usize = 120;

/// The index of the gate's position: the latest entry into its state.
fn gate_index(positions: &[Position], gate: &TaskGateAutomationView) -> Option<usize> {
    positions
        .iter()
        .rposition(|position| position.state_id == gate.step)
}

/// Whether `label` is the position the gate's overlay applies to.
pub fn is_gate_position(
    positions: &[Position],
    gate: Option<&TaskGateAutomationView>,
    label: &str,
) -> bool {
    gate.and_then(|gate| gate_index(positions, gate))
        .is_some_and(|index| positions[index].label == label)
}

/// `positions` with the gate's overlay: the gate shows `failure`, with the
/// failed entry script as its error, while the runner still has it in
/// progress and an entry script failed its last attempt.
pub fn overlay(positions: &[Position], gate: Option<&TaskGateAutomationView>) -> Vec<Position> {
    let mut positions = positions.to_vec();
    let Some(gate) = gate.filter(|gate| gate.phase == GateAutomationPhase::EntryFailed) else {
        return positions;
    };
    let Some(index) = gate_index(&positions, gate) else {
        return positions;
    };
    let position = &mut positions[index];
    if matches!(
        position.status,
        PositionStatus::InProgress | PositionStatus::AwaitingDecision
    ) {
        position.status = PositionStatus::Failed;
        position.error = Some(entry_failure(gate));
    }
    positions
}

/// Why the gate failed: the entry script that failed its last attempt.
fn entry_failure(gate: &TaskGateAutomationView) -> String {
    let failed = gate
        .hooks
        .iter()
        .find(|hook| hook.hook == "in" && hook.status == "failed");
    match failed {
        Some(hook) => {
            let error = hook
                .last_error
                .as_deref()
                .map(|error| format!(": {}", excerpt(error)))
                .unwrap_or_default();
            format!(
                "skrypt wejścia {}: próba {} z {} nieudana{error}",
                one_line(&hook.automation),
                hook.attempts,
                hook.max_attempts
            )
        }
        None => "skrypt wejścia nie powiódł się".to_string(),
    }
}

/// The lines under the gate's step: where its scripts stand, then every
/// ended attempt.
pub fn lines(gate: &TaskGateAutomationView, styles: &Styles) -> Vec<Line<'static>> {
    let (phase, role) = match gate.phase {
        GateAutomationPhase::Entering => ("⏳ czeka na skrypty wejścia", Role::Live),
        GateAutomationPhase::Ready => ("wejście gotowe", Role::Done),
        GateAutomationPhase::EntryFailed => ("✗ skrypt wejścia nie powiódł się", Role::Problem),
        GateAutomationPhase::Exiting => ("⏳ czeka na skrypty wyjścia", Role::Live),
        GateAutomationPhase::Released => ("skrypty wyjścia zakończone", Role::Done),
        GateAutomationPhase::Unrecognized => ("stan skryptów nieznany", Role::Done),
    };
    let mut head = format!("kolejka {} · {phase}", one_line(&gate.queue));
    if let Some(exit) = &gate.exit {
        let choice = exit.choice.as_deref().map(one_line).unwrap_or_default();
        head.push_str(&format!(
            " · wyjście {choice} ({})",
            author_word(&exit.author)
        ));
    }
    let mut lines = vec![Line::styled(head, styles.of(role))];
    for hook in &gate.hooks {
        lines.extend(hook_lines(hook, styles));
    }
    lines
}

fn hook_lines<'a>(
    hook: &'a GateHookView,
    styles: &'a Styles,
) -> impl Iterator<Item = Line<'static>> + 'a {
    let phase = if hook.hook == "out" {
        "wyjście"
    } else {
        "wejście"
    };
    hook.log.iter().map(move |entry| {
        let mut text = format!(
            "  {phase} · {} · próba {} z {}",
            one_line(&hook.automation),
            entry.attempt,
            hook.max_attempts
        );
        let role = match (&entry.error, &entry.skipped) {
            (Some(error), _) => {
                text.push_str(&format!(" · ✗ {}", excerpt(error)));
                Role::Problem
            }
            (None, Some(reason)) => {
                text.push_str(&format!(" · pominięta: {}", excerpt(reason)));
                Role::Done
            }
            (None, None) => {
                text.push_str(" · ✓ udana");
                Role::Done
            }
        };
        if let Some(at) = entry.retry_at {
            text.push_str(&format!(
                " · następna próba {}",
                at.with_timezone(&chrono::Local).format("%H:%M:%S")
            ));
        }
        Line::styled(text, styles.of(role))
    })
}

fn author_word(author: &str) -> &'static str {
    match author {
        "person" => "osoba",
        "automation" => "automatyzacja",
        "operator" => "operator",
        "task_end" => "koniec zadania",
        "runner" => "runner",
        _ => "nieznany autor",
    }
}

fn excerpt(text: &str) -> String {
    let line = one_line(text);
    if line.chars().count() > ERROR_EXCERPT {
        let cut: String = line.chars().take(ERROR_EXCERPT - 1).collect();
        format!("{cut}…")
    } else {
        line
    }
}
