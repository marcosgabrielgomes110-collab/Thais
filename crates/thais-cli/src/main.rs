#![forbid(unsafe_code)]

//! Command-line entry point for Thais.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use thais_core::{ContextManager, Runtime, SystemClock, UuidGenerator};
use thais_protocol::event::{EventEnvelope, EventKind};
use thais_protocol::jsonrpc::{Message, Notification};
use thais_protocol::method;
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
        /// Model identifier.
        #[arg(long)]
        model: Option<String>,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
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
            model,
            fake,
            json,
            task,
            workspace,
        } => run(fake, provider, model, json, task, workspace).await,
        Command::Tui {
            provider,
            model,
            workspace,
        } => tui(provider, model, workspace).await,
        Command::Doctor => doctor(),
        Command::Auth { command } => auth(command),
        Command::Trust { status, remove } => trust(status, remove),
    }
}

fn build_runtime(
    fake: Option<PathBuf>,
    provider: Option<ProviderChoice>,
    model: Option<String>,
) -> anyhow::Result<Arc<Runtime>> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let storage = StorageHandle::open(paths.database_path())?;

    let provider: Arc<dyn Provider> = match provider.unwrap_or(ProviderChoice::Fake) {
        ProviderChoice::Fake => {
            let script = match fake {
                Some(path) => FakeScript::from_file(&path)
                    .with_context(|| format!("could not load fake script {}", path.display()))?,
                None => FakeScript {
                    model: Some(model.unwrap_or_else(|| "fake-model".to_owned())),
                    steps: vec![FakeStep::Text {
                        text: "M0 fake provider ready.".to_owned(),
                    }],
                },
            };
            Arc::new(FakeProvider::new("fake", script))
        }
        ProviderChoice::Cohere => {
            let key = std::env::var("THAIS_COHERE_API_KEY")
                .or_else(|_| std::env::var("COHERE_API_KEY"))
                .context("set THAIS_COHERE_API_KEY or COHERE_API_KEY for `--provider cohere`")?;
            let model = model.unwrap_or_else(|| COHERE_DEFAULT_MODEL.to_owned());
            Arc::new(OpenAiCompatibleProvider::cohere(key, model)?)
        }
    };

    Ok(Arc::new(Runtime::new(
        storage,
        provider,
        Arc::new(SystemClock),
        Arc::new(UuidGenerator),
        ContextManager::new(paths),
    )))
}

async fn run(
    fake: Option<PathBuf>,
    provider: Option<ProviderChoice>,
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
    let runtime = build_runtime(fake, provider, model)?;
    let (session, turn_id, events) = runtime
        .run_once(workspace.to_string_lossy().into_owned(), task, None)
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
    thais_tui::run(client, workspace, model).await?;
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
            EventKind::SessionCreated
            | EventKind::TurnStarted
            | EventKind::ToolCall
            | EventKind::ToolResult
            | EventKind::PermissionRequested
            | EventKind::PermissionResolved => {}
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
