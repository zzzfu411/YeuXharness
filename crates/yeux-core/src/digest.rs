use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DigestError {
    #[error("value cannot be serialized for canonical hashing: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// RFC-8785-like deterministic JSON for security bindings.
///
/// Object keys are recursively sorted, arrays retain order, and serde_json's
/// canonical scalar representation is used. Inputs containing non-finite
/// floats are already rejected by serde_json.
pub fn canonical_json(value: &Value) -> String {
    fn write(value: &Value, output: &mut String) {
        match value {
            Value::Null => output.push_str("null"),
            Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Value::Number(value) => output.push_str(&value.to_string()),
            Value::String(value) => {
                output.push_str(
                    &serde_json::to_string(value).expect("string serialization cannot fail"),
                );
            }
            Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write(value, output);
                }
                output.push(']');
            }
            Value::Object(values) => {
                output.push('{');
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_unstable();
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push_str(
                        &serde_json::to_string(key).expect("key serialization cannot fail"),
                    );
                    output.push(':');
                    write(&values[key], output);
                }
                output.push('}');
            }
        }
    }

    let mut output = String::new();
    write(value, &mut output);
    output
}

pub fn digest_value(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(value).as_bytes());
    hex::encode(hasher.finalize())
}

pub fn digest_serializable(value: &impl Serialize) -> Result<String, DigestError> {
    serde_json::to_value(value)
        .map(|value| digest_value(&value))
        .map_err(Into::into)
}
