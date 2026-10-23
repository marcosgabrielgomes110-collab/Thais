//! Redaction hook applied before an event is persisted or rendered.

use serde_json::Value;

/// Redacts sensitive values from structured payloads and free text.
pub trait Redactor: Send + Sync {
    /// Redact a JSON value in place.
    fn redact_value(&self, value: &mut Value);

    /// Redact a free-text string in place.
    fn redact_text(&self, text: &mut String);
}

/// Redactor used by tests that need to assert exact payloads.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopRedactor;

impl Redactor for NoopRedactor {
    fn redact_value(&self, _value: &mut Value) {}

    fn redact_text(&self, _text: &mut String) {}
}

/// Basic redactor for M0. It removes common credential shapes and values under
/// sensitive JSON keys. The full policy and its tests land in M6.
#[derive(Debug, Default, Clone, Copy)]
pub struct BasicRedactor;

impl Redactor for BasicRedactor {
    fn redact_value(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    if is_sensitive_key(key) {
                        *value = Value::String("[REDACTED]".to_owned());
                    } else {
                        self.redact_value(value);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.redact_value(value);
                }
            }
            Value::String(text) => {
                *text = redact_text_value(text);
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn redact_text(&self, text: &mut String) {
        *text = redact_text_value(text);
    }
}

const SENSITIVE_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "authorization",
    "private_key",
    "secret",
    "token",
    "password",
];

const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "AIza",
    "Bearer ",
];

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SENSITIVE_KEYS.contains(&key.as_str())
}

fn redact_text_value(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        if let Some(prefix) = TOKEN_PREFIXES
            .iter()
            .find(|prefix| input[index..].starts_with(*prefix))
        {
            output.push_str(prefix);
            index += prefix.len();

            let start = index;
            while index < input.len() && !is_token_delimiter(input.as_bytes()[index]) {
                index += 1;
            }
            if index > start {
                output.push_str("[REDACTED]");
            }
        } else {
            let character = input[index..]
                .chars()
                .next()
                .expect("index is on a char boundary");
            output.push(character);
            index += character.len_utf8();
        }
    }

    output
}

fn is_token_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b',' | b';' | b'}' | b']'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_sensitive_object_keys_recursively() {
        let mut value = json!({
            "api_key": "sk-live-123",
            "nested": {
                "authorization": "Bearer abc.def",
                "note": "safe"
            },
            "items": [{"password": "hunter2"}]
        });

        BasicRedactor.redact_value(&mut value);
        let encoded = serde_json::to_string(&value).expect("json");
        assert!(!encoded.contains("sk-live-123"));
        assert!(!encoded.contains("abc.def"));
        assert!(!encoded.contains("hunter2"));
        assert!(encoded.contains("[REDACTED]"));
        assert!(encoded.contains("safe"));
    }

    #[test]
    fn redacts_common_token_prefixes_in_text() {
        let text = "use sk-abc123 and ghp_deadbeef plus AKIA1234567890";
        let redacted = redact_text_value(text);
        assert!(!redacted.contains("sk-abc123"));
        assert!(!redacted.contains("ghp_deadbeef"));
        assert!(!redacted.contains("AKIA1234567890"));
        assert_eq!(redacted.matches("[REDACTED]").count(), 3);
    }

    #[test]
    fn noop_redactor_leaves_values_unchanged() {
        let mut value = json!({"api_key": "sk-live-123"});
        NoopRedactor.redact_value(&mut value);
        assert_eq!(value["api_key"], "sk-live-123");
    }
}
