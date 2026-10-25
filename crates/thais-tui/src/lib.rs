#![forbid(unsafe_code)]

//! Minimal TUI: streaming transcript, tool cards, approval modal, slash
//! commands, Markdown, scroll, and a status bar. The TUI depends only on the
//! SDK and protocol.

mod commands;
mod markdown;

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::json;
use thais_protocol::event::{EventEnvelope, EventKind};
use thais_protocol::jsonrpc::Notification;
use thais_protocol::method;
use thais_protocol::session::{
    ApprovalDecision, HistorySearchParams, InitializeParams, SessionApproveParams,
    SessionArchiveParams, SessionCancelParams, SessionCreateParams, SessionDeleteParams,
    SessionInfo, SessionListParams, SessionMode, SessionReleaseLeaseParams, SessionResumeParams,
    SessionSendParams, SessionSetModelParams, SessionSubscribeParams,
};
use thais_sdk::{Client, Transport};
use tokio::sync::mpsc;

use commands::SlashCommand;
use markdown::markdown_lines;

const MAX_RENDERED_LINES: usize = 4_000;

/// Why the TUI exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiOutcome {
    /// Normal quit.
    Quit,
    /// A provider was connected; rebuild the runtime and start again.
    Restart,
}

/// Start the TUI against a connected client.
pub async fn run<T: Transport + 'static>(
    client: Client<T>,
    workspace: String,
    model: Option<String>,
    mode: SessionMode,
) -> Result<TuiOutcome> {
    let mut notifications = client.notifications();
    client
        .initialize(InitializeParams {
            client_name: "thais-tui".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: thais_protocol::PROTOCOL_VERSION.to_owned(),
            capabilities: json!({}),
        })
        .await
        .context("initialize")?;
    let created = client
        .session_create(SessionCreateParams {
            workspace,
            name: None,
            parent_id: None,
            mode,
            model,
            steal_lease: false,
        })
        .await
        .context("session.create")?;
    let session = created.session;
    client
        .session_subscribe(SessionSubscribeParams {
            session_id: session.session_id.clone(),
            from_seq: None,
        })
        .await
        .context("session.subscribe")?;

    let mut app = AppState::new(session);
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("terminal")?;

    let result = event_loop(&mut terminal, &client, &mut app, &mut notifications).await;
    let _ = client
        .session_release_lease(SessionReleaseLeaseParams {
            session_id: app.session_id.clone(),
        })
        .await;

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();
    result
}

#[derive(Clone, Default)]
struct UsageView {
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

#[derive(Clone)]
struct ConnectState {
    provider: String,
    model: Option<String>,
}

#[derive(Clone)]
struct PendingApproval {
    approval_id: String,
    action: String,
    diff: Option<String>,
}

struct AppState {
    session_id: String,
    session_name: String,
    workspace: String,
    mode: String,
    model: String,
    transcript: String,
    reasoning: String,
    input: String,
    status: String,
    pending: Option<PendingApproval>,
    usage: UsageView,
    sessions: Vec<SessionInfo>,
    picker: Option<usize>,
    streaming: bool,
    auto_scroll: bool,
    scroll: u16,
    max_scroll: u16,
    viewport_height: u16,
    suggestions: Vec<&'static SlashCommand>,
    suggestion_index: usize,
    connect: Option<ConnectState>,
    restart: bool,
    should_quit: bool,
}

impl AppState {
    fn new(session: SessionInfo) -> Self {
        Self {
            session_id: session.session_id,
            session_name: session.name,
            workspace: session.workspace,
            mode: match session.mode {
                SessionMode::Plan => "plan".to_owned(),
                SessionMode::Build => "build".to_owned(),
            },
            model: session.model,
            transcript: String::new(),
            reasoning: String::new(),
            input: String::new(),
            status: "ready".to_owned(),
            pending: None,
            usage: UsageView {
                input_tokens: session.input_tokens,
                output_tokens: session.output_tokens,
                cost_usd: session.cost_usd,
            },
            sessions: Vec::new(),
            picker: None,
            streaming: false,
            auto_scroll: true,
            scroll: 0,
            max_scroll: 0,
            viewport_height: 0,
            suggestions: Vec::new(),
            suggestion_index: 0,
            connect: None,
            restart: false,
            should_quit: false,
        }
    }

