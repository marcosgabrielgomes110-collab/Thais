#![no_main]

use libfuzzer_sys::fuzz_target;
use thais_protocol::jsonrpc::Message;
use thais_protocol::session::{
    SessionApproveParams, SessionCreateParams, SessionSendParams, SessionSubscribeParams,
};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(message) = serde_json::from_str::<Message>(text) else {
        return;
    };
    if let Message::Request(request) = message {
        let _ = request.validate();
        let _ = request.params_as::<SessionCreateParams>();
        let _ = request.params_as::<SessionSendParams>();
        let _ = request.params_as::<SessionApproveParams>();
        let _ = request.params_as::<SessionSubscribeParams>();
    }
});
