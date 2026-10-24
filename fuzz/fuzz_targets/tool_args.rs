#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    if let Some(object) = value.as_object() {
        let _ = object.get("path").and_then(Value::as_str);
        let _ = object.get("command").and_then(Value::as_str);
        let _ = object.get("content").and_then(Value::as_str);
        let _ = object.get("old").and_then(Value::as_str);
        let _ = object.get("new").and_then(Value::as_str);
    }
});