    fn append(&mut self, text: &str) {
        if !self.transcript.is_empty() && !self.transcript.ends_with('\n') {
            self.transcript.push('\n');
        }
        self.transcript.push_str(text);
        if !text.ends_with('\n') {
            self.transcript.push('\n');
        }
    }

    fn refresh_suggestions(&mut self) {
        self.suggestions = commands::matching(&self.input);
        self.suggestion_index = 0;
    }

    fn accept_suggestion(&mut self) {
        if let Some(input) = commands::complete(&self.input, self.suggestion_index) {
            self.input = input;
            self.refresh_suggestions();
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        if self.max_scroll == 0 {
            self.auto_scroll = true;
            self.scroll = 0;
            return;
        }
        if delta < 0 {
            self.auto_scroll = false;
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs() as u16);
        } else {
            let next = self.scroll.saturating_add(delta as u16);
            if next >= self.max_scroll {
                self.scroll = self.max_scroll;
                self.auto_scroll = true;
            } else {
                self.scroll = next;
                self.auto_scroll = false;
            }
        }
    }

    fn scroll_to_top(&mut self) {
        self.auto_scroll = false;
        self.scroll = 0;
    }

    fn scroll_page(&mut self, up: bool) {
        let amount = i32::from(self.viewport_height.saturating_sub(2).max(5));
        self.scroll_by(if up { -amount } else { amount });
    }

