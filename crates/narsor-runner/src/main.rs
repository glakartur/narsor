use clap::Parser;
use narsor::runner::cli::{run_command, RunnerArgs};

#[derive(Debug, Parser)]
#[command(name = "narsor-runner", about = "Run one task workload")]
struct Args {
    #[command(flatten)]
    runner: RunnerArgs,
}

#[tokio::main]
async fn main() {
    let exit = run_command(Args::parse().runner).await;
    std::process::exit(exit);
}
