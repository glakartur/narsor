//! Template substitution (U4): `{{ inputs.* }}` and `{{ context.variables.* }}`.
//!
//! The reference fixture is `tests/fixtures/implement_feature.yaml`, copied
//! verbatim from `docs/dev/workflow-schema.md` §4.

use std::collections::HashMap;

use narsor_engine::domain::{ActionConfig, ArtifactReference, Context};
use narsor_engine::workflow::load_from_file;
use narsor_engine::workflow::template::{DeclaredNames, TemplateError, TemplateScope};
use serde_json::{json, Map, Value};

const WHERE: &str = "state 'implement', prompt";

/// Builds an inputs object from `(name, value)` pairs.
fn inputs(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

/// Builds a context whose `variables` map holds the given pairs.
fn context(pairs: &[(&str, Value)]) -> Context {
    let mut context = Context::new();
    context.variables = pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect::<HashMap<_, _>>();
    context
}

fn render(template: &str, inputs: &Map<String, Value>, context: &Context) -> String {
    TemplateScope::new(inputs, context)
        .render(template, WHERE)
        .expect("template must render")
}

fn render_error(template: &str, inputs: &Map<String, Value>, context: &Context) -> TemplateError {
    TemplateScope::new(inputs, context)
        .render(template, WHERE)
        .expect_err("template must fail to render")
}

#[test]
fn substitutes_a_workflow_input() {
    let inputs = inputs(&[("description", json!("login"))]);

    let rendered = render(
        "Zadanie: {{ inputs.description }}",
        &inputs,
        &Context::new(),
    );

    assert_eq!(rendered, "Zadanie: login");
}

#[test]
fn renders_a_number_without_quotes() {
    let context = context(&[("pr_number", json!(123))]);

    let rendered = render("{{ context.variables.pr_number }}", &Map::new(), &context);

    assert_eq!(rendered, "123");
}

#[test]
fn context_shorthand_is_an_alias_for_variables() {
    // `docs/dev/workflow-schema.md` §3.3.D writes the wait filter as
    // `{{ context.pr_number }}`, which must mean the same variable.
    let context = context(&[("pr_number", json!(123))]);

    let shorthand = render("{{ context.pr_number }}", &Map::new(), &context);
    let full_path = render("{{ context.variables.pr_number }}", &Map::new(), &context);

    assert_eq!(shorthand, full_path);
    assert_eq!(shorthand, "123");
}

#[test]
fn substitutes_a_named_step_reply_once() {
    let replies = HashMap::from([("review".to_string(), json!("finding {{ context.secret }}"))]);
    let rendered = TemplateScope::new(&Map::new(), &Context::new())
        .with_step_replies(&replies)
        .render("Report: {{ steps.review.reply }}", WHERE)
        .unwrap();
    assert_eq!(rendered, "Report: finding {{ context.secret }}");
}

#[test]
fn substitutes_two_occurrences_independently() {
    let inputs = inputs(&[("feature", json!("login"))]);
    let context = context(&[("branch", json!("feat/login"))]);

    let rendered = render(
        "{{ inputs.feature }} na gałęzi {{ context.branch }}",
        &inputs,
        &context,
    );

    assert_eq!(rendered, "login na gałęzi feat/login");
}

#[test]
fn ignores_whitespace_inside_the_placeholder() {
    let inputs = inputs(&[("x", json!("value"))]);

    assert_eq!(
        render("{{   inputs.x   }}", &inputs, &Context::new()),
        "value"
    );
    assert_eq!(render("{{inputs.x}}", &inputs, &Context::new()), "value");
}

#[test]
fn returns_a_plain_string_unchanged() {
    let text = "Bez żadnego szablonu — { pojedynczy nawias } zostaje.";

    assert_eq!(render(text, &Map::new(), &Context::new()), text);
}

#[test]
fn does_not_rescan_a_substituted_value() {
    // Single-pass substitution: a value that itself looks like a template is
    // inserted literally, which is what rules out recursion.
    let inputs = inputs(&[("x", json!("{{ inputs.y }}")), ("y", json!("boom"))]);

    let rendered = render("{{ inputs.x }}", &inputs, &Context::new());

    assert_eq!(rendered, "{{ inputs.y }}");
}

#[test]
fn renders_a_list_as_compact_json() {
    let context = context(&[("files", json!(["a", "b"]))]);

    let rendered = render("{{ context.files }}", &Map::new(), &context);

    assert_eq!(rendered, r#"["a","b"]"#);
}

#[test]
fn renders_a_boolean_and_an_object_as_json() {
    let context = context(&[("ok", json!(true)), ("meta", json!({ "n": 1 }))]);

    assert_eq!(render("{{ context.ok }}", &Map::new(), &context), "true");
    assert_eq!(
        render("{{ context.meta }}", &Map::new(), &context),
        r#"{"n":1}"#
    );
}

#[test]
fn unknown_input_is_an_error_naming_the_variable() {
    let error = render_error("A {{ inputs.missing }} B", &Map::new(), &Context::new());

    assert_eq!(
        error,
        TemplateError::UnknownVariable {
            reference: "inputs.missing".to_string(),
            location: WHERE.to_string(),
        }
    );
    let message = error.to_string();
    assert!(message.contains("inputs.missing"), "{message}");
    assert!(message.contains(WHERE), "{message}");
}

#[test]
fn unknown_context_variable_is_an_error_naming_the_variable() {
    let error = render_error(
        "{{ context.variables.missing }}",
        &Map::new(),
        &Context::new(),
    );

    assert_eq!(
        error,
        TemplateError::UnknownVariable {
            reference: "context.variables.missing".to_string(),
            location: WHERE.to_string(),
        }
    );
    assert!(error.to_string().contains("context.variables.missing"));
}

#[test]
fn a_filter_expression_is_rejected_with_a_dedicated_message() {
    let error = render_error(
        r#"{{ context.x | default("y") }}"#,
        &Map::new(),
        &Context::new(),
    );

    assert!(
        matches!(error, TemplateError::FilterUnsupported { .. }),
        "expected a filter-specific error, got {error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("filters are not supported"), "{message}");
    assert!(message.contains("plain variable"), "{message}");
}

#[test]
fn an_unterminated_placeholder_is_a_syntax_error_not_a_panic() {
    let inputs = inputs(&[("x", json!("value"))]);

    let error = render_error("prefix {{ inputs.x", &inputs, &Context::new());

    assert_eq!(
        error,
        TemplateError::UnterminatedPlaceholder {
            location: WHERE.to_string(),
        }
    );
}

#[test]
fn an_unsupported_root_is_rejected() {
    let error = render_error("{{ env.PATH }}", &Map::new(), &Context::new());

    assert!(
        matches!(error, TemplateError::UnsupportedReference { .. }),
        "expected an unsupported-reference error, got {error:?}"
    );
}

#[test]
fn renders_every_templated_field_of_a_script_action() {
    let inputs = inputs(&[("description", json!("login"))]);
    let context = context(&[("branch", json!("feat/login"))]);
    let scope = TemplateScope::new(&inputs, &context);

    let definition = load_reference();
    let action = definition
        .states
        .get("create_pr")
        .and_then(|state| state.action.as_ref())
        .expect("state 'create_pr' must carry an action");

    let rendered = scope
        .render_action(action, "state 'create_pr'")
        .expect("the §4 script action must render");

    match rendered {
        ActionConfig::Script(config) => {
            assert_eq!(config.args, vec!["--title", "Feature: login"]);
            assert_eq!(config.command, "./create_pr.sh");
        }
        other => panic!("expected a script action, got {other:?}"),
    }
}

#[test]
fn renders_a_wait_filter_recursively() {
    let context = context(&[("pr_number", json!(42))]);
    let no_inputs = Map::new();
    let scope = TemplateScope::new(&no_inputs, &context);

    let filter =
        json!({ "pr_number": "{{ context.pr_number }}", "nested": ["{{ context.pr_number }}"] });

    let rendered = scope
        .render_value(&filter, "state 'await_review', filter")
        .expect("the filter must render");

    assert_eq!(rendered, json!({ "pr_number": "42", "nested": ["42"] }));
}

#[test]
fn execution_inputs_must_be_an_object() {
    let context = Context::new();
    let inputs = json!("not an object");

    let error = TemplateScope::from_execution(&inputs, &context)
        .expect_err("a non-object inputs value must be rejected");

    assert_eq!(error, TemplateError::InvalidInputs { actual: "a string" });
}

#[test]
fn implement_prompt_from_the_reference_fixture_expands_fully() {
    // The stated verification criterion of U4: after the §4 example is
    // corrected, the `implement` prompt renders with no `{{` left in it.
    let definition = load_reference();
    let action = definition
        .states
        .get("implement")
        .and_then(|state| state.action.as_ref())
        .expect("state 'implement' must carry an action");

    let inputs = inputs(&[("description", json!("logowanie przez OAuth"))]);
    // Deliberately empty: `implement` is reachable from `analyze` (label `ready`)
    // without passing through `human_clarification`, so the prompt must render
    // on a context that carries nothing at all.
    let context = context(&[]);

    let rendered = TemplateScope::new(&inputs, &context)
        .render_action(action, "state 'implement'")
        .expect("the corrected §4 prompt must render");

    let prompt = match rendered {
        ActionConfig::Agent(config) => config.prompt.to_string(),
        other => panic!("expected an agent action, got {other:?}"),
    };

    assert!(
        !prompt.contains("{{"),
        "the rendered prompt still contains a placeholder: {prompt}"
    );
    assert!(prompt.contains("logowanie przez OAuth"), "{prompt}");
}

fn load_reference() -> narsor_engine::domain::WorkflowDefinition {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/implement_feature.yaml");
    load_from_file(path).expect("the §4 example must load without modification")
}

// ---------------------------------------------------------------------------
// Static references (KTD15)
// ---------------------------------------------------------------------------

use narsor_engine::workflow::{action_context_references, context_references};

#[test]
fn context_references_are_listed_once_in_first_use_order() {
    let template = "{{ context.summary }} then {{context.variables.units}}, \
                    {{ inputs.ticket }}, again {{ context.summary }}";
    assert_eq!(
        context_references(template),
        vec!["summary".to_string(), "units".to_string()]
    );
}

#[test]
fn what_rendering_would_refuse_is_not_a_reference() {
    let template = "{{ context.a | default('x') }} {{ context.a.b }} {{ context. }} \
                    {{ workspace.path }} {{ context.open";
    assert!(context_references(template).is_empty());
    assert!(context_references("no placeholders at all").is_empty());
}

#[test]
fn an_actions_references_cover_every_rendered_field() {
    let script: ActionConfig = serde_json::from_value(json!({
        "type": "script",
        "command": "./{{ context.tool }}",
        "args": ["--ticket", "{{ context.ticket }}"],
        "working_dir": "{{ context.dir }}",
        "env": { "B": "{{ context.b_env }}", "A": "{{ context.a_env }}" },
    }))
    .expect("a script action");
    assert_eq!(
        action_context_references(&script),
        vec!["tool", "ticket", "dir", "a_env", "b_env"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );

    let human: ActionConfig = serde_json::from_value(json!({
        "type": "human",
        "instruction": "Decide on {{ context.question }}.",
    }))
    .expect("a human action");
    assert_eq!(action_context_references(&human), vec!["question"]);
}

#[test]
fn strict_rendering_still_refuses_an_unknown_variable() {
    let error = render_error("{{ context.variables.nope }}", &Map::new(), &Context::new());

    assert!(matches!(error, TemplateError::UnknownVariable { .. }));
}

#[test]
fn check_syntax_ignores_unset_variables_but_reports_broken_syntax() {
    use narsor_engine::workflow::template::check_syntax;

    assert!(check_syntax("{{ context.variables.later }} {{ inputs.x }}", "f").is_ok());
    assert!(matches!(
        check_syntax("{{ context.a | upper }}", "f"),
        Err(TemplateError::FilterUnsupported { .. })
    ));
    assert!(matches!(
        check_syntax("{{ inputs.x", "f"),
        Err(TemplateError::UnterminatedPlaceholder { .. })
    ));
    assert!(matches!(
        check_syntax("{{ nope.x }}", "f"),
        Err(TemplateError::UnsupportedReference { .. })
    ));
}

// ---------------------------------------------------------------------------
// Artifacts and declared names (KTD6, KTD7)
// ---------------------------------------------------------------------------

/// A workflow whose `plan` step defines the artifact `plan_doc`, whose
/// `work` step declares the variable `requirements_blocker`, and whose gate
/// asks for the field `comment`.
fn declaring_workflow() -> narsor_engine::domain::WorkflowDefinition {
    narsor_engine::workflow::load_from_str(
        r#"
name: declared-names
version: 1
states:
  plan:
    action: { type: agent, prompt: plan }
    artifacts:
      - { name: plan_doc, description: The plan document. }
    choices: [ready]
    transitions: { ready: work }
  work:
    action: { type: agent, prompt: work }
    variables:
      - { name: requirements_blocker, description: What blocks the requirements. }
    choices: [done]
    transitions: { done: gate }
  gate:
    action:
      type: human
      message: Review.
      inputs:
        - { name: comment, type: text, required: false }
    choices: [approve]
    transitions: { approve: finished }
  finished: { description: done }
terminal_states: [finished]
"#,
    )
    .expect("the workflow loads")
}

fn render_declared(template: &str, context: &Context) -> Result<String, TemplateError> {
    let definition = declaring_workflow();
    let declared = DeclaredNames::of(&definition);
    let inputs = Map::new();
    TemplateScope::new(&inputs, context)
        .with_declared(&declared)
        .render(template, WHERE)
}

fn with_artifact(name: &str, location: &str) -> Context {
    let mut context = Context::new();
    context.add_artifact(
        name.to_string(),
        ArtifactReference {
            id: "artifact-1".to_string(),
            name: Some(name.to_string()),
            location: location.to_string(),
            metadata: Value::Null,
        },
    );
    context
}

#[test]
fn an_artifact_renders_the_path_it_was_last_written_to() {
    let context = with_artifact("plan_doc", "docs/plan.md");

    let rendered = render("Read {{ artifacts.plan_doc }}.", &Map::new(), &context);

    assert_eq!(rendered, "Read docs/plan.md.");
}

#[test]
fn a_defined_artifact_without_a_write_renders_empty() {
    let rendered = render_declared("[{{ artifacts.plan_doc }}]", &Context::new());

    assert_eq!(rendered.expect("a defined artifact renders"), "[]");
}

#[test]
fn an_artifact_no_step_defines_is_unknown() {
    let error = render_declared("{{ artifacts.unknown }}", &Context::new())
        .expect_err("an undefined artifact has no value");

    assert!(
        matches!(error, TemplateError::UnknownVariable { ref reference, .. } if reference == "artifacts.unknown"),
        "{error:?}"
    );
}

#[test]
fn a_context_variable_declared_by_another_step_renders_empty_while_unset() {
    let rendered = render_declared("[{{ context.requirements_blocker }}]", &Context::new());

    assert_eq!(rendered.expect("a declared variable renders"), "[]");
}

#[test]
fn a_gate_input_field_counts_as_a_declared_context_variable() {
    let rendered = render_declared("[{{ context.variables.comment }}]", &Context::new());

    assert_eq!(rendered.expect("a gate field renders"), "[]");
}

#[test]
fn an_undeclared_unset_context_variable_is_still_unknown() {
    let error = render_declared("{{ context.typo }}", &Context::new())
        .expect_err("nothing declares the variable");

    assert!(
        matches!(error, TemplateError::UnknownVariable { ref reference, .. } if reference == "context.typo"),
        "{error:?}"
    );
}

#[test]
fn check_syntax_and_reference_collectors_know_the_artifacts_root() {
    use narsor_engine::workflow::template::{artifact_references, check_syntax};

    assert!(check_syntax("{{ artifacts.plan_doc }}", "f").is_ok());
    assert_eq!(
        artifact_references(
            "{{ artifacts.plan_doc }} {{ artifacts.review }} {{ artifacts.plan_doc }}"
        ),
        vec!["plan_doc".to_string(), "review".to_string()]
    );
}

// ---------------------------------------------------------------------------
// `{{ task.ticket }}` and `{{ task.title }}`
// ---------------------------------------------------------------------------

#[test]
fn task_fields_render_the_ticket_and_title() {
    let (inputs, context) = (inputs(&[]), context(&[]));
    let scope = TemplateScope::new(&inputs, &context)
        .with_task_fields(Some("NAR-12"), Some("Zmiana API zamówień"));
    assert_eq!(
        scope
            .render("Ticket {{ task.ticket }}: {{ task.title }}", WHERE)
            .unwrap(),
        "Ticket NAR-12: Zmiana API zamówień"
    );
}

#[test]
fn task_fields_without_a_value_render_empty_text() {
    let (inputs, context) = (inputs(&[]), context(&[]));
    let none = TemplateScope::new(&inputs, &context).with_task_fields(None, None);
    assert_eq!(none.render("[{{ task.ticket }}]", WHERE).unwrap(), "[]");
    // A scope that never got them reads both as empty, not as an error.
    assert_eq!(render("[{{ task.title }}]", &inputs, &context), "[]");
}

#[test]
fn a_task_ticket_does_not_collide_with_an_input_of_the_same_name() {
    let (inputs, context) = (inputs(&[("ticket", json!("B"))]), context(&[]));
    let scope = TemplateScope::new(&inputs, &context).with_task_fields(Some("A"), None);
    assert_eq!(
        scope
            .render("{{ task.ticket }}/{{ inputs.ticket }}", WHERE)
            .unwrap(),
        "A/B"
    );
}

#[test]
fn an_unknown_task_field_is_an_error_and_filters_stay_unsupported() {
    let (inputs, context) = (inputs(&[]), context(&[]));
    assert!(matches!(
        render_error("{{ task.foo }}", &inputs, &context),
        TemplateError::UnknownVariable { .. }
    ));
    assert!(matches!(
        render_error("{{ task.ticket | upper }}", &inputs, &context),
        TemplateError::FilterUnsupported { .. }
    ));
}

#[test]
fn a_workflow_may_use_task_fields_in_prompts_and_workspace() {
    let yaml = r#"
name: labelled
version: 1
globals:
  workspace:
    type: git
    repository: "{{ task.ticket }}"
states:
  plan:
    action:
      type: agent
      prompt: "Ticket {{ task.ticket }}: {{ task.title }}"
    choices: [done]
    transitions:
      done: completed
"#;
    narsor_engine::workflow::load_from_str(yaml).expect("task.* passes validation");
}

use narsor_engine::domain::{PromptPart, PromptText};

const PROMPT: &str = "state 'work', prompt";

fn conditional(text: &str, when: &[&str]) -> PromptPart {
    PromptPart::when(text, when.iter().map(|name| (*name).to_string()).collect())
}

/// Renders `parts` on `context` with the names in `declared` known to the
/// workflow, and returns the joined text.
fn render_parts(parts: Vec<PromptPart>, context: &Context) -> String {
    try_render_parts(parts, context, &HashMap::new())
        .expect("the parts must render")
        .to_string()
}

fn try_render_parts(
    parts: Vec<PromptPart>,
    context: &Context,
    replies: &HashMap<String, Value>,
) -> Result<PromptText, TemplateError> {
    let inputs = inputs(&[]);
    let declared =
        DeclaredNames::of(&narsor_engine::workflow::load_from_str(PARTS_WORKFLOW).unwrap());
    TemplateScope::new(&inputs, context)
        .with_declared(&declared)
        .with_step_replies(replies)
        .render_text(&PromptText::from_parts(parts), PROMPT)
}

/// A workflow that declares the variables the parts below refer to.
const PARTS_WORKFLOW: &str = r#"
schema: v2
name: parts
version: 1
states:
  work:
    action:
      type: agent
      prompt: x
    variables:
      - name: notes
      - name: flag
      - name: count
    exit:
      next: done
  done:
    description: end
terminal_states:
  done: completed
"#;

#[test]
fn a_part_whose_condition_names_an_empty_variable_is_left_out() {
    let parts = || {
        vec![
            PromptPart::always("first"),
            conditional("second {{ context.notes }}", &["context.notes"]),
            PromptPart::always("third"),
        ]
    };

    for empty in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!(" \n\t")),
    ] {
        let pairs: Vec<(&str, Value)> = empty.into_iter().map(|value| ("notes", value)).collect();
        assert_eq!(render_parts(parts(), &context(&pairs)), "first\n\nthird");
    }
}

#[test]
fn false_and_zero_are_values_so_their_part_stays() {
    for value in [json!(false), json!(0)] {
        let rendered = render_parts(
            vec![conditional("kept", &["context.flag"])],
            &context(&[("flag", value)]),
        );
        assert_eq!(rendered, "kept");
    }
}

#[test]
fn every_reference_of_a_condition_must_have_a_value() {
    let parts = || vec![conditional("both", &["context.notes", "context.flag"])];

    let one = context(&[("notes", json!("n"))]);
    let both = context(&[("notes", json!("n")), ("flag", json!(false))]);

    assert_eq!(render_parts(parts(), &one), "");
    assert_eq!(render_parts(parts(), &both), "both");
}

#[test]
fn the_parts_that_apply_are_joined_by_an_empty_line() {
    let rendered = render_parts(
        vec![
            PromptPart::always("one"),
            PromptPart::always("two"),
            conditional("three", &["context.notes"]),
        ],
        &context(&[("notes", json!("x"))]),
    );

    assert_eq!(rendered, "one\n\ntwo\n\nthree");
}

#[test]
fn an_error_in_a_part_names_the_part() {
    let error = try_render_parts(
        vec![
            PromptPart::always("fine"),
            PromptPart::always("{{ context.undeclared }}"),
        ],
        &Context::new(),
        &HashMap::new(),
    )
    .expect_err("the second part refers to an unknown variable");

    let TemplateError::UnknownVariable { location, .. } = error else {
        panic!("expected an unknown variable, got {error:?}");
    };
    assert_eq!(location, format!("{PROMPT}, part 2"));
}

#[test]
fn an_error_in_a_condition_names_the_part_and_the_condition() {
    let error = try_render_parts(
        vec![
            PromptPart::always("fine"),
            conditional("x", &["context.undeclared"]),
        ],
        &Context::new(),
        &HashMap::new(),
    )
    .expect_err("the condition refers to an unknown variable");

    let TemplateError::UnknownVariable { location, .. } = error else {
        panic!("expected an unknown variable, got {error:?}");
    };
    assert_eq!(location, format!("{PROMPT}, part 2, when"));
}

#[test]
fn a_condition_on_a_step_that_has_not_answered_leaves_the_part_out() {
    let parts = || {
        vec![
            PromptPart::always("always"),
            conditional("reply: {{ steps.review.reply }}", &["steps.review.reply"]),
        ]
    };

    let without = try_render_parts(parts(), &Context::new(), &HashMap::new()).unwrap();
    let with = try_render_parts(
        parts(),
        &Context::new(),
        &HashMap::from([("review".to_string(), json!("looks fine"))]),
    )
    .unwrap();

    assert_eq!(without.to_string(), "always");
    assert_eq!(with.to_string(), "always\n\nreply: looks fine");
}

#[test]
fn a_reference_to_a_missing_step_result_in_the_text_stays_an_error() {
    let error = try_render_parts(
        vec![PromptPart::always("reply: {{ steps.review.reply }}")],
        &Context::new(),
        &HashMap::new(),
    )
    .expect_err("the text needs the result");

    assert!(matches!(error, TemplateError::UnknownVariable { .. }));
}

#[test]
fn a_filter_in_a_condition_is_refused() {
    let error = try_render_parts(
        vec![conditional("x", &["context.notes | upper"])],
        &Context::new(),
        &HashMap::new(),
    )
    .expect_err("filters stay unsupported");

    assert!(matches!(error, TemplateError::FilterUnsupported { .. }));
}

#[test]
fn a_plain_string_prompt_renders_as_before() {
    let inputs = inputs(&[("description", json!("login"))]);
    let context = Context::new();

    let rendered = TemplateScope::new(&inputs, &context)
        .render_text(&"Do {{ inputs.description }}".into(), PROMPT)
        .unwrap();

    assert_eq!(rendered.to_string(), "Do login");
    assert!(rendered.is_plain());
}

#[test]
fn an_error_in_a_plain_prompt_does_not_name_a_part() {
    let inputs = inputs(&[]);
    let context = Context::new();

    let error = TemplateScope::new(&inputs, &context)
        .render_text(&"{{ inputs.missing }}".into(), PROMPT)
        .expect_err("the input is not there");

    let TemplateError::UnknownVariable { location, .. } = error else {
        panic!("expected an unknown variable, got {error:?}");
    };
    assert_eq!(location, PROMPT);
}

#[test]
fn a_stored_definition_with_a_string_prompt_still_loads() {
    let definition = narsor_engine::workflow::load_from_str(PARTS_WORKFLOW).unwrap();
    let mut stored = serde_json::to_value(&definition).unwrap();
    // The shape every definition stored before parts existed has.
    assert_eq!(stored["states"]["work"]["action"]["prompt"], json!("x"));
    stored["states"]["work"]["action"]["prompt"] = json!("written long ago");

    let loaded: narsor_engine::domain::WorkflowDefinition =
        serde_json::from_value(stored).expect("a string prompt must load");

    let Some(ActionConfig::Agent(agent)) = &loaded.states["work"].action else {
        panic!("expected an agent action");
    };
    assert!(agent.prompt.is_plain());
    assert_eq!(agent.prompt.to_string(), "written long ago");
}

#[test]
fn a_list_of_parts_survives_storing_and_loading() {
    let yaml = PARTS_WORKFLOW.replace(
        "prompt: x",
        "prompt:\n        - first\n        - when: [context.notes, context.flag]\n          text: second",
    );
    let definition = narsor_engine::workflow::load_from_str(&yaml).unwrap();

    let stored = serde_json::to_string(&definition).unwrap();
    let loaded: narsor_engine::domain::WorkflowDefinition = serde_json::from_str(&stored).unwrap();

    let Some(ActionConfig::Agent(agent)) = &loaded.states["work"].action else {
        panic!("expected an agent action");
    };
    assert_eq!(
        agent.prompt.parts(),
        [
            PromptPart::always("first"),
            conditional("second", &["context.notes", "context.flag"]),
        ]
    );
}

#[test]
fn a_condition_without_references_does_not_load() {
    let yaml = PARTS_WORKFLOW.replace(
        "prompt: x",
        "prompt:\n        - when: []\n          text: second",
    );

    assert!(narsor_engine::workflow::load_from_str(&yaml).is_err());
}
