use clap::Parser;
use narsor_controller::controller::{
    edit_profile_command, run_command, ControllerArgs, EditProfileArgs,
};

/// The subcommand of [`Edit`]; anything else runs the controller.
const EDIT_CLAUDE_PROFILE: &str = "edit-claude-profile";

#[derive(Debug, Parser)]
#[command(
    name = "narsor-controller",
    about = "Reconcile one Narsor backend",
    after_help = "Run `narsor-controller edit-claude-profile --help` to edit the Claude profile Secret."
)]
struct Args {
    #[command(flatten)]
    controller: ControllerArgs,
}

// A separate parser rather than a clap subcommand: the controller takes its
// required arguments from the environment, which an optional flattened
// group next to a subcommand would not see.
#[derive(Debug, Parser)]
#[command(
    name = "narsor-controller edit-claude-profile",
    about = "Edit the Claude profile Secret in Claude Code, then save it"
)]
struct Edit {
    #[command(flatten)]
    edit: EditProfileArgs,
}

#[tokio::main]
async fn main() {
    let edit = std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == EDIT_CLAUDE_PROFILE);
    let code = if edit {
        edit_profile_command(Edit::parse_from(std::env::args_os().skip(1)).edit).await
    } else {
        run_command(Args::parse().controller).await
    };
    std::process::exit(code);
}
