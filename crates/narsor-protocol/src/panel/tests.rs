use serde_json::{json, Value};

use super::*;
use crate::api::{
    HumanInputActionDto, PendingHumanInputView, ENTRY_LIMIT_CONTINUE, ENTRY_LIMIT_DECISION_FIELD,
};

fn definition_of(form: Value) -> PanelDefinition {
    let pending: PendingHumanInputView = serde_json::from_value(form).expect("a pending input");
    define(&pending, &|_| None)
}

fn labels(definition: &PanelDefinition) -> Vec<&str> {
    definition
        .entries
        .iter()
        .map(|entry| entry.label.as_str())
        .collect()
}

fn choose(
    definition: &PanelDefinition,
    index: usize,
    values: &[(&str, &str)],
) -> HumanInputDeliveryDto {
    let values: Vec<(String, String)> = values
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    deliver(definition, index, &values).expect("a delivery")
}

fn question() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Która wersja API?",
        "fields": [{ "name": "answer", "type": "text", "required": true }],
        "continuation": "resume_agent",
        "episode": "ep-q",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn approval() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Zatwierdź wynik",
        "outputs": [{ "label": "reviewed" }],
        "continuation": "approve_output",
        "approval": {
            "action_execution_id": "act-7",
            "result": { "output_label": "reviewed", "data": {}, "variables": {}, "exit_code": null, "stdout": "", "stderr": "" },
            "agent_session_id": "sess-1"
        },
        "episode": "ep-a",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn gate() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "review_scope",
        "action_execution_id": "act-7",
        "instruction": "Przejrzyj pracę.",
        "fields": [],
        "outputs": [{ "label": "approve" }, { "label": "reject" }],
        "episode": "ep-g",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn problem() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "implement",
        "action_execution_id": "act-9",
        "instruction": "Krok nie dał etykiety",
        "outputs": [{ "label": "implemented" }, { "label": "needs_human_decision" }],
        "continuation": "resolve_output",
        "problem": {
            "reason": "Odpowiedź agenta nie zawiera etykiety.",
            "answers": [],
            "requirements": [
                { "label": "implemented", "variables": ["count"], "artifacts": [] },
                { "label": "needs_human_decision", "variables": [], "artifacts": [] }
            ],
            "analysis": {
                "label": "implemented",
                "rationale": "Wszystko gotowe.",
                "variables": { "count": 3 }
            }
        },
        "episode": "ep-p",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn permission(command: &str) -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "implement",
        "action_execution_id": "act-8",
        "instruction": "Claude Code needs permission.",
        "fields": [],
        "continuation": "resolve_permission",
        "permission": { "denials": [{
            "tool_name": "Bash",
            "tool_use_id": "t1",
            "input": { "command": command },
            "cwd": "/tmp/task",
            "reason_type": "asyncAgent",
            "reason": "This command requires approval."
        }] },
        "agent_harness": "claude-code",
        "agent_session_id": "s1",
        "episode": "ep-8",
        "requested_at": "2026-10-09T11:54:00Z"
    })
}

fn limit() -> Value {
    json!({
        "execution_id": "exec-1",
        "state_id": "code_review",
        "action_execution_id": "act-limit",
        "instruction": "Krok osiągnął limit wejść.",
        "fields": [{
            "name": "entry_limit_decision",
            "type": "select",
            "options": ["continue", "reset_counter"],
            "required": false
        }],
        "outputs": [{ "label": "clean" }, { "label": "findings" }],
        "continuation": "entry_limit",
        "problem": { "reason": "entry limit reached (5/5)", "requirements": [] },
        "entry_limit": { "entries": 5, "max_entries": 5 },
        "episode": "ep-limit",
        "requested_at": "2026-10-18T12:00:00Z"
    })
}

#[test]
fn a_question_is_answered_with_the_typed_text() {
    let definition = definition_of(question());
    let delivery = choose(&definition, 0, &[("answer", "v2")]);
    assert_eq!(delivery.episode.as_deref(), Some("ep-q"));
    assert_eq!(delivery.values.get("answer"), Some(&json!("v2")));
    assert_eq!(definition.entries[0].summary, "odpowiedziano agentowi");
}

