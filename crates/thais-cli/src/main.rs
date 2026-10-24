#![forbid(unsafe_code)]

//! Command-line entry point for Thais.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use thais_core::{ContextManager, ProviderRegistry, Runtime, SystemClock, UuidGenerator, Verifier};
use thais_protocol::event::{EventEnvelope, EventKind};
use thais_protocol::jsonrpc::{Message, Notification};
use thais_protocol::method;
use thais_protocol::session::SessionMode;
use thais_providers::{
    COHERE_DEFAULT_MODEL, FakeProvider, FakeScript, FakeStep, OpenAiCompatibleProvider, Provider,
};
use thais_sdk::channel;
use thais_storage::{CredentialStore, Paths, Storage, StorageHandle, trust_key};

#[derive(Debug, Parser)]
#[command(
    name = "thais",
    version,
    about = "Local-first coding agent",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the JSON-RPC server over stdio.
    Serve {
        /// Use NDJSON framing over stdio.
        #[arg(long)]
        stdio: bool,
        /// Provider backend.
        #[arg(long, value_enum)]
        provider: Option<ProviderChoice>,
        /// Model identifier.
        #[arg(long)]
        model: Option<String>,
        /// Fake provider script (JSON). Conflicts with --provider.
        #[arg(long, conflicts_with = "provider")]
        fake: Option<PathBuf>,
    },
    /// Run a headless turn.
    Run {
        /// Provider backend.
        #[arg(long, value_enum)]
        provider: Option<ProviderChoice>,
        /// Session mode.
        #[arg(long, value_enum)]
        mode: Option<ModeChoice>,
        /// Model identifier.
        #[arg(long)]
        model: Option<String>,
        /// Fake provider script (JSON). Conflicts with --provider.
        #[arg(long, conflicts_with = "provider")]
        fake: Option<PathBuf>,
        /// Emit NDJSON events instead of human-readable output.
        #[arg(long)]
        json: bool,
        /// Task text.
        #[arg(long)]
        task: Option<String>,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Open the interactive TUI.
    Tui {
        /// Provider backend.
        #[arg(long, value_enum)]
        provider: Option<ProviderChoice>,
        /// Session mode.
        #[arg(long, value_enum)]
        mode: Option<ModeChoice>,
        /// Model identifier.
        #[arg(long)]
        model: Option<String>,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// List persisted sessions.
    Sessions {
        /// Include archived sessions.
        #[arg(long)]
        all: bool,
    },
    /// List models from providers.toml/models.toml.
    Models,
    /// Search the Event Log with FTS5.
    History {
        /// FTS5 query.
        query: String,
        /// Limit to one session.
        #[arg(long)]
        session: Option<String>,
        /// Limit to one workspace.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Maximum number of matches.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Emit one JSON object per match.
        #[arg(long)]
        json: bool,
    },
    /// Validate paths, permissions, credentials, and SQLite FTS5.
    Doctor,
    /// Manage provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Manage workspace trust.
    Trust {
        /// Show trust status for the current workspace.
        #[arg(long, conflicts_with = "remove")]
        status: bool,
        /// Remove trust records for the current workspace.
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProviderChoice {
    /// Deterministic scripted provider.
    Fake,
    /// Cohere through the OpenAI-compatible Compatibility API.
    Cohere,
    /// Native OpenAI.
    Openai,
    /// Native Anthropic Messages API.
    Anthropic,
    /// OpenRouter.
    Openrouter,
    /// DeepSeek.
    Deepseek,
    /// Alibaba Qwen (DashScope compatible mode).
    Qwen,
    /// Z.ai.
    Zai,
    /// Local Ollama.
    Ollama,
    /// Local vLLM.
    Vllm,
    /// Local LiteLLM proxy.
    Litellm,
}

impl ProviderChoice {
    const fn preset_id(self) -> Option<&'static str> {
        match self {
            Self::Fake | Self::Cohere => None,
            Self::Openai => Some("openai"),
            Self::Anthropic => Some("anthropic"),
            Self::Openrouter => Some("openrouter"),
            Self::Deepseek => Some("deepseek"),
            Self::Qwen => Some("qwen"),
            Self::Zai => Some("zai"),
            Self::Ollama => Some("ollama"),
            Self::Vllm => Some("vllm"),
            Self::Litellm => Some("litellm"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeChoice {
    /// Read-only planning.
    Plan,
    /// Build mode with mutations.
    Build,
}

impl From<ModeChoice> for SessionMode {
    fn from(value: ModeChoice) -> Self {
        match value {
            ModeChoice::Plan => Self::Plan,
            ModeChoice::Build => Self::Build,
        }
    }
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Store a credential. The secret is read from stdin.
    Login {
        /// Provider identifier.
        provider: String,
        /// Account label.
        #[arg(long)]
        account: Option<String>,
    },
    /// Remove a credential.
    Logout {
        /// Provider identifier.
        provider: String,
        /// Account label.
        #[arg(long)]
        account: Option<String>,
    },
    /// Show credential backends and environment variable names.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Serve {
            stdio,
            provider,
            model,
            fake,
        } => {
            if !stdio {
                bail!("only `thais serve --stdio` is supported in M0");
            }
            let runtime = build_runtime(fake, provider, model)?;
            thais_core::serve_stdio(runtime).await?;
            Ok(())
        }
        Command::Run {
            provider,
            mode,
            model,
            fake,
            json,
            task,
            workspace,
        } => {
            run(
                fake,
                provider,
                mode.unwrap_or(ModeChoice::Build),
                model,
                json,
                task,
                workspace,
            )
            .await
        }
        Command::Tui {
            provider,
            mode,
            model,
            workspace,
        } => {
            tui(
                provider,
                mode.unwrap_or(ModeChoice::Build),
                model,
                workspace,
            )
            .await
        }
        Command::Sessions { all } => sessions(all).await,
        Command::Models => models().await,
        Command::History {
            query,
            session,
            workspace,
            limit,
            json,
        } => history(query, session, workspace, limit, json).await,
        Command::Doctor => doctor(),
        Command::Auth { command } => auth(command),
        Command::Trust { status, remove } => trust(status, remove),
    }
}

async fn history(
    query: String,
    session: Option<String>,
    workspace: Option<PathBuf>,
    limit: u32,
    json: bool,
) -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let storage = StorageHandle::open(paths.database_path())?;
    let workspace = workspace
        .map(|workspace| thais_storage::canonical_workspace(&workspace))
        .transpose()?
        .and_then(|workspace| workspace.to_str().map(ToOwned::to_owned));
    let hits = storage
        .search_filtered(query, session, workspace, limit.clamp(1, 200))
        .await?;
    if hits.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for hit in hits {
        if json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "session_id": hit.event.session_id,
                    "session_name": hit.session_name,
                    "workspace": hit.workspace,
                    "seq": hit.event.seq,
                    "kind": hit.event.kind.as_str(),
                    "payload": hit.event.payload
                }))?
            );
        } else {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                hit.session_name,
                hit.event.session_id,
                hit.event.seq,
                hit.event.kind.as_str(),
                hit.event.payload
            );
        }
    }
    Ok(())
}

