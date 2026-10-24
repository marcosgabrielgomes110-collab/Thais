//! Provider registry, presets, and model catalog.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use thais_providers::{
    AnthropicProvider, FakeProvider, FakeScript, FakeStep, OpenAiCompatibleProvider, Provider,
    ProviderError,
};
use thais_storage::{CredentialStore, Paths};
use thiserror::Error;

/// One model in the registry catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogModel {
    /// Model identifier.
    pub id: String,
    /// Provider identifier.
    pub provider: String,
    /// Maximum context window.
    pub context_window: u64,
    /// Whether tool calling is supported.
    pub supports_tools: bool,
    /// Input cost per million tokens.
    pub input_cost_per_million: f64,
    /// Output cost per million tokens.
    pub output_cost_per_million: f64,
}

/// A registry of provider instances and their model catalog.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
    model_providers: HashMap<String, String>,
    configured: BTreeMap<String, CatalogModel>,
    overrides: BTreeMap<String, ModelOverride>,
    default_model: Option<String>,
}

impl ProviderRegistry {
    /// Registry with one provider.
    #[must_use]
    pub fn single(provider: Arc<dyn Provider>) -> Self {
        let mut providers = HashMap::new();
        providers.insert(provider.id().to_owned(), provider);
        Self {
            providers,
            model_providers: HashMap::new(),
            configured: BTreeMap::new(),
            overrides: BTreeMap::new(),
            default_model: None,
        }
    }

    /// Build a registry from user configuration.
    pub fn load(paths: &Paths) -> Result<Self, RegistryError> {
        let credentials = CredentialStore::new(paths.clone());
        let mut registry = Self {
            providers: HashMap::new(),
            model_providers: HashMap::new(),
            configured: BTreeMap::new(),
            overrides: BTreeMap::new(),
            default_model: None,
        };
        registry.load_models(&paths.config_file("models.toml"))?;
        let config_path = paths.config_file("providers.toml");
        let text = match std::fs::read_to_string(&config_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(registry),
            Err(source) => {
                return Err(RegistryError::Io {
                    path: config_path,
                    source,
                });
            }
        };
        let config: ProvidersFile = toml::from_str(&text)?;
        for config in config
            .providers
            .into_iter()
            .filter(|provider| provider.enabled)
        {
            let preset = preset(&config.id);
            let models = if config.models.is_empty() {
                preset
                    .as_ref()
                    .map(|preset| vec![preset.model.to_owned()])
                    .unwrap_or_default()
            } else {
                config.models.clone()
            };
            let Some(model) = models.first().cloned() else {
                continue;
            };
            let base_url = config
                .base_url
                .or_else(|| preset.as_ref().map(|preset| preset.base_url.to_owned()))
                .unwrap_or_default();
            let kind = config
                .kind
                .or_else(|| preset.as_ref().map(|preset| preset.kind.to_owned()))
                .unwrap_or_else(|| "openai-compatible".to_owned());
            if kind != "fake" && base_url.is_empty() {
                continue;
            }
            let provider: Arc<dyn Provider> = if kind == "fake" {
                Arc::new(FakeProvider::new(
                    config.id.clone(),
                    FakeScript {
                        model: Some(model.clone()),
                        steps: vec![FakeStep::Text {
                            text: "registry fake".to_owned(),
                        }],
                    },
                ))
            } else {
                let Some(key) = resolve_key(&credentials, &config.id, config.auth.as_deref())
                else {
                    continue;
                };
                if kind == "anthropic" {
                    Arc::new(
                        AnthropicProvider::with_base_url(base_url, key, model.clone())?
                            .with_default_extra(config.extra.clone()),
                    )
                } else {
                    Arc::new(
                        OpenAiCompatibleProvider::new(
                            config.id.clone(),
                            base_url,
                            key,
                            model.clone(),
                        )?
                        .with_default_extra(config.extra),
                    )
                }
            };
            for model in &models {
                registry
                    .model_providers
                    .insert(model.clone(), config.id.clone());
                registry
                    .configured
                    .entry(model.clone())
                    .or_insert(CatalogModel {
                        id: model.clone(),
                        provider: config.id.clone(),
                        context_window: 128_000,
                        supports_tools: true,
                        input_cost_per_million: 0.0,
                        output_cost_per_million: 0.0,
                    });
            }
            registry.providers.insert(config.id.clone(), provider);
            if registry.default_model.is_none() {
                registry.default_model = models.first().cloned();
            }
        }
        let aliases = registry
            .overrides
            .iter()
            .flat_map(|(model, replacement)| {
                replacement
                    .aliases
                    .iter()
                    .cloned()
                    .map(move |alias| (alias, model.clone()))
            })
            .collect::<Vec<_>>();
        for (alias, model) in aliases {
            if let Some(provider) = registry.model_providers.get(&model).cloned() {
                registry.model_providers.insert(alias, provider);
            }
        }
        Ok(registry)
    }

