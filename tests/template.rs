//! Template substitution (U4): `{{ inputs.* }}` and `{{ context.variables.* }}`.
//!
//! The reference fixture is `tests/fixtures/implement_feature.yaml`, copied
//! verbatim from `WORKFLOW_SCHEMA.md` §4.

use std::collections::HashMap;

use narsor::domain::{ActionConfig, Context};
use narsor::workflow::load_from_file;
use narsor::workflow::template::{TemplateError, TemplateScope};
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
    let inputs = inputs(&[("feature_description", json!("login"))]);

    let rendered = render(
        "Zadanie: {{ inputs.feature_description }}",
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
    // `WORKFLOW_SCHEMA.md` §3.3.D writes the wait filter as
    // `{{ context.pr_number }}`, which must mean the same variable.
    let context = context(&[("pr_number", json!(123))]);

    let shorthand = render("{{ context.pr_number }}", &Map::new(), &context);
    let full_path = render("{{ context.variables.pr_number }}", &Map::new(), &context);

    assert_eq!(shorthand, full_path);
    assert_eq!(shorthand, "123");
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
    let inputs = inputs(&[("feature_description", json!("login"))]);
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

    let inputs = inputs(&[
        ("feature_description", json!("logowanie przez OAuth")),
        ("target_repo", json!("acme/app")),
    ]);
    // Deliberately empty: `implement` is reachable from `analyze` (label `ready`)
    // without passing through `human_clarification`, so the prompt must render
    // on a context that carries nothing at all.
    let context = context(&[]);

    let rendered = TemplateScope::new(&inputs, &context)
        .render_action(action, "state 'implement'")
        .expect("the corrected §4 prompt must render");

    let prompt = match rendered {
        ActionConfig::Agent(config) => config.prompt,
        other => panic!("expected an agent action, got {other:?}"),
    };

    assert!(
        !prompt.contains("{{"),
        "the rendered prompt still contains a placeholder: {prompt}"
    );
    assert!(prompt.contains("logowanie przez OAuth"), "{prompt}");
}

fn load_reference() -> narsor::domain::WorkflowDefinition {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/implement_feature.yaml");
    load_from_file(path).expect("the §4 example must load without modification")
}

// ---------------------------------------------------------------------------
// Static references (KTD15)
// ---------------------------------------------------------------------------

use narsor::workflow::{action_context_references, context_references};

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
