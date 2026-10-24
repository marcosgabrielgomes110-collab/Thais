//! OpenAI-compatible provider (Cohere compatibility API, OpenRouter, DeepSeek,
//! and other `/chat/completions` endpoints).

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::provider::{ChatEvent, ChatRequest, Model, Provider, ProviderError, Usage};

/// Cohere's OpenAI-compatible endpoint.
pub const COHERE_BASE_URL: &str = "https://api.cohere.ai/compatibility/v1";
/// Default Cohere model used by the M1 integration test.
pub const COHERE_DEFAULT_MODEL: &str = "command-a-plus-05-2026";

/// Provider for OpenAI-shaped `/chat/completions` endpoints.
#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    id: String,
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
    max_retries: u32,
    first_token_timeout: Duration,
    idle_timeout: Duration,
    total_timeout: Duration,
}

impl OpenAiCompatibleProvider {
    /// Build a provider. The API key is never logged.
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|error| ProviderError::Message(format!("HTTP client: {error}")))?;
        Ok(Self {
            id: id.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            client,
            max_retries: 2,
            first_token_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            total_timeout: Duration::from_secs(180),
        })
    }

    /// Build a Cohere provider through the Compatibility API.
    pub fn cohere(
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Self::new("cohere", COHERE_BASE_URL, api_key, model)
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let messages = request
            .messages
            .iter()
            .map(message_to_value)
            .collect::<Vec<_>>();
        let model = if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        };
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true
        });

        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters
                            }
                        })
                    })
                    .collect(),
            );
        }
        if let Some(extra) = request.extra.as_object() {
            for (key, value) in extra {
                body[key] = value.clone();
            }
        }
        body
    }

    async fn send_with_retries(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let url = self.endpoint();
        let mut attempt = 0_u32;

        loop {
            let response = self
                .client
                .post(&url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", self.api_key),
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(body)
                .send()
                .await;

            match response {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response)
                    if response.status().as_u16() == 429 || response.status().is_server_error() =>
                {
                    let status = response.status().as_u16();
                    let message = response.text().await.unwrap_or_default();
                    if attempt >= self.max_retries {
                        return Err(ProviderError::Http {
                            status,
                            message: summarize(&message),
                        });
                    }
                    sleep_backoff(attempt).await;
                    attempt += 1;
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    let message = response.text().await.unwrap_or_default();
                    return Err(ProviderError::Http {
                        status,
                        message: summarize(&message),
                    });
                }
                Err(error) => {
                    if attempt >= self.max_retries {
                        return Err(ProviderError::Message(format!("request failed: {error}")));
                    }
                    sleep_backoff(attempt).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn stream_chat(
        &self,
        body: Value,
        sender: mpsc::Sender<Result<ChatEvent, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let response = self.send_with_retries(&body).await?;
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut first = true;
        let mut saw_done = false;
        let started = tokio::time::Instant::now();

        loop {
            if started.elapsed() > self.total_timeout {
                return Err(ProviderError::Timeout("total request".to_owned()));
            }
            let timeout = if first {
                self.first_token_timeout
            } else {
                self.idle_timeout
            };
            let next = tokio::time::timeout(timeout, stream.next()).await;
            let item = match next {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => return Err(ProviderError::Timeout("stream idle".to_owned())),
            };
            first = false;
            let chunk = item.map_err(|error| ProviderError::Message(format!("stream: {error}")))?;

            for payload in parser.push(&chunk) {
                if payload.trim() == "[DONE]" {
                    saw_done = true;
                    break;
                }
                let value: Value = serde_json::from_str(&payload).map_err(|error| {
                    ProviderError::Message(format!("invalid stream JSON: {error}"))
                })?;

                if let Some(error) = value.get("error") {
                    return Err(ProviderError::Message(format!(
                        "provider stream error: {}",
                        summarize(&error.to_string())
                    )));
                }

                if let Some(usage) = value.get("usage").and_then(parse_usage)
                    && sender.send(Ok(ChatEvent::Usage { usage })).await.is_err()
                {
                    return Ok(());
                }

                if let Some(choices) = value.get("choices").and_then(Value::as_array) {
                    for choice in choices {
                        if let Some(delta) = choice.get("delta") {
                            if let Some(text) = delta.get("content").and_then(Value::as_str)
                                && !text.is_empty()
                                && sender
                                    .send(Ok(ChatEvent::Content {
                                        text: text.to_owned(),
                                    }))
                                    .await
                                    .is_err()
                            {
                                return Ok(());
                            }
                            for key in ["reasoning_content", "reasoning"] {
                                if let Some(text) = delta.get(key).and_then(Value::as_str)
                                    && !text.is_empty()
                                    && sender
                                        .send(Ok(ChatEvent::Reasoning {
                                            text: text.to_owned(),
                                        }))
                                        .await
                                        .is_err()
                                {
                                    return Ok(());
                                }
                            }
                            if let Some(tool_calls) =
                                delta.get("tool_calls").and_then(Value::as_array)
                            {
                                for call in tool_calls {
                                    let index =
                                        call.get("index").and_then(Value::as_u64).unwrap_or(0)
                                            as usize;
                                    let id = call
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .map(ToOwned::to_owned);
                                    let name = call
                                        .pointer("/function/name")
                                        .and_then(Value::as_str)
                                        .map(ToOwned::to_owned);
                                    let arguments = call
                                        .pointer("/function/arguments")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned();
                                    if sender
                                        .send(Ok(ChatEvent::ToolCallDelta {
                                            index,
                                            id,
                                            name,
                                            arguments,
                                        }))
                                        .await
                                        .is_err()
                                    {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        if choice.get("finish_reason").and_then(Value::as_str) == Some("error") {
                            return Err(ProviderError::Message(
                                "provider reported finish_reason=error".to_owned(),
                            ));
                        }
                    }
                }
            }
            if saw_done {
                break;
            }
        }

        let _ = sender.send(Ok(ChatEvent::Done)).await;
        Ok(())
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn models(&self) -> Result<Vec<Model>, ProviderError> {
        Ok(vec![Model {
            id: self.model.clone(),
            name: self.model.clone(),
            context_window: 128_000,
            supports_tools: true,
        }])
    }

    async fn chat(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let body = self.build_body(&request);
        let (sender, receiver) = mpsc::channel(32);
        let provider = self.clone();
        tokio::spawn(async move {
            if let Err(error) = provider.stream_chat(body, sender.clone()).await {
                let _ = sender.send(Err(error)).await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

fn message_to_value(message: &crate::provider::ChatMessage) -> Value {
    let mut object = json!({"role": message.role.as_str()});
    if let Some(content) = &message.content {
        object["content"] = json!(content);
    }
    if !message.tool_calls.is_empty() {
        object["tool_calls"] = Value::Array(
            message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        object["tool_call_id"] = json!(tool_call_id);
    }
    object
}

fn parse_usage(value: &Value) -> Option<Usage> {
    Some(Usage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn summarize(message: &str) -> String {
    let compact = message.replace(['\n', '\r'], " ");
    if compact.chars().count() <= 500 {
        compact
    } else {
        format!("{}…", compact.chars().take(500).collect::<String>())
    }
}

async fn sleep_backoff(attempt: u32) {
    let millis = 250_u64.saturating_mul(1_u64 << attempt.min(4));
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

/// Minimal server-sent-events decoder. It buffers bytes, so UTF-8 sequences
/// split across HTTP chunks are safe.
#[derive(Debug, Default)]
struct SseParser {
    line: Vec<u8>,
    data: String,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    if !self.data.is_empty() {
                        events.push(std::mem::take(&mut self.data));
                    }
                } else if let Ok(text) = std::str::from_utf8(&line)
                    && let Some(value) = text.strip_prefix("data:")
                {
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                }
            } else {
                self.line.push(byte);
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn sse_parser_handles_comments_crlf_and_split_chunks() {
        let mut parser = SseParser::default();
        assert!(parser.push(b": keepalive\r\n\r\n").is_empty());
        assert!(parser.push(b"data: {\"a\":").is_empty());
        assert_eq!(parser.push(b"1}\r\n\r\n"), vec!["{\"a\":1}".to_owned()]);
        assert_eq!(
            parser.push(b"data: one\ndata: two\n\n"),
            vec!["one\ntwo".to_owned()]
        );
    }

    #[test]
    fn sse_parser_handles_utf8_split_across_chunks() {
        let text = "olá";
        let payload = format!("data: {{\"text\":\"{text}\"}}\n\n");
        let bytes = payload.as_bytes();
        let split = bytes
            .iter()
            .position(|byte| *byte == 0xC3)
            .expect("utf8 start");
        let mut parser = SseParser::default();
        assert!(parser.push(&bytes[..split + 1]).is_empty());
        assert_eq!(
            parser.push(&bytes[split + 1..]),
            vec![format!("{{\"text\":\"{text}\"}}")]
        );
    }

    #[tokio::test]
    async fn parses_a_streamed_openai_compatible_response() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Cargo.toml\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiCompatibleProvider::new("test", server.uri(), "test-key", "test-model")
                .expect("provider");
        let mut stream = provider
            .chat(ChatRequest::user("test-model", "hi"))
            .await
            .expect("chat");

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.expect("event"));
        }

        assert_eq!(
            events,
            vec![
                ChatEvent::Reasoning {
                    text: "think".to_owned()
                },
                ChatEvent::Content {
                    text: "hello".to_owned()
                },
                ChatEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("read".to_owned()),
                    arguments: "{\"path\":".to_owned()
                },
                ChatEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: "\"Cargo.toml\"}".to_owned()
                },
                ChatEvent::Done,
            ]
        );
    }

    #[tokio::test]
    async fn reports_http_errors_without_leaking_the_key() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("no api key supplied"))
            .mount(&server)
            .await;

        let provider =
            OpenAiCompatibleProvider::new("test", server.uri(), "super-secret-key", "test-model")
                .expect("provider");
        let mut stream = provider
            .chat(ChatRequest::user("test-model", "hi"))
            .await
            .expect("chat");
        let error = stream
            .next()
            .await
            .expect("stream item")
            .expect_err("expected an HTTP error");
        let message = error.to_string();
        assert!(message.contains("401"), "{message}");
        assert!(!message.contains("super-secret-key"), "{message}");
    }

    #[tokio::test]
    #[ignore = "requires THAIS_COHERE_API_KEY"]
    async fn cohere_live_chat() {
        let key = std::env::var("THAIS_COHERE_API_KEY")
            .expect("set THAIS_COHERE_API_KEY for the live Cohere test");
        let provider =
            OpenAiCompatibleProvider::cohere(key, COHERE_DEFAULT_MODEL).expect("provider");
        let request = ChatRequest {
            model: COHERE_DEFAULT_MODEL.to_owned(),
            messages: vec![crate::provider::ChatMessage::user("Reply with exactly: ok")],
            extra: json!({"reasoning_effort": "none", "max_tokens": 20}),
            ..ChatRequest::default()
        };
        let mut stream = provider.chat(request).await.expect("chat");
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.expect("event") {
                ChatEvent::Content { text: delta } => text.push_str(&delta),
                ChatEvent::Done => break,
                _ => {}
            }
        }
        assert!(!text.trim().is_empty(), "Cohere returned no content");
    }
}