    /// Build a single preset provider, resolving auth from the credential store.
    pub fn preset(
        paths: &Paths,
        id: &str,
        model: Option<String>,
        extra: Value,
    ) -> Result<Self, RegistryError> {
        let preset = preset(id).ok_or_else(|| RegistryError::UnknownPreset(id.to_owned()))?;
        let credentials = CredentialStore::new(paths.clone());
        let key = key_for(&credentials, id, None)
            .ok_or_else(|| RegistryError::MissingCredential(id.to_owned()))?;
        let model = model.unwrap_or_else(|| preset.model.to_owned());
        let provider: Arc<dyn Provider> = if preset.kind == "anthropic" {
            Arc::new(
                AnthropicProvider::with_base_url(preset.base_url, key, model.clone())?
                    .with_default_extra(extra),
            )
        } else {
            Arc::new(
                OpenAiCompatibleProvider::new(id, preset.base_url, key, model.clone())?
                    .with_default_extra(extra),
            )
        };
        let mut registry = Self::single(provider);
        registry
            .model_providers
            .insert(model.clone(), id.to_owned());
        registry.default_model = Some(model);
        Ok(registry)
    }

    /// Catalog models across all providers.
    pub async fn models(&self) -> Result<Vec<CatalogModel>, ProviderError> {
        let mut models = Vec::new();
        for (provider_id, provider) in &self.providers {
            for model in provider.models().await? {
                let override_model = self.overrides.get(&model.id);
                models.push(CatalogModel {
                    id: model.id.clone(),
                    provider: provider_id.clone(),
                    context_window: override_model
                        .and_then(|value| value.context_window)
                        .unwrap_or(model.context_window),
                    supports_tools: override_model
                        .and_then(|value| value.supports_tools)
                        .unwrap_or(model.supports_tools),
                    input_cost_per_million: override_model
                        .and_then(|value| value.input_cost_per_million)
                        .unwrap_or(0.0),
                    output_cost_per_million: override_model
                        .and_then(|value| value.output_cost_per_million)
                        .unwrap_or(0.0),
                });
            }
        }
        for configured in self.configured.values() {
            if models.iter().any(|model| model.id == configured.id) {
                continue;
            }
            models.push(apply_override(
                configured.clone(),
                self.overrides.get(&configured.id),
            ));
        }
        models.sort_by(|left, right| left.id.cmp(&right.id));
        models.dedup_by(|left, right| left.id == right.id);
        Ok(models)
    }

    /// Resolve the provider for a model.
    #[must_use]
    pub fn provider_for(&self, model: &str) -> Option<Arc<dyn Provider>> {
        if let Some(provider_id) = self.model_providers.get(model)
            && let Some(provider) = self.providers.get(provider_id)
        {
            return Some(Arc::clone(provider));
        }
        if let Some(provider) = self.providers.get(model) {
            return Some(Arc::clone(provider));
        }
        if self.providers.len() == 1 {
            return self.providers.values().next().cloned();
        }
        None
    }

