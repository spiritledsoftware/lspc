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
        providers.insert(name.to_owned(), normalize_provider(capabilities, path));
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

fn normalize_provider(capabilities: &Map<String, Value>, path: &str) -> Value {
    let capability_path = format!("capabilities.{path}");
    match capabilities.get(path) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {
            json!({"state": "unsupported", "capabilityPath": capability_path})
        }
        Some(Value::Bool(true)) => {
            json!({"state": "supported", "capabilityPath": capability_path, "options": {}})
        }
        Some(Value::Object(options)) => json!({
            "state": "supported",
            "capabilityPath": capability_path,
            "options": options
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_position_encoding_and_isolates_optional_corruption() {
        let capabilities = normalize_initialize_result(json!({
            "capabilities": {"hoverProvider": 7, "definitionProvider": true}
        }))
        .unwrap();
        assert_eq!(capabilities.position_encoding, PositionEncoding::Utf16);
        assert_eq!(capabilities.providers["hover"]["state"], "invalid");
        assert_eq!(capabilities.providers["definition"]["state"], "supported");
    }
}
