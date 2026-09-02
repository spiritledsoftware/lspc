use std::{collections::BTreeSet, sync::OnceLock};

use serde::Serialize;
use serde_json::{Map, Value, json};

pub(crate) const INPUT_ERROR_EXIT_CODE: u8 = 2;
pub(crate) const INTERNAL_ERROR_EXIT_CODE: u8 = 8;

/// A fully classified failure ready for the stable JSON envelope.
#[derive(Debug)]
pub(crate) struct ContractFailure {
    pub(crate) exit_code: u8,
    pub(crate) category: &'static str,
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) stage: &'static str,
    pub(crate) delivery: &'static str,
    pub(crate) retry: &'static str,
    pub(crate) data: Value,
}

/// Wraps one classified failure in the stable machine envelope.
pub(crate) fn failure_envelope(command: Vec<String>, failure: &ContractFailure) -> Value {
    json!({
        "schemaVersion": 1,
        "ok": false,
        "command": command,
        "error": {
            "category": failure.category,
            "code": failure.code,
            "message": failure.message,
            "stage": failure.stage,
            "delivery": failure.delivery,
            "retry": failure.retry,
            "data": failure.data
        }
    })
}

/// Returns the checked v1 command and machine-contract catalog.
pub(crate) fn contract_catalog() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/contract/catalog.json"))
            .expect("embedded contract catalog must be valid JSON")
    })
}

/// Returns the checked Draft 2020-12 schema registry.
pub(crate) fn contract_schemas() -> &'static Value {
    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/contract/schemas.json"))
            .expect("embedded contract schemas must be valid JSON")
    })
}

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

/// Returns an internal failure for a contract-valid command that has no handler yet.
pub(crate) fn internal_error_envelope(command: Vec<String>, incident_id: &str) -> Value {
    json!({
        "schemaVersion": 1,
        "ok": false,
        "command": command,
        "error": {
            "category": "internal",
            "code": "internal_error",
            "message": "lspc could not complete the command.",
            "stage": "dispatch",
            "delivery": "not_applicable",
            "retry": "never",
            "data": { "incidentId": incident_id }
        }
    })
}

/// Returns the offline schema or help result for one optional subject.
pub(crate) fn schema_success_envelope(
    command_name: &str,
    subject: Vec<String>,
    full: bool,
) -> Result<Value, &'static str> {
    if !subject.is_empty() && !known_schema_subject(&subject) {
        return Err("The schema subject is not recognized.");
    }

    let catalog = if full {
        contract_catalog().clone()
    } else if subject.is_empty() {
        compact_catalog_index()
    } else {
        focused_catalog(&subject)
    };
    let schemas = if full {
        contract_schemas().clone()
    } else if subject.is_empty() {
        Value::Object(Map::new())
    } else {
        focused_schemas(&subject)
    };
    let mut command = vec![command_name.to_owned()];
    command.extend(subject.iter().cloned());
    let mut result = json!({
        "binaryVersion": env!("CARGO_PKG_VERSION"),
        "contractVersion": 1,
        "configVersion": 1,
        "capabilityProfileVersion": 1,
        "ownerProtocolVersion": 1,
        "jsonSchemaDialect": "https://json-schema.org/draft/2020-12/schema",
        "catalog": catalog,
        "schemas": schemas
    });
    if !subject.is_empty() {
        result["subject"] = json!(subject);
    }
    if subject == ["config", "user"]
        && let Some(path) = user_config_path()
    {
        result["resolvedPath"] = Value::String(path);
    }

    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": command,
        "result": result
    }))
}