#[test]
fn a_question_without_text_or_option_is_refused() {
    let definition = definition_of(question());
    let refused = deliver(&definition, 0, &[]);
    assert_eq!(refused.unwrap_err(), "Wpisz odpowiedź albo wybierz opcję.");
}

#[test]
fn a_question_is_the_composer_when_its_only_field_is_the_answer() {
    let definition = definition_of(question());
    let composer = definition.composer.expect("a composer");
    assert_eq!(composer.entry, 0);
    assert_eq!(composer.label, "Odpowiedź agentowi");
}

#[test]
fn an_approval_is_accepted_or_returned_to_the_agent() {
    let definition = definition_of(approval());
    assert_eq!(labels(&definition), ["Akceptuj", "Zwróć agentowi"]);
    let accepted = choose(&definition, 0, &[]);
    assert_eq!(accepted.action, Some(HumanInputActionDto::Approve));
    let returned = choose(&definition, 1, &[(COMMENT_TO_AGENT, "popraw")]);
    assert_eq!(returned.action, Some(HumanInputActionDto::Resume));
    assert_eq!(returned.comment.as_deref(), Some("popraw"));
    assert_eq!(definition.entries[0].summary, "zaakceptowano reviewed");
}

#[test]
fn a_gate_offers_its_outputs_and_completes_the_step_with_the_chosen_one() {
    let definition = definition_of(gate());
    assert_eq!(labels(&definition), ["approve", "reject"]);
    let delivery = choose(&definition, 1, &[]);
    assert_eq!(delivery.chosen_label.as_deref(), Some("reject"));
    assert_eq!(delivery.episode.as_deref(), Some("ep-g"));
    assert_eq!(definition.entries[1].summary, "odpowiedziano reject");
}

#[test]
fn a_problem_offers_its_outputs_and_a_message_to_the_agent() {
    let definition = definition_of(problem());
    let all = labels(&definition);
    assert_eq!(all, ["implemented", "needs_human_decision", MESSAGE_ENTRY]);
    let delivery = choose(&definition, 1, &[]);
    assert_eq!(
        delivery.chosen_label.as_deref(),
        Some("needs_human_decision")
    );
    assert_eq!(delivery.episode.as_deref(), Some("ep-p"));
    let message = choose(&definition, 2, &[(MESSAGE_FIELD, "dopracuj")]);
    assert_eq!(message.agent_message.as_deref(), Some("dopracuj"));
}

#[test]
fn a_problem_recommends_the_output_the_conductor_proposed() {
    let definition = definition_of(problem());
    assert!(definition.entries[0].recommended);
    assert!(!definition.entries[1].recommended);
    assert!(definition
        .context
        .iter()
        .any(|line| line.tone == PanelTone::Conductor && line.text.contains("Wszystko gotowe.")));
}

#[test]
fn a_proposed_value_stays_a_number_until_the_operator_changes_it() {
    let definition = definition_of(problem());
    let field = &definition.entries[0].fields[0];
    assert_eq!(field.name, "variables.count");
    assert_eq!(field.value.as_deref(), Some("3"));
    assert_eq!(field.proposed, Some(json!(3)));

    let untouched = choose(&definition, 0, &[("variables.count", "3")]);
    assert_eq!(untouched.variables.get("count"), Some(&json!(3)));
    let typed = choose(&definition, 0, &[("variables.count", "4")]);
    assert_eq!(typed.variables.get("count"), Some(&json!("4")));
}

#[test]
fn an_output_takes_artifacts_variables_and_extra_lines() {
    let definition = definition_of(problem());
    let delivery = choose(
        &definition,
        1,
        &[
            ("artifacts.report", "out/report.md"),
            (EXTRA_VARIABLES_FIELD, "mode = fast\n\nowner=me"),
            (COMMENT_FIELD, " ok "),
        ],
    );
    assert_eq!(
        delivery.artifacts.get("report").map(String::as_str),
        Some("out/report.md")
    );
    assert_eq!(delivery.variables.get("mode"), Some(&json!("fast")));
    assert_eq!(delivery.variables.get("owner"), Some(&json!("me")));
    assert_eq!(delivery.comment.as_deref(), Some("ok"));

    let twice = deliver(
        &definition,
        1,
        &[(EXTRA_VARIABLES_FIELD.to_string(), "a=1\na=2".to_string())],
    );
    assert_eq!(
        twice.unwrap_err(),
        "Klucz 'variables.a' jest podany dwa razy."
    );
    let broken = deliver(
        &definition,
        1,
        &[(EXTRA_VARIABLES_FIELD.to_string(), "bez znaku".to_string())],
    );
    assert!(broken.is_err());
}

