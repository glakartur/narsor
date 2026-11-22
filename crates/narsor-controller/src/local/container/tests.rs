use std::ffi::OsString;
use std::io;
use std::sync::Mutex;
use std::time::Instant;

use super::*;
use crate::local::backend::LocalBackend;
use crate::local::paths::{ControllerStatePaths, ControllerWorkPaths};
use crate::local::process::process_alive;
use crate::local::test_support::Scratch;
use crate::local::workload::LastRunner;
use narsor_controller_runtime::backend::{ExecutionBackend, Removal, StorageState};
use narsor_model::domain::TaskId;

/// KTD8: the image label of a controller is its `controller-id` file. A
/// controller without one gets the label computed from its canonical
/// directory (what the label always was), written down once; a recorded id
/// is kept even when it differs, so a moved controller keeps its images.
#[test]
fn the_controller_id_is_initialised_from_the_controller_directory_and_then_kept() {
    use sha2::{Digest as _, Sha256};
    let scratch = Scratch::new();
    let state = scratch.backend.config.state.clone();

    let id = image::controller_id(&state);

    let canonical = fs::canonicalize(state.dir()).unwrap();
    let hash: String = Sha256::digest(canonical.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(id, hash[..16]);
    assert_eq!(
        fs::read_to_string(state.controller_id_file())
            .unwrap()
            .trim(),
        id
    );

    fs::write(state.controller_id_file(), "0123456789abcdef\n").unwrap();
    assert_eq!(image::controller_id(&state), "0123456789abcdef");
}

/// KTD7: a container gets the pass-through variables, the configured
/// ones and the mark; nothing else of the controller's environment.
#[test]
fn a_containers_environment_is_exactly_the_allowlist() {
    let process: Vec<(OsString, OsString)> = [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/me"),
        ("USER", "me"),
        ("XDG_CONFIG_HOME", "/home/me/.config"),
        ("NARSOR_ENROLLMENT_CODE", "secret"),
        ("ANTHROPIC_API_KEY", "sk-ant"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "token"),
        ("SSH_AUTH_SOCK", "/run/ssh"),
        ("TERM", "xterm"),
        ("LANG", "pl_PL.UTF-8"),
        ("LC_ALL", "C"),
        ("TZ", "Europe/Warsaw"),
        ("https_proxy", "http://proxy:3128"),
        ("NO_PROXY", "localhost"),
    ]
    .into_iter()
    .map(|(name, value)| (name.into(), value.into()))
    .collect();
    let configured = vec![("RUST_LOG".into(), "debug".into())];

    let env = container_env(process, &configured, "narsor-local-1");

    let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "TERM",
            "LANG",
            "LC_ALL",
            "TZ",
            "https_proxy",
            "NO_PROXY",
            "RUST_LOG",
            WORKLOAD_MARK_ENV
        ]
    );
    assert_eq!(env.last().unwrap().1, "narsor-local-1");
}

#[test]
fn only_a_loader_failure_reads_as_a_runner_that_cannot_run_in_the_image() {
    let cases = [
        (
            "/opt/narsor/bin/narsor-runner: /lib/x86_64-linux-gnu/libc.so.6: version \
             `GLIBC_2.39' not found (required by /opt/narsor/bin/narsor-runner)",
            true,
        ),
        (
            "[FATAL tini (7)] exec /opt/narsor/bin/narsor-runner failed: No such file or \
             directory",
            true,
        ),
        (
            "exec /opt/narsor/bin/narsor-runner: exec format error",
            true,
        ),
        (
            "runner failed: cannot read /x/credential: No such file or directory",
            false,
        ),
        (
            "runner problem (secret): 'gitlab-token' is not mounted",
            false,
        ),
    ];
    for (logs, expected) in cases {
        let problem = loader_problem(&format!("starting\n{logs}\n"));
        assert_eq!(problem.is_some(), expected, "{logs}");
        if let Some(problem) = problem {
            assert!(problem.contains("glibc 2.39"), "{problem}");
        }
    }
}

/// The Docker work mode against the in-memory Docker.
mod containers {
    use std::sync::mpsc;

    use super::*;
    use crate::local::container::docker_fake::{DockerCall, FakeDocker};
    use crate::local::container::image::{BuildSettings, TaskImageBuilds};
    use narsor_model::domain::{
        ComponentSecret, HomeSource, ImageDigest, ImageSource, ImageSpec, ResolvedEnvironment,
        ResolvedImage, ResourceProfile, SecretDelivery, TaskImageComponent, TaskRepository,
        TaskWorkspace,
    };

    const USER: UserIds = UserIds {
        uid: 1000,
        gid: 100,
    };

