//! The Kubernetes adapter (U5) without a live cluster.
//!
//! * The backend contract suite of U4, **unchanged**, against an in-process
//!   mock of the Kubernetes API whose kubelet runs the real `narsor-runner`
//!   from each generated Pod spec (AE2).
//! * Manifest-shape tests: what the generated Pod, PVC and Secret look like
//!   and what is refused before any API call.
//! * Adapter-level scenarios against the mock (deletions, retention,
//!   storage classes, foreign objects) and end-to-end acceptance examples
//!   AE1, AE4, AE5, AE8, AE9 and AE11.
//! * The deployment manifests under `deploy/kubernetes/`: RBAC and
//!   admission rules, parsed and checked.
//!
//! An optional smoke test against a real cluster is `#[ignore]`d and
//! gated by environment variables.

#[path = "engine_support/mod.rs"]
mod engine_support;
mod environment_support;
#[macro_use]
mod backend_contract_support;
mod kubernetes_support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use backend_contract_support::{ContractTarget, Harness};
use kubernetes_support::{MockCluster, DELETE_CLASS, NAMESPACE, PROFILE_SECRET, RETAIN_CLASS};
use narsor_controller::controller::{edit_profile, ClusterArgs, EditProfileArgs};
use narsor_controller::kubernetes::manifest::{self, check_pod_shape, KubernetesBackendConfig};
use narsor_controller::kubernetes::{launched_under_epoch, names, KubernetesBackend};
use narsor_engine::backends::{
    BackendErrorKind, ExecutionBackend, Removal, RunnerCredential, StorageState, TaskResources,
    WorkloadLaunch, WorkloadPhase,
};
use narsor_engine::domain::{
    BackendId, GitIdentity, ImageDigest, ImageSource, ResolvedEnvironment, ResolvedImage,
    ResourceProfile, SecretDelivery, SecretReference, TaskId, TaskLaunchSpec, TaskWorkspace,
};
use narsor_engine::orchestration::{Observation, OwnerCommand, TaskLifecycle};
use narsor_engine::runner::{HarnessHome, HomeMode};
use serde_json::Value;

/// Registry prefix every test image comes from.
const REGISTRY: &str = "registry.test/";

/// A workflow of one script step that finishes at once; the runner then
/// idles until the control plane tells it otherwise.
const WORKFLOW: &str = r#"
name: contract-task
version: 1
states:
  work:
    action:
      type: script
      command: sh
      args: [-c, "echo worked >> \"$HOME/steps\"; printf '{\"output\": \"done\"}'"]
    outputs: [done]
    transitions:
      done: completed
  completed:
    description: Done.
terminal_states: [completed]
"#;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn image(repository: &str, ch: char, source: ImageSource) -> ResolvedImage {
    let digest = ImageDigest::parse(format!("sha256:{}", ch.to_string().repeat(64))).unwrap();
    ResolvedImage {
        reference: format!("{repository}:1"),
        pinned: format!("{repository}@{digest}"),
        digest,
        tag_resolved: false,
        source,
    }
}

/// An environment the adapter accepts: pinned base image and runtime
/// bundle from the allowed registry.
fn environment() -> ResolvedEnvironment {
    let mut environment = ResolvedEnvironment::harness_only("claude-code");
    environment.base_image = Some(image(
        "registry.test/acme/dev",
        'a',
        ImageSource::Repository {
            repository: "acme".into(),
        },
    ));
    environment.runtime_bundle = Some(image(
        "registry.test/narsor/runtime",
        'b',
        ImageSource::RuntimeBundle,
    ));
    environment.sealed()
}

fn spec_with(environment: ResolvedEnvironment, resources: ResourceProfile) -> TaskLaunchSpec {
    let definition = narsor_engine::workflow::load_from_str(WORKFLOW).expect("a valid workflow");
    TaskLaunchSpec::new(
        definition.id.clone(),
        definition.version,
        serde_json::json!({}),
        TaskWorkspace::None,
        environment,
        resources,
    )
    .with_definition(&definition)
    .expect("snapshot the definition")
}

fn spec() -> TaskLaunchSpec {
    spec_with(environment(), ResourceProfile::new(1500, 3072))
}

fn config() -> KubernetesBackendConfig {
    let mut config = KubernetesBackendConfig::new(DELETE_CLASS, vec![REGISTRY.to_string()]);
    config.allowed_secrets = vec!["narsor-github/token".into(), "narsor-git/credential".into()];
    config
}

fn launch_of(task: &str, generation: u64, spec: TaskLaunchSpec) -> WorkloadLaunch {
    WorkloadLaunch {
        task_id: TaskId::parse(task).unwrap(),
        generation,
        incarnation: format!(
            "{}-g{generation}-{}",
            names::workload(task),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ),
        lease_epoch: 7,
        launch_spec: serde_json::to_value(spec).unwrap(),
        control_plane_url: "https://narsor.example".into(),
        credential: RunnerCredential::new("nsr.secret-runner-credential"),
    }
}

fn github_secret() -> SecretReference {
    SecretReference {
        backend: BackendId::parse("k8s").unwrap(),
        name: "github-token".into(),
        tool: "gh".into(),
        key: "narsor-github/token".into(),
        delivery: SecretDelivery::Env("GH_TOKEN".into()),
    }
}

fn task_resources(backend: &KubernetesBackend, task: &str) -> TaskResources {
    backend
        .resources(&TaskId::parse(task).unwrap(), task)
        .expect("resource names")
}

fn scratch() -> PathBuf {
    std::env::temp_dir().join(format!("narsor-k8s-{}", uuid::Uuid::new_v4()))
}

/// A mock cluster whose Pods stay Pending, and an adapter on it.
struct Quiet {
    cluster: Arc<MockCluster>,
    backend: KubernetesBackend,
    dir: PathBuf,
}

impl Quiet {
    fn new() -> Self {
        Self::with(config())
    }

