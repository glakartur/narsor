//! Which permission denials of a finished run stop the step.

use super::response::{closing_answer, final_answer_kind};
use crate::engine::actions::answer::AnswerShape;
use crate::engine::actions::harness::{HarnessRun, PermissionDenial};

/// The denials of a run, split by whether they stop the step.
#[derive(Debug, Default)]
pub(super) struct DenialSplit<'a> {
    pub blocking: Vec<&'a PermissionDenial>,
    pub ignored: Vec<&'a PermissionDenial>,
}

/// A denial of the main agent stops the step only when the run has no usable
/// final answer. A denial of a subagent never does: the engine talks to the
/// main agent only.
pub(super) fn split_denials(run: &HarnessRun, shape: AnswerShape) -> DenialSplit<'_> {
    if run.permission_denials.is_empty() {
        return DenialSplit::default();
    }
    let answered = has_usable_final_answer(run, shape);
    let (ignored, blocking) = run
        .permission_denials
        .iter()
        .partition(|denial| denial.subagent.is_some() || answered);
    DenialSplit { blocking, ignored }
}

/// A run that failed has none, even when a `structured_output` survives from
/// an earlier result line.
fn has_usable_final_answer(run: &HarnessRun, shape: AnswerShape) -> bool {
    run.failure.is_none()
        && (run
            .structured_output
            .as_ref()
            .is_some_and(|value| final_answer_kind(value, shape).is_some())
            || closing_answer(&run.text, shape).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use narsor_protocol::chat::SubagentRef;
    use serde_json::json;

    const ANSWER: &str = r#"Done. {"choice":"findings"}"#;

    fn denial(id: &str, subagent: Option<&str>) -> PermissionDenial {
        PermissionDenial {
            tool_name: "Bash".into(),
            tool_use_id: id.into(),
            input: json!({}),
            reason_type: None,
            reason: None,
            session_id: None,
            subagent: subagent.map(|name| SubagentRef {
                id: format!("task-{name}"),
                name: name.into(),
            }),
            summary: String::new(),
        }
    }

    fn run(text: &str, denials: Vec<PermissionDenial>) -> HarnessRun {
        HarnessRun {
            text: text.into(),
            permission_denials: denials,
            ..HarnessRun::default()
        }
    }

    fn ids(denials: &[&PermissionDenial]) -> Vec<String> {
        denials.iter().map(|d| d.tool_use_id.clone()).collect()
    }

    #[test]
    fn a_subagent_denial_with_a_closing_answer_blocks_nothing() {
        let run = run(ANSWER, vec![denial("a", Some("validator"))]);
        let split = split_denials(&run, AnswerShape::Choice);
        assert!(split.blocking.is_empty());
        assert_eq!(ids(&split.ignored), ["a"]);
    }

    #[test]
    fn a_main_agent_denial_with_a_closing_answer_is_ignored() {
        let run = run(ANSWER, vec![denial("a", None)]);
        let split = split_denials(&run, AnswerShape::Choice);
        assert!(split.blocking.is_empty());
        assert_eq!(ids(&split.ignored), ["a"]);
        assert!(split.ignored[0].subagent.is_none());
    }

    #[test]
    fn a_main_agent_denial_with_only_prose_blocks() {
        let run = run("I could not run the command.", vec![denial("a", None)]);
        let split = split_denials(&run, AnswerShape::Choice);
        assert_eq!(ids(&split.blocking), ["a"]);
        assert!(split.ignored.is_empty());
    }

    #[test]
    fn a_subagent_denial_without_an_answer_is_ignored() {
        let run = run("no answer", vec![denial("a", Some("validator"))]);
        let split = split_denials(&run, AnswerShape::Choice);
        assert!(split.blocking.is_empty());
        assert_eq!(ids(&split.ignored), ["a"]);
    }

    #[test]
    fn an_answer_kept_from_an_earlier_line_does_not_count_after_a_failure() {
        let mut run = run("", vec![denial("a", None)]);
        run.structured_output = Some(json!({"choice": "findings"}));
        run.failure = Some("error result".into());
        let split = split_denials(&run, AnswerShape::Choice);
        assert_eq!(ids(&split.blocking), ["a"]);
    }

    #[test]
    fn a_structured_answer_counts_without_a_failure() {
        let mut run = run("", vec![denial("a", None)]);
        run.structured_output = Some(json!({"choice": "findings"}));
        let split = split_denials(&run, AnswerShape::Choice);
        assert!(split.blocking.is_empty());
    }

    #[test]
    fn only_the_main_agent_denial_blocks_without_an_answer() {
        let run = run(
            "prose",
            vec![denial("sub", Some("validator")), denial("main", None)],
        );
        let split = split_denials(&run, AnswerShape::Choice);
        assert_eq!(ids(&split.blocking), ["main"]);
        assert_eq!(ids(&split.ignored), ["sub"]);
    }
}