    /// A scratch backend in the Docker mode over a fake Docker, with a
    /// home source, a local workspace, a configuration file and a
    /// token store.
    struct Fixture {
        scratch: Scratch,
        docker: Arc<FakeDocker>,
        version_file: PathBuf,
        /// How long a checked image stays current: zero checks the
        /// component versions at every start.
        update_interval: Duration,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_update_interval(Duration::ZERO)
        }

        fn with_update_interval(update_interval: Duration) -> Self {
            let scratch = Scratch::new();
            for dir in ["home/.claude/skills", "project"] {
                fs::create_dir_all(scratch.root.join(dir)).unwrap();
            }
            fs::write(scratch.root.join("config.yaml"), "{}\n").unwrap();
            let version_file = scratch.root.join("version");
            fs::write(&version_file, "1.0\n").unwrap();
            let mut fixture = Self {
                scratch,
                docker: Arc::new(FakeDocker::new()),
                version_file,
                update_interval,
            };
            fixture.use_backend(true);
            fixture
        }

        fn root(&self) -> &Path {
            &self.scratch.root
        }

        /// The controller's state, below the state root `<root>/state`.
        fn state(&self) -> ControllerStatePaths {
            self.scratch.backend.config.state.clone()
        }

        fn store(&self) -> SecretStore {
            SecretStore::of(&self.state())
        }

        /// A backend over the same root: in the Docker mode, or in the
        /// process mode with the transport.
        fn backend(&self, docker_mode: bool) -> LocalBackend {
            let mut config = (*self.scratch.backend.config).clone();
            config.docker = Some(self.docker.clone());
            config.container_mode = docker_mode.then(|| ContainerMode {
                builds: TaskImageBuilds::new(
                    self.docker.clone(),
                    &self.state(),
                    BuildSettings {
                        update_interval: Some(self.update_interval),
                        version_timeout: Duration::from_secs(5),
                        version_output_cap: 64,
                        user: USER,
                        retry_delay: Duration::from_millis(100),
                    },
                ),
                secrets: self.store(),
                state_root: self.root().join("state"),
                user: USER,
                host_home: Some(self.root().join("home")),
                config_file: Some(self.root().join("config.yaml")),
                runner_args: Vec::new(),
                start_window: DEFAULT_START_WINDOW,
            });
            LocalBackend::new(config)
        }

        fn use_backend(&mut self, docker_mode: bool) {
            self.scratch.backend = self.backend(docker_mode);
        }

        fn backend_ref(&self) -> &LocalBackend {
            &self.scratch.backend
        }

        fn task(&self, key: &str) -> TaskResources {
            let id = TaskId::parse(key).unwrap();
            self.backend_ref().resources(&id, key).unwrap()
        }

        fn image(&self) -> TaskImage {
            TaskImage {
                base: ImageSpec::pinned("debian:trixie-slim"),
                components: vec![
                    TaskImageComponent {
                        name: "claude-code".into(),
                        install: "echo claude\n".into(),
                        version: Some(format!("cat '{}'", self.version_file.display())),
                        secrets: vec![ComponentSecret {
                            name: "claude-token".into(),
                            delivery: SecretDelivery::Env("CLAUDE_CODE_OAUTH_TOKEN".into()),
                        }],
                        home: vec![HomeSource {
                            from: "~/.claude/skills".into(),
                            to: ".claude/skills".into(),
                        }],
                    },
                    TaskImageComponent {
                        name: "glab".into(),
                        install: "echo glab\n".into(),
                        version: None,
                        secrets: Vec::new(),
                        home: Vec::new(),
                    },
                ],
                secrets: Vec::new(),
            }
        }

        fn environment(&self) -> ResolvedEnvironment {
            let mut environment = ResolvedEnvironment::harness_only("claude-code");
            environment.task_image = Some(self.image());
            environment
        }

        fn launch(&self, key: &str, environment: ResolvedEnvironment) -> WorkloadLaunch {
            self.launch_from(key, environment, &self.root().join("project"))
        }

        /// A launch whose local workspace source is `project`.
        fn launch_from(
            &self,
            key: &str,
            environment: ResolvedEnvironment,
            project: &Path,
        ) -> WorkloadLaunch {
            let workspace = TaskWorkspace::single(TaskRepository::new(
                "project",
                TaskRepositorySource::Local {
                    path: project.display().to_string(),
                },
            ));
            let spec = TaskLaunchSpec::new(
                "wf",
                1,
                serde_json::json!({}),
                workspace,
                environment,
                ResourceProfile::new(1000, 2048),
            );
            WorkloadLaunch {
                task_id: TaskId::parse(key).unwrap(),
                generation: 1,
                incarnation: format!("{key}-g1"),
                lease_epoch: 1,
                launch_spec: serde_json::to_value(spec).unwrap(),
                control_plane_url: "http://127.0.0.1:9".into(),
                credential: narsor_controller_runtime::backend::RunnerCredential::new("nsr.test"),
            }
        }

