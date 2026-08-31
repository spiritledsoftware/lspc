use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};

use clap::{Parser, Subcommand, error::ErrorKind};
use serde::Serialize;

use crate::contract::{
    INPUT_ERROR_EXIT_CODE, INTERNAL_ERROR_EXIT_CODE, invalid_arguments_envelope,
    version_success_envelope,
};

#[derive(Parser)]
#[command(name = "lspc", disable_help_flag = true, disable_version_flag = true)]
struct CliArguments {
    #[arg(long, exclusive = true)]
    version: bool,

    #[command(subcommand)]
    command: Option<PublicCommand>,
}

#[derive(Subcommand)]
enum PublicCommand {
    Version,
}

/// Parses one CLI invocation and emits exactly one JSON envelope.
pub(crate) fn run_cli(arguments: Vec<OsString>) -> ExitCode {
    let command_path = command_path_from_arguments(&arguments);

    match CliArguments::try_parse_from(arguments) {
        Ok(CliArguments { version: true, .. })
        | Ok(CliArguments {
            command: Some(PublicCommand::Version),
            ..
        }) => emit_envelope(&version_success_envelope(), ExitCode::SUCCESS),
        Ok(_) => emit_envelope(
            &invalid_arguments_envelope(command_path, "missing", "A required command is missing."),
            ExitCode::from(INPUT_ERROR_EXIT_CODE),
        ),
        Err(error) => {
            let (problem_code, problem_message) = describe_clap_error(error.kind());
            emit_envelope(
                &invalid_arguments_envelope(command_path, problem_code, problem_message),
                ExitCode::from(INPUT_ERROR_EXIT_CODE),
            )
        }
    }
}

fn command_path_from_arguments(arguments: &[OsString]) -> Vec<String> {
    match arguments.get(1).and_then(|argument| argument.to_str()) {
        Some("version" | "--version") => vec!["version".to_owned()],
        _ => Vec::new(),
    }
}

fn describe_clap_error(kind: ErrorKind) -> (&'static str, &'static str) {
    match kind {
        ErrorKind::MissingRequiredArgument | ErrorKind::MissingSubcommand => {
            ("missing", "A required argument or command is missing.")
        }
        ErrorKind::ArgumentConflict => ("conflict", "Arguments cannot be used together."),
        ErrorKind::InvalidValue
        | ErrorKind::ValueValidation
        | ErrorKind::TooManyValues
        | ErrorKind::TooFewValues
        | ErrorKind::WrongNumberOfValues => ("invalid_value", "An argument has an invalid value."),
        ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand => {
            ("invalid_value", "An argument or command is not recognized.")
        }
        _ => ("invalid_value", "The command line is invalid."),
    }
}

fn emit_envelope(envelope: &impl Serialize, success: ExitCode) -> ExitCode {
    let mut bytes = match serde_json::to_vec(envelope) {
        Ok(bytes) => bytes,
        Err(_) => return serialization_failure(),
    };
    bytes.push(b'\n');

    if io::stdout().lock().write_all(&bytes).is_err() {
        return serialization_failure();
    }

    success
}

fn serialization_failure() -> ExitCode {
    let _ = writeln!(
        io::stderr().lock(),
        "lspc: failed to serialize the result envelope"
    );
    ExitCode::from(INTERNAL_ERROR_EXIT_CODE)
}
