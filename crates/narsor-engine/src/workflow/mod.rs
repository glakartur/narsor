//! Workflow definition loading.
//!
//! `schema` mirrors the YAML contract from `docs/dev/workflow-schema.md` one to one,
//! `loader` compiles those structures into the domain model. Keeping the two
//! apart means the file format can evolve (the `version` field) without the
//! runtime types following along, and error messages can name YAML fields
//! instead of Rust fields.

pub mod loader;
pub mod schema;
pub mod template;
pub mod validator;

pub use loader::{compile, load_from_file, load_from_str, LoadError};
pub use schema::{parse_duration, DurationParseError, DurationSecs, WorkflowFile};
pub use template::{
    action_context_references, context_references, places_step_history, DeclaredNames, StepMemory,
    TemplateError, TemplateScope,
};
pub use validator::{validate, validate_path, validate_source};
