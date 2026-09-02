mod canonical_value;
mod cli;
mod configuration;
mod contract;
mod mutation;
mod query;
mod session;
mod skill_install;
mod state_permissions;
mod workspace;

use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "__owner")
    {
        session::run_hidden_owner(&arguments[1..])
    } else {
        cli::run_cli(arguments)
    }
}
