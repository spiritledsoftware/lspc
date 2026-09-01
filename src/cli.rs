use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};

use clap::{
    Arg, ArgAction, ArgMatches, Command, ValueHint,
    builder::{PossibleValuesParser, ValueParser},
    error::ErrorKind,
    parser::ValueSource,
};
use serde::Serialize;
use serde_json::Value;

use crate::contract::{
    INPUT_ERROR_EXIT_CODE, INTERNAL_ERROR_EXIT_CODE, contract_catalog, failure_envelope,
    internal_error_envelope, invalid_arguments_envelope, schema_success_envelope,
    version_success_envelope,
};

pub(crate) struct ParsedInvocation {
    pub(crate) command: Vec<String>,
    pub(crate) options: BTreeMap<String, Vec<OsString>>,
    pub(crate) positionals: Vec<OsString>,
}

impl ParsedInvocation {
    pub(crate) fn command_path(&self) -> &[String] {
        &self.command
    }

    pub(crate) fn option_string(&self, name: &str) -> Option<String> {
        self.options
            .get(name)
            .and_then(|values| values.first())
            .map(|value| value.to_string_lossy().into_owned())
    }

    pub(crate) fn option_strings(&self, name: &str) -> Option<Vec<String>> {
        self.options.get(name).map(|values| {
            values
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect()
        })
    }

    pub(crate) fn option_path(&self, name: &str) -> Option<std::path::PathBuf> {
        self.options
            .get(name)
            .and_then(|values| values.first())
            .map(std::path::PathBuf::from)
    }

    #[allow(dead_code)]
    pub(crate) fn option_paths(&self, name: &str) -> Vec<std::path::PathBuf> {
        self.options
            .get(name)
            .map(|values| values.iter().map(std::path::PathBuf::from).collect())
            .unwrap_or_default()
    }

