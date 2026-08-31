use serde::Serialize;

pub(crate) const INPUT_ERROR_EXIT_CODE: u8 = 2;
pub(crate) const INTERNAL_ERROR_EXIT_CODE: u8 = 8;

/// Returns the version command's stable machine-readable success envelope.
pub(crate) fn version_success_envelope() -> VersionSuccessEnvelope {
    VersionSuccessEnvelope {
        schema_version: 1,
        ok: true,
        command: ["version"],
        result: VersionResult {
            name: "lspc",
            version: env!("CARGO_PKG_VERSION"),
            contract_version: 1,
            config_version: 1,
            capability_profile_version: 1,
            owner_protocol_version: 1,
            target: env!("LSPC_BUILD_TARGET"),
            commit: env!("LSPC_BUILD_COMMIT"),
        },
    }
}

/// Returns an `invalid_arguments` failure without Clap's prose output.
pub(crate) fn invalid_arguments_envelope(
    command: Vec<String>,
    problem_code: &'static str,
    problem_message: &'static str,
) -> InvalidArgumentsEnvelope {
    InvalidArgumentsEnvelope {
        schema_version: 1,
        ok: false,
        command,
        error: InvalidArgumentsError {
            category: "input",
            code: "invalid_arguments",
            message: "The command line is invalid.",
            stage: "parse_cli",
            delivery: "not_applicable",
            retry: "never",
            data: InvalidArgumentsData {
                problems: vec![InputProblem {
                    code: problem_code,
                    message: problem_message,
                }],
            },
        },
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionSuccessEnvelope {
    schema_version: u8,
    ok: bool,
    command: [&'static str; 1],
    result: VersionResult,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionResult {
    name: &'static str,
    version: &'static str,
    contract_version: u8,
    config_version: u8,
    capability_profile_version: u8,
    owner_protocol_version: u8,
    target: &'static str,
    commit: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InvalidArgumentsEnvelope {
    schema_version: u8,
    ok: bool,
    command: Vec<String>,
    error: InvalidArgumentsError,
}

#[derive(Serialize)]
struct InvalidArgumentsError {
    category: &'static str,
    code: &'static str,
    message: &'static str,
    stage: &'static str,
    delivery: &'static str,
    retry: &'static str,
    data: InvalidArgumentsData,
}

#[derive(Serialize)]
struct InvalidArgumentsData {
    problems: Vec<InputProblem>,
}

#[derive(Serialize)]
struct InputProblem {
    code: &'static str,
    message: &'static str,
}
