use proptest::prelude::*;
use thais_protocol::jsonrpc::Message;
use thais_protocol::session::{
    SessionApproveParams, SessionCreateParams, SessionSendParams, SessionSubscribeParams,
};

proptest! {
    #[test]
    fn arbitrary_json_never_panics(data in any::<Vec<u8>>()) {
        if let Ok(text) = std::str::from_utf8(&data)
            && let Ok(message) = serde_json::from_str::<Message>(text)
            && let Message::Request(request) = message
        {
            let _ = request.validate();
            let _ = request.params_as::<SessionCreateParams>();
            let _ = request.params_as::<SessionSendParams>();
            let _ = request.params_as::<SessionApproveParams>();
            let _ = request.params_as::<SessionSubscribeParams>();
        }
    }

    #[test]
    fn arbitrary_values_do_not_panic_in_tool_argument_reads(data in any::<Vec<u8>>()) {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data) else {
            return Ok(());
        };
        if let Some(object) = value.as_object() {
            let _ = object.get("path").and_then(serde_json::Value::as_str);
            let _ = object.get("command").and_then(serde_json::Value::as_str);
            let _ = object.get("content").and_then(serde_json::Value::as_str);
        }
    }
}