async fn sessions(all: bool) -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let storage = StorageHandle::open(paths.database_path())?;
    let sessions = storage.list_sessions_with_archived(all).await?;
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}\ttok={}/{}\t${:.4}\t{}",
            session.session_id,
            session.name,
            session.mode.as_str(),
            session.status.as_str(),
            session.model,
            session.input_tokens,
            session.output_tokens,
            session.cost_usd,
            session.workspace
        );
    }
    Ok(())
}

async fn models() -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let registry = ProviderRegistry::load(&paths)?;
    if registry.is_empty() {
        println!(
            "no providers configured; add providers.toml under {}",
            paths.config_dir().display()
        );
        return Ok(());
    }
    for model in registry.models().await? {
        println!(
            "{}\t{}\tcontext={}\ttools={}\tin=${:.2}/M\tout=${:.2}/M",
            model.id,
            model.provider,
            model.context_window,
            model.supports_tools,
            model.input_cost_per_million,
            model.output_cost_per_million
        );
    }
    Ok(())
}

fn build_runtime(
    fake: Option<PathBuf>,
    provider: Option<ProviderChoice>,
    model: Option<String>,
) -> anyhow::Result<Arc<Runtime>> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let storage = StorageHandle::open(paths.database_path())?;
    let use_config = provider.is_none() && fake.is_none();
    let choice = provider.unwrap_or(ProviderChoice::Fake);
    let configured = if use_config {
        let registry = ProviderRegistry::load(&paths)?;
        (!registry.is_empty()).then_some(registry)
    } else {
        None
    };

    let runtime = if let Some(registry) = configured {
        Runtime::with_registry(
            storage,
            Arc::new(registry),
            Arc::new(SystemClock),
            Arc::new(UuidGenerator),
            ContextManager::new(paths.clone()),
        )
    } else {
        match choice {
            ProviderChoice::Fake => {
                let script = match fake {
                    Some(path) => FakeScript::from_file(&path).with_context(|| {
                        format!("could not load fake script {}", path.display())
                    })?,
                    None => FakeScript {
                        model: Some(model.unwrap_or_else(|| "fake-model".to_owned())),
                        steps: vec![FakeStep::Text {
                            text: "M0 fake provider ready.".to_owned(),
                        }],
                    },
                };
                let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("fake", script));
                Runtime::new(
                    storage,
                    provider,
                    Arc::new(SystemClock),
                    Arc::new(UuidGenerator),
                    ContextManager::new(paths.clone()),
                )
            }
            ProviderChoice::Cohere => {
                if let Ok(registry) = ProviderRegistry::preset(
                    &paths,
                    "cohere",
                    model.clone(),
                    serde_json::Value::Null,
                ) {
                    Runtime::with_registry(
                        storage,
                        Arc::new(registry),
                        Arc::new(SystemClock),
                        Arc::new(UuidGenerator),
                        ContextManager::new(paths.clone()),
                    )
                } else {
                    let key = std::env::var("THAIS_COHERE_API_KEY")
                        .or_else(|_| std::env::var("COHERE_API_KEY"))
                        .context(
                            "set THAIS_COHERE_API_KEY or COHERE_API_KEY for `--provider cohere`",
                        )?;
                    let model = model.unwrap_or_else(|| COHERE_DEFAULT_MODEL.to_owned());
                    let provider: Arc<dyn Provider> =
                        Arc::new(OpenAiCompatibleProvider::cohere(key, model)?);
                    Runtime::new(
                        storage,
                        provider,
                        Arc::new(SystemClock),
                        Arc::new(UuidGenerator),
                        ContextManager::new(paths.clone()),
                    )
                }
            }
            other => {
                let id = other.preset_id().context("provider preset not found")?;
                let registry =
                    ProviderRegistry::preset(&paths, id, model, serde_json::Value::Null)?;
                Runtime::with_registry(
                    storage,
                    Arc::new(registry),
                    Arc::new(SystemClock),
                    Arc::new(UuidGenerator),
                    ContextManager::new(paths.clone()),
                )
            }
        }
    };

    Ok(Arc::new(
        runtime
            .with_snapshot_base(paths.snapshots_dir())
            .with_checkpoint_base(paths.sessions_dir())
            .with_verifier(Verifier::load(&paths.config_file("verifier.toml"))?),
    ))
}