    fn apply_notification(&mut self, notification: Notification) {
        if notification.method != method::SESSION_EVENT {
            return;
        }
        let Some(value) = notification.params.get("event") else {
            return;
        };
        let Ok(event) = serde_json::from_value::<EventEnvelope>(value.clone()) else {
            return;
        };
        if event.session_id != self.session_id {
            return;
        }

        match event.kind {
            EventKind::ModelDelta => {
                if let Some(text) = event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                {
                    self.transcript.push_str(text);
                    self.streaming = true;
                    self.status = "streaming".to_owned();
                }
            }
            EventKind::ReasoningDelta => {
                if let Some(text) = event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                {
                    self.reasoning.push_str(text);
                    self.status = "thinking".to_owned();
                }
            }
            EventKind::ToolCall => {
                let name = event
                    .payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool");
                let arguments = event
                    .payload
                    .get("arguments")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                self.transcript
                    .push_str(&format!("\n[tool] {name} {}\n", truncate(arguments, 300)));
                self.status = "tool call".to_owned();
            }
            EventKind::ToolResult => {
                let content = event
                    .payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let is_error = event
                    .payload
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                self.transcript.push_str(&format!(
                    "[{}] {}\n",
                    if is_error { "error" } else { "result" },
                    truncate(content, 500)
                ));
                if let Some(diff) = event
                    .payload
                    .get("diff")
                    .and_then(serde_json::Value::as_str)
                {
                    self.transcript
                        .push_str(&format!("[diff]\n{}\n", truncate(diff, 1200)));
                }
                self.status = "tool result".to_owned();
            }
            EventKind::PermissionRequested => {
                let approval_id = event
                    .payload
                    .get("approval_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let action = event
                    .payload
                    .get("action")
                    .cloned()
                    .unwrap_or_default()
                    .to_string();
                let diff = event
                    .payload
                    .get("action")
                    .and_then(|action| action.get("diff"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                self.pending = Some(PendingApproval {
                    approval_id,
                    action,
                    diff,
                });
                self.status = "approval required (y/n)".to_owned();
            }
            EventKind::PermissionGranted => {
                self.status = "permission granted".to_owned();
            }
            EventKind::PermissionDenied => {
                let reason = event
                    .payload
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("denied");
                self.transcript
                    .push_str(&format!("\n[permission denied] {reason}\n"));
                self.status = "permission denied".to_owned();
            }
            EventKind::PermissionResolved => {
                self.pending = None;
                self.status = "permission resolved".to_owned();
            }
            EventKind::TurnCompleted => {
                self.streaming = false;
                self.transcript.push('\n');
                self.status = "ready".to_owned();
                if let Some(usage) = event.payload.get("session_usage") {
                    self.usage.input_tokens = usage
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    self.usage.output_tokens = usage
                        .get("output_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    self.usage.cost_usd = usage
                        .get("cost_usd")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                }
            }
            EventKind::TurnCancelled => {
                self.streaming = false;
                self.transcript.push_str("\n[cancelled]\n");
                self.status = "cancelled".to_owned();
            }
            EventKind::Error => {
                self.streaming = false;
                let message = event
                    .payload
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error");
                self.transcript.push_str(&format!("\n[error] {message}\n"));
                self.status = "error".to_owned();
            }
            EventKind::SnapshotCreated => {
                self.status = "snapshot created".to_owned();
            }
            EventKind::RollbackCompleted => {
                self.transcript.push_str("\n[rollback completed]\n");
                self.status = "rolled back".to_owned();
            }
            EventKind::VerificationStarted => {
                self.status = "verifying".to_owned();
            }
            EventKind::VerificationPassed => {
                self.status = "verified".to_owned();
            }
            EventKind::VerificationFailed => {
                let output = event
                    .payload
                    .get("output")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                self.transcript.push_str(&format!(
                    "\n[verification failed]\n{}\n",
                    truncate(output, 800)
                ));
                self.status = "verification failed".to_owned();
            }
            EventKind::VerificationUnavailable => {
                self.status = "unverified".to_owned();
            }
            EventKind::SessionCreated | EventKind::TurnStarted => {}
            EventKind::ModelChanged => {
                if let Some(model) = event
                    .payload
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                {
                    self.model = model.to_owned();
                }
                self.status = "model changed".to_owned();
            }
            EventKind::BudgetWarning => {
                let budget = event
                    .payload
                    .get("budget")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("budget");
                self.transcript
                    .push_str(&format!("\n[{budget} budget warning]\n"));
                self.status = "budget warning".to_owned();
            }
            EventKind::ContextRebuilt => {
                self.transcript.push_str("\n[context rebuilt]\n");
                self.status = "context rebuilt".to_owned();
            }
            EventKind::ContextRebuildFailed => {
                self.transcript
                    .push_str("\n[context rebuild failed; using checkpoint]\n");
                self.status = "rebuild fallback".to_owned();
            }
            EventKind::LspDiagnostics => {
                let path = event
                    .payload
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("file");
                self.transcript
                    .push_str(&format!("\n[lsp] {path}: diagnostics updated\n"));
            }
            EventKind::MemoryUpdated => {
                let rule = event
                    .payload
                    .get("rule")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("rule");
                self.transcript
                    .push_str(&format!("\n[memory] {}\n", truncate(rule, 200)));
                self.status = "memory updated".to_owned();
            }
        }
    }
}

async fn event_loop<T: Transport>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &Client<T>,
    app: &mut AppState,
    notifications: &mut futures_util::stream::BoxStream<'static, Notification>,
) -> Result<TuiOutcome> {
    let (key_sender, mut key_receiver) = mpsc::channel(64);
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if key_sender.blocking_send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let mut tick = tokio::time::interval(Duration::from_millis(80));
    loop {
        terminal.draw(|frame| draw_ui(frame, app))?;
        if app.should_quit {
            break;
        }
        tokio::select! {
            Some(event) = key_receiver.recv() => {
                handle_key(app, event, client).await?;
            }
            notification = notifications.next() => {
                if let Some(notification) = notification {
                    app.apply_notification(notification);
                }
            }
            _ = tick.tick() => {}
        }
    }
    Ok(if app.restart {
        TuiOutcome::Restart
    } else {
        TuiOutcome::Quit
    })
}

async fn handle_key<T: Transport>(
    app: &mut AppState,
    event: Event,
    client: &Client<T>,
) -> Result<()> {
    match event {
        Event::Mouse(mouse) => {
            handle_mouse(app, mouse.kind);
            return Ok(());
        }
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            handle_key_press(app, key, client).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_key_press<T: Transport>(
    app: &mut AppState,
    key: crossterm::event::KeyEvent,
    client: &Client<T>,
) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                app.should_quit = true;
                return Ok(());
            }
            KeyCode::Char('s') => {
                if let Err(error) = open_session_picker(app, client).await {
                    app.append(&format!("[error] {error}"));
                    app.status = "error".to_owned();
                }
                return Ok(());
            }
            KeyCode::Char('l') => {
                app.transcript.clear();
                app.reasoning.clear();
                app.scroll = 0;
                app.auto_scroll = true;
                return Ok(());
            }
            KeyCode::Char('u') => {
                app.input.clear();
                app.refresh_suggestions();
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    if app.picker.is_some() {
        return handle_picker_key(app, key, client).await;
    }

    if let Some(pending) = app.pending.clone() {
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Allow),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalDecision::Deny),
            _ => None,
        };
        if let Some(decision) = decision {
            client
                .session_approve(SessionApproveParams {
                    session_id: app.session_id.clone(),
                    approval_id: pending.approval_id,
                    decision,
                })
                .await?;
            app.pending = None;
            app.status = "resolving approval".to_owned();
        }
        return Ok(());
    }

    if app.connect.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.connect = None;
                app.input.clear();
                app.status = "connect cancelled".to_owned();
            }
            KeyCode::Enter => perform_connect(app).await?,
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(character) => app.input.push(character),
            _ => {}
        }
        return Ok(());
    }

    if !app.suggestions.is_empty() {
        match key.code {
            KeyCode::Up => {
                app.suggestion_index = app.suggestion_index.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down => {
                if app.suggestion_index + 1 < app.suggestions.len() {
                    app.suggestion_index += 1;
                }
                return Ok(());
            }
            KeyCode::Tab => {
                app.accept_suggestion();
                return Ok(());
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter => {
            let input = app.input.trim().to_owned();
            if input.is_empty() {
                return Ok(());
            }
            if input.starts_with('/') {
                if commands::parse(&input).is_some() {
                    if let Err(error) = execute_command(app, client, &input).await {
                        app.append(&format!("[error] {error}"));
                        app.status = "error".to_owned();
                    }
                } else if !app.suggestions.is_empty() {
                    app.accept_suggestion();
                } else {
                    app.append(&format!("unknown command: {input}\nUse /help."));
                }
                return Ok(());
            }
            if app.streaming {
                return Ok(());
            }
            app.input.clear();
            app.suggestions.clear();
            app.append(&format!("> {input}"));
            app.streaming = true;
            app.status = "thinking".to_owned();
            if let Err(error) = client
                .session_send(SessionSendParams {
                    session_id: app.session_id.clone(),
                    input,
                    turn_id: None,
                    model: None,
                })
                .await
            {
                app.streaming = false;
                app.append(&format!("[error] {error}"));
                app.status = "error".to_owned();
            }
        }
        KeyCode::Tab => {
            app.accept_suggestion();
        }
        KeyCode::Char(character) => {
            app.input.push(character);
            app.refresh_suggestions();
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.refresh_suggestions();
        }
        KeyCode::Esc => {
            if !app.suggestions.is_empty() {
                app.suggestions.clear();
            } else if app.streaming {
                client
                    .session_cancel(SessionCancelParams {
                        session_id: app.session_id.clone(),
                        turn_id: None,
                    })
                    .await?;
                app.status = "cancelling".to_owned();
            } else if !app.input.is_empty() {
                app.input.clear();
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::PageUp => app.scroll_page(true),
        KeyCode::PageDown => app.scroll_page(false),
        KeyCode::Home => app.scroll_to_top(),
        KeyCode::End => {
            app.scroll = app.max_scroll;
            app.auto_scroll = true;
        }
        _ => {}
    }
    Ok(())
}

fn handle_mouse(app: &mut AppState, kind: MouseEventKind) {
    if app.picker.is_some() || app.pending.is_some() {
        return;
    }
    match kind {
        MouseEventKind::ScrollUp => app.scroll_by(-3),
        MouseEventKind::ScrollDown => app.scroll_by(3),
        _ => {}
    }
}

async fn execute_command<T: Transport>(
    app: &mut AppState,
    client: &Client<T>,
    input: &str,
) -> Result<()> {
    let Some((command, argument)) = commands::parse(input) else {
        app.append("unknown command\nUse /help.");
        return Ok(());
    };
    app.input.clear();
    app.refresh_suggestions();
    if command.name != "clear" && command.name != "quit" {
        app.append(&format!("> {input}"));
    }
    match command.name {
        "help" => app.append(&commands::help_text()),
        "clear" => {
            app.transcript.clear();
            app.reasoning.clear();
            app.scroll = 0;
            app.auto_scroll = true;
            app.status = "cleared".to_owned();
        }
        "quit" => app.should_quit = true,
        "connect" => {
            let mut parts = argument.split_whitespace();
            let provider = parts.next();
            let model = parts.next().map(str::to_owned);
            match provider {
                Some(provider)
                    if commands::PROVIDERS
                        .iter()
                        .any(|(candidate, _)| *candidate == provider) =>
                {
                    app.connect = Some(ConnectState {
                        provider: provider.to_owned(),
                        model,
                    });
                    app.input.clear();
                    app.refresh_suggestions();
                    app.status = format!("paste API key for {provider}");
                    app.append(&format!(
                        "Connecting {provider}. Paste the API key and press Enter.\nThe key is not written to the transcript."
                    ));
                }
                Some(provider) => app.append(&format!(
                    "unknown provider: {provider}\n{}",
                    commands::connect_help()
                )),
                None => app.append(&commands::connect_help()),
            }
        }
        "status" => app.append(&status_line(app)),
        "sessions" => open_session_picker(app, client).await?,
        "new" => new_session(app, client).await?,
        "model" => {
            if argument.is_empty() {
                let result = client.models_list().await?;
                let mut text = String::from("Models\n");
                for model in result.models.iter().take(30) {
                    text.push_str(&format!(
                        "  {:<32} {:<12} {}k ctx\n",
                        model.id,
                        model.provider,
                        model.context_window / 1_000
                    ));
                }
                text.push_str("\nUse /model <id> to change the default model.");
                app.append(&text);
            } else {
                let result = client
                    .session_set_model(SessionSetModelParams {
                        session_id: app.session_id.clone(),
                        role: "default".to_owned(),
                        model: argument.to_owned(),
                    })
                    .await?;
                app.model = result.session.model;
                app.status = "model changed".to_owned();
                app.append(&format!("model: {}", app.model));
            }
        }
        "history" => {
            if argument.is_empty() {
                app.append("usage: /history <query>");
            } else {
                let result = client
                    .history_search(HistorySearchParams {
                        query: argument.to_owned(),
                        session_id: Some(app.session_id.clone()),
                        workspace: None,
                        limit: Some(10),
                    })
                    .await?;
                app.append(&format!("History: {argument}"));
                for hit in result.matches {
                    let payload = serde_json::to_string(&hit.event.payload)
                        .unwrap_or_default()
                        .chars()
                        .take(180)
                        .collect::<String>();
                    app.append(&format!(
                        "  {} seq {} {}: {}",
                        hit.session_name,
                        hit.event.seq,
                        hit.event.kind.as_str(),
                        payload
                    ));
                }
            }
        }
        "cancel" => {
            if app.streaming {
                client
                    .session_cancel(SessionCancelParams {
                        session_id: app.session_id.clone(),
                        turn_id: None,
                    })
                    .await?;
                app.status = "cancelling".to_owned();
            } else {
                app.append("no active turn");
            }
        }
        "approve" | "deny" => {
            let Some(pending) = app.pending.clone() else {
                app.append("no pending approval");
                return Ok(());
            };
            client
                .session_approve(SessionApproveParams {
                    session_id: app.session_id.clone(),
                    approval_id: pending.approval_id,
                    decision: if command.name == "approve" {
                        ApprovalDecision::Allow
                    } else {
                        ApprovalDecision::Deny
                    },
                })
                .await?;
            app.pending = None;
            app.status = "resolving approval".to_owned();
        }
        _ => app.append("command is not implemented"),
    }
    Ok(())
}

async fn perform_connect(app: &mut AppState) -> Result<()> {
    let Some(connect) = app.connect.take() else {
        return Ok(());
    };
    let secret = app.input.trim().to_owned();
    app.input.clear();
    app.status = "connecting".to_owned();
    if secret.is_empty() {
        app.append("empty API key; connect cancelled");
        app.status = "connect failed".to_owned();
        return Ok(());
    }
    let executable = std::env::current_exe().context("current executable")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("connect")
        .arg(&connect.provider)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(model) = &connect.model {
        command.arg("--model").arg(model);
    }
    let mut child = command.spawn().context("start thais connect")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(secret.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
        app.append(&format!(
            "Provider {} connected. Restarting the TUI...",
            connect.provider
        ));
        app.restart = true;
        app.should_quit = true;
    } else {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        app.append(&format!(
            "connect failed{}{}",
            if error.is_empty() { "" } else { ": " },
            error
        ));
        app.status = "connect failed".to_owned();
    }
    Ok(())
}

async fn open_session_picker<T: Transport>(app: &mut AppState, client: &Client<T>) -> Result<()> {
    app.sessions = client
        .session_list(SessionListParams {
            include_archived: true,
        })
        .await?
        .sessions;
    app.picker = Some(0);
    app.status = "sessions".to_owned();
    Ok(())
}

async fn new_session<T: Transport>(app: &mut AppState, client: &Client<T>) -> Result<()> {
    let _ = client
        .session_release_lease(SessionReleaseLeaseParams {
            session_id: app.session_id.clone(),
        })
        .await;
    let mode = mode_from_str(&app.mode);
    let created = client
        .session_create(SessionCreateParams {
            workspace: app.workspace.clone(),
            name: None,
            parent_id: None,
            mode,
            model: None,
            steal_lease: false,
        })
        .await?;
    let session = created.session;
    app.session_id = session.session_id.clone();
    app.session_name = session.name;
    app.workspace = session.workspace;
    app.mode = session.mode.as_str().to_owned();
    app.model = session.model;
    app.usage = UsageView {
        input_tokens: session.input_tokens,
        output_tokens: session.output_tokens,
        cost_usd: session.cost_usd,
    };
    app.transcript.clear();
    app.reasoning.clear();
    app.pending = None;
    app.streaming = false;
    app.scroll = 0;
    app.auto_scroll = true;
    app.suggestions.clear();
    client
        .session_subscribe(SessionSubscribeParams {
            session_id: app.session_id.clone(),
            from_seq: Some(0),
        })
        .await?;
    app.status = "new session".to_owned();
    Ok(())
}

fn mode_from_str(mode: &str) -> SessionMode {
    if mode == "plan" {
        SessionMode::Plan
    } else {
        SessionMode::Build
    }
}

fn status_line(app: &AppState) -> String {
    format!(
        "{} {}\nmode: {}\nmodel: {}\nworkspace: {}\ntokens: {} in / {} out\ncost: ${:.4}\nscroll: {}/{}\nstatus: {}",
        short_id(&app.session_id),
        app.session_name,
        app.mode,
        app.model,
        app.workspace,
        app.usage.input_tokens,
        app.usage.output_tokens,
        app.usage.cost_usd,
        app.scroll,
        app.max_scroll,
        app.status
    )
}

async fn handle_picker_key<T: Transport>(
    app: &mut AppState,
    key: crossterm::event::KeyEvent,
    client: &Client<T>,
) -> Result<()> {
    let Some(mut selected) = app.picker else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc => {
            app.picker = None;
            app.status = "ready".to_owned();
        }
        KeyCode::Up => {
            selected = selected.saturating_sub(1);
            app.picker = Some(selected);
        }
        KeyCode::Down => {
            if selected + 1 < app.sessions.len() {
                selected += 1;
            }
            app.picker = Some(selected);
        }
        KeyCode::Enter => {
            let Some(session) = app.sessions.get(selected).cloned() else {
                return Ok(());
            };
            let _ = client
                .session_release_lease(SessionReleaseLeaseParams {
                    session_id: app.session_id.clone(),
                })
                .await;
            let resumed = client
                .session_resume(SessionResumeParams {
                    session_id: session.session_id.clone(),
                })
                .await?;
            app.session_id = resumed.session.session_id.clone();
            app.session_name = resumed.session.name.clone();
            app.workspace = resumed.session.workspace.clone();
            app.mode = resumed.session.mode.as_str().to_owned();
            app.model = resumed.session.model.clone();
            app.transcript.clear();
            app.reasoning.clear();
            app.usage = UsageView {
                input_tokens: resumed.session.input_tokens,
                output_tokens: resumed.session.output_tokens,
                cost_usd: resumed.session.cost_usd,
            };
            app.pending = None;
            app.streaming = false;
            app.auto_scroll = true;
            app.scroll = 0;
            app.suggestions.clear();
            app.picker = None;
            app.status = "resumed".to_owned();
            if let Some(approval) = resumed.pending_approvals.first() {
                app.pending = Some(PendingApproval {
                    approval_id: approval.approval_id.clone(),
                    action: serde_json::to_string_pretty(&approval.action)
                        .unwrap_or_else(|_| approval.action.to_string()),
                    diff: None,
                });
                app.status = "pending approval".to_owned();
            }
            client
                .session_subscribe(SessionSubscribeParams {
                    session_id: resumed.session.session_id,
                    from_seq: Some(0),
                })
                .await?;
        }
        KeyCode::Char('a') => {
            let Some(session) = app.sessions.get(selected).cloned() else {
                return Ok(());
            };
            client
                .session_archive(SessionArchiveParams {
                    session_id: session.session_id.clone(),
                })
                .await?;
            app.sessions = client
                .session_list(SessionListParams {
                    include_archived: true,
                })
                .await?
                .sessions;
            app.picker = Some(selected.min(app.sessions.len().saturating_sub(1)));
            app.status = "archived".to_owned();
        }
        KeyCode::Char('x') => {
            let Some(session) = app.sessions.get(selected).cloned() else {
                return Ok(());
            };
            client
                .session_delete(SessionDeleteParams {
                    session_id: session.session_id.clone(),
                })
                .await?;
            app.sessions = client
                .session_list(SessionListParams {
                    include_archived: true,
                })
                .await?
                .sessions;
            app.picker = Some(selected.min(app.sessions.len().saturating_sub(1)));
            app.status = "deleted".to_owned();
        }
        _ => {}
    }
    Ok(())
}

fn draw_ui(frame: &mut Frame<'_>, app: &mut AppState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let mut lines = markdown_lines(&app.transcript);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Type a message, or /help for commands.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    if lines.len() > MAX_RENDERED_LINES {
        lines = lines.split_off(lines.len() - MAX_RENDERED_LINES);
    }
    let transcript_height = chunks[0].height.saturating_sub(2) as usize;
    app.viewport_height = chunks[0].height;
    app.max_scroll = lines.len().saturating_sub(transcript_height) as u16;
    let scroll = if app.auto_scroll {
        app.max_scroll
    } else {
        app.scroll.min(app.max_scroll)
    };
    app.scroll = scroll;
    let transcript = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " thais ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(transcript, chunks[0]);

    let input_title = if app.connect.is_some() {
        " api key · invisible "
    } else if app.input.is_empty() {
        " message · /help "
    } else {
        " message "
    };
    let displayed_input = if app.connect.is_some() {
        "*".repeat(app.input.chars().count())
    } else {
        app.input.clone()
    };
    let input = Paragraph::new(displayed_input).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                input_title,
                Style::default().fg(Color::DarkGray),
            )),
    );
    frame.render_widget(input, chunks[1]);

    let status = format!(
        "{} · {} · {} · {} · tok {}/{} · ${:.4} · {}/{} · {}",
        short_id(&app.session_id),
        truncate(&app.session_name, 20),
        app.mode,
        truncate(&app.model, 24),
        app.usage.input_tokens,
        app.usage.output_tokens,
        app.usage.cost_usd,
        app.scroll,
        app.max_scroll,
        app.status
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );

    if !app.suggestions.is_empty() {
        draw_autocomplete(frame, chunks[1], app);
    }

    if let Some(selected) = app.picker {
        let area = centered_rect(80, 60, area);
        frame.render_widget(Clear, area);
        let lines = app
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                let marker = if index == selected { ">" } else { " " };
                let style = if index == selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(
                    format!(
                        "{marker} {}  {}  {}  {}  {}",
                        short_id(&session.session_id),
                        truncate(&session.name, 28),
                        session.mode.as_str(),
                        session.status.as_str(),
                        session.model
                    ),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        let modal = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(" sessions · enter resume · a archive · x delete · esc close "),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(modal, area);
    }

    if let Some(pending) = &app.pending {
        let area = centered_rect(75, 35, area);
        frame.render_widget(Clear, area);
        let text = format!(
            "Approval required\n\n{}\n\n[y] allow    [n] deny",
            truncate(&pending.action, 600)
        );
        let text = if let Some(diff) = &pending.diff {
            format!("{text}\n\n{}", truncate(diff, 2000))
        } else {
            text
        };
        let modal = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(Span::styled(
                        " permission ",
                        Style::default().fg(Color::Yellow),
                    )),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(modal, area);
    }
}