#[test]
fn an_output_that_resumes_the_agent_names_its_attempt_and_state() {
    let mut form = gate();
    form["outputs"] = json!([{ "label": "revise", "resume_comment_field": "comment" }]);
    form["fields"] = json!([{ "name": "comment", "type": "text", "required": false }]);
    form["resume_targets"] = json!({
        "revise": { "action_execution_id": "att-1", "harness": "claude-code", "session_id": "s1" }
    });
    let pending: PendingHumanInputView = serde_json::from_value(form).expect("a pending input");

    let unknown = define(&pending, &|_| None);
    assert_eq!(unknown.entries[0].resumes.as_deref(), Some(""));
    assert_eq!(unknown.entries[0].resumes_attempt.as_deref(), Some("att-1"));
    assert_eq!(
        unknown.entries[0].summary,
        "odpowiedziano revise ↩ wraca do agenta"
    );

    let named = define(&pending, &|attempt| {
        (attempt == "att-1").then(|| "plan".to_string())
    });
    assert_eq!(named.entries[0].resumes.as_deref(), Some("plan"));
}

#[test]
fn a_resolvable_bash_denial_offers_scoped_allows_and_a_refusal() {
    let definition = definition_of(permission("cargo test"));
    let all = labels(&definition);
    assert_eq!(all[0], "Odmów");
    assert!(all.len() >= 2, "{all:?}");
    let refusal = choose(&definition, 0, &[]);
    let decision = refusal.permission_decision.expect("a decision");
    assert_eq!(decision.decision, PermissionDecisionKindDto::Deny);
    assert_eq!(definition.entries[0].summary, "odmówiono dostępu agentowi");
}

#[test]
fn a_pattern_without_a_rule_is_refused_at_the_delivery() {
    let definition = definition_of(permission("cargo test"));
    let pattern = definition
        .entries
        .iter()
        .position(|entry| entry.label.starts_with("Zezwól według wzorca"))
        .expect("a pattern entry");
    let empty = deliver(
        &definition,
        pattern,
        &[(PATTERN_FIELD.to_string(), String::new())],
    );
    assert!(empty.is_err(), "an empty pattern is refused");
    let typed = choose(&definition, pattern, &[(PATTERN_FIELD, " npm run * ")]);
    let decision = typed.permission_decision.expect("a decision");
    assert_eq!(decision.pattern.as_deref(), Some("npm run *"));
    assert_eq!(decision.scope, Some(PermissionGrantScopeDto::Pattern));
}

#[test]
fn the_decision_at_the_limit_is_sent_as_its_field() {
    let definition = definition_of(limit());
    assert_eq!(
        labels(&definition),
        ["clean", "findings", "Kontynuuj", "Wyzeruj licznik"]
    );
    let delivery = choose(&definition, 2, &[]);
    assert_eq!(
        delivery.values.get(ENTRY_LIMIT_DECISION_FIELD),
        Some(&Value::String(ENTRY_LIMIT_CONTINUE.to_string()))
    );
    let reset = choose(&definition, 3, &[]);
    assert_eq!(
        reset.values.get(ENTRY_LIMIT_DECISION_FIELD),
        Some(&Value::String(ENTRY_LIMIT_RESET_COUNTER.to_string()))
    );
    assert_eq!(
        definition.entries[3].summary,
        "wyzerowano licznik i kontynuowano"
    );
}

#[test]
fn a_definition_survives_the_wire() {
    for form in [
        question(),
        approval(),
        gate(),
        problem(),
        permission("ls"),
        limit(),
    ] {
        let definition = definition_of(form);
        let wire = serde_json::to_string(&definition).expect("serialises");
        let back: PanelDefinition = serde_json::from_str(&wire).expect("deserialises");
        assert_eq!(back, definition);
    }
}

#[test]
fn an_entry_that_does_not_exist_is_refused() {
    let definition = definition_of(gate());
    assert_eq!(
        deliver(&definition, 9, &[]).unwrap_err(),
        "Ta akcja nie istnieje."
    );
}
