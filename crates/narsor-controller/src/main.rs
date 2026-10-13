use clap::Parser;
use narsor_controller::controller::{run_command, ControllerArgs};

#[derive(Debug, Parser)]
#[command(name = "narsor-controller", about = "Reconcile one Narsor backend")]
struct Args {
    #[command(flatten)]
    controller: ControllerArgs,
}

#[tokio::main]
async fn main() {
    std::process::exit(run_command(Args::parse().controller).await);
}
