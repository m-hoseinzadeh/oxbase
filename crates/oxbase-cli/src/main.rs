//! Headless runner: the same core as the desktop app with no UI. Runs jobs and scheduled tasks
//! (invoked by OS cron / Task Scheduler).

use clap::Parser;

/// oxbase command-line runner.
#[derive(Debug, Parser)]
#[command(name = "oxbase", version, about, long_about = None)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    println!("oxbase {}", env!("CARGO_PKG_VERSION"));
}