        fn default_launch(&self, key: &str) -> WorkloadLaunch {
            self.launch(key, self.environment())
        }

        /// Observes until the workload leaves `Starting`, issuing the fresh
        /// credential its start asks for as the controller would.
        async fn settled(&self, task: &TaskResources) -> Option<WorkloadState> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let observed = self.backend_ref().observe(task).await.unwrap().workload;
                self.issue_wanted_credential(task, FRESH_CREDENTIAL);
                match &observed {
                    Some(state) if state.phase == WorkloadPhase::Starting => {
                        assert!(Instant::now() < deadline, "still starting");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    _ => return observed,
                }
            }
        }

        /// Hands the backend `credential` when it asked for one of `task`.
        fn issue_wanted_credential(&self, task: &TaskResources, credential: &str) {
            let specs = self.backend_ref().launch_specs().expect("shared");
            for generation in 1..=2 {
                if let Some(incarnation) = specs.credential_request(&task.task_id, generation) {
                    specs.provide_credential(
                        &task.task_id,
                        generation,
                        &incarnation,
                        narsor_controller_runtime::backend::RunnerCredential::new(credential),
                    );
                }
            }
        }

        /// Provisions the task and waits for its running container.
        async fn running(&self, key: &str) -> (TaskResources, RunRequest) {
            let task = self.task(key);
            self.backend_ref()
                .provision(&task, &self.default_launch(key))
                .await
                .unwrap();
            let state = self.settled(&task).await.unwrap();
            assert_eq!(state.phase, WorkloadPhase::Running);
            let run = self
                .docker
                .runs()
                .into_iter()
                .rev()
                .find(|run| run.labels[TASK_LABEL] == key)
                .expect("a container ran");
            (task, run)
        }

        fn task_record(&self, task: &TaskResources) -> Option<TaskRecord> {
            self.backend_ref().store.read_task_record(task)
        }
    }

    /// The credential the test's "controller" issues when a container
    /// starts.
    const FRESH_CREDENTIAL: &str = "nsr.fresh";

    fn arg_after<'a>(run: &'a RunRequest, flag: &str) -> Vec<&'a str> {
        run.args
            .windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
            .collect()
    }

    /// KTD6: every path the runner is told about is inside a mount.
    fn assert_paths_mounted(run: &RunRequest) {
        let mounted = |path: &Path| {
            run.mounts
                .iter()
                .any(|mount| path.starts_with(&mount.target))
        };
        assert!(mounted(Path::new(&run.program)), "the runner binary");
        assert!(mounted(run.workdir.as_deref().unwrap()), "the workdir");
        for arg in run.args.iter().filter(|arg| arg.starts_with('/')) {
            // A home source names its mount before the last '='.
            let path = arg.rsplit_once('=').map_or(arg.as_str(), |(path, _)| path);
            assert!(mounted(Path::new(path)), "{arg} is not mounted");
        }
    }

    #[tokio::test]
    async fn a_container_starts_once_its_image_is_built_with_only_what_the_task_needs() {
        let fixture = Fixture::new();
        fixture.store().set("claude-token", "oauth-secret").unwrap();
        fixture.store().set("db-password", "db-secret").unwrap();
        let mut environment = fixture.environment();
        environment.task_image.as_mut().unwrap().secrets = vec![ComponentSecret {
            name: "db-password".into(),
            delivery: SecretDelivery::Env("DATABASE_PASSWORD".into()),
        }];
        let launch = fixture.launch("task-1", environment);
        let (release, wait) = mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        fixture.docker.on_build(move |_| {
            let _ = wait.lock().unwrap().recv();
        });
        let task = fixture.task("task-1");

        let started = Instant::now();
        let provisioned = fixture
            .backend_ref()
            .provision(&task, &launch)
            .await
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "no wait for the build"
        );
        assert_eq!(provisioned.workload.phase, WorkloadPhase::Starting);
        let observed = fixture.backend_ref().observe(&task).await.unwrap();
        assert_eq!(observed.workload.unwrap().phase, WorkloadPhase::Starting);
        assert!(fixture.docker.runs().is_empty());

        release.send(()).unwrap();
        let state = fixture.settled(&task).await.unwrap();
        assert_eq!(state.phase, WorkloadPhase::Running);
        let runs = fixture.docker.runs();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];

        // The image just built, the workload's name and labels, the host
        // user (the hardening flags are fixed by the transport).
        assert_eq!(run.name, state.uid);
        assert_eq!(run.labels[WORKLOAD_LABEL], state.uid);
        assert_eq!(run.labels[MANAGED_LABEL], "true");
        assert_eq!(
            fixture.task_record(&task).unwrap(),
            TaskRecord {
                kind: LastRunner::Container,
                image: Some(run.image.clone())
            }
        );
        assert_eq!(run.user, USER);
        assert_eq!(run.program, RUNNER_IN_CONTAINER);

        let backend = fixture.backend_ref();
        let storage = backend.storage_dir(&task);
        let dir = backend.store.workload_dir(&task, Role::Work);
        let secrets = dir.join("secrets");
        let mounts: Vec<(PathBuf, PathBuf, bool)> = run
            .mounts
            .iter()
            .map(|m| (m.source.clone(), m.target.clone(), m.read_only))
            .collect();
        let same = |path: PathBuf, read_only| (path.clone(), path, read_only);
        assert_eq!(
            mounts,
            [
                same(storage.clone(), false),
                same(dir.clone(), true),
                (
                    backend.config.program.clone(),
                    RUNNER_IN_CONTAINER.into(),
                    true
                ),
                same(fixture.root().join("config.yaml"), true),
                same(secrets.clone(), true),
                same(fixture.root().join("project"), true),
                (
                    fixture.root().join("home/.claude/skills"),
                    "/run/narsor/home/claude-code-0".into(),
                    true
                ),
            ]
        );
        assert_eq!(run.workdir.as_deref(), Some(storage.as_path()));
        assert_eq!(arg_after(run, "--home"), ["private"]);
        assert_eq!(arg_after(run, "--secrets-dir"), [secrets.to_str().unwrap()]);
        assert_eq!(
            arg_after(run, "--deliver-secret"),
            [
                "db-password=env:DATABASE_PASSWORD",
                "claude-token=env:CLAUDE_CODE_OAUTH_TOKEN"
            ]
        );
        assert_eq!(
            arg_after(run, "--home-source"),
            ["/run/narsor/home/claude-code-0=.claude/skills"]
        );
        assert_eq!(
            fs::read_to_string(secrets.join("claude-token")).unwrap(),
            "oauth-secret"
        );
        assert_eq!(
            fs::read_to_string(secrets.join("db-password")).unwrap(),
            "db-secret",
            "an image-level secret is copied like a component's"
        );

        // AE5: no host harness directory, no host HOME, keys or socket.
        for flag in [
            "--codex-home",
            "--claude-config-dir",
            "--claude-profile-source",
            "--read-only",
        ] {
            assert!(!run.args.iter().any(|arg| arg == flag), "{flag}");
        }
        for mount in &run.mounts {
            let source = mount.source.display().to_string();
            assert_ne!(mount.source, fixture.root().join("home"));
            assert!(
                !source.contains(".ssh") && !source.contains("docker.sock"),
                "{source}"
            );
        }
        for (name, _) in &run.env {
            assert!(
                name == WORKLOAD_MARK_ENV || !name.starts_with("NARSOR_"),
                "{name}"
            );
            for forbidden in [
                "PATH",
                "HOME",
                "USER",
                "ANTHROPIC_API_KEY",
                "CODEX_API_KEY",
                "OPENAI_API_KEY",
            ] {
                assert_ne!(name, forbidden);
            }
            assert!(!name.starts_with("XDG_"), "{name}");
        }
        assert!(run
            .env
            .iter()
            .any(|(name, value)| name == WORKLOAD_MARK_ENV && *value == state.uid));
        assert!(
            run.env.iter().any(|(name, _)| name == "ARGS_FILE"),
            "configured"
        );
        assert!(!run
            .env
            .iter()
            .any(|(_, value)| value.contains("oauth-secret")));
        assert_paths_mounted(run);
    }

    /// AE2 and the refusals of KTD8, KTD10 and per-step images: each a
    /// readable problem, with nothing created and no host runner. A
    /// daemon that does not answer is the one transient refusal.
    #[tokio::test]
    async fn the_docker_mode_refuses_what_it_cannot_run_without_a_host_fallback() {
        use narsor_controller_runtime::backend::BackendErrorKind::{Rejected, Transient};
        type Setup = fn(&Fixture);
        let healthy: Setup = |fixture| fixture.store().set("claude-token", "t").unwrap();
        let step_image = {
            let digest = ImageDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
            ResolvedImage {
                reference: "node:22".into(),
                pinned: format!("node@{digest}"),
                digest,
                tag_resolved: false,
                source: ImageSource::Step {
                    workflow: "wf".into(),
                    state: "review".into(),
                },
            }
        };
        type Launch = Box<dyn Fn(&Fixture) -> WorkloadLaunch>;
        let home_source = |from: fn(&Fixture) -> PathBuf| -> Launch {
            Box::new(move |fixture| {
                let mut environment = fixture.environment();
                let image = environment.task_image.as_mut().unwrap();
                image.components[0].home.push(HomeSource {
                    from: from(fixture).display().to_string(),
                    to: ".narsor".into(),
                });
                fixture.launch("task-1", environment)
            })
        };
        let default: fn() -> Launch = || Box::new(|fixture| fixture.default_launch("task-1"));
        let cases: Vec<(&str, Setup, Launch, &str, _)> = vec![
            (
                "docker not installed",
                |fixture| {
                    fixture.docker.set_not_installed();
                    fixture.store().set("claude-token", "t").unwrap();
                },
                default(),
                "docker not found",
                Rejected,
            ),
            (
                "rootless docker",
                |fixture| {
                    fixture.docker.set_rootless();
                    fixture.store().set("claude-token", "t").unwrap();
                },
                default(),
                "rootless",
                Rejected,
            ),
            (
                "unreachable daemon",
                |fixture| {
                    fixture.docker.set_unreachable(true);
                    fixture.store().set("claude-token", "t").unwrap();
                },
                default(),
                "cannot be reached",
                Transient,
            ),
            (
                "no sealed task image",
                healthy,
                Box::new(|fixture| {
                    fixture.launch("task-1", ResolvedEnvironment::harness_only("claude-code"))
                }),
                "then run `narsor-cli task refresh-env task-1`",
                Rejected,
            ),
            (
                "a step image",
                healthy,
                Box::new(move |fixture| {
                    let mut environment = fixture.environment();
                    environment
                        .step_images
                        .insert("review".into(), step_image.clone());
                    fixture.launch("task-1", environment)
                }),
                "step 'review'",
                Rejected,
            ),
            (
                "a secret missing from the store",
                |_| {},
                default(),
                "narsor-cli secret set claude-token",
                Rejected,
            ),
            (
                "a home source over the controller's token store",
                healthy,
                home_source(|fixture| fixture.store().dir().to_path_buf()),
                "overlaps Narsor's protected directory",
                Rejected,
            ),
            (
                "a home source inside the state root, beside the controller's directory",
                |fixture| {
                    fixture.store().set("claude-token", "t").unwrap();
                    fs::create_dir_all(fixture.root().join("state/run")).unwrap();
                },
                home_source(|fixture| fixture.root().join("state/run")),
                "overlaps Narsor's protected directory",
                Rejected,
            ),
            (
                "a local workspace source over the controller's directory",
                healthy,
                Box::new(|fixture| {
                    let environment = fixture.environment();
                    fixture.launch_from("task-1", environment, &fixture.root().join("state"))
                }),
                "the local workspace source",
                Rejected,
            ),
        ];
        for (case, setup, launch, expected, kind) in cases {
            let fixture = Fixture::new();
            setup(&fixture);
            let task = fixture.task("task-1");
            let launch = launch(&fixture);

            let refused = fixture
                .backend_ref()
                .provision(&task, &launch)
                .await
                .expect_err(case);

            assert_eq!(refused.kind, kind, "{case}");
            assert!(refused.message.contains(expected), "{case}: {refused}");
            let backend = fixture.backend_ref();
            assert!(!backend.storage_dir(&task).exists(), "{case}: no storage");
            assert!(
                !backend.store.workload_dir(&task, Role::Work).exists(),
                "{case}"
            );
            assert!(
                !fixture.root().join("args").exists(),
                "{case}: no host runner"
            );
            assert!(fixture.docker.runs().is_empty(), "{case}: no container");
        }
    }

    /// AE1 and R3: a task paused in a container, resumed by a
    /// controller restarted in the process mode, runs as a host process
    /// with a warning that names it; the per-task record survived.
    #[tokio::test]
    async fn a_task_that_ran_in_a_container_resumes_as_a_host_process_with_a_warning() {
        let mut fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();
        let (task, run) = fixture.running("task-1").await;

        let released = fixture.backend_ref().release_compute(&task).await.unwrap();
        assert_eq!(released, Removal::Removed);
        assert!(fixture.docker.container_names().is_empty());
        assert_eq!(
            fixture.task_record(&task).map(|record| record.kind),
            Some(LastRunner::Container),
            "the per-task record survives the release"
        );

        fixture.use_backend(false);
        let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || LogWriter(writer.clone()))
            .with_ansi(false)
            .finish();
        let launch = fixture.default_launch("task-1");
        let resumed = tracing::subscriber::with_default(subscriber, || {
            fixture.backend_ref().create(&task, &launch, false)
        })
        .unwrap();

        assert_eq!(resumed.workload.phase, WorkloadPhase::Running);
        assert!(
            fixture.backend_ref().runner_pid(&task).is_some(),
            "a host runner"
        );
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("WARN"), "{logs}");
        assert!(
            logs.contains("task-1 last ran in a Docker container"),
            "{logs}"
        );
        assert_eq!(
            fixture.task_record(&task).unwrap(),
            TaskRecord {
                kind: LastRunner::Process,
                image: Some(run.image),
            }
        );
    }

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// R11: a resumed task runs on the image it started on, even when
    /// its definition has a newer build.
    #[tokio::test]
    async fn a_resumed_task_runs_on_its_recorded_image_after_a_newer_build() {
        let fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();
        let (task, first) = fixture.running("task-1").await;
        fixture.backend_ref().release_compute(&task).await.unwrap();

        fs::write(&fixture.version_file, "2.0\n").unwrap();
        let (_, newer) = fixture.running("task-2").await;
        assert_ne!(newer.image, first.image, "a new build");

        fixture
            .backend_ref()
            .resume(&task, &fixture.default_launch("task-1"))
            .await
            .unwrap();
        let state = fixture.settled(&task).await.unwrap();

        assert_eq!(state.phase, WorkloadPhase::Running);
        let resumed = fixture.docker.runs().pop().unwrap();
        assert_eq!(resumed.labels[TASK_LABEL], "task-1");
        assert_eq!(resumed.image, first.image);
        assert!(fixture.docker.image_ids().contains(&first.image));
    }

    /// D58: an unreachable daemon is "cannot tell", never a loss; a
    /// container that is gone, with its storage, is absent; an early
    /// exit with a loader error records why.
    #[tokio::test]
    async fn a_started_container_is_observed_without_guessing() {
        let fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();
        let (task, run) = fixture.running("task-1").await;
        let backend = fixture.backend_ref();

        fixture.docker.set_unreachable(true);
        let unknown = backend.observe(&task).await.expect_err("cannot tell");
        assert_eq!(
            unknown.kind,
            narsor_controller_runtime::backend::BackendErrorKind::Transient
        );
        fixture.docker.set_unreachable(false);

        fixture
            .docker
            .set_container_state(&run.name, ContainerState::Exited(127));
        fixture.docker.set_container_logs(
            &run.name,
            "/opt/narsor/bin/narsor-runner: /lib/libc.so.6: version `GLIBC_2.39' not found",
        );
        let observed = backend.observe(&task).await.unwrap();
        assert_eq!(
            observed.workload.unwrap().phase,
            WorkloadPhase::Exited { code: Some(127) }
        );
        let record = backend
            .store
            .read_record(&task, Role::Work)
            .unwrap()
            .unwrap();
        let WorkloadKind::Container { container } = record.kind else {
            panic!("a container record");
        };
        assert!(
            container
                .problem
                .unwrap()
                .contains("cannot run in this image"),
            "the loader failure is recorded"
        );

        let (other, other_run) = fixture.running("task-2").await;
        fixture.docker.remove_container(&other_run.name).unwrap();
        let observed = backend.observe(&other).await.unwrap();
        assert_eq!(observed.workload, None);
        assert_eq!(observed.storage, StorageState::Present);
    }

    /// KTD1: the record's kind decides how a workload is removed, not
    /// the mode the controller runs in now.
    #[tokio::test]
    async fn a_workload_is_removed_the_way_its_record_says() {
        let mut fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();

        // A container left by a Docker-mode controller, removed by a
        // process-mode one.
        let (task, run) = fixture.running("task-1").await;
        fixture.use_backend(false);
        let removed = fixture.backend_ref().release_compute(&task).await.unwrap();
        assert_eq!(removed, Removal::Removed);
        assert!(!fixture.docker.container_names().contains(&run.name));

        // A host runner left by a process-mode controller, removed by a
        // Docker-mode one.
        let other = fixture.task("task-2");
        fixture
            .backend_ref()
            .provision(&other, &fixture.default_launch("task-2"))
            .await
            .unwrap();
        let pid = fixture
            .backend_ref()
            .runner_pid(&other)
            .expect("a host runner");
        fixture.use_backend(true);
        let calls_before = fixture.docker.calls().len();
        let removed = fixture.backend_ref().release_compute(&other).await.unwrap();

        assert_eq!(removed, Removal::Removed);
        assert!(!process_alive(pid, None), "the process group was killed");
        assert!(!fixture.docker.calls()[calls_before..]
            .iter()
            .any(|call| matches!(call, DockerCall::RemoveContainer(_))));
    }

    /// A start refused because the image build already failed leaves no
    /// new task storage behind.
    #[tokio::test]
    async fn a_start_refused_for_a_failed_build_leaves_no_task_storage() {
        let fixture = Fixture::with_update_interval(Duration::from_secs(3600));
        fixture.store().set("claude-token", "t").unwrap();
        fixture.docker.fail_builds(Some("exit code: 1"));
        let first = fixture.task("task-1");
        fixture
            .backend_ref()
            .provision(&first, &fixture.default_launch("task-1"))
            .await
            .unwrap();
        fixture.settled(&first).await.unwrap();
        let task = fixture.task("task-2");

        let refused = fixture
            .backend_ref()
            .provision(&task, &fixture.default_launch("task-2"))
            .await
            .expect_err("the build already failed");

        assert_eq!(
            refused.kind,
            narsor_controller_runtime::backend::BackendErrorKind::Rejected
        );
        assert!(!fixture.backend_ref().storage_dir(&task).exists());
    }

    /// KTD4: a controller restarted while the image builds no longer holds
    /// the launch spec; the container waits until the controller fetched it
    /// again, then starts from it.
    #[tokio::test]
    async fn after_a_restart_the_container_starts_once_its_spec_is_fetched_again() {
        let mut fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();
        let (release, wait) = mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        fixture.docker.on_build(move |_| {
            let _ = wait.lock().unwrap().recv();
        });
        let task = fixture.task("task-1");
        let launch = fixture.default_launch("task-1");
        fixture
            .backend_ref()
            .provision(&task, &launch)
            .await
            .unwrap();

        fixture.use_backend(true);
        drop(release);
        let specs = fixture.backend_ref().launch_specs().expect("shared");
        let observed = fixture.backend_ref().observe(&task).await.unwrap();
        assert_eq!(observed.workload.unwrap().phase, WorkloadPhase::Starting);
        assert!(specs.is_requested(&task.task_id, launch.generation));
        assert!(fixture.docker.runs().is_empty(), "no start without a spec");

        specs.insert(&task.task_id, launch.generation, launch.launch_spec.clone());
        let state = fixture.settled(&task).await.unwrap();

        assert_eq!(state.phase, WorkloadPhase::Running);
        assert_eq!(fixture.docker.runs().len(), 1);
    }

    /// The image build can outlast the credential issued at creation: the
    /// container starts only with a fresh one, issued for the same
    /// incarnation when the start asks for it, in the file the runner reads.
    /// The workload directory holds no launch spec, only its digest in the
    /// arguments (R5).
    #[tokio::test]
    async fn a_container_starts_with_a_credential_issued_at_its_start() {
        let fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();
        let task = fixture.task("task-1");
        let launch = fixture.default_launch("task-1");
        fixture
            .backend_ref()
            .provision(&task, &launch)
            .await
            .unwrap();
        let specs = fixture.backend_ref().launch_specs().expect("shared");
        let deadline = Instant::now() + Duration::from_secs(10);
        while specs.credential_request(&task.task_id, 1).is_none() {
            assert!(Instant::now() < deadline, "no credential was asked for");
            fixture.backend_ref().observe(&task).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.docker.runs().is_empty(), "no start without it");
        assert_eq!(
            specs.credential_request(&task.task_id, 1).as_deref(),
            Some(launch.incarnation.as_str())
        );

        let state = fixture.settled(&task).await.unwrap();

        assert_eq!(state.phase, WorkloadPhase::Running);
        let dir = fixture.backend_ref().store.workload_dir(&task, Role::Work);
        assert_eq!(
            fs::read_to_string(dir.join("credential")).unwrap(),
            FRESH_CREDENTIAL
        );
        let run = &fixture.docker.runs()[0];
        assert_eq!(
            arg_after(run, "--launch-digest"),
            [launch.launch_digest().unwrap()]
        );
        assert!(!run.args.iter().any(|arg| arg == "--launch-spec"));
        assert!(!dir.join("launch-spec.json").exists());
    }

    /// A build that cannot reach Docker is tried again after a short
    /// delay: its task waits as for a build and a new task is not
    /// refused meanwhile.
    #[tokio::test]
    async fn a_task_waits_while_its_image_build_cannot_reach_docker() {
        let fixture = Fixture::with_update_interval(Duration::from_secs(3600));
        fixture.store().set("claude-token", "t").unwrap();
        fixture
            .docker
            .fail_builds_with(Some(DockerError::Unreachable("restarting".into())));
        let task = fixture.task("task-1");
        fixture
            .backend_ref()
            .provision(&task, &fixture.default_launch("task-1"))
            .await
            .unwrap();

        let waited_until = Instant::now() + Duration::from_millis(400);
        while Instant::now() < waited_until {
            let observed = fixture.backend_ref().observe(&task).await.unwrap();
            assert_eq!(observed.workload.unwrap().phase, WorkloadPhase::Starting);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let other = fixture.task("task-2");
        let accepted = fixture
            .backend_ref()
            .provision(&other, &fixture.default_launch("task-2"))
            .await
            .unwrap();
        assert_eq!(accepted.workload.phase, WorkloadPhase::Starting);
        assert!(fixture.docker.builds().len() >= 2, "the build was retried");

        fixture.docker.fail_builds_with(None);
        let state = fixture.settled(&task).await.unwrap();
        assert_eq!(state.phase, WorkloadPhase::Running);
    }

    /// A relative storage root (the server's default layout) still gives
    /// the container absolute mount sources and workdir.
    #[tokio::test]
    async fn a_relative_storage_root_gives_the_container_absolute_paths() {
        let mut fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();
        let mut config = (*fixture.backend(true).config).clone();
        let up = std::env::current_dir()
            .unwrap()
            .components()
            .skip(1)
            .map(|_| "..")
            .collect::<PathBuf>();
        let relative = up.join(fixture.root().strip_prefix("/").unwrap());
        assert!(relative.is_relative());
        config.state = ControllerStatePaths::new(relative.join("state"));
        config.work = ControllerWorkPaths::new(relative.join("work"));
        fixture.scratch.backend = LocalBackend::new(config);

        let (_, run) = fixture.running("task-1").await;

        for mount in &run.mounts {
            assert!(mount.source.is_absolute(), "{}", mount.source.display());
        }
        assert!(run.workdir.unwrap().is_absolute());
    }

    /// KTD12: a failed build with no previous image ends the workload
    /// with the failing component named, and the next start says so.
    #[tokio::test]
    async fn a_failed_build_ends_the_workload_naming_the_component() {
        let fixture = Fixture::with_update_interval(Duration::from_secs(3600));
        fixture.store().set("claude-token", "t").unwrap();
        fixture.docker.fail_builds(Some(
            "process \"/bin/sh -c sh /opt/narsor/components/glab.sh\" did not complete \
             successfully: exit code: 1",
        ));
        let task = fixture.task("task-1");
        let launch = fixture.default_launch("task-1");
        fixture
            .backend_ref()
            .provision(&task, &launch)
            .await
            .unwrap();

        let state = fixture.settled(&task).await.unwrap();

        assert_eq!(state.phase, WorkloadPhase::Exited { code: None });
        assert!(fixture.docker.runs().is_empty());
        fixture.backend_ref().release_compute(&task).await.unwrap();
        let refused = fixture
            .backend_ref()
            .resume(&task, &launch)
            .await
            .expect_err("the build still fails");
        assert!(refused.message.contains("component 'glab'"), "{refused}");
    }

    /// Waits, in real time, up to `limit` for `check`.
    async fn eventually(limit: Duration, check: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while !check() {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    /// How long a wake or a follow that should come may take.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// Whether the backend was woken since its last wake was taken. A wake
    /// is kept until taken, so once the follower handled an event this says
    /// whether that event woke it, without waiting for anything.
    async fn woken(backend: &LocalBackend) -> bool {
        tokio::time::timeout(Duration::ZERO, backend.changed())
            .await
            .is_ok()
    }

    /// R4: a managed container that dies wakes the controller, whose
    /// observation then sees it exited; an event of a container without the
    /// managed label wakes nothing.
    #[tokio::test]
    async fn a_managed_containers_death_wakes_the_controller_and_a_strangers_does_not() {
        let fixture = Fixture::new();
        fixture.store().set("claude-token", "t").unwrap();
        let (task, run) = fixture.running("task-1").await;
        let backend = fixture.backend_ref();
        assert!(!woken(backend).await, "nothing yet");

        fixture
            .docker
            .emit_event("die", "someone-elses", BTreeMap::new());
        let handled = eventually(PATIENCE, || fixture.docker.events_handled() == 1).await;
        assert!(handled, "the follower saw the event");
        assert!(!woken(backend).await, "not a managed container");

        fixture.docker.kill_container(&run.name);
        fixture
            .docker
            .emit_event("die", &run.name, run.labels.clone());
        tokio::time::timeout(PATIENCE, backend.changed())
            .await
            .expect("woken by the death");

        let observed = backend.observe(&task).await.unwrap().workload.unwrap();
        assert_eq!(observed.phase, WorkloadPhase::Exited { code: Some(137) });
    }

    /// An event stream that ends is followed again, and wakes as before.
    #[tokio::test]
    async fn an_ended_event_stream_is_followed_again() {
        let fixture = Fixture::new();
        let backend = fixture.backend_ref();
        assert!(!woken(backend).await, "nothing yet");
        assert!(eventually(PATIENCE, || fixture.docker.event_follows() == 1).await);

        fixture.docker.end_event_stream();

        assert!(
            eventually(PATIENCE, || fixture.docker.event_follows() == 2).await,
            "followed again"
        );
        let labels = BTreeMap::from([(MANAGED_LABEL.to_string(), "true".to_string())]);
        fixture.docker.emit_event("die", "narsor-task-1", labels);
        tokio::time::timeout(PATIENCE, backend.changed())
            .await
            .expect("woken after the restart");
    }
}
