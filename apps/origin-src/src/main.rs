//! The `origin` binary.

use std::process::ExitCode;

use clap::Parser;
use origin_src::cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
