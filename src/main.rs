mod canonical_value;
mod cli;
mod configuration;
mod contract;
mod session;

use std::{env, process::ExitCode};

fn main() -> ExitCode {
    cli::run_cli(env::args_os().collect())
}