fn draw_autocomplete(frame: &mut Frame<'_>, input: Rect, app: &AppState) {
    let visible = app.suggestions.len().min(6);
    let height = visible as u16 + 2;
    let width = frame.area().width.min(76);
    let y = input.y.saturating_sub(height);
    let area = Rect::new(input.x, y, width, height);
    frame.render_widget(Clear, area);
    let start = app
        .suggestion_index
        .saturating_sub(visible.saturating_sub(1))
        .min(app.suggestions.len().saturating_sub(visible));
    let lines = app
        .suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, command)| {
            let invocation = if command.args.is_empty() {
                format!("/{}", command.name)
            } else {
                format!("/{} {}", command.name, command.args)
            };
            let style = if index == app.suggestion_index {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!("{invocation:<22} {}", command.description),
                style,
            ))
        })
        .collect::<Vec<_>>();
    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " commands · tab complete ",
                Style::default().fg(Color::DarkGray),
            )),
    );
    frame.render_widget(popup, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max_chars).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn session() -> SessionInfo {
        SessionInfo {
            session_id: "session_1234567890".to_owned(),
            parent_id: None,
            name: "test".to_owned(),
            workspace: "/tmp".to_owned(),
            mode: SessionMode::Build,
            model: "command-a-plus-05-2026".to_owned(),
            model_policy: json!({"default": "command-a-plus-05-2026"}),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            status: thais_protocol::session::SessionStatus::Active,
            created_at: "2026-10-23T00:00:00Z".to_owned(),
            updated_at: "2026-10-23T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn markdown_lines_handle_fences_tables_and_headings() {
        let lines = markdown_lines("# Title\n| a | b |\n```rust\nlet x = 1;\n```\n");
        assert!(format!("{lines:?}").contains("Title"));
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("| a | b |"))
        );
        assert!(lines.iter().any(|line| line.to_string().contains("let x")));
    }

    #[test]
    fn unclosed_code_fence_does_not_panic() {
        let lines = markdown_lines("```rust\nlet x = 1;");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn model_delta_updates_the_transcript() {
        let mut app = AppState::new(session());
        let notification = Notification::new(
            method::SESSION_EVENT,
            json!({
                "event": {
                    "event_id": "e1",
                    "session_id": app.session_id,
                    "seq": 1,
                    "timestamp": "2026-10-23T00:00:00Z",
                    "kind": "model_delta",
                    "payload": {"text": "hello"}
                }
            }),
        );
        app.apply_notification(notification);
        assert_eq!(app.transcript, "hello");
        assert!(app.streaming);
    }

    #[test]
    fn permission_request_opens_a_diff_modal() {
        let mut app = AppState::new(session());
        let session_id = app.session_id.clone();
        let notification = Notification::new(
            method::SESSION_EVENT,
            json!({
                "event": {
                    "event_id": "e2",
                    "session_id": session_id,
                    "seq": 2,
                    "timestamp": "2026-10-23T00:00:00Z",
                    "kind": "permission_requested",
                    "payload": {
                        "approval_id": "approval_1",
                        "action": {
                            "tool": "write",
                            "diff": "+hello\n"
                        }
                    }
                }
            }),
        );
        app.apply_notification(notification);
        assert!(app.pending.is_some());
        assert_eq!(
            app.pending
                .as_ref()
                .and_then(|pending| pending.diff.as_deref()),
            Some("+hello\n")
        );

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw");
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(content.contains("Approval required"));
        assert!(content.contains("+hello"));
    }

    #[test]
    fn draw_ui_renders_the_session_picker() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = AppState::new(session());
        app.sessions = vec![session()];
        app.picker = Some(0);
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw");
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(content.contains("sessions"));
        assert!(content.contains("resume"));
    }

    #[test]
    fn slash_input_populates_and_completes_autocomplete() {
        let mut app = AppState::new(session());
        app.input = "/he".to_owned();
        app.refresh_suggestions();
        assert_eq!(app.suggestions.len(), 1);
        app.accept_suggestion();
        assert_eq!(app.input, "/help ");
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn scroll_is_clamped_and_returns_to_the_bottom() {
        let mut app = AppState::new(session());
        app.max_scroll = 20;
        app.viewport_height = 12;
        app.scroll_by(-5);
        assert_eq!(app.scroll, 0);
        assert!(!app.auto_scroll);
        app.scroll_by(100);
        assert_eq!(app.scroll, 20);
        assert!(app.auto_scroll);
        app.scroll_to_top();
        assert_eq!(app.scroll, 0);
        assert!(!app.auto_scroll);
    }

    #[test]
    fn draw_ui_uses_no_background_colors() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = AppState::new(session());
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw");
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );
    }

    #[test]
    fn draw_ui_renders_autocomplete() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = AppState::new(session());
        app.input = "/".to_owned();
        app.refresh_suggestions();
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw");
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(content.contains("/help"));
        assert!(content.contains("commands"));
    }

    #[test]
    fn draw_ui_renders_the_status_bar() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = AppState::new(session());
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let content = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(content.contains("thais"));
        assert!(content.contains("build"));
        assert!(content.contains("command-a-plus"));
    }
}