    fn with(config: KubernetesBackendConfig) -> Self {
        let dir = scratch();
        let cluster = MockCluster::start(
            &dir,
            engine_support::workspace_binary("narsor-runner"),
            false,
        );
        let backend = KubernetesBackend::new(&cluster.access(), config).expect("adapter");
        Self {
            cluster,
            backend,
            dir,
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        self.cluster.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn wait_until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// The contract suite (AE2)
// ---------------------------------------------------------------------------

/// The mock cluster with a kubelet running real runners.
struct KubernetesTarget {
    cluster: Arc<MockCluster>,
}

impl ContractTarget for KubernetesTarget {
    type Backend = KubernetesBackend;

    fn setup(scratch: &Path) -> Option<Self> {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: the runner refuses to run as root");
            return None;
        }
        Some(Self {
            cluster: MockCluster::start(
                scratch,
                engine_support::workspace_binary("narsor-runner"),
                true,
            ),
        })
    }

    fn backend(&self) -> KubernetesBackend {
        // A new instance per controller, sharing only the cluster.
        KubernetesBackend::new(&self.cluster.access(), config()).expect("adapter")
    }

    fn launch_spec(&self) -> TaskLaunchSpec {
        spec()
    }

    fn kill_workload(&self, _backend: &KubernetesBackend, task: &TaskResources) {
        assert!(self.cluster.kill_runner(&task.workload), "a runner to kill");
    }

    fn cleanup(&self, _backend: &KubernetesBackend) {
        self.cluster.shutdown();
    }
}

contract_suite!(kubernetes, crate::KubernetesTarget);

// ---------------------------------------------------------------------------
// Manifest shape
// ---------------------------------------------------------------------------

/// A named change to a generated Pod.
type Mutation = (&'static str, Box<dyn Fn(&mut Value)>);

fn generated_pod(spec: TaskLaunchSpec) -> (Value, TaskResources, WorkloadLaunch) {
    generated_pod_with(&config(), spec)
}

fn generated_pod_with(
    config: &KubernetesBackendConfig,
    spec: TaskLaunchSpec,
) -> (Value, TaskResources, WorkloadLaunch) {
    let quiet_names = TaskResources {
        task_id: TaskId::parse("Task_1").unwrap(),
        resource_key: "Task_1".into(),
        workload: names::workload("Task_1"),
        storage: names::storage("Task_1"),
    };
    let launch = launch_of("Task_1", 3, spec);
    let plan = manifest::plan(config, &launch).expect("a valid launch");
    (
        manifest::pod(config, NAMESPACE, &quiet_names, &launch, &plan),
        quiet_names,
        launch,
    )
}

fn runner_args(pod: &Value) -> Vec<String> {
    pod["spec"]["containers"][0]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect()
}

/// A Pod keeps the private HOME unless the backend says otherwise, and the
/// runner is told so on every start.
#[test]
fn the_runner_is_told_the_backends_home_mode() {
    let (pod, _, _) = generated_pod(spec());
    let args = runner_args(&pod);
    let at = args.iter().position(|a| a == "--home").expect("--home");
    assert_eq!(args[at + 1], "private");
    assert!(!args.iter().any(|a| a == "--codex-home"));

    let mut config = config();
    config.harness_home = HarnessHome {
        mode: HomeMode::Host,
        codex_home: Some("/mnt/codex".into()),
        claude_config_dir: None,
        claude_profile_source: None,
    };
    let args = runner_args(&generated_pod_with(&config, spec()).0);
    let tail: Vec<&str> = args[args.len() - 4..].iter().map(String::as_str).collect();
    assert_eq!(tail, ["--home", "host", "--codex-home", "/mnt/codex"]);
}

/// One main runner container, one init container, separate task and PATH
/// volumes, a non-root-compatible security context (U5 scenario 1).
#[test]
fn the_pod_has_one_runner_one_init_container_and_separate_volumes() {
    let (pod, task, launch) = generated_pod(spec());
    let spec = &pod["spec"];
    assert_eq!(spec["containers"].as_array().unwrap().len(), 1);
    assert_eq!(spec["initContainers"].as_array().unwrap().len(), 1);
    let runner = &spec["containers"][0];
    let init = &spec["initContainers"][0];

    assert_eq!(runner["name"], "runner");
    assert_eq!(
        runner["image"],
        format!("registry.test/acme/dev@sha256:{}", "a".repeat(64)),
        "the repository's own pinned base image (R21)"
    );
    assert_eq!(
        runner["command"],
        serde_json::json!(["/opt/narsor/bin/narsor-runner"])
    );
    let args: Vec<&str> = runner["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(args[0], "--task-id");
    for (flag, value) in [
        ("--task-id", "Task_1"),
        ("--task-storage", "/task"),
        ("--incarnation", launch.incarnation.as_str()),
        ("--control-plane", "https://narsor.example"),
        ("--credential-file", "/var/run/narsor/launch/credential"),
        ("--launch-spec", "/var/run/narsor/launch/launch-spec.json"),
    ] {
        let at = args
            .iter()
            .position(|a| *a == flag)
            .unwrap_or_else(|| panic!("{flag}"));
        assert_eq!(args[at + 1], value, "{flag}");
    }
    assert!(
        !args.contains(&"--secrets-dir"),
        "no secrets, no secrets dir"
    );
    let env = runner["env"].as_array().unwrap();
    assert_eq!(env.len(), 1, "PATH only; HOME is the runner's: {env:?}");
    assert_eq!(env[0]["name"], "PATH");
    assert!(env[0]["value"]
        .as_str()
        .unwrap()
        .starts_with("/opt/narsor/bin:"));

    assert_eq!(
        init["image"],
        format!("registry.test/narsor/runtime@sha256:{}", "b".repeat(64))
    );
    assert_eq!(
        init["command"],
        serde_json::json!(manifest::INSTALL_COMMAND)
    );
    assert_eq!(init["volumeMounts"][0]["mountPath"], "/opt/narsor");
    assert_eq!(init["volumeMounts"][0]["name"], "narsor-runtime");

    let mounts: Vec<(&str, &str, bool)> = runner["volumeMounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["name"].as_str().unwrap(),
                m["mountPath"].as_str().unwrap(),
                m["readOnly"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        mounts,
        vec![
            ("task-storage", "/task", false),
            ("narsor-runtime", "/opt/narsor", true),
            ("narsor-launch", "/var/run/narsor/launch", true),
        ],
        "task storage writable; runtime and launch material read-only and outside it"
    );
    let volumes = spec["volumes"].as_array().unwrap();
    assert_eq!(
        volumes[0]["persistentVolumeClaim"]["claimName"],
        task.storage
    );
    assert!(
        volumes[1]["emptyDir"].is_object(),
        "the PATH volume is its own emptyDir"
    );
    assert_eq!(
        volumes[2]["secret"]["secretName"],
        names::launch_secret(&task.workload, &launch.incarnation)
    );

    let pod_security = &spec["securityContext"];
    assert_eq!(pod_security["runAsNonRoot"], true);
    assert!(pod_security["runAsUser"].as_u64().unwrap() > 0);
    assert_eq!(pod_security["seccompProfile"]["type"], "RuntimeDefault");
    for container in [runner, init] {
        let security = &container["securityContext"];
        assert_eq!(security["allowPrivilegeEscalation"], false);
        assert_eq!(security["privileged"], false);
        assert_eq!(security["capabilities"]["drop"], serde_json::json!(["ALL"]));
        assert_eq!(container["resources"]["limits"]["cpu"], "1500m");
        assert_eq!(container["resources"]["limits"]["memory"], "3072Mi");
    }

    // Labels carry task, generation and lease epoch (KTD2).
    let labels = &pod["metadata"]["labels"];
    assert_eq!(labels[manifest::TASK_LABEL], names::key_hash("Task_1"));
    assert_eq!(labels[manifest::GENERATION_KEY], "3");
    assert_eq!(launched_under_epoch(&pod), Some(7));
    assert_eq!(
        pod["metadata"]["annotations"][manifest::TASK_ID_ANNOTATION],
        "Task_1"
    );
}

#[test]
fn configured_claude_profile_mounts_read_only_for_every_runner() {
    let mut configured = config();
    configured.claude_profile_secret = Some("narsor-claude-profile".into());
    configured.claude_profile_files = vec![
        "approved-settings=settings.json".into(),
        "auth=.credentials.json".into(),
        "skill=skills/review/SKILL.md".into(),
    ];
    let task = TaskResources {
        task_id: TaskId::parse("Task_1").unwrap(),
        resource_key: "Task_1".into(),
        workload: names::workload("Task_1"),
        storage: names::storage("Task_1"),
    };
    let launch = launch_of("Task_1", 3, spec());
    let plan = manifest::plan(&configured, &launch).expect("valid plan");
    let pod = manifest::pod(&configured, "narsor-user", &task, &launch, &plan);
    let profile = pod["spec"]["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|volume| volume["name"] == "claude-profile")
        .unwrap();
    assert_eq!(profile["secret"]["secretName"], "narsor-claude-profile");
    assert_eq!(
        profile["secret"]["items"],
        serde_json::json!([
            {"key":"approved-settings","path":"settings.json"},
            {"key":"auth","path":".credentials.json"},
            {"key":"skill","path":"skills/review/SKILL.md"}
        ])
    );
    let runner = &pod["spec"]["containers"][0];
    assert!(runner["volumeMounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|mount| mount["name"] == "claude-profile" && mount["readOnly"] == true));
    let args = runner["args"].as_array().unwrap();
    assert!(args
        .windows(2)
        .any(|pair| pair[0] == "--claude-profile-source"
            && pair[1] == "/var/run/narsor/claude-profile"));
    manifest::check_pod_shape(&pod, &configured.shape_policy())
        .expect("profile volume passes admission mirror");
}

#[test]
fn the_claude_profile_source_comes_only_from_the_profile_secret() {
    let (pod, _, _) = generated_pod(spec());
    assert!(!runner_args(&pod)
        .iter()
        .any(|arg| arg == "--claude-profile-source"));

    let mut configured = config();
    configured.harness_home.claude_profile_source = Some("/mnt/profile".into());
    assert!(configured
        .problems()
        .iter()
        .any(|problem| problem.contains("claude_profile_source")));
}

// ---------------------------------------------------------------------------
// Editing the Claude profile Secret in the controller (U7)
// ---------------------------------------------------------------------------

fn profile_config() -> KubernetesBackendConfig {
    let mut configured = config();
    configured.claude_profile_secret = Some(PROFILE_SECRET.into());
    configured.claude_profile_files = vec![
        "settings=settings.json".into(),
        "auth=.credentials.json".into(),
        "lint-skill=skills/lint/SKILL.md".into(),
    ];
    configured
}

/// `edit-claude-profile` against the mock cluster, with `harness` (a shell
/// script) in place of Claude Code.
fn edit_args(quiet: &Quiet, harness: &str) -> EditProfileArgs {
    let config_file = quiet.dir.join("backend.yaml");
    std::fs::write(
        &config_file,
        serde_yaml::to_string(quiet.backend.config()).unwrap(),
    )
    .unwrap();
    let token_file = quiet.dir.join("token");
    std::fs::write(&token_file, kubernetes_support::TOKEN).unwrap();
    let script = quiet.dir.join("claude.sh");
    std::fs::write(&script, format!("#!/bin/sh\nset -e\n{harness}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    EditProfileArgs {
        kubernetes_config: config_file,
        cluster: ClusterArgs {
            namespace: Some(NAMESPACE.into()),
            api_server: Some(quiet.cluster.url.clone()),
            token_file: Some(token_file),
            ca_file: None,
        },
        claude_command: script,
        session_parent: Some(quiet.dir.clone()),
        yes: true,
    }
}

#[tokio::test]
async fn a_profile_edit_session_saves_changed_profile_files_to_the_secret() {
    let quiet = Quiet::with(profile_config());
    quiet.cluster.seed_profile_secret(&[
        ("settings", r#"{"model":"opus"}"#),
        ("auth", r#"{"token":"a"}"#),
        ("unmapped", "kept"),
    ]);
    let seen = quiet.dir.join("seen");
    let harness = format!(
        r#"test "$HOME" != "{host_home}"
printf '%s\n' "$CLAUDE_CONFIG_DIR" "${{ANTHROPIC_API_KEY-unset}}" > "{seen}"
test "$(cat "$CLAUDE_CONFIG_DIR/.credentials.json")" = '{{"token":"a"}}'
printf '{{"model":"fable"}}' > "$CLAUDE_CONFIG_DIR/settings.json"
printf '{{"token":"b"}}' > "$CLAUDE_CONFIG_DIR/.credentials.json"
mkdir -p "$CLAUDE_CONFIG_DIR/projects/-work"
printf 'session' > "$CLAUDE_CONFIG_DIR/projects/-work/session.jsonl"
printf 'history' > "$CLAUDE_CONFIG_DIR/history.jsonl""#,
        host_home = std::env::var("HOME").unwrap_or_default(),
        seen = seen.display(),
    );

    edit_profile(edit_args(&quiet, &harness))
        .await
        .expect("the session is saved");

    let data = quiet.cluster.profile_secret_data();
    assert_eq!(
        data,
        std::collections::BTreeMap::from([
            ("auth".to_string(), r#"{"token":"b"}"#.to_string()),
            ("settings".to_string(), r#"{"model":"fable"}"#.to_string()),
            ("unmapped".to_string(), "kept".to_string()),
        ]),
        "history and sessions never reach the Secret"
    );
    let writes: Vec<String> = quiet
        .cluster
        .mutations()
        .into_iter()
        .map(|request| format!("{} {}", request.method, request.path))
        .collect();
    assert_eq!(
        writes,
        vec![format!(
            "PUT /api/v1/namespaces/{NAMESPACE}/secrets/{PROFILE_SECRET}"
        )],
        "the configured Secret is the only write"
    );
    let seen = std::fs::read_to_string(seen).unwrap();
    let mut seen = seen.lines();
    let config_dir = PathBuf::from(seen.next().unwrap());
    assert_eq!(seen.next(), Some("unset"), "no API key reaches the harness");
    assert!(
        !config_dir.exists(),
        "the session's login is removed after saving"
    );
}

#[tokio::test]
async fn a_change_that_runs_programs_is_never_saved_unseen() {
    let quiet = Quiet::with(profile_config());
    quiet
        .cluster
        .seed_profile_secret(&[("settings", r#"{"model":"opus"}"#)]);
    let harness =
        r#"printf '{"model":"opus","hooks":{"Stop":[]}}' > "$CLAUDE_CONFIG_DIR/settings.json""#;
    let error = edit_profile(edit_args(&quiet, harness))
        .await
        .expect_err("--yes does not cover hooks");
    assert!(error.message.contains("without --yes"), "{}", error.message);
    assert!(quiet.cluster.mutations().is_empty());
}

#[tokio::test]
async fn an_unchanged_profile_is_not_written() {
    let quiet = Quiet::with(profile_config());
    quiet
        .cluster
        .seed_profile_secret(&[("settings", r#"{"model":"opus"}"#)]);
    edit_profile(edit_args(&quiet, "true")).await.unwrap();
    assert!(quiet.cluster.mutations().is_empty());
}

#[tokio::test]
async fn the_last_profile_write_wins_over_an_external_change() {
    let quiet = Quiet::with(profile_config());
    quiet
        .cluster
        .seed_profile_secret(&[("settings", "{}"), ("auth", "old")]);
    let read = quiet.backend.read_claude_profile().await.unwrap();
    assert_eq!(read["auth"], b"old");
    // An operator replaces the Secret while the session is open.
    quiet
        .cluster
        .seed_profile_secret(&[("settings", "{}"), ("auth", "rotated")]);
    let updated =
        std::collections::BTreeMap::from([("settings".to_string(), b"{\"a\":1}".to_vec())]);
    quiet
        .backend
        .write_claude_profile(&updated)
        .await
        .expect("no compare-and-swap refuses the write");
    let data = quiet.cluster.profile_secret_data();
    assert_eq!(data["settings"], r#"{"a":1}"#);
    assert_eq!(
        data["auth"], "rotated",
        "keys the session did not change stay"
    );
}

#[tokio::test]
async fn a_profile_edit_needs_the_configured_secret() {
    let quiet = Quiet::new();
    let error = edit_profile(edit_args(&quiet, "true")).await.unwrap_err();
    assert!(
        error.message.contains("claude_profile_secret"),
        "{}",
        error.message
    );

    let quiet = Quiet::with(profile_config());
    let error = edit_profile(edit_args(&quiet, "exit 9")).await.unwrap_err();
    assert!(error.message.contains(PROFILE_SECRET), "{}", error.message);
    assert!(quiet.cluster.mutations().is_empty());
}

/// `restartPolicy: Never`, no service-account token, and the admission
/// check refuses privileged, host, foreign-volume, foreign-image,
/// foreign-account and unapproved-secret shapes (U5 scenario 2).
#[test]
fn the_pod_never_restarts_and_hostile_shapes_are_refused() {
    let (pod, _, _) = generated_pod(spec());
    assert_eq!(pod["spec"]["restartPolicy"], "Never");
    assert_eq!(pod["spec"]["automountServiceAccountToken"], false);
    assert_eq!(pod["spec"]["serviceAccountName"], "narsor-task");
    for host in ["hostNetwork", "hostPID", "hostIPC"] {
        assert_eq!(pod["spec"][host], false, "{host}");
    }
    let policy = config().shape_policy();
    check_pod_shape(&pod, &policy).expect("the generated Pod passes");

    let mutations: Vec<Mutation> = vec![
        (
            "privileged",
            Box::new(|p| p["spec"]["containers"][0]["securityContext"]["privileged"] = true.into()),
        ),
        (
            "escalation",
            Box::new(|p| {
                p["spec"]["containers"][0]["securityContext"]["allowPrivilegeEscalation"] =
                    true.into()
            }),
        ),
        (
            "capability",
            Box::new(|p| {
                p["spec"]["containers"][0]["securityContext"]["capabilities"]["add"] =
                    serde_json::json!(["SYS_ADMIN"])
            }),
        ),
        (
            "host network",
            Box::new(|p| p["spec"]["hostNetwork"] = true.into()),
        ),
        ("host pid", Box::new(|p| p["spec"]["hostPID"] = true.into())),
        (
            "host path",
            Box::new(|p| {
                p["spec"]["volumes"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({"name": "root", "hostPath": {"path": "/"}}))
            }),
        ),
        (
            "host port",
            Box::new(|p| {
                p["spec"]["containers"][0]["ports"] =
                    serde_json::json!([{"containerPort": 80, "hostPort": 80}])
            }),
        ),
        (
            "foreign image",
            Box::new(|p| {
                p["spec"]["containers"][0]["image"] =
                    format!("docker.io/evil/x@sha256:{}", "c".repeat(64)).into()
            }),
        ),
        (
            "tag image",
            Box::new(|p| {
                p["spec"]["containers"][0]["image"] = "registry.test/acme/dev:latest".into()
            }),
        ),
        (
            "foreign account",
            Box::new(|p| p["spec"]["serviceAccountName"] = "narsor-controller".into()),
        ),
        (
            "token",
            Box::new(|p| p["spec"]["automountServiceAccountToken"] = true.into()),
        ),
        (
            "restart",
            Box::new(|p| p["spec"]["restartPolicy"] = "Always".into()),
        ),
        (
            "root",
            Box::new(|p| p["spec"]["securityContext"]["runAsNonRoot"] = false.into()),
        ),
        (
            "foreign claim",
            Box::new(|p| {
                p["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"] =
                    "narsor-other-0123456789abcdef-storage".into()
            }),
        ),
        (
            "foreign secret volume",
            Box::new(|p| {
                p["spec"]["volumes"][2]["secret"]["secretName"] =
                    "narsor-controller-credential".into()
            }),
        ),
        (
            "unapproved projected secret",
            Box::new(|p| {
                p["spec"]["volumes"].as_array_mut().unwrap().push(serde_json::json!({"name": "x", "projected": {"sources": [{"secret": {"name": "kube-admin", "items": [{"key": "token", "path": "t"}]}}]}}))
            }),
        ),
        (
            "whole projected secret",
            Box::new(|p| {
                p["spec"]["volumes"].as_array_mut().unwrap().push(serde_json::json!({"name": "x", "projected": {"sources": [{"secret": {"name": "narsor-github"}}]}}))
            }),
        ),
        (
            "projected token",
            Box::new(|p| {
                p["spec"]["volumes"].as_array_mut().unwrap().push(serde_json::json!({"name": "x", "projected": {"sources": [{"serviceAccountToken": {"path": "t"}}]}}))
            }),
        ),
        (
            "ephemeral",
            Box::new(|p| {
                p["spec"]["ephemeralContainers"] =
                    serde_json::json!([{"name": "debug", "image": "busybox"}])
            }),
        ),
        (
            "env from secret",
            Box::new(|p| {
                p["spec"]["containers"][0]["env"].as_array_mut().unwrap().push(serde_json::json!({"name": "X", "valueFrom": {"secretKeyRef": {"name": "kube-admin", "key": "token"}}}))
            }),
        ),
        (
            "envFrom secret",
            Box::new(|p| {
                p["spec"]["containers"][0]["envFrom"] =
                    serde_json::json!([{"secretRef": {"name": "kube-admin"}}])
            }),
        ),
    ];
    for (what, mutate) in mutations {
        let mut hostile = pod.clone();
        mutate(&mut hostile);
        assert!(
            check_pod_shape(&hostile, &policy).is_err(),
            "{what} must be refused"
        );
    }
}

/// Tool secrets: one projected file per reference name, only approved
/// `secret/key` pairs, the Git clone credential included, no value in the
/// Pod spec, no secret as an environment variable (R19).
#[test]
fn tool_secrets_are_projected_files_never_values() {
    let mut environment = environment();
    environment.secrets = vec![github_secret()];
    let environment = environment.sealed();
    let mut spec = spec_with(environment.clone(), ResourceProfile::new(1000, 2048));
    spec = TaskLaunchSpec::new(
        spec.workflow_id.clone(),
        spec.workflow_version,
        spec.inputs.clone(),
        TaskWorkspace::Git(GitIdentity {
            repository: Some("acme".into()),
            remote_url: "https://git.example/acme.git".into(),
            commit: "a".repeat(40),
            git_ref: None,
            credential: Some(SecretReference {
                name: "git-credential".into(),
                tool: "git".into(),
                key: "narsor-git/credential".into(),
                delivery: SecretDelivery::File(".git-credential".into()),
                ..github_secret()
            }),
        }),
        environment,
        spec.resources,
    );
    let (pod, _, _) = generated_pod(spec);
    let text = pod.to_string();
    assert!(
        !text.contains("nsr.secret-runner-credential"),
        "the credential is not in the Pod"
    );
    let runner = &pod["spec"]["containers"][0];
    assert_eq!(
        runner["env"].as_array().unwrap().len(),
        1,
        "no GH_TOKEN env in the Pod"
    );
    let args = runner["args"].as_array().unwrap();
    assert!(args.iter().any(|a| a == "--secrets-dir"));
    let projected = pod["spec"]["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "tool-secrets")
        .expect("a projected secrets volume");
    let sources = projected["projected"]["sources"].as_array().unwrap();
    let files: BTreeSet<(String, String, String)> = sources
        .iter()
        .flat_map(|source| {
            let name = source["secret"]["name"].as_str().unwrap().to_string();
            source["secret"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |item| {
                    (
                        name.clone(),
                        item["key"].as_str().unwrap().to_string(),
                        item["path"].as_str().unwrap().to_string(),
                    )
                })
        })
        .collect();
    assert_eq!(
        files,
        BTreeSet::from([
            (
                "narsor-git".into(),
                "credential".into(),
                "git-credential".into()
            ),
            (
                "narsor-github".into(),
                "token".into(),
                "github-token".into()
            ),
        ])
    );
    check_pod_shape(&pod, &config().shape_policy()).expect("approved mappings pass");
}

/// Tool packages get their own init container and PATH entry.
#[test]
fn tool_bundles_are_materialized_by_init_containers_onto_path() {
    let mut environment = environment();
    environment.tools.insert(
        "gh".into(),
        image(
            "registry.test/narsor/tool-gh",
            'c',
            ImageSource::Tool { tool: "gh".into() },
        ),
    );
    let (pod, _, _) = generated_pod(spec_with(
        environment.sealed(),
        ResourceProfile::new(1000, 1024),
    ));
    let init: Vec<&str> = pod["spec"]["initContainers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(init, ["runtime-bundle", "tool-gh"]);
    let path = pod["spec"]["containers"][0]["env"][0]["value"]
        .as_str()
        .unwrap();
    assert!(
        path.starts_with("/opt/narsor/bin:/opt/narsor-tools/gh/bin:"),
        "{path}"
    );
}

/// Missing digest, foreign registry, missing base image (AE11), missing
/// bundle, per-step images, invalid resources and unapproved secrets are
/// refused before any API call (U5 scenario "rejected before creation").
#[tokio::test]
async fn invalid_launches_are_rejected_before_any_api_call() {
    let quiet = Quiet::new();
    let task = task_resources(&quiet.backend, "task-1");

    let unpinned = {
        let mut environment = environment();
        let base = environment.base_image.as_mut().unwrap();
        base.pinned = "registry.test/acme/dev:1".into();
        environment.sealed()
    };
    let foreign = {
        let mut environment = environment();
        environment.base_image = Some(image(
            "docker.io/library/ubuntu",
            'a',
            ImageSource::Repository {
                repository: "x".into(),
            },
        ));
        environment.sealed()
    };
    let no_base = {
        let mut environment = environment();
        environment.base_image = None;
        environment.sealed()
    };
    let no_bundle = {
        let mut environment = environment();
        environment.runtime_bundle = None;
        environment.sealed()
    };
    let step_images = {
        let mut environment = environment();
        environment.step_images.insert(
            "work".into(),
            image(
                "registry.test/acme/other",
                'd',
                ImageSource::Step {
                    workflow: "w".into(),
                    state: "work".into(),
                },
            ),
        );
        environment.sealed()
    };
    let unapproved = {
        let mut environment = environment();
        let mut secret = github_secret();
        secret.key = "kube-admin/token".into();
        environment.secrets = vec![secret];
        environment.sealed()
    };
    // Edited after sealing: the identity no longer describes the content.
    let mut tampered = spec_with(environment(), ResourceProfile::new(1000, 1024));
    tampered.environment.harness = "other".into();
    let cases = [
        (
            "unpinned base image",
            spec_with(unpinned, ResourceProfile::new(1000, 1024)),
            "pinned",
        ),
        (
            "foreign registry",
            spec_with(foreign, ResourceProfile::new(1000, 1024)),
            "allowed registry",
        ),
        (
            "no base image (AE11)",
            spec_with(no_base, ResourceProfile::new(1000, 1024)),
            "no base image",
        ),
        (
            "no runtime bundle",
            spec_with(no_bundle, ResourceProfile::new(1000, 1024)),
            "runtime bundle",
        ),
        (
            "per-step images",
            spec_with(step_images, ResourceProfile::new(1000, 1024)),
            "per-step images",
        ),
        (
            "invalid resources",
            spec_with(environment(), ResourceProfile::new(0, 1024)),
            "resource profile",
        ),
        (
            "unapproved secret",
            spec_with(unapproved, ResourceProfile::new(1000, 1024)),
            "does not approve",
        ),
        ("tampered environment", tampered, "identity"),
    ];
    for (what, spec, needle) in cases {
        for resume in [false, true] {
            let launch = launch_of("task-1", 1, spec.clone());
            let result = if resume {
                quiet.backend.resume(&task, &launch).await
            } else {
                quiet.backend.provision(&task, &launch).await
            };
            let error = result.expect_err(what);
            assert_eq!(error.kind, BackendErrorKind::Rejected, "{what}: {error}");
            assert!(error.message.contains(needle), "{what}: {}", error.message);
        }
    }
    assert!(
        quiet.cluster.requests().is_empty(),
        "{:?}",
        quiet.cluster.requests()
    );
}

/// The resource names are DNS-1123 labels and stay apart for task ids that
/// differ only in case or punctuation.
#[test]
fn resource_names_are_dns_safe_and_injective() {
    let dir = scratch();
    let backend = KubernetesBackend::new(
        &narsor_controller::kubernetes::client::ClusterAccess::explicit(
            "http://127.0.0.1:1",
            NAMESPACE,
        ),
        config(),
    )
    .unwrap();
    let mut seen = BTreeSet::new();
    for key in [
        "Task_1",
        "task-1",
        "task.1",
        "TASK.1",
        "t",
        &"Z".repeat(128),
    ] {
        let task = task_resources(&backend, key);
        assert!(names::is_dns_label(&task.workload), "{}", task.workload);
        assert!(names::is_dns_label(&task.storage), "{}", task.storage);
        assert_eq!(task.storage, format!("{}-storage", task.workload));
        assert!(seen.insert(task.workload.clone()), "{key} collides");
        assert_eq!(task_resources(&backend, key), task, "deterministic");
    }
    assert!(backend
        .resources(&TaskId::parse("x").unwrap(), "../evil")
        .is_err());
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// The adapter against the mock API
// ---------------------------------------------------------------------------

/// Provision creates the PVC, the Pod and its launch Secret (owned by the
/// Pod); repeating it returns the existing Pod instead of a second one.
#[tokio::test]
async fn provision_creates_once_and_reconciles_by_name() {
    let quiet = Quiet::new();
    let task = task_resources(&quiet.backend, "task-1");
    let launch = launch_of("task-1", 1, spec());

    let first = quiet
        .backend
        .provision(&task, &launch)
        .await
        .expect("provision");
    assert!(first.created_workload && first.created_storage);
    assert_eq!(first.workload.phase, WorkloadPhase::Starting);
    assert_eq!(first.workload.incarnation, launch.incarnation);
    assert_eq!(first.workload.launch_generation, 1);

    let again = launch_of("task-1", 1, spec());
    let second = quiet
        .backend
        .provision(&task, &again)
        .await
        .expect("reconcile");
    assert!(!second.created_workload && !second.created_storage);
    assert_eq!(second.workload.uid, first.workload.uid);
    assert_eq!(
        second.workload.incarnation, launch.incarnation,
        "the existing Pod, unchanged"
    );

    assert_eq!(
        quiet.cluster.pod_names(),
        std::slice::from_ref(&task.workload)
    );
    let secrets = quiet.cluster.created_of("Secret");
    assert_eq!(
        secrets.len(),
        1,
        "no launch Secret for a Pod that was not created"
    );
    let secret = &secrets[0];
    assert_eq!(secret["immutable"], true);
    assert_eq!(secret["metadata"]["ownerReferences"][0]["kind"], "Pod");
    assert_eq!(
        secret["metadata"]["ownerReferences"][0]["uid"],
        first.workload.uid.as_str()
    );
    assert_eq!(
        secret["stringData"]["credential"],
        "nsr.secret-runner-credential"
    );
    let stored: TaskLaunchSpec =
        serde_json::from_str(secret["stringData"]["launch-spec.json"].as_str().unwrap()).unwrap();
    assert_eq!(serde_json::to_value(stored).unwrap(), launch.launch_spec);

    let pvc = quiet.cluster.pvc(&task.storage).expect("the PVC");
    assert!(
        pvc["metadata"]["ownerReferences"].is_null(),
        "the PVC outlives every Pod"
    );
    assert_eq!(pvc["spec"]["storageClassName"], DELETE_CLASS);
    assert_eq!(
        pvc["spec"]["accessModes"],
        serde_json::json!(["ReadWriteOnce"])
    );
    // Never a read of a Secret: the controller holds no such permission.
    assert!(quiet
        .cluster
        .requests()
        .iter()
        .all(|r| !(r.path.contains("/secrets") && r.method != "POST")));
}

/// Pause preserves the PVC while removing the Pod; the Pod's launch Secret
/// goes with it; resume creates one Pod on the same PVC (U5 scenario 4).
#[tokio::test]
async fn release_keeps_the_storage_and_resume_reuses_it() {
    let quiet = Quiet::new();
    let task = task_resources(&quiet.backend, "task-1");
    let first = quiet
        .backend
        .provision(&task, &launch_of("task-1", 1, spec()))
        .await
        .unwrap();
    let pvc_uid = quiet.cluster.pvc(&task.storage).unwrap()["metadata"]["uid"].clone();

    let removal = quiet.backend.release_compute(&task).await.unwrap();
    assert!(
        matches!(removal, Removal::Initiated | Removal::Removed),
        "{removal:?}"
    );
    let observed = quiet.backend.observe(&task).await.unwrap();
    if let Some(workload) = &observed.workload {
        assert_eq!(workload.phase, WorkloadPhase::Deleting);
        assert!(workload.resource_generation > first.workload.resource_generation);
        assert_eq!(
            quiet.backend.release_compute(&task).await.unwrap(),
            Removal::InProgress
        );
    }
    let backend = quiet.backend.clone();
    let probe = task.clone();
    wait_until("the Pod to go", async || {
        backend.observe(&probe).await.unwrap().workload.is_none()
    })
    .await;
    assert_eq!(
        quiet.backend.release_compute(&task).await.unwrap(),
        Removal::AlreadyAbsent
    );
    assert!(
        quiet.cluster.secret_names().is_empty(),
        "the launch Secret went with its Pod"
    );
    assert_eq!(
        quiet.backend.observe(&task).await.unwrap().storage,
        StorageState::Present
    );

    let resumed = quiet
        .backend
        .resume(&task, &launch_of("task-1", 2, spec()))
        .await
        .unwrap();
    assert!(resumed.created_workload && !resumed.created_storage);
    assert_ne!(resumed.workload.uid, first.workload.uid);
    assert_eq!(
        quiet.cluster.pvc(&task.storage).unwrap()["metadata"]["uid"],
        pvc_uid,
        "the same PVC"
    );
    let pods = quiet.cluster.created_of("Pod");
    assert_eq!(pods.len(), 2);
    assert!(pods.iter().all(
        |pod| pod["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"]
            == task.storage.as_str()
    ));
    assert_eq!(quiet.cluster.created_of("PersistentVolumeClaim").len(), 1);
}

/// A relaunch never recreates storage: a missing PVC is `StorageMissing`,
/// a PVC being deleted (an archive under way) is waited out — so a
/// tombstoned task is never brought back (U5 race scenario).
#[tokio::test]
async fn relaunch_never_recreates_or_races_deleted_storage() {
    let quiet = Quiet::new();
    let task = task_resources(&quiet.backend, "task-1");
    let error = quiet
        .backend
        .resume(&task, &launch_of("task-1", 2, spec()))
        .await
        .expect_err("no storage");
    assert_eq!(error.kind, BackendErrorKind::StorageMissing);
    assert!(quiet.cluster.mutations().is_empty());

    quiet
        .backend
        .provision(&task, &launch_of("task-1", 1, spec()))
        .await
        .unwrap();
    quiet.cluster.hold_pvc_deletion(true);
    let archive = quiet.backend.archive(&task).await.unwrap();
    assert!(!archive.is_complete());
    let backend = quiet.backend.clone();
    let probe = task.clone();
    wait_until("the PVC deletion to start", async || {
        backend.archive(&probe).await.unwrap();
        backend.observe(&probe).await.unwrap().storage == StorageState::Deleting
    })
    .await;
    assert_eq!(
        quiet.backend.observe(&task).await.unwrap().storage,
        StorageState::Deleting
    );

    let pods_before = quiet.cluster.created_of("Pod").len();
    for resume in [false, true] {
        let launch = launch_of("task-1", 3, spec());
        let error = if resume {
            quiet.backend.resume(&task, &launch).await
        } else {
            quiet.backend.provision(&task, &launch).await
        }
        .expect_err("storage being deleted");
        assert_eq!(error.kind, BackendErrorKind::Transient, "{error}");
    }
    assert_eq!(
        quiet.cluster.created_of("Pod").len(),
        pods_before,
        "no Pod over a dying PVC"
    );

    // Archive reports success only after both deletions are observed.
    assert!(!quiet.backend.archive(&task).await.unwrap().is_complete());
    quiet.cluster.hold_pvc_deletion(false);
    let backend = quiet.backend.clone();
    let probe = task.clone();
    wait_until("the archive to complete", async || {
        backend.archive(&probe).await.unwrap().is_complete()
    })
    .await;
    assert!(quiet.cluster.pod(&task.workload).is_none());
    assert!(quiet.cluster.pvc(&task.storage).is_none());
    let progress = quiet.backend.archive(&task).await.unwrap();
    assert_eq!(progress.workload, Removal::AlreadyAbsent);
    assert_eq!(progress.storage, Removal::AlreadyAbsent);
}

/// A storage class that keeps data after its claim is deleted is refused
/// up front for new storage, and an archive over it fails visibly instead
/// of claiming a destruction it did not do (KTD2).
#[tokio::test]
async fn a_retaining_storage_class_is_refused() {
    let retaining = Quiet::with(KubernetesBackendConfig {
        storage_class: RETAIN_CLASS.into(),
        ..config()
    });
    let error = retaining
        .backend
        .preflight()
        .await
        .expect_err("Retain refused");
    assert_eq!(error.kind, BackendErrorKind::Rejected);
    let task = task_resources(&retaining.backend, "task-1");
    let error = retaining
        .backend
        .provision(&task, &launch_of("task-1", 1, spec()))
        .await
        .expect_err("Retain refused");
    assert_eq!(error.kind, BackendErrorKind::Rejected);
    assert!(error.message.contains("Retain"), "{error}");
    assert!(retaining.cluster.mutations().is_empty(), "nothing created");

    // A class switched to Retain after the claim was made.
    let quiet = Quiet::new();
    quiet.backend.preflight().await.expect("Delete is fine");
    let task = task_resources(&quiet.backend, "task-1");
    quiet
        .backend
        .provision(&task, &launch_of("task-1", 1, spec()))
        .await
        .unwrap();
    quiet.cluster.set_reclaim_policy(DELETE_CLASS, "Retain");
    let backend = quiet.backend.clone();
    let probe = task.clone();
    wait_until("the Pod to go", async || {
        match backend.archive(&probe).await {
            Ok(progress) => {
                assert!(!progress.is_complete());
                false
            }
            Err(error) => {
                assert_eq!(error.kind, BackendErrorKind::Rejected);
                true
            }
        }
    })
    .await;
    assert!(
        quiet.cluster.pvc(&task.storage).is_some(),
        "the claim is left for the operator"
    );
    assert!(quiet.cluster.pvc(&task.storage).unwrap()["metadata"]["deletionTimestamp"].is_null());
}

/// Observation maps Pod phases, exit codes and deletion onto the neutral
/// phases with a monotonic resource generation, and refuses an object of
/// that name that is not the task's.
#[tokio::test]
async fn observation_is_normalized_and_foreign_objects_are_refused() {
    let quiet = Quiet::new();
    let task = task_resources(&quiet.backend, "task-1");
    assert!(quiet
        .backend
        .observe(&task)
        .await
        .unwrap()
        .workload
        .is_none());
    let provisioned = quiet
        .backend
        .provision(&task, &launch_of("task-1", 4, spec()))
        .await
        .unwrap();
    let starting = provisioned.workload;
    assert_eq!(starting.phase, WorkloadPhase::Starting);

    // The kubelet is off here: the test plays it.
    quiet.cluster.fail_requests(1);
    let error = quiet.backend.observe(&task).await.expect_err("API down");
    assert_eq!(error.kind, BackendErrorKind::Transient);

    let observed = quiet.backend.observe(&task).await.unwrap();
    assert_eq!(observed.workload.as_ref().unwrap().uid, starting.uid);
    assert!(quiet.cluster.evict(&task.workload));
    let deleting = quiet.backend.observe(&task).await.unwrap().workload;
    if let Some(deleting) = deleting {
        assert_eq!(deleting.phase, WorkloadPhase::Deleting);
        assert!(deleting.resource_generation > starting.resource_generation);
    }

    // An object under the task's name that someone else made.
    let other = task_resources(&quiet.backend, "task-2");
    let mut foreign = task.clone();
    foreign.workload = other.workload.clone();
    foreign.storage = other.storage.clone();
    quiet
        .backend
        .provision(&other, &launch_of("task-2", 1, spec()))
        .await
        .unwrap();
    let error = quiet.backend.observe(&foreign).await.expect_err("not ours");
    assert_eq!(error.kind, BackendErrorKind::Rejected);
    assert!(
        quiet.backend.release_compute(&foreign).await.is_err(),
        "never deleted"
    );
    assert!(quiet.cluster.pod(&other.workload).is_some());
}

// ---------------------------------------------------------------------------
// End to end through the controller
// ---------------------------------------------------------------------------

/// AE1 + AE5: two tasks of one repository run side by side on the same
/// pinned base image and runtime bundle, without building any image, each
/// with its own PVC, limits and private HOME; pausing one leaves the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_tasks_share_pinned_images_but_nothing_writable() {
    let Some(mut h) = Harness::<KubernetesTarget>::new().await else {
        return;
    };
    h.start_controller();
    h.create("task-a");
    h.create("task-b");
    h.wait_lifecycle("task-a", TaskLifecycle::Running).await;
    h.wait_lifecycle("task-b", TaskLifecycle::Running).await;
    let cluster = Arc::clone(&h.target.cluster);
    let (a, b) = (h.resources("task-a"), h.resources("task-b"));

    let pods = cluster.created_of("Pod");
    assert_eq!(pods.len(), 2);
    let images = |pod: &Value| -> Vec<String> {
        ["initContainers", "containers"]
            .iter()
            .flat_map(|key| pod["spec"][key].as_array().unwrap().clone())
            .map(|c| c["image"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        images(&pods[0]),
        images(&pods[1]),
        "the same pinned images (AE5)"
    );
    let kinds: BTreeSet<String> = cluster
        .created()
        .iter()
        .map(|o| o["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from([
            "Pod".into(),
            "PersistentVolumeClaim".into(),
            "Secret".into()
        ]),
        "no image build, no other object"
    );
    for pod in &pods {
        assert_eq!(
            pod["spec"]["containers"][0]["resources"]["limits"]["memory"],
            "3072Mi"
        );
    }
    let claims: BTreeSet<&str> = pods
        .iter()
        .map(|p| {
            p["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        claims,
        BTreeSet::from([a.storage.as_str(), b.storage.as_str()])
    );

    // Each runner made its private HOME on its own storage (R17).
    h.wait_for("both HOMEs", |_| {
        [&a, &b].iter().all(|task| {
            cluster
                .volume_dir(&task.storage)
                .is_some_and(|dir| dir.join("home").is_dir())
        })
    })
    .await;
    assert_ne!(
        cluster.volume_dir(&a.storage),
        cluster.volume_dir(&b.storage)
    );

    h.must("task-a", OwnerCommand::Pause, "pause-a");
    h.wait_lifecycle("task-a", TaskLifecycle::Paused).await;
    assert!(cluster.pod(&a.workload).is_none());
    assert!(
        cluster.pvc(&a.storage).is_some(),
        "storage kept through the pause"
    );
    assert!(cluster.pod(&b.workload).is_some());
    assert_eq!(h.lifecycle("task-b"), TaskLifecycle::Running);

    // Resume: one new Pod on the same claim, the same HOME.
    let home = cluster.volume_dir(&a.storage).unwrap().join("home");
    h.must("task-a", OwnerCommand::Resume, "resume-a");
    h.wait_lifecycle("task-a", TaskLifecycle::Running).await;
    assert_eq!(cluster.volume_dir(&a.storage).unwrap().join("home"), home);
    assert_eq!(cluster.created_of("PersistentVolumeClaim").len(), 2);
    assert_eq!(cluster.created_of("Pod").len(), 3);
}

/// AE4: a Pod deleted behind the controller's back (an eviction) makes the
/// task Interrupted; the resume policy replaces it without the owner, and
/// the resumed runner continues on the same storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_evicted_pod_is_replaced_automatically() {
    let Some(mut h) = Harness::<KubernetesTarget>::new().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    let cluster = Arc::clone(&h.target.cluster);
    let task = h.resources("task-1");
    assert!(cluster.evict(&task.workload));
    h.wait_for("the replacement Pod", |h| {
        cluster.created_of("Pod").len() == 2 && h.lifecycle("task-1") == TaskLifecycle::Running
    })
    .await;
    assert_eq!(cluster.created_of("PersistentVolumeClaim").len(), 1);
    assert!(h
        .reported("task-1")
        .contains(&Observation::WorkloadLost { state_known: true }));
    assert_eq!(
        backend_contract_support::auto_resumes(&h.events("task-1")),
        vec![(1, 5)]
    );
}

/// Without the resume policy an evicted Pod waits for the owner (R13).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_evicted_pod_waits_for_the_owner_without_the_policy() {
    let Some(mut h) = Harness::<KubernetesTarget>::without_auto_resume().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    let cluster = Arc::clone(&h.target.cluster);
    let task = h.resources("task-1");
    assert!(cluster.evict(&task.workload));
    h.wait_lifecycle("task-1", TaskLifecycle::Interrupted).await;
    h.settle().await;
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Interrupted);
    assert!(
        cluster.pod(&task.workload).is_none(),
        "no replacement without the policy"
    );
    assert_eq!(cluster.created_of("Pod").len(), 1);

    h.must("task-1", OwnerCommand::Resume, "resume-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Running).await;
    assert_eq!(cluster.created_of("Pod").len(), 2);
    assert_eq!(cluster.created_of("PersistentVolumeClaim").len(), 1);
}

/// A workflow whose one step waits for the owner's approval of its output:
/// a runner of it stands at that waiting point until answered.
const GATED_WORKFLOW: &str = r#"
name: gated-task
version: 1
states:
  work:
    action:
      type: script
      command: sh
      args: [-c, "echo worked >> \"$HOME/steps\"; printf '{\"output\": \"ok\"}'"]
    outputs: [ok, rejected]
    transitions:
      ok: completed
      rejected: failed
    decision:
      type: human
      message: Ship it?
  completed:
    description: Done.
  failed:
    description: Rejected.
terminal_states:
  completed: completed
  failed: failed
"#;

fn gated_spec() -> TaskLaunchSpec {
    let definition =
        narsor_engine::workflow::load_from_str(GATED_WORKFLOW).expect("a valid workflow");
    TaskLaunchSpec::new(
        definition.id.clone(),
        definition.version,
        serde_json::json!({}),
        TaskWorkspace::None,
        environment(),
        ResourceProfile::new(1500, 3072),
    )
    .with_definition(&definition)
    .expect("snapshot the definition")
}

/// The id of the input request the task's runner waits on, if it waits.
fn waiting_on(h: &Harness<KubernetesTarget>, task: &str) -> Option<String> {
    h.record(task)
        .runner
        .and_then(|runner| runner.snapshot.pending_input)
        .map(|input| input.id)
}

/// A Pod whose node stops reporting (phase `Unknown`) makes the task
/// Unknown and keeps it there — it may still run behind the partition.
/// Once the node reports the runner failed, the loss is confirmed, the
/// resume policy starts one new Pod on the same claim, and its runner
/// returns to the very waiting point the lost one stood at without running
/// the finished step again (R1, R3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partitioned_pod_waits_until_its_failure_is_reported() {
    let Some(mut h) = Harness::<KubernetesTarget>::new().await else {
        return;
    };
    h.start_controller();
    h.create_with("task-1", gated_spec());
    h.wait_for("the runner at its approval", |h| {
        h.lifecycle("task-1") == TaskLifecycle::Running && waiting_on(h, "task-1").is_some()
    })
    .await;
    let waiting = waiting_on(&h, "task-1").unwrap();
    let first = h.record("task-1").runner.unwrap().incarnation;
    let cluster = Arc::clone(&h.target.cluster);
    let task = h.resources("task-1");

    assert!(cluster.partition(&task.workload));
    h.wait_lifecycle("task-1", TaskLifecycle::Unknown).await;
    h.observed_again(3).await;
    assert_eq!(
        h.lifecycle("task-1"),
        TaskLifecycle::Unknown,
        "a Pod in phase Unknown is no confirmed loss"
    );
    assert_eq!(cluster.created_of("Pod").len(), 1);
    assert!(!h.reported("task-1").contains(&Observation::LossConfirmed));

    assert!(cluster.kill_runner(&task.workload));
    h.wait_for("the resumed runner at the same approval", |h| {
        let record = h.record("task-1");
        record.lifecycle == TaskLifecycle::Running
            && record
                .runner
                .as_ref()
                .is_some_and(|runner| runner.incarnation != first)
            && waiting_on(h, "task-1").is_some()
    })
    .await;
    assert_eq!(
        waiting_on(&h, "task-1"),
        Some(waiting),
        "the same waiting point"
    );
    assert!(h.reported("task-1").contains(&Observation::LossConfirmed));
    assert_eq!(
        backend_contract_support::auto_resumes(&h.events("task-1")),
        vec![(1, 5)]
    );
    assert_eq!(cluster.created_of("Pod").len(), 2);
    assert_eq!(cluster.created_of("PersistentVolumeClaim").len(), 1);
    let steps = cluster
        .volume_dir(&task.storage)
        .unwrap()
        .join("home/steps");
    assert_eq!(
        std::fs::read_to_string(steps).unwrap(),
        "worked\n",
        "the finished step ran once"
    );
}

/// An Unknown task whose Pod is then deleted is a confirmed loss: without
/// the resume policy it waits Interrupted for the owner (R3, R13).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_pod_confirms_the_loss_of_an_unknown_task() {
    let Some(mut h) = Harness::<KubernetesTarget>::without_auto_resume().await else {
        return;
    };
    h.start_controller();
    h.running("task-1").await;
    let cluster = Arc::clone(&h.target.cluster);
    let task = h.resources("task-1");

    assert!(cluster.partition(&task.workload));
    h.wait_lifecycle("task-1", TaskLifecycle::Unknown).await;
    assert!(cluster.evict(&task.workload));
    h.wait_lifecycle("task-1", TaskLifecycle::Interrupted).await;
    assert!(h.reported("task-1").contains(&Observation::LossConfirmed));
    h.settle().await;
    assert_eq!(h.lifecycle("task-1"), TaskLifecycle::Interrupted);
    assert_eq!(cluster.created_of("Pod").len(), 1, "nothing replaced it");
    assert!(cluster.pvc(&task.storage).is_some());
}

/// AE8 + AE9 specifics on Kubernetes: a severed controller leaves the Pod
/// running; a finished task keeps its PVC until archive, which then
/// removes Pod, PVC and (by garbage collection) every launch Secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_leaves_nothing_of_the_task_behind() {
    let Some(mut h) = Harness::<KubernetesTarget>::new().await else {
        return;
    };
    let index = h.start_controller();
    h.running("task-1").await;
    let cluster = Arc::clone(&h.target.cluster);
    let task = h.resources("task-1");
    h.sever(index);
    h.settle().await;
    assert!(
        cluster.pod(&task.workload).is_some(),
        "the Pod runs on (AE8)"
    );
    h.restore(index);
    h.wait_for("the workflow to finish", |h| {
        h.record("task-1").runner.is_some_and(|runner| {
            runner.snapshot.execution_status == narsor_engine::domain::ExecutionStatus::Completed
        })
    })
    .await;
    h.settle().await;
    assert!(
        cluster.pvc(&task.storage).is_some(),
        "kept until archive (AE9)"
    );

    h.must("task-1", OwnerCommand::Archive, "archive-1");
    h.wait_lifecycle("task-1", TaskLifecycle::Archived).await;
    assert!(cluster.pod_names().is_empty());
    assert!(cluster.pvc(&task.storage).is_none());
    assert!(cluster.secret_names().is_empty());
    assert_eq!(cluster.created_of("Pod").len(), 1, "exactly one Pod ever");
}

/// `narsor-controller` enrolls once with a one-time code, persists the
/// credential `0600`, and loads it on every later start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_controller_command_enrolls_once_and_then_loads_its_credential() {
    use narsor_engine::controller_protocol::CreateEnrollmentRequest;
    use narsor_engine::controller_runtime::load_or_enroll;
    use std::os::unix::fs::PermissionsExt;

    let Some(h) = Harness::<KubernetesTarget>::new().await else {
        return;
    };
    let dir = scratch();
    let file = dir.join("state/controller.credential");
    assert!(
        load_or_enroll(&h.url, &file, None).await.is_err(),
        "no file, no code"
    );

    let code = h
        .service
        .create_enrollment(
            backend_contract_support::BACKEND,
            CreateEnrollmentRequest::default(),
        )
        .unwrap()
        .enrollment_code;
    let client = load_or_enroll(&h.url, &file, Some(&code))
        .await
        .expect("enroll");
    client.heartbeat().await.expect("the new identity works");
    let stored = std::fs::read_to_string(&file).unwrap();
    assert!(stored.starts_with("nsk."));
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // A restart with the spent code still around: the file wins.
    let again = load_or_enroll(&h.url, &file, Some(&code))
        .await
        .expect("load");
    again.heartbeat().await.expect("the same identity");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), stored);
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Deployment manifests
// ---------------------------------------------------------------------------

fn deploy_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/kubernetes")
}

/// Every YAML document under `deploy/kubernetes/`, as JSON.
fn deploy_documents() -> Vec<Value> {
    let mut documents = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(deploy_dir())
        .expect("deploy/kubernetes")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "yaml"))
        .collect();
    files.sort();
    assert!(!files.is_empty());
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for document in serde_yaml::Deserializer::from_str(&text) {
            let value: serde_yaml::Value = serde::Deserialize::deserialize(document)
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            if value.is_null() {
                continue;
            }
            documents.push(serde_json::to_value(value).unwrap());
        }
    }
    documents
}

fn of_kind<'a>(documents: &'a [Value], kind: &str) -> Vec<&'a Value> {
    documents.iter().filter(|d| d["kind"] == kind).collect()
}

/// The controller's RBAC stays inside its user's namespace, grants only what
/// the adapter uses, and never exec, attach, port-forward or ephemeral
/// containers; the task service account has no permissions at all.
#[test]
fn controller_rbac_is_namespaced_and_minimal() {
    let documents = deploy_documents();
    let roles = of_kind(&documents, "Role");
    let role = roles
        .iter()
        .find(|r| r["metadata"]["name"] == "narsor-controller")
        .expect("the controller Role");
    assert_eq!(role["metadata"]["namespace"], NAMESPACE);
    let mut granted: BTreeSet<(String, String)> = BTreeSet::new();
    for rule in role["rules"].as_array().unwrap() {
        for resource in rule["resources"].as_array().unwrap() {
            for verb in rule["verbs"].as_array().unwrap() {
                granted.insert((
                    resource.as_str().unwrap().to_string(),
                    verb.as_str().unwrap().to_string(),
                ));
            }
        }
        assert!(rule["resourceNames"].is_null() || rule["resourceNames"].is_array());
    }
    for forbidden in [
        "pods/exec",
        "pods/attach",
        "pods/portforward",
        "pods/ephemeralcontainers",
        "*",
    ] {
        assert!(
            !granted.iter().any(|(r, _)| r == forbidden),
            "{forbidden} granted"
        );
    }
    assert!(!granted.iter().any(|(_, v)| v == "*"
        || v == "deletecollection"
        || v == "escalate"
        || v == "bind"
        || v == "impersonate"));
    let secrets: BTreeSet<&str> = granted
        .iter()
        .filter(|(r, _)| r == "secrets")
        .map(|(_, v)| v.as_str())
        .collect();
    assert!(
        secrets.contains("create"),
        "launch Secret creation remains available"
    );
    let profile_rule = role["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| {
            rule["resources"] == serde_json::json!(["secrets"])
                && rule["resourceNames"] == serde_json::json!(["narsor-claude-profile"])
        })
        .expect("profile Secret access is name-scoped");
    let profile_verbs: BTreeSet<&str> = profile_rule["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(profile_verbs, BTreeSet::from(["get", "update"]));
    for (resource, verb) in [
        ("pods", "get"),
        ("pods", "create"),
        ("pods", "delete"),
        ("persistentvolumeclaims", "get"),
        ("persistentvolumeclaims", "create"),
        ("persistentvolumeclaims", "delete"),
        ("secrets", "create"),
    ] {
        assert!(
            granted.contains(&(resource.into(), verb.into())),
            "{resource} {verb}"
        );
    }

    // The only cluster-wide grant: reading storage classes.
    for cluster_role in of_kind(&documents, "ClusterRole") {
        for rule in cluster_role["rules"].as_array().unwrap() {
            assert_eq!(rule["resources"], serde_json::json!(["storageclasses"]));
            assert_eq!(rule["verbs"], serde_json::json!(["get"]));
        }
    }
    for binding in of_kind(&documents, "RoleBinding") {
        assert_eq!(binding["metadata"]["namespace"], NAMESPACE);
        assert_eq!(
            binding["roleRef"]["kind"], "Role",
            "never a ClusterRole in the namespace"
        );
    }

    // The task service account: no token, bound to nothing.
    let accounts = of_kind(&documents, "ServiceAccount");
    let task = accounts
        .iter()
        .find(|a| a["metadata"]["name"] == "narsor-task")
        .expect("the workload service account");
    assert_eq!(task["automountServiceAccountToken"], false);
    let bound: Vec<&Value> = of_kind(&documents, "RoleBinding")
        .into_iter()
        .chain(of_kind(&documents, "ClusterRoleBinding"))
        .filter(|b| {
            b["subjects"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["name"] == "narsor-task"))
        })
        .collect();
    assert!(bound.is_empty(), "the task account has zero RBAC");
}

/// The admission policy rejects every Pod shape but the generated one:
/// foreign image or registry, privilege and host fields, a foreign service
/// account, unapproved volumes, claims and secret mappings. Its bindings
/// cover the dedicated user namespace, but each policy matches only requests
/// authenticated as the Narsor controller, so a compromised controller cannot
/// evade the policy by dropping its labels.
#[test]
fn the_admission_policy_covers_the_pod_contract() {
    let documents = deploy_documents();
    let policies = of_kind(&documents, "ValidatingAdmissionPolicy");
    let pod_policy = policies
        .iter()
        .find(|p| p["metadata"]["name"] == "narsor-task-pods")
        .expect("the Pod policy");
    assert_eq!(pod_policy["spec"]["failurePolicy"], "Fail");
    let rules = pod_policy["spec"]["matchConstraints"]["resourceRules"]
        .as_array()
        .unwrap();
    assert!(rules.iter().any(|r| r["resources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x == "pods")
        && r["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "pods/ephemeralcontainers")
        && r["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "CREATE")
        && r["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "UPDATE")));
    let expressions: String = ["variables", "validations"]
        .iter()
        .flat_map(|key| pod_policy["spec"][key].as_array().unwrap().iter())
        .map(|v| v["expression"].as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "serviceAccountName",
        "automountServiceAccountToken",
        "hostNetwork",
        "hostPID",
        "hostIPC",
        "restartPolicy",
        "ephemeralContainers",
        "@sha256:",
        "allowedRegistries",
        "privileged",
        "allowPrivilegeEscalation",
        "capabilities",
        "hostPort",
        "runAsNonRoot",
        "hostPath",
        "persistentVolumeClaim",
        "-storage",
        "-launch-",
        "allowedSecrets",
        "valueFrom",
    ] {
        assert!(expressions.contains(needle), "no rule mentions {needle}");
    }
    assert!(pod_policy["spec"]["paramKind"]["kind"] == "ConfigMap");

    let pvc_policy = policies
        .iter()
        .find(|p| p["metadata"]["name"] == "narsor-task-storage")
        .expect("the PVC policy");
    assert!(pvc_policy["spec"]["validations"]
        .to_string()
        .contains("storageClassName"));

    let bindings = of_kind(&documents, "ValidatingAdmissionPolicyBinding");
    for policy in ["narsor-task-pods", "narsor-task-storage"] {
        let binding = bindings
            .iter()
            .find(|b| b["spec"]["policyName"] == policy)
            .unwrap_or_else(|| panic!("a binding of {policy}"));
        assert_eq!(
            binding["spec"]["validationActions"],
            serde_json::json!(["Deny"])
        );
        assert_eq!(
            binding["spec"]["paramRef"]["parameterNotFoundAction"],
            "Deny"
        );
        assert_eq!(binding["spec"]["paramRef"]["namespace"], NAMESPACE);
        assert_eq!(
            binding["spec"]["matchResources"]["namespaceSelector"]["matchLabels"]
                ["kubernetes.io/metadata.name"],
            NAMESPACE
        );
        assert!(binding["spec"]["matchResources"]
            .get("objectSelector")
            .is_none());

        let policy = policies
            .iter()
            .find(|p| p["metadata"]["name"] == policy)
            .unwrap_or_else(|| panic!("the {policy} policy"));
        let conditions = policy["spec"]["matchConditions"]
            .as_array()
            .expect("controller identity match condition");
        assert!(conditions.iter().any(|condition| {
            condition["expression"].as_str().is_some_and(|expression| {
                expression.contains("request.userInfo.username")
                    && expression.contains("request.namespace")
                    && expression.contains("narsor-controller")
            })
        }));
    }

    let delete_guard = policies
        .iter()
        .find(|p| p["metadata"]["name"] == "narsor-controller-delete-guard")
        .expect("the controller delete guard");
    assert!(delete_guard["spec"]["matchConstraints"]["resourceRules"]
        .to_string()
        .contains("DELETE"));
    let delete_expression = delete_guard["spec"]["validations"].to_string();
    for needle in ["oldObject", "workload", "storage"] {
        assert!(
            delete_expression.contains(needle),
            "delete guard misses {needle}"
        );
    }
    let delete_binding = bindings
        .iter()
        .find(|b| b["spec"]["policyName"] == "narsor-controller-delete-guard")
        .expect("a delete-guard binding");
    assert_eq!(
        delete_binding["spec"]["matchResources"]["namespaceSelector"]["matchLabels"]
            ["kubernetes.io/metadata.name"],
        NAMESPACE
    );

    // The policy's parameters and the controller's configuration agree.
    let params = of_kind(&documents, "ConfigMap")
        .into_iter()
        .find(|c| c["metadata"]["name"] == "narsor-admission")
        .expect("the policy parameters");
    let config_map = of_kind(&documents, "ConfigMap")
        .into_iter()
        .find(|c| c["metadata"]["name"] == "narsor-controller-config")
        .expect("the controller configuration");
    let config: KubernetesBackendConfig =
        serde_yaml::from_str(config_map["data"]["backend.yaml"].as_str().unwrap()).unwrap();
    assert!(config.problems().is_empty(), "{:?}", config.problems());
    assert_eq!(
        params["data"]["workloadServiceAccount"],
        config.workload_service_account.as_str()
    );
    assert_eq!(
        params["data"]["storageClass"],
        config.storage_class.as_str()
    );
    assert_eq!(
        params["data"]["allowedRegistries"]
            .as_str()
            .unwrap()
            .split(',')
            .map(str::trim)
            .collect::<Vec<_>>(),
        config.allowed_registries
    );
    assert_eq!(
        params["data"]["allowedSecrets"]
            .as_str()
            .unwrap()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>(),
        config.allowed_secrets
    );
}

/// The controller Deployment runs the controller as a non-root, single
/// replica, with its credential on its own small volume.
#[test]
fn the_controller_deployment_is_locked_down() {
    let documents = deploy_documents();
    let deployment = of_kind(&documents, "Deployment")
        .into_iter()
        .find(|d| d["metadata"]["name"] == "narsor-controller")
        .expect("the controller Deployment");
    let spec = &deployment["spec"];
    assert_eq!(spec["replicas"], 1);
    assert_eq!(spec["strategy"]["type"], "Recreate");
    let pod = &spec["template"]["spec"];
    assert_eq!(pod["serviceAccountName"], "narsor-controller");
    assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
    let container = &pod["containers"][0];
    assert_eq!(container["command"][0], "/opt/narsor/bin/narsor-controller");
    assert!(container.get("args").is_none());
    assert_eq!(
        container["securityContext"]["allowPrivilegeEscalation"],
        false
    );
    assert_eq!(container["securityContext"]["readOnlyRootFilesystem"], true);
    assert!(container["image"].as_str().unwrap().contains("@sha256:"));
}

// ---------------------------------------------------------------------------
// Optional live-cluster smoke test
// ---------------------------------------------------------------------------

/// Against a disposable namespace of a real cluster: reachability and the
/// storage-class check only, nothing is created.
///
/// `NARSOR_K8S_SMOKE_API_SERVER`, `NARSOR_K8S_SMOKE_NAMESPACE`,
/// `NARSOR_K8S_SMOKE_TOKEN_FILE`, `NARSOR_K8S_SMOKE_CA_FILE` and
/// `NARSOR_K8S_SMOKE_STORAGE_CLASS` select it.
#[tokio::test]
#[ignore = "needs a live cluster (NARSOR_K8S_SMOKE_*)"]
async fn live_cluster_smoke() {
    use narsor_controller::kubernetes::client::{ClusterAccess, TokenSource};
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let mut access = ClusterAccess::explicit(
        var("NARSOR_K8S_SMOKE_API_SERVER"),
        var("NARSOR_K8S_SMOKE_NAMESPACE"),
    );
    access.token = TokenSource::File(var("NARSOR_K8S_SMOKE_TOKEN_FILE").into());
    access.ca_pem = Some(std::fs::read(var("NARSOR_K8S_SMOKE_CA_FILE")).unwrap());
    let backend = KubernetesBackend::new(
        &access,
        KubernetesBackendConfig::new(var("NARSOR_K8S_SMOKE_STORAGE_CLASS"), vec![REGISTRY.into()]),
    )
    .unwrap();
    backend.preflight().await.expect("a usable cluster");
    let task = task_resources(&backend, "narsor-smoke");
    let observed = backend.observe(&task).await.expect("observe");
    assert!(observed.workload.is_none());
}
