#![forbid(unsafe_code)]

//! Minimal M1 TUI: streaming transcript, tool cards, approval modal, cancel,
//! scroll, and a status bar. The TUI depends only on the SDK and protocol.

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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
    ApprovalDecision, InitializeParams, SessionApproveParams, SessionCancelParams,
    SessionCreateParams, SessionInfo, SessionMode, SessionSendParams, SessionSubscribeParams,
};
use thais_sdk::{Client, Transport};
use tokio::sync::mpsc;

/// Start the TUI against a connected client.
pub async fn run<T: Transport + 'static>(
    client: Client<T>,
    workspace: String,
    model: Option<String>,
) -> Result<()> {
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
            mode: SessionMode::Build,
            model,
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
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("terminal")?;

    let result = event_loop(&mut terminal, &client, &mut app, &mut notifications).await;

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

#[derive(Clone)]
struct PendingApproval {
    approval_id: String,
    action: String,
}

struct AppState {
    session_id: String,
    mode: String,
    model: String,
    transcript: String,
    reasoning: String,
    input: String,
    status: String,
    pending: Option<PendingApproval>,
    streaming: bool,
    auto_scroll: bool,
    scroll: u16,
    should_quit: bool,
}

impl AppState {
    fn new(session: SessionInfo) -> Self {
        Self {
            session_id: session.session_id,
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
            streaming: false,
            auto_scroll: true,
            scroll: 0,
            should_quit: false,
        }
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
                self.pending = Some(PendingApproval {
                    approval_id,
                    action,
                });
                self.status = "approval required (y/n)".to_owned();
            }
            EventKind::PermissionResolved => {
                self.pending = None;
                self.status = "permission resolved".to_owned();
            }
            EventKind::TurnCompleted => {
                self.streaming = false;
                self.transcript.push('\n');
                self.status = "ready".to_owned();
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
            EventKind::SessionCreated | EventKind::TurnStarted => {}
        }
    }
}

async fn event_loop<T: Transport>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &Client<T>,
    app: &mut AppState,
    notifications: &mut futures_util::stream::BoxStream<'static, Notification>,
) -> Result<()> {
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
    Ok(())
}

async fn handle_key<T: Transport>(
    app: &mut AppState,
    event: Event,
    client: &Client<T>,
) -> Result<()> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return Ok(());
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

    match key.code {
        KeyCode::Enter => {
            let task = app.input.trim().to_owned();
            if task.is_empty() || app.streaming {
                return Ok(());
            }
            app.input.clear();
            app.transcript.push_str(&format!("\n> {task}\n"));
            app.streaming = true;
            app.status = "thinking".to_owned();
            if let Err(error) = client
                .session_send(SessionSendParams {
                    session_id: app.session_id.clone(),
                    input: task,
                    turn_id: None,
                })
                .await
            {
                app.streaming = false;
                app.transcript.push_str(&format!("\n[error] {error}\n"));
                app.status = "error".to_owned();
            }
        }
        KeyCode::Char(character) => app.input.push(character),
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Esc => {
            if app.streaming {
                client
                    .session_cancel(SessionCancelParams {
                        session_id: app.session_id.clone(),
                        turn_id: None,
                    })
                    .await?;
                app.status = "cancelling".to_owned();
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::PageUp => {
            app.auto_scroll = false;
            app.scroll = app.scroll.saturating_sub(5);
        }
        KeyCode::PageDown => {
            app.scroll = app.scroll.saturating_add(5);
        }
        KeyCode::End => {
            app.auto_scroll = true;
        }
        _ => {}
    }
    Ok(())
}

fn draw_ui(frame: &mut Frame<'_>, app: &AppState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let lines = markdown_lines(&app.transcript);
    let transcript_height = chunks[0].height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(transcript_height) as u16;
    let scroll = if app.auto_scroll {
        max_scroll
    } else {
        app.scroll.min(max_scroll)
    };
    let transcript = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Thais"))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(transcript, chunks[0]);

    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("Message"));
    frame.render_widget(input, chunks[1]);

    let status = format!(
        "{} | mode: {} | model: {} | {}",
        short_id(&app.session_id),
        app.mode,
        app.model,
        app.status
    );
    let status = Paragraph::new(Line::from(Span::styled(
        status,
        Style::default().fg(Color::Black).bg(Color::Cyan),
    )));
    frame.render_widget(status, chunks[2]);

    if let Some(pending) = &app.pending {
        let area = centered_rect(75, 30, area);
        frame.render_widget(Clear, area);
        let text = format!(
            "Approval required\n\n{}\n\n[y] allow    [n] deny",
            truncate(&pending.action, 800)
        );
        let modal = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("Permission"))
            .wrap(Wrap { trim: false });
        frame.render_widget(modal, area);
    }
}

fn markdown_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            lines.push(Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(Color::Yellow),
            )));
            continue;
        }
        if in_code {
            lines.push(Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(Color::Yellow),
            )));
        } else if line.starts_with('#') {
            lines.push(Line::from(Span::styled(
                line.to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
        } else if line.starts_with('|') {
            lines.push(Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(Color::Cyan),
            )));
        } else if line.starts_with("- ") || line.starts_with("* ") {
            lines.push(Line::from(Span::raw(format!("• {}", &line[2..]))));
        } else {
            lines.push(Line::from(line.to_owned()));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from("Type a message and press Enter."));
    }
    lines
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
            workspace: "/tmp".to_owned(),
            mode: SessionMode::Build,
            model: "command-a-plus-05-2026".to_owned(),
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
    fn draw_ui_renders_the_status_bar() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let app = AppState::new(session());
        terminal.draw(|frame| draw_ui(frame, &app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let content = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(content.contains("Thais"));
        assert!(content.contains("mode: build"));
    }
}
