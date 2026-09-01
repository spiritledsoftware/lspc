use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::workspace::{PositionEncoding, TextSynchronization};

pub(crate) const CAPABILITY_PROFILE_VERSION: u32 = 1;

pub(crate) struct NegotiatedCapabilities {
    pub(crate) initialize_result: Value,
    pub(crate) providers: BTreeMap<String, Value>,
    pub(crate) position_encoding: PositionEncoding,
    pub(crate) text_synchronization: TextSynchronization,
}

impl NegotiatedCapabilities {
    pub(crate) fn providers_json(&self) -> Value {
        Value::Object(self.providers.clone().into_iter().collect())
    }
}

pub(crate) fn fixed_initialize_capabilities() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/contract/initialize-capabilities.json"
    )))
    .expect("the checked capability profile is valid JSON")
}

pub(crate) fn normalize_initialize_result(
    initialize_result: Value,
) -> Result<NegotiatedCapabilities, String> {
    let capabilities = initialize_result
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| "InitializeResult.capabilities must be an object".to_owned())?;
    let position_encoding = match capabilities
        .get("positionEncoding")
        .and_then(|value| (!value.is_null()).then_some(value))
    {
        None => PositionEncoding::Utf16,
        Some(Value::String(value)) if value == "utf-8" => PositionEncoding::Utf8,
        Some(Value::String(value)) if value == "utf-16" => PositionEncoding::Utf16,
        Some(_) => return Err("The server selected an unoffered position encoding".to_owned()),
    };
    let text_synchronization = normalize_text_synchronization(capabilities)?;
    let mut providers = BTreeMap::new();
    for (name, path) in [
        ("hover", "hoverProvider"),
        ("definition", "definitionProvider"),
        ("references", "referencesProvider"),
        ("documentSymbols", "documentSymbolProvider"),
        ("workspaceSymbols", "workspaceSymbolProvider"),
        ("formatting", "documentFormattingProvider"),
        ("rename", "renameProvider"),
        ("codeActions", "codeActionProvider"),
        ("executeCommand", "executeCommandProvider"),
        ("diagnostics", "diagnosticProvider"),
    ] {
        providers.insert(
            name.to_owned(),
            normalize_provider(capabilities, name, path),
        );
    }
    Ok(NegotiatedCapabilities {
        initialize_result,
        providers,
        position_encoding,
        text_synchronization,
    })
}

fn normalize_text_synchronization(
    capabilities: &Map<String, Value>,
) -> Result<TextSynchronization, String> {
    match capabilities.get("textDocumentSync") {
        None | Some(Value::Null) => Ok(TextSynchronization::None),
        Some(Value::Number(value)) if value.as_u64() == Some(0) => Ok(TextSynchronization::None),
        Some(Value::Number(value)) if matches!(value.as_u64(), Some(1 | 2)) => {
            Ok(TextSynchronization::OpenClose)
        }
        Some(Value::Object(options)) => match options.get("openClose") {
            None | Some(Value::Bool(false)) | Some(Value::Null) => Ok(TextSynchronization::None),
            Some(Value::Bool(true)) => Ok(TextSynchronization::OpenClose),
            Some(_) => Err("textDocumentSync.openClose must be a boolean".to_owned()),
        },
        Some(_) => Err("textDocumentSync has an invalid core capability value".to_owned()),
    }
}

fn normalize_provider(capabilities: &Map<String, Value>, name: &str, path: &str) -> Value {
    let capability_path = format!("capabilities.{path}");
    match capabilities.get(path) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {
            json!({"state": "unsupported", "capabilityPath": capability_path})
        }
        Some(Value::Bool(true)) if !matches!(name, "executeCommand" | "diagnostics") => {
            json!({"state": "supported", "capabilityPath": capability_path, "options": {}})
        }
        Some(Value::Bool(true)) => json!({
            "state": "invalid",
            "capabilityPath": capability_path,
            "problems": [{
                "code": "malformed_capability",
                "message": "This provider capability requires an options object.",
                "jsonPath": capability_path
            }]
        }),
        Some(Value::Object(options)) => {
            let problems = provider_option_problems(name, options, &capability_path);
            if problems.is_empty() {
                json!({
                    "state": "supported",
                    "capabilityPath": capability_path,
                    "options": options
                })
            } else {
                json!({
                    "state": "invalid",
                    "capabilityPath": capability_path,
                    "options": options,
                    "problems": problems
                })
            }
        }
        Some(_) => json!({
            "state": "invalid",
            "capabilityPath": capability_path,
            "problems": [{
                "code": "malformed_capability",
                "message": "The optional provider capability is neither a boolean nor an options object.",
                "jsonPath": capability_path
            }]
        }),
    }
}

