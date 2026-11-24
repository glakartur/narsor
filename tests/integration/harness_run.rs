//! The unified harness `run` (R1–R6): every CLI harness takes the same
//! `AgentRequest`, gets a prompt of any size to the agent, makes context
//! files readable, and reports the model and effort it resolved. The
//! harnesses run as the fake harness, so nothing here costs money.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use narsor_engine::engine::{
    AgentRequest, ClaudeCodeAdapter, CodexAdapter, ContextFile, ContextFiles, HarnessAdapter,
    OpenCodeAdapter, RunObservers, RunStarted,
};

use crate::engine_support::FakeHarness;

/// Linux's limit for one command-line argument.
const MAX_ARG_STRLEN: usize = 128 * 1024;

/// A fresh directory, removed when dropped.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("narsor-harness-run-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn harness(name: &str, program: String) -> Arc<dyn HarnessAdapter> {
    match name {
        "claude-code" => Arc::new(ClaudeCodeAdapter::with_program(program)),
        "codex" => Arc::new(CodexAdapter::with_program(program)),
        "opencode" => Arc::new(OpenCodeAdapter::with_program(program)),
        other => unreachable!("{other}"),
    }
}

/// Runs `request` on the CLI harness `name` (the fake binary) and returns the
/// fake, to read what the process got.
async fn run_on(name: &str, root: &Path, request: &AgentRequest) -> FakeHarness {
    let fake = FakeHarness::install(root.join(name));
    harness(name, fake.program())
        .run(request, RunObservers::none())
        .await
        .expect("the harness ran");
    fake
}

/// AE1: a prompt over the single-argument limit reaches every CLI harness
/// on stdin; codex is told to read it with `-`.
#[tokio::test]
async fn every_cli_harness_takes_a_long_prompt_on_stdin() {
    let root = TempDir::new();
    let prompt = "x".repeat(200 * 1024);
    let request = AgentRequest::new(prompt.clone(), &*root);

    for name in ["claude-code", "codex", "opencode"] {
        let fake = run_on(name, &root, &request).await;
        let args = fake.args(1);
        assert!(
            args.iter().all(|arg| arg.len() < MAX_ARG_STRLEN),
            "{name}: an argument over the limit"
        );
        assert_eq!(fake.stdin(1), prompt, "{name}: the prompt on stdin");
        assert_eq!(
            args.last().map(String::as_str) == Some("-"),
            name == "codex",
            "{name}"
        );
    }
}

/// AE2: the context files are written to their directory and the prompt
/// lists them; claude gets read access with `--add-dir`, codex and opencode
/// read outside their working directory without a flag.
#[tokio::test]
async fn context_files_are_written_listed_and_readable() {
    let root = TempDir::new();
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let context = root.join("context");
    let request = AgentRequest {
        context: Some(ContextFiles {
            dir: context.clone(),
            files: vec![ContextFile::new("events.md", "Every event", "event one")],
        }),
        ..AgentRequest::new("decide", &work)
    };

    for name in ["claude-code", "codex", "opencode"] {
        let fake = run_on(name, &root, &request).await;
        let args = fake.args(1);
        let prompt = args.last().unwrap();
        let file = context.join("events.md");
        assert!(prompt.starts_with("decide"), "{name}: {prompt}");
        assert!(
            prompt.contains(&format!("`{}` (9 bytes): Every event", file.display())),
            "{name}: {prompt}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "event one", "{name}");
        let add_dir = args
            .windows(2)
            .any(|pair| pair[0] == "--add-dir" && Path::new(&pair[1]) == context);
        assert_eq!(add_dir, name == "claude-code", "{name}: {args:?}");
    }
}

/// R4: the observer hears the model name and the effort the harness runs
/// with — not the tier, and for codex the effort it passes in `-c`.
#[tokio::test]
async fn the_started_observer_gets_the_resolved_model_and_effort() {
    let root = TempDir::new();
    let request = AgentRequest {
        model: Some("max".into()),
        effort: Some("max".into()),
        ..AgentRequest::new("go", &*root)
    };

    for (name, model, effort) in [
        ("claude-code", "opus", Some("max")),
        ("codex", "gpt-5.2-codex", Some("xhigh")),
        ("opencode", "anthropic/claude-opus-4-5", None),
    ] {
        let fake = FakeHarness::install(root.join(name));
        let heard = Mutex::new(None);
        let on_started = |started: &RunStarted| *heard.lock().unwrap() = Some(started.clone());
        harness(name, fake.program())
            .run(&request, RunObservers::none().on_started(&on_started))
            .await
            .unwrap();
        assert_eq!(
            heard.into_inner().unwrap(),
            Some(RunStarted {
                model: Some(model.into()),
                effort: effort.map(Into::into),
            }),
            "{name}"
        );
    }
}

/// Extra arguments go right before the prompt even when the harness changed
/// the prompt (codex and opencode add the read-only paths to it), which the
/// old search for the prompt text in argv missed.
#[tokio::test]
async fn extra_args_stand_right_before_a_prompt_the_harness_extended() {
    let root = TempDir::new();
    let request = AgentRequest {
        read_only_paths: vec!["vendor".into()],
        extra_args: vec!["--foo".into(), "bar".into()],
        ..AgentRequest::new("go", &*root)
    };

    for name in ["codex", "opencode"] {
        let fake = run_on(name, &root, &request).await;
        let args = fake.args(1);
        let tail = &args[args.len() - 3..];
        assert_eq!(tail[..2], ["--foo", "bar"], "{name}: {args:?}");
        assert!(tail[2].contains("vendor"), "{name}: {}", tail[2]);
    }
}