fn known_schema_subject(subject: &[String]) -> bool {
    contract_catalog()["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|command| command["path"].as_array())
        .chain(
            contract_catalog()["schemaSubjects"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_array),
        )
        .chain(
            contract_catalog()["schemaGroups"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_array),
        )
        .any(|candidate| {
            candidate
                .iter()
                .filter_map(Value::as_str)
                .eq(subject.iter().map(String::as_str))
        })
}

fn compact_catalog_index() -> Value {
    json!({
        "commands": contract_catalog()["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["path"].clone())
            .collect::<Vec<_>>(),
        "subjects": contract_catalog()["schemaSubjects"].clone(),
        "groups": contract_catalog()["schemaGroups"].clone()
    })
}

fn focused_catalog(subject: &[String]) -> Value {
    let matching_commands = contract_catalog()["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|command| {
            let path = command["path"].as_array().unwrap();
            path.iter()
                .filter_map(Value::as_str)
                .zip(subject.iter().map(String::as_str))
                .all(|(left, right)| left == right)
                && (path.len() == subject.len() || subject.len() == 1)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut options = Map::new();
    for command in &matching_commands {
        for option_set in command["optionSets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            for flag in contract_catalog()["optionSets"][option_set]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
            {
                options.insert(flag.to_owned(), contract_catalog()["options"][flag].clone());
            }
        }
        for key in ["requiredFlags", "optionalFlags"] {
            for flag in command[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                options.insert(flag.to_owned(), contract_catalog()["options"][flag].clone());
            }
        }
    }

    let mut catalog = Map::new();
    catalog.insert("commands".to_owned(), Value::Array(matching_commands));
    catalog.insert("options".to_owned(), Value::Object(options));
    catalog.insert("enums".to_owned(), contract_catalog()["enums"].clone());
    catalog.insert(
        "exitCodes".to_owned(),
        contract_catalog()["exitCodes"].clone(),
    );
    if subject.first().is_some_and(|segment| segment == "config") {
        catalog.insert("config".to_owned(), contract_catalog()["config"].clone());
    }
    if subject == ["errors"] {
        catalog.insert(
            "errorCodes".to_owned(),
            contract_catalog()["errorCodes"].clone(),
        );
    }
    Value::Object(catalog)
}

fn focused_schemas(subject: &[String]) -> Value {
    let schemas = contract_schemas().as_object().unwrap();
    let path = subject.join("/");
    let mut selected = BTreeSet::new();

    if subject.first().is_some_and(|segment| segment == "config") {
        selected.insert(format!("lspc://schema/v1/{path}"));
    } else if subject == ["output"] {
        selected.extend(
            schemas
                .keys()
                .filter(|uri| uri.contains("/output/") || uri.contains("/success/"))
                .cloned(),
        );
    } else if subject == ["errors"] {
        selected.extend(schemas.keys().filter(|uri| uri.contains("/error")).cloned());
    } else if subject == ["skill-install"] {
        selected.insert("lspc://schema/v1/skill-install/marker".to_owned());
    } else {
        for command in contract_catalog()["commands"].as_array().unwrap() {
            let command_path = command["path"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            if command_path.starts_with(&subject.iter().map(String::as_str).collect::<Vec<_>>()) {
                let command_path = command_path.join("/");
                selected.insert(format!("lspc://schema/v1/cli/{command_path}"));
                selected.insert(format!("lspc://schema/v1/command/{command_path}"));
                selected.insert(format!("lspc://schema/v1/command/{command_path}/success"));
                selected.insert(format!("lspc://schema/v1/command/{command_path}/failure"));
            }
        }
    }

    let mut pending = selected.iter().cloned().collect::<Vec<_>>();
    while let Some(uri) = pending.pop() {
        let Some(schema) = schemas.get(&uri) else {
            continue;
        };
        let mut references = Vec::new();
        collect_schema_references(schema, &mut references);
        for reference in references {
            if !reference.starts_with('#') && selected.insert(reference.clone()) {
                pending.push(reference);
            }
        }
    }

    Value::Object(
        selected
            .into_iter()
            .filter_map(|uri| schemas.get(&uri).cloned().map(|schema| (uri, schema)))
            .collect(),
    )
}

fn collect_schema_references(value: &Value, references: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                references.push(reference.to_owned());
            }
            for value in object.values() {
                collect_schema_references(value, references);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_schema_references(value, references);
            }
        }
        _ => {}
    }
}

fn user_config_path() -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = std::path::PathBuf::from(xdg);
        if !path.is_absolute() {
            return None;
        }
        return Some(path.join("lspc/config.toml").to_string_lossy().into_owned());
    }
    #[cfg(windows)]
    if let Some(app_data) = std::env::var_os("APPDATA") {
        let path = std::path::PathBuf::from(app_data);
        if !path.is_absolute() {
            return None;
        }
        return Some(path.join("lspc/config.toml").to_string_lossy().into_owned());
    }
    directories::ProjectDirs::from("", "", "lspc").map(|directories| {
        directories
            .config_dir()
            .join("config.toml")
            .to_string_lossy()
            .into_owned()
    })
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
