use serde_json::json;
use thais_protocol::error::{ErrorObject, INVALID_PARAMS};
use thais_protocol::event::{EventEnvelope, EventKind};
use thais_protocol::jsonrpc::{Id, Message, Notification, Request, Response};
use thais_protocol::method;
use thais_protocol::session::{InitializeParams, InitializeResult, SessionEventParams};

#[test]
fn request_round_trips() {
    let request = Request::new(
        1_i64,
        method::SESSION_CREATE,
        json!({"workspace": "/tmp/repo"}),
    );
    request.validate().expect("valid request");

    let encoded = serde_json::to_string(&request).expect("encode");
    let decoded: Request = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded.method, method::SESSION_CREATE);
    assert_eq!(decoded.id, Id::Number(1));
    assert_eq!(decoded.params["workspace"], "/tmp/repo");
}

#[test]
fn notification_has_no_id_and_round_trips() {
    let notification = Notification::new(method::SESSION_EVENT, json!({"event": {}}));
    let encoded = serde_json::to_string(&notification).expect("encode");
    assert!(!encoded.contains("\"id\""));

    let decoded: Notification = serde_json::from_str(&encoded).expect("decode");
    decoded.validate().expect("valid notification");
    assert_eq!(decoded.method, method::SESSION_EVENT);
}

#[test]
fn response_success_and_error_shapes() {
    let success = Response::success(7_i64, json!({"ok": true}));
    success.validate().expect("valid success");
    let encoded = serde_json::to_string(&success).expect("encode");
    assert!(!encoded.contains("\"error\""));

    let error = Response::error(
        7_i64,
        ErrorObject::new(INVALID_PARAMS, "bad params").with_data(json!({"field": "workspace"})),
    );
    error.validate().expect("valid error");
    let encoded = serde_json::to_string(&error).expect("encode");
    assert!(!encoded.contains("\"result\""));
    assert!(encoded.contains("bad params"));
}

#[test]
fn message_enum_deserializes_each_shape() {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    let notification = json!({"jsonrpc": "2.0", "method": "session.event", "params": {}});
    let response = json!({"jsonrpc": "2.0", "id": 1, "result": {}});

    assert!(matches!(
        serde_json::from_value::<Message>(request).expect("request"),
        Message::Request(_)
    ));
    assert!(matches!(
        serde_json::from_value::<Message>(notification).expect("notification"),
        Message::Notification(_)
    ));
    assert!(matches!(
        serde_json::from_value::<Message>(response).expect("response"),
        Message::Response(_)
    ));
}

#[test]
fn invalid_version_is_rejected() {
    let request = Request::new(1_i64, method::INITIALIZE, json!({}));
    let mut value = serde_json::to_value(&request).expect("value");
    value["jsonrpc"] = json!("1.0");
    let decoded: Request = serde_json::from_value(value).expect("decode");
    assert!(decoded.validate().is_err());
}

#[test]
fn typed_params_and_results() {
    let request = Request::new(
        1_i64,
        method::INITIALIZE,
        json!({
            "client_name": "test",
            "client_version": "0.1",
            "protocol_version": "1.0",
            "capabilities": {}
        }),
    );
    let params: InitializeParams = request.params_as().expect("params");
    assert_eq!(params.client_name, "test");

    let response = Response::success(
        1_i64,
        serde_json::to_value(InitializeResult {
            server_name: "thais".to_owned(),
            server_version: "0.1.0".to_owned(),
            protocol_version: "1.0".to_owned(),
            capabilities: json!({"sessions": true}),
        })
        .expect("result"),
    );
    let result: InitializeResult = response.result_as().expect("result");
    assert_eq!(result.server_name, "thais");
    assert_eq!(result.protocol_version, "1.0");
}

#[test]
fn null_id_round_trips() {
    let response = Response::error(Id::Null, ErrorObject::new(-32700, "parse error"));
    let encoded = serde_json::to_value(&response).expect("encode");
    assert_eq!(encoded["id"], json!(null));
}

#[test]
fn event_envelope_round_trips() {
    let event = EventEnvelope::new(
        "evt-1",
        "s-1",
        1,
        "2026-10-23T00:00:00Z",
        EventKind::ModelDelta,
        json!({"text": "hello"}),
    );
    let encoded = serde_json::to_string(&event).expect("encode");
    let decoded: EventEnvelope = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, event);

    let notification = Notification::new(
        method::SESSION_EVENT,
        serde_json::to_value(SessionEventParams { event }).expect("params"),
    );
    let decoded: SessionEventParams = notification.params_as().expect("event params");
    assert_eq!(decoded.event.kind, EventKind::ModelDelta);
}
