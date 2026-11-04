//! The `origin` binary.

use std::process::ExitCode;

use clap::Parser;
use origin_src::cli::Cli;
use origin_src::output;

fn main() -> ExitCode {
    output::exit_quietly_on_broken_pipe();
    match Cli::parse().run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
