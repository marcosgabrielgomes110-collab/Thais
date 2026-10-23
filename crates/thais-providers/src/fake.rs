//! Deterministic fake provider used by M0 tests and `thais run --fake`.

use std::path::Path;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::provider::{ChatEvent, ChatRequest, Model, Provider, ProviderError, Usage};

/// A scripted fake-provider response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeScript {
    /// Model identifier. Defaults to `fake-model`.
    #[serde(default)]
    pub model: Option<String>,
    /// Steps emitted in order.
    pub steps: Vec<FakeStep>,
}

/// One step in a fake response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FakeStep {
    /// Emit a text delta.
    Text {
        /// Delta text.
        text: String,
    },
    /// Emit a reasoning delta.
    Reasoning {
        /// Delta text.
        text: String,
    },
    /// Emit token usage.
    Usage {
        /// Input tokens.
        #[serde(default)]
        input_tokens: u64,
        /// Output tokens.
        #[serde(default)]
        output_tokens: u64,
    },
    /// Wait before the next step.
    Delay {
        /// Duration in milliseconds.
        millis: u64,
    },
    /// Fail the stream.
    Error {
        /// Error message.
        message: String,
    },
    /// Finish the stream. A `done` event is also emitted automatically.
    Done,
}

impl FakeScript {
    /// Load a script from JSON.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Load a script from a file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, FakeProviderError> {
        let text = std::fs::read_to_string(path).map_err(FakeProviderError::Io)?;
        Self::from_json(&text).map_err(FakeProviderError::Json)
    }
}

/// Deterministic fake provider.
#[derive(Debug, Clone)]
pub struct FakeProvider {
    id: String,
    model: String,
    script: FakeScript,
}

impl FakeProvider {
    /// Build a fake provider from a script.
    #[must_use]
    pub fn new(id: impl Into<String>, script: FakeScript) -> Self {
        let model = script
            .model
            .clone()
            .unwrap_or_else(|| "fake-model".to_owned());
        Self {
            id: id.into(),
            model,
            script,
        }
    }

    /// Build a provider from a JSON script file.
    pub fn from_file(
        id: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<Self, FakeProviderError> {
        Ok(Self::new(id, FakeScript::from_file(path)?))
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn models(&self) -> Result<Vec<Model>, ProviderError> {
        Ok(vec![Model {
            id: self.model.clone(),
            name: "Fake model".to_owned(),
            context_window: 8_192,
            supports_tools: true,
        }])
    }

    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let steps = self.script.steps.clone();
        let (sender, receiver) = mpsc::channel(8);
        tokio::spawn(async move {
            for step in steps {
                let event = match step {
                    FakeStep::Text { text } => Ok(ChatEvent::Content { text }),
                    FakeStep::Reasoning { text } => Ok(ChatEvent::Reasoning { text }),
                    FakeStep::Usage {
                        input_tokens,
                        output_tokens,
                    } => Ok(ChatEvent::Usage {
                        usage: Usage {
                            input_tokens,
                            output_tokens,
                        },
                    }),
                    FakeStep::Delay { millis } => {
                        tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
                        continue;
                    }
                    FakeStep::Error { message } => {
                        let _ = sender.send(Err(ProviderError::Message(message))).await;
                        return;
                    }
                    FakeStep::Done => break,
                };
                if sender.send(event).await.is_err() {
                    return;
                }
            }
            let _ = sender.send(Ok(ChatEvent::Done)).await;
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

/// Errors while loading a fake script.
#[derive(Debug, thiserror::Error)]
pub enum FakeProviderError {
    /// I/O failure.
    #[error("could not read fake script: {0}")]
    Io(#[source] std::io::Error),
    /// JSON parse failure.
    #[error("invalid fake script: {0}")]
    Json(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn script() -> FakeScript {
        FakeScript {
            model: Some("fake-test".to_owned()),
            steps: vec![
                FakeStep::Text {
                    text: "hello".to_owned(),
                },
                FakeStep::Reasoning {
                    text: "thinking".to_owned(),
                },
                FakeStep::Usage {
                    input_tokens: 3,
                    output_tokens: 5,
                },
            ],
        }
    }

    #[tokio::test]
    async fn fake_provider_streams_events_in_order() {
        let provider = FakeProvider::new("fake", script());
        assert_eq!(provider.id(), "fake");
        let models = provider.models().await.expect("models");
        assert_eq!(models[0].id, "fake-test");

        let mut stream = provider.chat(ChatRequest::default()).await.expect("chat");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.expect("event"));
        }

        assert_eq!(
            events,
            vec![
                ChatEvent::Content {
                    text: "hello".to_owned()
                },
                ChatEvent::Reasoning {
                    text: "thinking".to_owned()
                },
                ChatEvent::Usage {
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 5
                    }
                },
                ChatEvent::Done,
            ]
        );
    }

    #[tokio::test]
    async fn fake_provider_reports_scripted_errors() {
        let provider = FakeProvider::new(
            "fake",
            FakeScript {
                model: None,
                steps: vec![FakeStep::Error {
                    message: "boom".to_owned(),
                }],
            },
        );
        let mut stream = provider.chat(ChatRequest::default()).await.expect("chat");
        let event = stream.next().await.expect("event");
        assert!(event.is_err());
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn script_parses_from_json() {
        let script = FakeScript::from_json(
            r#"{"model":"m","steps":[{"type":"text","text":"hi"},{"type":"done"}]}"#,
        )
        .expect("parse");
        assert_eq!(script.model.as_deref(), Some("m"));
        assert_eq!(script.steps.len(), 2);
    }
}
