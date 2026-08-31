#[allow(dead_code)]
mod canonical_value;
mod cli;
#[allow(dead_code, unused_imports)]
mod configuration;
mod contract;
mod session;
mod skill_install;
#[allow(dead_code, unused_imports)]
mod workspace;

use std::{env, process::ExitCode};

fn main() -> ExitCode {
    cli::run_cli(env::args_os().collect())
}