    pub(crate) fn has_option(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    pub(crate) fn positional_string(&self, index: usize) -> Option<String> {
        self.positionals
            .get(index)
            .map(|value| value.to_string_lossy().into_owned())
    }

    pub(crate) fn positionals_strings(&self) -> Vec<String> {
        self.positionals
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }
}

/// Parses one CLI invocation and emits exactly one JSON envelope.
pub(crate) fn run_cli(arguments: Vec<OsString>) -> ExitCode {
    let arguments = normalize_help_and_version_aliases(arguments);
    let fallback_command = command_path_from_arguments(&arguments);
    let matches = match build_cli_command().try_get_matches_from(arguments) {
        Ok(matches) => matches,
        Err(error) => {
            let (problem_code, problem_message) = describe_clap_error(error.kind());
            return emit_envelope(
                &invalid_arguments_envelope(fallback_command, problem_code, problem_message),
                ExitCode::from(INPUT_ERROR_EXIT_CODE),
            );
        }
    };
    let invocation = match parse_invocation(matches) {
        Ok(invocation) => invocation,
        Err(message) => {
            return emit_envelope(
                &invalid_arguments_envelope(fallback_command, "conflict", message),
                ExitCode::from(INPUT_ERROR_EXIT_CODE),
            );
        }
    };

    dispatch_invocation(invocation)
}

fn dispatch_invocation(invocation: ParsedInvocation) -> ExitCode {
    match invocation.command.as_slice() {
        [command] if command == "version" => {
            emit_envelope(&version_success_envelope(), ExitCode::SUCCESS)
        }
        [command] if command == "schema" || command == "help" => {
            let subject = invocation
                .positionals
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let full = invocation.options.contains_key("--full");
            match schema_success_envelope(command, subject, full) {
                Ok(envelope) => emit_envelope(&envelope, ExitCode::SUCCESS),
                Err(message) => emit_envelope(
                    &invalid_arguments_envelope(vec![command.clone()], "invalid_value", message),
                    ExitCode::from(INPUT_ERROR_EXIT_CODE),
                ),
            }
        }
        [group, ..] if group == "trust" => {
            match crate::configuration::dispatch_trust_command(&invocation) {
                Ok(envelope) => emit_envelope(&envelope, ExitCode::SUCCESS),
                Err(failure) => {
                    let exit_code = failure.exit_code;
                    emit_envelope(
                        &failure_envelope(invocation.command, &failure),
                        ExitCode::from(exit_code),
                    )
                }
            }
        }
        [group, ..] if group == "session" => {
            match crate::session::dispatch_session_command(&invocation) {
                Ok(envelope) => emit_envelope(&envelope, ExitCode::SUCCESS),
                Err(failure) => {
                    let exit_code = failure.exit_code;
                    emit_envelope(
                        &failure_envelope(invocation.command, &failure),
                        ExitCode::from(exit_code),
                    )
                }
            }
        }
        [_] if crate::query::QueryCommand::from_path(invocation.command_path()).is_some() => {
            match crate::session::dispatch_owner_query_command(&invocation) {
                Ok(envelope) => emit_envelope(&envelope, ExitCode::SUCCESS),
                Err(failure) => {
                    let exit_code = failure.exit_code;
                    emit_envelope(
                        &failure_envelope(invocation.command, &failure),
                        ExitCode::from(exit_code),
                    )
                }
            }
        }
        [group, command] if group == "skill" && command == "install" => {
            match crate::skill_install::install(&invocation) {
                Ok(envelope) => emit_envelope(&envelope, ExitCode::SUCCESS),
                Err(failure) => {
                    let exit_code = failure.exit_code;
                    emit_envelope(
                        &failure_envelope(invocation.command, &failure),
                        ExitCode::from(exit_code),
                    )
                }
            }
        }
        [group, ..] if matches!(group.as_str(), "preview" | "recovery" | "receipt" | "state") => {
            dispatch_mutation(invocation)
        }
        [command] if command == "apply" => dispatch_mutation(invocation),
        _ => emit_envelope(
            &internal_error_envelope(invocation.command, "implementation_pending"),
            ExitCode::from(INTERNAL_ERROR_EXIT_CODE),
        ),
    }
}

fn dispatch_mutation(invocation: ParsedInvocation) -> ExitCode {
    match crate::mutation::dispatch_mutation_command(&invocation) {
        Ok(envelope) => emit_envelope(&envelope, ExitCode::SUCCESS),
        Err(failure) => {
            let exit_code = failure.exit_code;
            emit_envelope(
                &failure_envelope(invocation.command, &failure),
                ExitCode::from(exit_code),
            )
        }
    }
}

fn build_cli_command() -> Command {
    let catalog = contract_catalog();
    let mut root = base_command("lspc");
    let mut paths = BTreeMap::<String, Vec<&Value>>::new();
    for command in catalog["commands"].as_array().unwrap() {
        let path = command["path"].as_array().unwrap();
        paths
            .entry(path[0].as_str().unwrap().to_owned())
            .or_default()
            .push(command);
    }

    for (name, commands) in paths {
        if commands.len() == 1 && commands[0]["path"].as_array().unwrap().len() == 1 {
            root = root.subcommand(build_leaf_command(commands[0], catalog));
        } else {
            let mut group = base_command(name);
            for command in commands {
                group = group.subcommand(build_leaf_command(command, catalog));
            }
            root = root.subcommand(group.subcommand_required(true));
        }
    }
    root
}

fn base_command(name: impl Into<clap::builder::Str>) -> Command {
    Command::new(name)
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .disable_version_flag(true)
        .disable_colored_help(true)
}

fn build_leaf_command(command: &Value, catalog: &Value) -> Command {
    let path = command["path"].as_array().unwrap();
    let name = path.last().unwrap().as_str().unwrap().to_owned();
    let mut leaf = base_command(name);
    let required = command["requiredFlags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();

    for flag in command_flag_names(command, catalog) {
        let definition = &catalog["options"][flag];
        let id = flag.to_owned();
        let mut argument = Arg::new(id.clone())
            .long(flag.trim_start_matches("--").to_owned())
            .required(required.contains(flag));
        if definition["type"] == "boolean-flag" {
            argument = argument.action(ArgAction::SetTrue);
        } else {
            argument = argument
                .action(if definition["repeatable"] == true {
                    ArgAction::Append
                } else {
                    ArgAction::Set
                })
                .value_name(definition["value"].as_str().unwrap_or("VALUE").to_owned())
                .value_parser(option_value_parser(definition, catalog));
            if definition["type"].as_str().is_some_and(|kind| {
                kind.contains("file") || kind == "native-path" || kind == "json-file"
            }) {
                argument = argument.value_hint(ValueHint::FilePath);
            }
        }
        for peer in definition["exclusiveWith"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if command_uses_flag(command, catalog, peer) {
                argument = argument.conflicts_with(peer.to_owned());
            }
        }
        leaf = leaf.arg(argument);
    }

    if let Some(positional) = command["positionals"]
        .as_array()
        .and_then(|items| items.first())
    {
        let mut argument = Arg::new("$positionals")
            .value_name(positional["name"].as_str().unwrap().to_owned())
            .required(positional["required"] == true)
            .value_parser(positional_value_parser(
                positional["type"].as_str().unwrap(),
            ));
        if positional["repeatable"] == true {
            argument = argument.num_args(1..).action(ArgAction::Append);
        } else {
            argument = argument.index(1);
        }
        leaf = leaf.arg(argument);
    }

    leaf
}

fn option_value_parser(definition: &Value, catalog: &Value) -> ValueParser {
    match definition["type"].as_str().unwrap() {
        "boolean" => PossibleValuesParser::new(["true", "false"]).into(),
        "enum" => PossibleValuesParser::new(
            catalog["enums"][definition["enum"].as_str().unwrap()]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned()),
        )
        .into(),
        "integer" => {
            let minimum = definition["minimum"].as_u64().unwrap_or(0);
            let maximum = definition["maximum"].as_u64().unwrap_or(u64::MAX);
            ValueParser::new(move |value: &str| {
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|value| (minimum..=maximum).contains(value))
                    .map(|_| value.to_owned())
                    .ok_or_else(|| "value is outside the allowed integer range".to_owned())
            })
        }
        "duration" => ValueParser::new(|value: &str| {
            valid_duration(value)
                .then(|| value.to_owned())
                .ok_or_else(|| "value must be a positive duration ending in ms, s, or m".to_owned())
        }),
        "server-name" => ValueParser::new(|value: &str| {
            valid_server_name(value)
                .then(|| value.to_owned())
                .ok_or_else(|| "value is not a valid server name".to_owned())
        }),
        "session-identity" => prefixed_hex_parser("sid_", 64),
        "sha256-digest" => prefixed_hex_parser("sha256:", 64),
        "json-value" | "json-object" | "json-array" => {
            let expected = definition["type"].as_str().unwrap().to_owned();
            ValueParser::new(move |value: &str| {
                let parsed: Value = serde_json::from_str(value)
                    .map_err(|_| "value is not valid JSON".to_owned())?;
                if (expected == "json-object" && !parsed.is_object())
                    || (expected == "json-array" && !parsed.is_array())
                {
                    return Err(format!("value must be a JSON {}", &expected[5..]));
                }
                Ok(value.to_owned())
            })
        }
        "environment-entry" => ValueParser::new(|value: &str| {
            value
                .split_once('=')
                .filter(|(name, _)| !name.is_empty())
                .map(|_| value.to_owned())
                .ok_or_else(|| "value must use KEY=VALUE syntax".to_owned())
        }),
        "nonempty-string" | "lsp-method" | "native-path" | "json-file" | "json-object-file"
        | "json-array-file" => ValueParser::new(|value: &str| {
            (!value.is_empty())
                .then(|| value.to_owned())
                .ok_or_else(|| "value must not be empty".to_owned())
        }),
        _ => ValueParser::string(),
    }
}

