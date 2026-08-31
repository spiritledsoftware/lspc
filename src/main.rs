mod cli;
mod contract;

use std::{env, process::ExitCode};

fn main() -> ExitCode {
    cli::run_cli(env::args_os().collect())
}
