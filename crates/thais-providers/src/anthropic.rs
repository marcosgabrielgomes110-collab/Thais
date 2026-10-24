//! Native Anthropic Messages API adapter with SSE streaming.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::provider::{ChatEvent, ChatRequest, Model, Provider, ProviderError, Usage};

/// Anthropic native Messages API provider.
#[derive(Clone)]
pub struct AnthropicProvider {
    id: String,
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
    max_retries: u32,
    default_extra: Value,
    first_token_timeout: Duration,
    idle_timeout: Duration,
    total_timeout: Duration,
}

impl AnthropicProvider {
    /// Build an Anthropic provider.
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Self::with_base_url("https://api.anthropic.com", api_key, model)
    }

    /// Build an Anthropic provider with a custom base URL (tests/proxies).
    pub fn with_base_url(
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
            id: "anthropic".to_owned(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            client,
            max_retries: 2,
            default_extra: Value::Null,
            first_token_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            total_timeout: Duration::from_secs(180),
        })
    }

    /// Set provider-level default `extra` parameters.
    #[must_use]
    pub fn with_default_extra(mut self, extra: Value) -> Self {
        self.default_extra = extra;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let (system, messages) = anthropic_messages(&request.messages);
        let model = if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        };
        let mut extra = self.default_extra.clone();
        if let Some(object) = request.extra.as_object() {
            if !extra.is_object() {
                extra = json!({});
            }
            for (key, value) in object {
                extra[key] = value.clone();
            }
        }
        let max_tokens = extra
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
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
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.parameters
                        })
                    })
                    .collect(),
            );
        }
        if let Some(object) = extra.as_object() {
            for (key, value) in object {
                if !matches!(key.as_str(), "max_tokens") {
                    body[key] = value.clone();
                }
            }
        }
        if body.get("thinking").is_some()
            && let Some(object) = body.as_object_mut()
        {
            object.remove("temperature");
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
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
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
        let mut input_tokens = 0_u64;
        let mut output_tokens = 0_u64;
        let mut done = false;
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
                let value: Value = serde_json::from_str(&payload).map_err(|error| {
                    ProviderError::Message(format!("invalid SSE JSON: {error}"))
                })?;
                match value.get("type").and_then(Value::as_str) {
                    Some("message_start") => {
                        input_tokens = value
                            .pointer("/message/usage/input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                    }
                    Some("content_block_start") => {
                        let index =
                            value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let block = value.get("content_block").cloned().unwrap_or_default();
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            let id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            if sender
                                .send(Ok(ChatEvent::ToolCallDelta {
                                    index,
                                    id,
                                    name,
                                    arguments: String::new(),
                                }))
                                .await
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                    Some("content_block_delta") => {
                        let index =
                            value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let delta = value.get("delta").cloned().unwrap_or_default();
                        match delta.get("type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                if let Some(text) = delta.get("text").and_then(Value::as_str)
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
                            }
                            Some("thinking_delta") => {
                                if let Some(text) = delta.get("thinking").and_then(Value::as_str)
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
                            Some("input_json_delta") => {
                                let arguments = delta
                                    .get("partial_json")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                if sender
                                    .send(Ok(ChatEvent::ToolCallDelta {
                                        index,
                                        id: None,
                                        name: None,
                                        arguments,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    return Ok(());
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("message_delta") => {
                        output_tokens = value
                            .pointer("/usage/output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(output_tokens);
                    }
                    Some("message_stop") => {
                        done = true;
                        break;
                    }
                    Some("ping") => {}
                    Some("error") => {
                        let message = value
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("provider error");
                        return Err(ProviderError::Message(message.to_owned()));
                    }
                    _ => {}
                }
            }
            if input_tokens > 0 || output_tokens > 0 {
                let _ = sender
                    .send(Ok(ChatEvent::Usage {
                        usage: Usage {
                            input_tokens,
                            output_tokens,
                        },
                    }))
                    .await;
            }
            if done {
                break;
            }
        }

        let _ = sender.send(Ok(ChatEvent::Done)).await;
        Ok(())
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn models(&self) -> Result<Vec<Model>, ProviderError> {
        Ok(vec![Model {
            id: self.model.clone(),
            name: self.model.clone(),
            context_window: 200_000,
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

fn anthropic_messages(messages: &[crate::provider::ChatMessage]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut output: Vec<Value> = Vec::new();
    for message in messages {
        match message.role {
            crate::provider::Role::System => {
                if let Some(content) = &message.content {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(content);
                }
            }
            crate::provider::Role::User => {
                output.push(
                    json!({"role": "user", "content": message.content.clone().unwrap_or_default()}),
                );
            }
            crate::provider::Role::Assistant => {
                let mut blocks = Vec::new();
                if let Some(content) = &message.content
                    && !content.is_empty()
                {
                    blocks.push(json!({"type": "text", "text": content}));
                }
                for call in &message.tool_calls {
                    let input: Value =
                        serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input
                    }));
                }
                output.push(json!({"role": "assistant", "content": blocks}));
            }
            crate::provider::Role::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": message.content.clone().unwrap_or_default()
                });
                if let Some(last) = output.last_mut()
                    && last.get("role").and_then(Value::as_str) == Some("user")
                    && last.get("_thais_tool_results").is_some()
                {
                    if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                        content.push(block);
                    }
                    continue;
                }
                output.push(json!({
                    "role": "user",
                    "content": [block],
                    "_thais_tool_results": true
                }));
            }
        }
    }
    for message in &mut output {
        if let Some(object) = message.as_object_mut() {
            object.remove("_thais_tool_results");
        }
    }
    (system, output)
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

    #[test]
    fn anthropic_messages_merge_tool_results() {
        let messages = vec![
            crate::provider::ChatMessage::system("be brief"),
            crate::provider::ChatMessage::user("hi"),
            crate::provider::ChatMessage::assistant(
                Some("checking".to_owned()),
                vec![crate::provider::ToolCall {
                    id: "toolu_1".to_owned(),
                    name: "read".to_owned(),
                    arguments: "{\"path\":\"a\"}".to_owned(),
                }],
            ),
            crate::provider::ChatMessage::tool("toolu_1", "content"),
        ];
        let (system, output) = anthropic_messages(&messages);
        assert_eq!(system, "be brief");
        assert_eq!(output.len(), 3);
        assert_eq!(output[2]["role"], "user");
        assert_eq!(output[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn thinking_requests_drop_temperature() {
        let provider = AnthropicProvider::with_base_url(
            "https://api.anthropic.com",
            "test-key",
            "claude-test",
        )
        .expect("provider")
        .with_default_extra(json!({"thinking": {"type": "enabled", "budget_tokens": 2000}}));
        let request = ChatRequest {
            model: "claude-test".to_owned(),
            temperature: Some(0.5),
            ..ChatRequest::default()
        };
        let body = provider.build_body(&request);
        assert!(body.get("temperature").is_none());
    }

    #[tokio::test]
    async fn parses_anthropic_sse_stream() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::with_base_url(server.uri(), "test-key", "claude-test")
            .expect("provider");
        let mut stream = provider
            .chat(ChatRequest::user("claude-test", "hi"))
            .await
            .expect("chat");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.expect("event"));
        }
        assert!(events.contains(&ChatEvent::Content {
            text: "hello".to_owned()
        }));
        assert!(events.contains(&ChatEvent::Usage {
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2
            }
        }));
    }
}
