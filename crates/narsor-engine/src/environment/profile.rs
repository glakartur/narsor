//! User profiles (R16) and the runtime catalogue (R20).
//!
//! A profile is what one user adds to a repository's environment: allowed
//! settings, skills, tool packages and the names of the credentials they
//! need. It is stored as `{catalog}/profiles/{id}.yaml`:
//!
//! ```yaml
//! version: 3
//! harness: claude-code          # optional preferred harness
//! settings: { editor: vim }
//! skills: [code-review]
//! tools: [gh]
//! secrets: [github-token]       # names from the backend's allow list
//! ```
//!
//! The `version` is the owner's own counter (R18); the resolved environment
//! additionally records a digest of the whole profile, so an edit that forgot
//! to bump it still yields a different environment identity.
//!
//! The runtime catalogue, `{catalog}/runtime.yaml`, names the Narsor runtime
//! bundle, the tool packages profiles may pick from, and the global task
//! image (KTD8):
//!
//! ```yaml
//! bundle: ghcr.io/narsor/runtime@sha256:…   # needed by backends that run images
//! tools:
//!   gh: { reference: ghcr.io/narsor/tools/gh:2.60, digest_policy: resolve }
//! task_image:
//!   base: debian:trixie-slim
//!   components:
//!     - name: glab
//!       install: components/glab.sh          # a path inside the catalogue,
//!       version: { inline: "glab --version" } # or the script text itself
//!       secrets: [{ name: gitlab-token, env: GITLAB_TOKEN }]
//!       home: [{ from: ~/.config/glab-cli, to: .config/glab-cli }]
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Deserializer, Serialize};

use crate::domain::{
    canonical_sha256, ComponentSecret, HomeSource, ImageReference, ImageSpec, ResolvedProfile,
    SecretDelivery, TaskImage, TaskImageComponent,
};

use super::backend::is_reference_name;

/// The profile used when nothing names one and no file defines it.
pub const DEFAULT_PROFILE: &str = "default";

/// One user profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRecord {
    /// Identifier, carried by the file name.
    #[serde(skip)]
    pub id: String,
    /// The owner's version counter.
    pub version: u32,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Preferred harness for agent steps that name none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Allowed settings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, String>,
    /// Skills, materialized from the runtime bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Tool packages from the runtime catalogue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Names of backend secret references the tools need.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl ProfileRecord {
    /// An empty profile at version 0 — what [`DEFAULT_PROFILE`] means when no
    /// file defines it, so a fresh installation needs no profile file.
    pub fn empty(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// Every problem with the profile, empty when it is usable.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !is_reference_name(&self.id) {
            problems.push(format!("profile id '{}' is not a usable name", self.id));
        }
        if let Some(harness) = &self.harness {
            if harness.trim().is_empty() {
                problems.push("harness is blank".to_string());
            }
        }
        for key in self.settings.keys() {
            if !is_reference_name(key) {
                problems.push(format!("settings: key '{key}' is not a usable name"));
            }
        }
        for (field, names) in [
            ("skills", &self.skills),
            ("tools", &self.tools),
            ("secrets", &self.secrets),
        ] {
            let mut seen = BTreeSet::new();
            for name in names {
                if !is_reference_name(name) {
                    problems.push(format!("{field}: '{name}' is not a usable name"));
                } else if !seen.insert(name) {
                    problems.push(format!("{field}: '{name}' is listed twice"));
                }
            }
        }
        problems
    }

    /// The profile as a task records it.
    pub fn resolve(&self) -> ResolvedProfile {
        let mut skills = self.skills.clone();
        skills.sort();
        let mut tools = self.tools.clone();
        tools.sort();
        let content = serde_json::to_value(self).expect("a profile always serializes");
        ResolvedProfile {
            id: self.id.clone(),
            version: self.version,
            digest: canonical_sha256(&content),
            settings: self.settings.clone(),
            skills,
            tools,
        }
    }
}

/// The runtime bundle, the tool packages (R20) and the global task image
/// (KTD8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCatalog {
    /// The Narsor runtime bundle OCI image. Every backend that runs images
    /// needs it; the local backend does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<ImageSpec>,
    /// Tool package images, by tool name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ImageSpec>,
    /// The global task image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_image: Option<TaskImageConfig>,
}