fn provider_option_problems(
    name: &str,
    options: &Map<String, Value>,
    capability_path: &str,
) -> Vec<Value> {
    let mut problems = Vec::new();
    check_optional_bool(options, "workDoneProgress", capability_path, &mut problems);
    match name {
        "rename" => check_optional_bool(options, "prepareProvider", capability_path, &mut problems),
        "workspaceSymbols" => {
            check_optional_bool(options, "resolveProvider", capability_path, &mut problems)
        }
        "codeActions" => {
            check_optional_bool(options, "resolveProvider", capability_path, &mut problems);
            check_optional_string_array(options, "codeActionKinds", capability_path, &mut problems);
        }
        "executeCommand" => {
            check_required_string_array(options, "commands", capability_path, &mut problems)
        }
        "diagnostics" => {
            check_required_bool(
                options,
                "interFileDependencies",
                capability_path,
                &mut problems,
            );
            check_required_bool(
                options,
                "workspaceDiagnostics",
                capability_path,
                &mut problems,
            );
            if options
                .get("identifier")
                .is_some_and(|identifier| !identifier.is_string())
            {
                problems.push(malformed_provider_option(
                    capability_path,
                    "identifier",
                    "a string",
                ));
            }
        }
        _ => {}
    }
    problems
}

fn check_optional_bool(
    options: &Map<String, Value>,
    field: &str,
    capability_path: &str,
    problems: &mut Vec<Value>,
) {
    if options.get(field).is_some_and(|value| !value.is_boolean()) {
        problems.push(malformed_provider_option(
            capability_path,
            field,
            "a boolean",
        ));
    }
}

fn check_required_bool(
    options: &Map<String, Value>,
    field: &str,
    capability_path: &str,
    problems: &mut Vec<Value>,
) {
    if !options.get(field).is_some_and(Value::is_boolean) {
        problems.push(malformed_provider_option(
            capability_path,
            field,
            "a required boolean",
        ));
    }
}

fn check_optional_string_array(
    options: &Map<String, Value>,
    field: &str,
    capability_path: &str,
    problems: &mut Vec<Value>,
) {
    if options.get(field).is_some_and(|value| {
        value
            .as_array()
            .is_none_or(|items| items.iter().any(|item| !item.is_string()))
    }) {
        problems.push(malformed_provider_option(
            capability_path,
            field,
            "an array of strings",
        ));
    }
}

fn check_required_string_array(
    options: &Map<String, Value>,
    field: &str,
    capability_path: &str,
    problems: &mut Vec<Value>,
) {
    if options.get(field).is_none_or(|value| {
        value
            .as_array()
            .is_none_or(|items| items.iter().any(|item| !item.is_string()))
    }) {
        problems.push(malformed_provider_option(
            capability_path,
            field,
            "a required array of strings",
        ));
    }
}

fn malformed_provider_option(capability_path: &str, field: &str, expected: &str) -> Value {
    json!({
        "code": "malformed_capability",
        "message": format!("The provider option must be {expected}."),
        "jsonPath": format!("{capability_path}.{field}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_position_encoding_and_isolates_optional_corruption() {
        let capabilities = normalize_initialize_result(json!({
            "capabilities": {
                "hoverProvider": 7,
                "definitionProvider": true,
                "executeCommandProvider": {"commands": ["ok", 7]},
                "diagnosticProvider": {
                    "interFileDependencies": false,
                    "workspaceDiagnostics": true
                }
            }
        }))
        .unwrap();
        assert_eq!(capabilities.position_encoding, PositionEncoding::Utf16);
        assert_eq!(capabilities.providers["hover"]["state"], "invalid");
        assert_eq!(capabilities.providers["definition"]["state"], "supported");
        assert_eq!(capabilities.providers["executeCommand"]["state"], "invalid");
        assert_eq!(capabilities.providers["diagnostics"]["state"], "supported");
        assert_eq!(
            capabilities.providers["executeCommand"]["problems"][0]["jsonPath"],
            "capabilities.executeCommandProvider.commands"
        );
    }
}