async fn run(
    fake: Option<PathBuf>,
    provider: Option<ProviderChoice>,
    mode: ModeChoice,
    model: Option<String>,
    json: bool,
    task: Option<String>,
    workspace: Option<PathBuf>,
) -> anyhow::Result<()> {
    let workspace = match workspace {
        Some(workspace) => workspace,
        None => std::env::current_dir().context("could not determine the current directory")?,
    };
    let task = task.unwrap_or_else(|| "Say hello from the fake provider.".to_owned());
    let runtime = build_runtime(fake, provider, model.clone())?;
    let (session, turn_id, events) = runtime
        .run_once_mode(
            workspace.to_string_lossy().into_owned(),
            task,
            model,
            mode.into(),
        )
        .await?;

    if json {
        for event in &events {
            println!("{}", serde_json::to_string(event)?);
        }
    } else {
        print_human(&events);
    }

    tracing::info!(
        session_id = %session.session_id,
        turn_id = %turn_id,
        events = events.len(),
        "turn complete"
    );
    Ok(())
}

async fn tui(
    provider: Option<ProviderChoice>,
    mode: ModeChoice,
    model: Option<String>,
    workspace: Option<PathBuf>,
) -> anyhow::Result<()> {
    let workspace = match workspace {
        Some(workspace) => workspace.to_string_lossy().into_owned(),
        None => std::env::current_dir()
            .context("could not determine the current directory")?
            .to_string_lossy()
            .into_owned(),
    };
    let runtime = build_runtime(None, provider, model.clone())?;
    let (client_transport, mut server) = channel::pair();
    let request_sender = server.sender();
    let event_sender = server.sender();
    let request_runtime = Arc::clone(&runtime);

    tokio::spawn(async move {
        while let Some(request) = server.recv().await {
            let outcome = request_runtime.handle_request(request).await;
            if request_sender
                .send(Message::Response(outcome.response))
                .await
                .is_err()
            {
                break;
            }
            for event in outcome.replay {
                let notification =
                    Notification::new(method::SESSION_EVENT, serde_json::json!({"event": event}));
                if request_sender
                    .send(Message::Notification(notification))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });

    let mut live = runtime.subscribe_live();
    tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(event) => {
                    let notification = Notification::new(
                        method::SESSION_EVENT,
                        serde_json::json!({"event": event}),
                    );
                    if event_sender
                        .send(Message::Notification(notification))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    let client = thais_sdk::Client::new(client_transport);
    thais_tui::run(client, workspace, model, mode.into()).await?;
    Ok(())
}

fn print_human(events: &[EventEnvelope]) {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();

    for event in events {
        match event.kind {
            EventKind::ModelDelta => {
                if let Some(text) = event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                {
                    let _ = write!(stdout, "{text}");
                }
            }
            EventKind::ReasoningDelta => {
                if let Some(text) = event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                {
                    let _ = writeln!(stderr, "reasoning: {text}");
                }
            }
            EventKind::TurnCompleted => {
                let _ = writeln!(stdout);
                let _ = stdout.flush();
            }
            EventKind::TurnCancelled => {
                let _ = writeln!(stderr, "cancelled");
            }
            EventKind::Error => {
                let message = event
                    .payload
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error");
                let _ = writeln!(stderr, "error: {message}");
            }
            EventKind::ToolCall => {
                let name = event
                    .payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool");
                let args = event
                    .payload
                    .get("arguments")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let _ = writeln!(stderr, "tool: {name} {args}");
            }
            EventKind::ToolResult => {
                if let Some(diff) = event
                    .payload
                    .get("diff")
                    .and_then(serde_json::Value::as_str)
                {
                    let _ = writeln!(stderr, "{diff}");
                }
            }
            EventKind::PermissionRequested => {
                let _ = writeln!(stderr, "permission requested");
            }
            EventKind::PermissionGranted => {
                let _ = writeln!(stderr, "permission granted");
            }
            EventKind::PermissionDenied => {
                let _ = writeln!(stderr, "permission denied");
            }
            EventKind::PermissionResolved => {
                let _ = writeln!(stderr, "permission resolved");
            }
            EventKind::SnapshotCreated => {
                let _ = writeln!(stderr, "snapshot created");
            }
            EventKind::RollbackCompleted => {
                let _ = writeln!(stderr, "changes rolled back");
            }
            EventKind::VerificationStarted => {
                let _ = writeln!(stderr, "verifying...");
            }
            EventKind::VerificationPassed => {
                let _ = writeln!(stderr, "verification passed");
            }
            EventKind::VerificationFailed => {
                let output = event
                    .payload
                    .get("output")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let _ = writeln!(stderr, "verification failed:\n{output}");
            }
            EventKind::VerificationUnavailable => {
                let reason = event
                    .payload
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unavailable");
                let _ = writeln!(stderr, "verification unavailable: {reason}");
            }
            EventKind::SessionCreated | EventKind::TurnStarted => {}
            EventKind::ModelChanged => {
                let model = event
                    .payload
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let _ = writeln!(stderr, "model changed: {model}");
            }
            EventKind::BudgetWarning => {
                let budget = event
                    .payload
                    .get("budget")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("budget");
                let _ = writeln!(stderr, "{budget} budget warning");
            }
            EventKind::ContextRebuilt => {
                let _ = writeln!(stderr, "context rebuilt");
            }
            EventKind::ContextRebuildFailed => {
                let _ = writeln!(stderr, "context rebuild failed; using checkpoint");
            }
            EventKind::MemoryUpdated => {
                let rule = event
                    .payload
                    .get("rule")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("rule");
                let _ = writeln!(stderr, "memory updated: {rule}");
            }
        }
    }
}

fn doctor() -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    let mut healthy = true;

    println!("Thais doctor");
    println!("version: {}", env!("CARGO_PKG_VERSION"));
    println!("config: {}", paths.config_dir().display());
    println!("data:   {}", paths.data_dir().display());
    println!("state:  {}", paths.state_dir().display());
    println!("cache:  {}", paths.cache_dir().display());

    if let Err(error) = paths.ensure() {
        println!("[fail] could not create user directories: {error}");
        healthy = false;
    } else {
        for (label, path) in [
            ("config", paths.config_dir().to_path_buf()),
            ("data", paths.data_dir().to_path_buf()),
            ("state", paths.state_dir().to_path_buf()),
            ("cache", paths.cache_dir().to_path_buf()),
            ("sessions", paths.sessions_dir()),
            ("snapshots", paths.snapshots_dir()),
        ] {
            healthy &= check_private_dir(label, &path);
        }
    }

    let database = paths.database_path();
    match Storage::open(&database) {
        Ok(storage) => {
            println!("[ok] database: {}", database.display());
            match storage.schema_version() {
                Ok(version) => println!("[ok] schema version: {version}"),
                Err(error) => {
                    println!("[fail] schema version: {error}");
                    healthy = false;
                }
            }
            match storage.has_fts5() {
                Ok(true) => println!("[ok] SQLite FTS5 available"),
                Ok(false) => {
                    println!("[fail] SQLite FTS5 is not available");
                    healthy = false;
                }
                Err(error) => {
                    println!("[fail] could not check SQLite FTS5: {error}");
                    healthy = false;
                }
            }
            match storage.integrity_check() {
                Ok(report) if report == "ok" => println!("[ok] integrity check: ok"),
                Ok(report) => {
                    println!("[fail] integrity check: {report}");
                    healthy = false;
                }
                Err(error) => {
                    println!("[fail] integrity check: {error}");
                    healthy = false;
                }
            }
        }
        Err(error) => {
            println!("[fail] could not open {}: {error}", database.display());
            healthy = false;
        }
    }

    let credentials = CredentialStore::new(paths);
    println!(
        "[info] keyring backend: {} (available: {})",
        credentials.keyring_name(),
        credentials.keyring_available()
    );
    if credentials.fallback_file_exists() {
        println!("[warn] 0600 credential fallback file is present");
    } else {
        println!("[ok] no credential fallback file");
    }

    if !healthy {
        bail!("doctor found problems");
    }
    Ok(())
}

fn check_private_dir(label: &str, path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = metadata.permissions().mode() & 0o777;
                if mode != 0o700 {
                    println!("[fail] {label}: mode {mode:o}, expected 700");
                    return false;
                }
            }
            println!("[ok] {label}: {}", path.display());
            true
        }
        Ok(_) => {
            println!("[fail] {label}: not a directory");
            false
        }
        Err(error) => {
            println!("[fail] {label}: {error}");
            false
        }
    }
}