impl RuntimeCatalog {
    /// Every problem with the catalogue, empty when it is usable.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(bundle) = &self.bundle {
            if let Err(error) = bundle.check() {
                problems.push(format!("bundle: {error}"));
            }
        }
        for (name, image) in &self.tools {
            if !is_reference_name(name) {
                problems.push(format!("tools: '{name}' is not a usable name"));
            }
            if let Err(error) = image.check() {
                problems.push(format!("tools.{name}: {error}"));
            }
        }
        if let Some(task_image) = &self.task_image {
            problems.extend(task_image.problems());
        }
        problems
    }
}

/// Environment variables the runner's contract or its scrub owns, which a
/// component secret may never be delivered as. The names of the harness API
/// keys repeat `narsor_shared::config::API_KEY_ENVS`, which the runner
/// removes from every harness environment.
fn reserved_env_problem(name: &str) -> Option<String> {
    let reserved = name.starts_with("LD_")
        || name.starts_with("NARSOR_")
        || narsor_shared::config::API_KEY_ENVS.contains(&name);
    reserved.then(|| format!("environment variable '{name}' is reserved for the runner"))
}

/// The global task image as `runtime.yaml` writes it: a base and the
/// components installed on it, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskImageConfig {
    /// The base image. Only its syntax is checked here: the local Docker
    /// mode lets `docker` pull a tag itself, and a backend that runs images
    /// applies the digest policy when it falls back to it (KTD9).
    pub base: ImageSpec,
    /// The components, in installation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ComponentConfig>,
}

/// One component of the task image as `runtime.yaml` writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentConfig {
    /// Name, `[a-z0-9][a-z0-9-]*`, unique in the definition.
    pub name: String,
    /// The install script.
    pub install: ScriptSource,
    /// The script that prints the version to install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ScriptSource>,
    /// Token store entries the component needs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ComponentSecretConfig>,
    /// Host configuration copied into the private HOME.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub home: Vec<HomeSourceConfig>,
}

/// A component script: a bare string is a path relative to the catalogue
/// directory, `{ inline: … }` is the script text itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ScriptSource {
    /// A file inside the catalogue directory.
    Path(String),
    /// The script text.
    Inline {
        /// The text.
        inline: String,
    },
}

impl<'de> Deserialize<'de> for ScriptSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Inline {
            inline: String,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Path(String),
            Inline(Inline),
        }
        match Either::deserialize(deserializer).map_err(|_| {
            serde::de::Error::custom(
                "a script is a path relative to the catalogue, or a mapping with 'inline' \
                 and the script text",
            )
        })? {
            Either::Path(path) => Ok(Self::Path(path)),
            Either::Inline(Inline { inline }) => Ok(Self::Inline { inline }),
        }
    }
}

/// One token store entry a component needs, as `runtime.yaml` writes it:
/// `{ name, env: VAR }` or `{ name, file: path/under/home }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentSecretConfig {
    /// The token store entry.
    pub name: String,
    /// Deliver as this environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Deliver as this file, relative to the private HOME.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl ComponentSecretConfig {
    fn delivery(&self) -> Result<SecretDelivery, String> {
        match (&self.env, &self.file) {
            (Some(env), None) => Ok(SecretDelivery::Env(env.clone())),
            (None, Some(file)) => Ok(SecretDelivery::File(file.clone())),
            _ => Err("set exactly one of 'env' and 'file'".to_string()),
        }
    }
}

/// Host configuration copied into the private HOME, as `runtime.yaml`
/// writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeSourceConfig {
    /// An absolute host path, or one that starts with `~`; the controller
    /// expands `~`.
    pub from: String,
    /// The target, relative to the private HOME.
    pub to: String,
}