fn positional_value_parser(kind: &str) -> ValueParser {
    match kind {
        "generation-id" => prefixed_hex_parser("gen_", 32),
        "preview-id" => prefixed_hex_parser("prv_", 32),
        "transaction-id" => prefixed_hex_parser("txn_", 32),
        "receipt-id" => one_of_prefixed_hex_parser(&["prv_", "rcp_"]),
        "state-id" => one_of_prefixed_hex_parser(&["prv_", "rcp_", "txn_"]),
        _ => ValueParser::new(|value: &str| {
            (!value.is_empty())
                .then(|| value.to_owned())
                .ok_or_else(|| "value must not be empty".to_owned())
        }),
    }
}

fn prefixed_hex_parser(prefix: &'static str, digits: usize) -> ValueParser {
    ValueParser::new(move |value: &str| {
        value
            .strip_prefix(prefix)
            .filter(|hex| {
                hex.len() == digits
                    && hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .map(|_| value.to_owned())
            .ok_or_else(|| {
                format!("value must be {prefix} followed by {digits} lowercase hex digits")
            })
    })
}

fn one_of_prefixed_hex_parser(prefixes: &'static [&'static str]) -> ValueParser {
    ValueParser::new(move |value: &str| {
        prefixes
            .iter()
            .find_map(|prefix| value.strip_prefix(prefix))
            .filter(|hex| {
                hex.len() == 32
                    && hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .map(|_| value.to_owned())
            .ok_or_else(|| "value is not a recognized lspc state identifier".to_owned())
    })
}

fn valid_duration(value: &str) -> bool {
    let digits = value
        .strip_suffix("ms")
        .or_else(|| value.strip_suffix('s'))
        .or_else(|| value.strip_suffix('m'));
    digits.is_some_and(|digits| {
        digits
            .as_bytes()
            .first()
            .is_some_and(|byte| (b'1'..=b'9').contains(byte))
            && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn valid_server_name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        && value
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn command_flag_names<'a>(command: &'a Value, catalog: &'a Value) -> Vec<&'a str> {
    let mut flags = Vec::new();
    for option_set in command["optionSets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        flags.extend(
            catalog["optionSets"][option_set]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str),
        );
    }
    for key in ["requiredFlags", "optionalFlags"] {
        flags.extend(
            command[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
    }
    let mut seen = BTreeSet::new();
    flags.retain(|flag| seen.insert(*flag));
    flags
}

fn command_uses_flag(command: &Value, catalog: &Value, flag: &str) -> bool {
    command_flag_names(command, catalog).contains(&flag)
}

fn parse_invocation(matches: ArgMatches) -> Result<ParsedInvocation, &'static str> {
    let (command, leaf_matches) =
        selected_leaf(&matches).ok_or("A required command is missing.")?;
    let definition = contract_catalog()["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| {
            candidate["path"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .eq(command.iter().map(String::as_str))
        })
        .unwrap();
    validate_argument_rules(definition, leaf_matches)?;

    let mut options = BTreeMap::new();
    for flag in command_flag_names(definition, contract_catalog()) {
        if leaf_matches.value_source(flag) != Some(ValueSource::CommandLine) {
            continue;
        }
        if contract_catalog()["options"][flag]["type"] == "boolean-flag" {
            options.insert(flag.to_owned(), Vec::new());
        } else if let Some(values) = leaf_matches.get_raw(flag) {
            options.insert(flag.to_owned(), values.map(OsString::from).collect());
        }
    }
    let positionals = leaf_matches
        .try_get_raw("$positionals")
        .ok()
        .flatten()
        .map(|values| values.map(OsString::from).collect())
        .unwrap_or_default();
    Ok(ParsedInvocation {
        command,
        options,
        positionals,
    })
}

fn selected_leaf(matches: &ArgMatches) -> Option<(Vec<String>, &ArgMatches)> {
    let (first, first_matches) = matches.subcommand()?;
    if let Some((second, second_matches)) = first_matches.subcommand() {
        Some((vec![first.to_owned(), second.to_owned()], second_matches))
    } else {
        Some((vec![first.to_owned()], first_matches))
    }
}

fn validate_argument_rules(definition: &Value, matches: &ArgMatches) -> Result<(), &'static str> {
    for rule in definition["argumentRules"].as_array().into_iter().flatten() {
        if let Some(flags) = rule["exactlyOne"].as_array() {
            if flags
                .iter()
                .filter(|flag| argument_present(matches, flag.as_str().unwrap()))
                .count()
                != 1
            {
                return Err("Exactly one argument from the required set must be supplied.");
            }
        } else if let Some(families) = rule["exactlyOneFamily"].as_array() {
            if families
                .iter()
                .filter(|family| {
                    family
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|flag| argument_present(matches, flag.as_str().unwrap()))
                })
                .count()
                != 1
            {
                return Err("Exactly one argument family must be supplied.");
            }
        } else if let Some(requirement) = rule.get("requiresAny") {
            let triggered = requirement["triggers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|flag| argument_present(matches, flag.as_str().unwrap()));
            let satisfied = requirement["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|flag| argument_present(matches, flag.as_str().unwrap()));
            if triggered && !satisfied {
                return Err("An argument is missing a required companion argument.");
            }
        } else if let Some(flags) = rule["mutuallyExclusive"].as_array()
            && flags
                .iter()
                .all(|flag| argument_present(matches, flag.as_str().unwrap()))
        {
            return Err("Mutually exclusive arguments were supplied together.");
        }
    }
    Ok(())
}

fn argument_present(matches: &ArgMatches, id: &str) -> bool {
    if id == "$positionals" {
        matches.try_get_raw(id).ok().flatten().is_some()
    } else {
        matches.value_source(id) == Some(ValueSource::CommandLine)
    }
}

fn normalize_help_and_version_aliases(mut arguments: Vec<OsString>) -> Vec<OsString> {
    if arguments.len() == 2 && arguments[1] == "--version" {
        arguments[1] = "version".into();
        return arguments;
    }
    if arguments.len() == 2 && arguments[1] == "--help" {
        arguments[1] = "help".into();
        return arguments;
    }
    if arguments.len() == 3 && arguments[2] == "--help" {
        let group = arguments[1].to_string_lossy();
        let is_group = contract_catalog()["schemaGroups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|subject| subject.as_array().unwrap()[0] == group.as_ref());
        if is_group {
            arguments[2] = arguments[1].clone();
            arguments[1] = "help".into();
        }
    }
    arguments
}

fn command_path_from_arguments(arguments: &[OsString]) -> Vec<String> {
    let known_paths = contract_catalog()["commands"].as_array().unwrap();
    let supplied = arguments
        .iter()
        .skip(1)
        .take_while(|argument| !argument.to_string_lossy().starts_with('-'))
        .take(2)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    known_paths
        .iter()
        .filter_map(|command| command["path"].as_array())
        .map(|path| path.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .find(|path| path.iter().copied().eq(supplied.iter().map(String::as_str)))
        .map(|path| path.into_iter().map(str::to_owned).collect())
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_catalog_builds_every_command_path() {
        let command = build_cli_command();
        command.clone().debug_assert();
        for definition in contract_catalog()["commands"].as_array().unwrap() {
            let path = definition["path"]
                .as_array()
                .unwrap()
                .iter()
                .map(|segment| segment.as_str().unwrap())
                .collect::<Vec<_>>();
            let mut arguments = vec!["lspc"];
            arguments.extend(path);
            for flag in definition["requiredFlags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                push_test_flag(&mut arguments, flag);
            }
            let has_required_positional = definition["positionals"]
                .as_array()
                .and_then(|items| items.first())
                .is_some_and(|positional| positional["required"] == true);
            if has_required_positional {
                arguments.push(valid_test_positional(definition));
            }
            for rule in definition["argumentRules"].as_array().into_iter().flatten() {
                let selected = rule["exactlyOne"]
                    .as_array()
                    .and_then(|flags| flags.first())
                    .or_else(|| {
                        rule["exactlyOneFamily"]
                            .as_array()
                            .and_then(|families| families.first())
                            .and_then(Value::as_array)
                            .and_then(|flags| flags.first())
                    })
                    .and_then(Value::as_str);
                if let Some(flag) = selected {
                    if flag == "$positionals" {
                        if !has_required_positional {
                            arguments.push(valid_test_positional(definition));
                        }
                    } else if !arguments.contains(&flag) {
                        push_test_flag(&mut arguments, flag);
                    }
                }
            }
            let matches = command
                .clone()
                .try_get_matches_from(&arguments)
                .unwrap_or_else(|error| panic!("{}: {error}", arguments.join(" ")));
            parse_invocation(matches)
                .unwrap_or_else(|error| panic!("{}: {error}", arguments.join(" ")));
        }
    }

    fn push_test_flag(arguments: &mut Vec<&'static str>, flag: &'static str) {
        arguments.push(flag);
        if contract_catalog()["options"][flag]["type"] != "boolean-flag" {
            arguments.push(valid_test_value(flag));
        }
    }

    fn valid_test_positional(definition: &Value) -> &'static str {
        match definition["positionals"].as_array().unwrap()[0]["type"]
            .as_str()
            .unwrap()
        {
            "generation-id" => "gen_00000000000000000000000000000000",
            "preview-id" => "prv_00000000000000000000000000000000",
            "receipt-id" => "rcp_00000000000000000000000000000000",
            "transaction-id" => "txn_00000000000000000000000000000000",
            "state-id" => "prv_00000000000000000000000000000000",
            _ => "test-value",
        }
    }

    fn valid_test_value(flag: &str) -> &'static str {
        match flag {
            "--line" | "--column" | "--start-line" | "--start-column" | "--end-line"
            | "--end-column" => "0",
            "--include-declaration" => "false",
            "--position-encoding" => "utf-16",
            "--options-json"
            | "--context-json"
            | "--action-json"
            | "--code-action-json"
            | "--workspace-edit-json"
            | "--command-json"
            | "--server-settings-json" => "{}",
            "--arguments-json" => "[]",
            "--params-json" | "--initialization-options-json" => "null",
            "--digest" | "--manifest-digest" => {
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            }
            "--session-identity" => {
                "sid_0000000000000000000000000000000000000000000000000000000000000000"
            }
            _ => "test-value",
        }
    }
}
