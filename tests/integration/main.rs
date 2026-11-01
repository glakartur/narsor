//! The single integration-test crate of the `narsor` package.
//!
//! Every file in this directory is a module of this crate, so a new test file
//! only runs once it is declared here. Modules run concurrently in one
//! process: they must not share process-global state (`set_var`,
//! `set_current_dir`, fixed ports) and must use temporary directories and
//! ephemeral ports. Tests that spawn the built binaries or `cargo` do not
//! belong here.
//!
//! Shared helpers live in the `*_support` modules and are used as
//! `crate::<name>_support`.

#[macro_use]
mod backend_contract_support;
mod engine_support;
mod environment_support;
mod kubernetes_support;
mod remote_support;
mod task_app_support;

mod actions_agent;
mod actions_human;
mod actions_script;
mod agent_harness_selection;
mod api;
mod backend_contract;
mod builtin_workflows;
mod claude_code_classification;
mod cli_control_text;
mod cli_conversation_text;
mod cli_task_text;
mod conductor_chat;
mod conductor_failure;
mod conductor_invoke;
mod conductor_label;
mod conductor_store;
mod conductor_title;
mod config_cascade;
mod config_file;
mod config_resolve;
mod controller_protocol;
mod controller_supervision;
mod conversation_store;
mod docs_links;
mod engine_recovery;
mod engine_step_context;
mod environment;
mod kubernetes_adapter;
mod launch_spec_contract;
mod orchestration;
mod persistence;
mod registry;
mod schema_loading;
mod sse;
mod task_labels;
mod template;
mod timeline;
mod tui_background;
mod tui_conversation;
mod tui_form;
mod tui_panel;
mod tui_render;
mod tui_state;
mod tui_terminal_restore;
mod validation;
mod workflows;
