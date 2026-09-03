use serde_json::Value;
use sha2::{Digest, Sha256};

/// Hashes a JSON value with the frozen domain-separated canonical encoding.
pub(crate) fn digest_canonical_value(domain: &str, value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    encode_canonical_value(value, &mut hasher);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Hashes raw bytes without canonical-value tags or domain separation.
pub(crate) fn digest_raw_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn encode_canonical_value(value: &Value, output: &mut Sha256) {
    match value {
        Value::Null => output.update([0]),
        Value::Bool(false) => output.update([1]),
        Value::Bool(true) => output.update([2]),
        Value::Number(number) if number.as_i64().is_some_and(|value| value < 0) => {
            output.update([3]);
            output.update(number.as_i64().unwrap().to_be_bytes());
        }
        Value::Number(number) if number.as_u64().is_some() => {
            output.update([4]);
            output.update(number.as_u64().unwrap().to_be_bytes());
        }
        Value::Number(number) => {
            output.update([5]);
            let number = number.as_f64().expect("serde_json numbers are finite");
            let normalized = if number == 0.0 { 0.0 } else { number };
            output.update(normalized.to_bits().to_be_bytes());
        }
        Value::String(value) => {
            output.update([6]);
            encode_bytes(value.as_bytes(), output);
        }
        Value::Array(values) => {
            output.update([7]);
            output.update((values.len() as u64).to_be_bytes());
            for value in values {
                encode_canonical_value(value, output);
            }
        }
        Value::Object(values) => {
            output.update([8]);
            output.update((values.len() as u64).to_be_bytes());
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            for (key, value) in entries {
                encode_bytes(key.as_bytes(), output);
                encode_canonical_value(value, output);
            }
        }
    }
}

fn encode_bytes(bytes: &[u8], output: &mut Sha256) {
    output.update((bytes.len() as u64).to_be_bytes());
    output.update(bytes);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_digest_orders_objects_and_separates_domains() {
        let left = json!({"b": [true, null], "a": -1});
        let right = json!({"a": -1, "b": [true, null]});

        assert_eq!(
            digest_canonical_value("lspctl-test-v1", &left),
            digest_canonical_value("lspctl-test-v1", &right)
        );
        assert_ne!(
            digest_canonical_value("lspctl-test-v1", &left),
            digest_canonical_value("lspctl-other-v1", &left)
        );
        assert_eq!(
            digest_canonical_value("lspctl-test-v1", &json!(0.0)),
            digest_canonical_value("lspctl-test-v1", &json!(-0.0))
        );
    }
}