impl TaskImageConfig {
    /// Every problem with the definition, empty when it is usable. Each one
    /// names its field.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(error) = ImageReference::parse(&self.base.reference) {
            problems.push(format!("task_image.base: {error}"));
        }
        let mut names = BTreeSet::new();
        for (index, component) in self.components.iter().enumerate() {
            let field = format!("task_image.components[{index}]");
            if !is_component_name(&component.name) {
                problems.push(format!(
                    "{field}.name: '{}' must match [a-z0-9][a-z0-9-]*",
                    component.name
                ));
            } else if !names.insert(component.name.as_str()) {
                problems.push(format!(
                    "{field}.name: '{}' is listed twice",
                    component.name
                ));
            }
            let scripts = std::iter::once(("install", Some(&component.install)))
                .chain(std::iter::once(("version", component.version.as_ref())));
            for (script_field, script) in scripts {
                if let Some(problem) = script.and_then(ScriptSource::problem) {
                    problems.push(format!("{field}.{script_field}: {problem}"));
                }
            }
            for (secret_index, secret) in component.secrets.iter().enumerate() {
                let field = format!("{field}.secrets[{secret_index}]");
                if !is_reference_name(&secret.name) {
                    problems.push(format!(
                        "{field}.name: '{}' is not a usable name",
                        secret.name
                    ));
                }
                let problem = secret.delivery().map_or_else(Some, |delivery| {
                    delivery.problem().or_else(|| match &delivery {
                        SecretDelivery::Env(name) => reserved_env_problem(name),
                        SecretDelivery::File(_) => None,
                    })
                });
                if let Some(problem) = problem {
                    problems.push(format!("{field}: {problem}"));
                }
            }
            for (home_index, home) in component.home.iter().enumerate() {
                let field = format!("{field}.home[{home_index}]");
                if let Some(problem) = home_from_problem(&home.from) {
                    problems.push(format!("{field}.from: {problem}"));
                }
                if !is_relative_inside(&home.to) {
                    problems.push(format!(
                        "{field}.to: '{}' must be a relative path inside the private HOME, \
                         without '.' or '..' components",
                        home.to
                    ));
                }
            }
        }
        problems
    }

    /// The definition as a task seals it, with the text of every script
    /// read from under `catalog_root`. A script outside the catalogue
    /// directory (through a symbolic link, too) is refused. Call it on a
    /// definition without [`Self::problems`].
    pub fn seal(&self, catalog_root: &Path) -> Result<TaskImage, Vec<String>> {
        let mut problems = Vec::new();
        let mut components = Vec::new();
        for (index, component) in self.components.iter().enumerate() {
            let field = format!("task_image.components[{index}]");
            let mut load = |script_field: &str, script: &ScriptSource| {
                script
                    .text(catalog_root)
                    .map_err(|problem| problems.push(format!("{field}.{script_field}: {problem}")))
                    .ok()
            };
            let install = load("install", &component.install);
            // `None` when the script failed to load; `Some(None)` when the
            // component has no version script.
            let version = match &component.version {
                Some(script) => load("version", script).map(Some),
                None => Some(None),
            };
            let secrets = component
                .secrets
                .iter()
                .filter_map(|secret| {
                    Some(ComponentSecret {
                        name: secret.name.clone(),
                        delivery: secret.delivery().ok()?,
                    })
                })
                .collect();
            let home = component
                .home
                .iter()
                .map(|home| HomeSource {
                    from: home.from.clone(),
                    to: home.to.clone(),
                })
                .collect();
            if let (Some(install), Some(version)) = (install, version) {
                components.push(TaskImageComponent {
                    name: component.name.clone(),
                    install,
                    version,
                    secrets,
                    home,
                });
            }
        }
        if problems.is_empty() {
            Ok(TaskImage {
                base: self.base.clone(),
                components,
            })
        } else {
            Err(problems)
        }
    }
}

impl ScriptSource {
    fn problem(&self) -> Option<String> {
        match self {
            ScriptSource::Path(path) if !is_relative_inside(path) => Some(format!(
                "script path '{path}' must be relative to the catalogue, without '.' or '..' \
                 components"
            )),
            ScriptSource::Inline { inline } if inline.trim().is_empty() => {
                Some("the inline script is blank".to_string())
            }
            _ => None,
        }
    }

    fn text(&self, catalog_root: &Path) -> Result<String, String> {
        let path = match self {
            ScriptSource::Inline { inline } => return Ok(inline.clone()),
            ScriptSource::Path(path) => path,
        };
        let unreadable = |error: std::io::Error| format!("script '{path}' cannot be read: {error}");
        let root = catalog_root.canonicalize().map_err(unreadable)?;
        let file = root.join(path).canonicalize().map_err(unreadable)?;
        if !file.starts_with(&root) {
            return Err(format!(
                "script '{path}' lies outside the catalogue directory"
            ));
        }
        std::fs::read_to_string(&file).map_err(unreadable)
    }
}