fn auth(command: AuthCommand) -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let store = CredentialStore::new(paths);

    match command {
        AuthCommand::Login { provider, account } => {
            let account = account.unwrap_or_else(|| "default".to_owned());
            eprintln!("Paste the API key for {provider} ({account}) and press Enter:");
            let mut secret = String::new();
            std::io::stdin().read_to_string(&mut secret)?;
            let secret = secret.trim();
            if secret.is_empty() {
                bail!("empty secret");
            }
            let source = store.set(&provider, &account, secret)?;
            println!("stored {provider}:{account} in {}", source.as_str());
            Ok(())
        }
        AuthCommand::Logout { provider, account } => {
            let account = account.unwrap_or_else(|| "default".to_owned());
            let removed = store.delete(&provider, &account)?;
            println!(
                "{} {provider}:{account}",
                if removed {
                    "removed"
                } else {
                    "no credential for"
                }
            );
            Ok(())
        }
        AuthCommand::Status => {
            println!(
                "keyring backend: {} (available: {})",
                store.keyring_name(),
                store.keyring_available()
            );
            println!(
                "fallback file: {}",
                if store.fallback_file_exists() {
                    "present"
                } else {
                    "absent"
                }
            );
            let mut names: Vec<String> = std::env::vars()
                .filter(|(key, _)| key.starts_with("THAIS_") && key.ends_with("_API_KEY"))
                .map(|(key, _)| key)
                .collect();
            names.sort();
            if names.is_empty() {
                println!("environment: no THAIS_*_API_KEY variables");
            } else {
                println!("environment: {}", names.join(", "));
            }
            Ok(())
        }
    }
}

fn trust(status: bool, remove: bool) -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let storage = Storage::open(paths.database_path())?;
    let workspace = std::env::current_dir().context("could not determine the current directory")?;
    let (canonical, hash) = trust_key(&workspace)?;

    if status {
        println!("workspace: {canonical}");
        println!("config hash: {hash}");
        println!("trusted: {}", storage.is_trusted(&canonical, &hash)?);
        return Ok(());
    }

    if remove {
        let removed = storage.remove_trust(&canonical)?;
        println!("removed {removed} trust record(s) for {canonical}");
        return Ok(());
    }

    storage.trust_workspace(&canonical, &hash)?;
    println!("trusted {canonical}");
    println!("config hash: {hash}");
    Ok(())
}