    /// Default model, when configured.
    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    /// Whether the registry has no providers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Provider ids.
    #[must_use]
    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    fn load_models(&mut self, path: &std::path::Path) -> Result<(), RegistryError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(RegistryError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let file: ModelsFile = toml::from_str(&text)?;
        for model in file.models {
            self.overrides.insert(
                model.id.clone(),
                ModelOverride {
                    context_window: model.context_window,
                    supports_tools: model.supports_tools,
                    input_cost_per_million: model.input_cost_per_million,
                    output_cost_per_million: model.output_cost_per_million,
                    aliases: model.aliases,
                },
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ModelOverride {
    context_window: Option<u64>,
    supports_tools: Option<bool>,
    input_cost_per_million: Option<f64>,
    output_cost_per_million: Option<f64>,
    aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvidersFile {
    #[serde(default)]
    providers: Vec<ProviderConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfig {
    id: String,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    extra: Value,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFile {
    #[serde(default)]
    models: Vec<ModelConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelConfig {
    id: String,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    supports_tools: Option<bool>,
    #[serde(default)]
    input_cost_per_million: Option<f64>,
    #[serde(default)]
    output_cost_per_million: Option<f64>,
    #[serde(default)]
    aliases: Vec<String>,
}

fn apply_override(mut model: CatalogModel, override_model: Option<&ModelOverride>) -> CatalogModel {
    if let Some(override_model) = override_model {
        if let Some(context_window) = override_model.context_window {
            model.context_window = context_window;
        }
        if let Some(supports_tools) = override_model.supports_tools {
            model.supports_tools = supports_tools;
        }
        if let Some(cost) = override_model.input_cost_per_million {
            model.input_cost_per_million = cost;
        }
        if let Some(cost) = override_model.output_cost_per_million {
            model.output_cost_per_million = cost;
        }
    }
    model
}

fn default_true() -> bool {
    true
}

struct Preset {
    base_url: &'static str,
    model: &'static str,
    kind: &'static str,
}

fn preset(id: &str) -> Option<Preset> {
    let preset = match id {
        "openai" => Preset {
            base_url: "https://api.openai.com/v1",
            model: "gpt-4.1",
            kind: "openai",
        },
        "anthropic" => Preset {
            base_url: "https://api.anthropic.com",
            model: "claude-sonnet-4-5",
            kind: "anthropic",
        },
        "openrouter" => Preset {
            base_url: "https://openrouter.ai/api/v1",
            model: "openai/gpt-4.1",
            kind: "openai-compatible",
        },
        "deepseek" => Preset {
            base_url: "https://api.deepseek.com/v1",
            model: "deepseek-chat",
            kind: "openai-compatible",
        },
        "cohere" => Preset {
            base_url: "https://api.cohere.ai/compatibility/v1",
            model: "command-a-plus-05-2026",
            kind: "openai-compatible",
        },
        "qwen" => Preset {
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
            model: "qwen-plus",
            kind: "openai-compatible",
        },
        "zai" => Preset {
            base_url: "https://api.z.ai/api/paas/v4",
            model: "glm-4.5",
            kind: "openai-compatible",
        },
        "ollama" => Preset {
            base_url: "http://localhost:11434/v1",
            model: "llama3.2",
            kind: "openai-compatible",
        },
        "vllm" => Preset {
            base_url: "http://localhost:8000/v1",
            model: "local-model",
            kind: "openai-compatible",
        },
        "litellm" => Preset {
            base_url: "http://localhost:4000/v1",
            model: "gpt-4o-mini",
            kind: "openai-compatible",
        },
        "custom-openai-compatible" => Preset {
            base_url: "http://localhost:8000/v1",
            model: "local-model",
            kind: "openai-compatible",
        },
        "fake" => Preset {
            base_url: "",
            model: "fake-model",
            kind: "fake",
        },
        _ => return None,
    };
    Some(preset)
}

fn key_for(credentials: &CredentialStore, provider_id: &str, auth: Option<&str>) -> Option<String> {
    resolve_key(credentials, provider_id, auth).or_else(|| {
        matches!(
            provider_id,
            "ollama" | "vllm" | "litellm" | "custom-openai-compatible"
        )
        .then(String::new)
    })
}

fn resolve_key(
    credentials: &CredentialStore,
    provider_id: &str,
    auth: Option<&str>,
) -> Option<String> {
    if let Some(auth) = auth {
        if let Some(variable) = auth.strip_prefix("env:") {
            return std::env::var(variable)
                .ok()
                .filter(|value| !value.is_empty());
        }
        if let Some(reference) = auth.strip_prefix("keyring:") {
            let mut parts = reference.splitn(2, ':');
            let provider = parts.next().unwrap_or(provider_id);
            let account = parts.next().unwrap_or("default");
            return credentials
                .get(provider, account)
                .ok()
                .flatten()
                .map(|credential| credential.secret);
        }
    }
    credentials
        .get(provider_id, "default")
        .ok()
        .flatten()
        .map(|credential| credential.secret)
}

/// Registry errors.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// I/O failure.
    #[error("registry I/O error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Invalid TOML.
    #[error("invalid registry config: {0}")]
    Toml(#[from] toml::de::Error),
    /// Unknown preset.
    #[error("unknown provider preset: {0}")]
    UnknownPreset(String),
    /// Missing credential.
    #[error("missing credential for provider: {0}")]
    MissingCredential(String),
    /// Provider failure.
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use thais_providers::Provider;
    use thais_storage::Paths;

    fn paths(root: &std::path::Path) -> Paths {
        Paths::from_roots(
            root.join("config"),
            root.join("data"),
            root.join("state"),
            root.join("cache"),
        )
    }

    #[tokio::test]
    async fn fake_provider_registry_exposes_models() {
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(
            "fake",
            FakeScript {
                model: Some("fake-model".to_owned()),
                steps: vec![],
            },
        ));
        let registry = ProviderRegistry::single(provider);
        let models = registry.models().await.expect("models");
        assert_eq!(models[0].id, "fake-model");
        assert!(registry.provider_for("fake-model").is_some());
    }

    #[test]
    fn preset_has_expected_provider_kinds() {
        assert_eq!(preset("anthropic").expect("anthropic").kind, "anthropic");
        assert_eq!(
            preset("deepseek").expect("deepseek").kind,
            "openai-compatible"
        );
        assert!(preset("missing").is_none());
    }

    #[test]
    fn provider_config_rejects_secret_fields() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(temp.path());
        paths.ensure().expect("ensure");
        std::fs::write(
            paths.config_file("providers.toml"),
            r#"
            [[providers]]
            id = "openai"
            type = "openai"
            api_key = "must-not-be-here"
            "#,
        )
        .expect("providers");
        assert!(ProviderRegistry::load(&paths).is_err());
    }

    #[tokio::test]
    async fn provider_config_with_fake_kind_is_loaded() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(temp.path());
        paths.ensure().expect("ensure");
        std::fs::write(
            paths.config_file("providers.toml"),
            r#"
            [[providers]]
            id = "fake"
            type = "fake"
            models = ["fake-model"]
            "#,
        )
        .expect("providers");
        std::fs::write(
            paths.config_file("models.toml"),
            r#"
            [[models]]
            id = "fake-model"
            context_window = 1234
            supports_tools = true
            input_cost_per_million = 1.0
            output_cost_per_million = 2.0
            "#,
        )
        .expect("models");
        let registry = ProviderRegistry::load(&paths).expect("registry");
        assert_eq!(registry.provider_ids(), ["fake"]);
        let models = registry.models().await.expect("models");
        assert_eq!(models[0].context_window, 1234);
        assert_eq!(models[0].input_cost_per_million, 1.0);
    }
}