fn is_component_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Whether `path` is a non-empty relative path made only of normal
/// components.
fn is_relative_inside(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Why a home source's host path cannot be used, if it cannot. It must be
/// absolute or start with `~`, and must name neither a filesystem root nor
/// the Docker socket: either would hand the task the whole host.
fn home_from_problem(from: &str) -> Option<String> {
    let path = Path::new(from);
    if !(from.starts_with('/') || from == "~" || from.starts_with("~/")) {
        return Some(format!(
            "'{from}' must be an absolute path or start with '~/'"
        ));
    }
    if from.chars().any(char::is_control)
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Some(format!("'{from}' must not contain '.' or '..' components"));
    }
    if !path
        .components()
        .any(|part| matches!(part, Component::Normal(_)))
    {
        return Some(format!("'{from}' is a filesystem root"));
    }
    if path.file_name().is_some_and(|name| name == "docker.sock") {
        return Some(format!("'{from}' is the Docker socket"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(yaml: &str) -> RuntimeCatalog {
        serde_yaml::from_str(yaml).expect("runtime.yaml parses")
    }

    const VALID_COMPONENT: &str = concat!(
        "    - name: glab\n",
        "      install: components/glab.sh\n",
        "      version: { inline: \"glab --version\" }\n",
        "      secrets: [{ name: gitlab-token, env: GITLAB_TOKEN }, { name: netrc, file: .netrc }]\n",
        "      home: [{ from: ~/.config/glab-cli, to: .config/glab-cli }]\n",
    );

    #[test]
    fn a_runtime_with_only_a_task_image_is_valid() {
        let runtime = runtime(&format!(
            "task_image:\n  base: debian:trixie-slim\n  components:\n{VALID_COMPONENT}"
        ));
        assert!(runtime.bundle.is_none());
        assert_eq!(runtime.problems(), Vec::<String>::new());
    }

    #[test]
    fn an_unusable_task_image_is_refused_with_the_field_it_names() {
        let cases = [
            (
                "{ name: one, install: ../outside.sh }",
                "components[0].install",
            ),
            (
                "{ name: one, install: /etc/profile }",
                "components[0].install",
            ),
            ("{ name: Glab, install: a.sh }", "components[0].name"),
            (
                "{ name: one, install: a.sh, secrets: [{ name: t, env: PATH }] }",
                "components[0].secrets[0]",
            ),
            (
                "{ name: one, install: a.sh, secrets: [{ name: t, env: LD_PRELOAD }] }",
                "components[0].secrets[0]",
            ),
            (
                "{ name: one, install: a.sh, secrets: [{ name: t, env: NARSOR_TOKEN }] }",
                "components[0].secrets[0]",
            ),
            (
                "{ name: one, install: a.sh, secrets: [{ name: t, env: ANTHROPIC_API_KEY }] }",
                "components[0].secrets[0]",
            ),
            (
                "{ name: one, install: a.sh, secrets: [{ name: t, env: 1X }] }",
                "components[0].secrets[0]",
            ),
            (
                "{ name: one, install: a.sh, secrets: [{ name: t, file: ../x }] }",
                "components[0].secrets[0]",
            ),
            (
                "{ name: one, install: a.sh, secrets: [{ name: t }] }",
                "components[0].secrets[0]",
            ),
            (
                "{ name: one, install: a.sh, home: [{ from: ~/.x, to: ../x }] }",
                "components[0].home[0].to",
            ),
            (
                "{ name: one, install: a.sh, home: [{ from: /, to: x }] }",
                "components[0].home[0].from",
            ),
            (
                "{ name: one, install: a.sh, home: [{ from: /var/run/docker.sock, to: x }] }",
                "components[0].home[0].from",
            ),
            (
                "{ name: one, install: a.sh, home: [{ from: relative/x, to: x }] }",
                "components[0].home[0].from",
            ),
        ];
        for (component, field) in cases {
            let problems = runtime(&format!(
                "task_image:\n  base: debian:trixie-slim\n  components:\n    - {component}\n"
            ))
            .problems();
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.starts_with(&format!("task_image.{field}"))),
                "{component}: {problems:?}"
            );
        }

        let duplicate = runtime(
            "task_image:\n  base: debian:trixie-slim\n  components:\n    \
             - { name: one, install: a.sh }\n    - { name: one, install: b.sh }\n",
        );
        assert_eq!(
            duplicate.problems(),
            vec!["task_image.components[1].name: 'one' is listed twice".to_string()]
        );
    }
}
