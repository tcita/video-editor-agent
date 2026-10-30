use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use tokio::sync::{mpsc, Mutex};
use unicode_width::UnicodeWidthStr;

use crate::agent::{self, AgentHost, LlmClient};
use crate::media::Session;
use crate::runlog::RunLog;
use crate::tool::ToolRegistry;

enum Phase {
    Editing,
    Busy(String),
}

enum AppMsg {
    Log(String),
    State(String),
    Done(String),
    Failed(String),
}

struct TuiHost {
    tx: mpsc::UnboundedSender<AppMsg>,
}

impl TuiHost {
    fn send(&self, msg: AppMsg) {
        let _ = self.tx.send(msg);
    }
}

impl AgentHost for TuiHost {
    fn log(&self, msg: String) {
        self.send(AppMsg::Log(msg));
    }

    fn state(&self, summary: String) {
        self.send(AppMsg::State(summary));
    }
}

struct App {
    req_text: String,
    log: Vec<String>,
    state: String,
    phase: Phase,
    scroll: u16,
}

impl App {
    fn push(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
    }
}

pub async fn run(
    session: Session,
    client: LlmClient,
    registry: ToolRegistry,
    log: Arc<RunLog>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let res = event_loop(&mut term, session, client, registry, log).await;
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

async fn event_loop(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    session: Session,
    client: LlmClient,
    registry: ToolRegistry,
    log: Arc<RunLog>,
) -> Result<()> {
    let initial = session.summary();
    let session = Arc::new(Mutex::new(session));
    let client = Arc::new(client);
    let registry = Arc::new(registry);

    let (tx, mut rx) = mpsc::unbounded_channel::<AppMsg>();
    let host = Arc::new(TuiHost { tx });

    let mut app = App {
        req_text: String::new(),
        log: Vec::new(),
        state: initial,
        phase: Phase::Editing,
        scroll: 0,
    };

    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                AppMsg::Log(s) => app.push(s),
                AppMsg::State(s) => app.state = s,
                AppMsg::Done(text) => {
                    let text = text.trim();
                    if text.is_empty() {
                        app.push("✅ 已完成");
                    } else {
                        app.push(format!("✅ {text}"));
                    }
                    app.phase = Phase::Editing;
                }
                AppMsg::Failed(e) => {
                    app.push(format!("❌ {e}"));
                    app.phase = Phase::Editing;
                }
            }
        }

        term.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(7),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(f.area());

            let log_text = app.log.join("\n");
            let log = Paragraph::new(log_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" 日志（每次 tool_call 与结果） "),
                )
                .wrap(Wrap { trim: false })
                .scroll((app.scroll, 0));
            f.render_widget(log, chunks[0]);

            let (title, style, body) = match &app.phase {
                Phase::Busy(msg) => (
                    " 状态 ".to_string(),
                    Style::default().fg(Color::Cyan),
                    format!("…{msg}\n\n{}", app.state),
                ),
                Phase::Editing => (
                    " 当前时间线 ".to_string(),
                    Style::default(),
                    if app.state.is_empty() {
                        "（空）".to_string()
                    } else {
                        app.state.clone()
                    },
                ),
            };
            f.render_widget(
                Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .border_style(style),
                    )
                    .wrap(Wrap { trim: false }),
                chunks[1],
            );

            let input = Paragraph::new(app.req_text.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 需求（按 Enter 发送） "),
            );
            f.render_widget(input, chunks[2]);
            if matches!(app.phase, Phase::Editing) {
                f.set_cursor_position((
                    chunks[2].x + app.req_text.width() as u16 + 1,
                    chunks[2].y + 1,
                ));
            }

            let hint = " ↑/↓ 滚动 · Esc 清空/退出 · Ctrl+C 退出 ";
            f.render_widget(Paragraph::new(hint), chunks[3]);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                // Windows emits Press + Release for one keypress. Handle press
                // only (Repeat included), or every character doubles.
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('q'))
                {
                    break;
                }
                match app.phase {
                    Phase::Editing => match k.code {
                        KeyCode::Char(c) => app.req_text.push(c),
                        KeyCode::Backspace => {
                            app.req_text.pop();
                        }
                        KeyCode::Up => app.scroll = app.scroll.saturating_sub(1),
                        KeyCode::Down => app.scroll = app.scroll.saturating_add(1),
                        KeyCode::Esc => {
                            if app.req_text.is_empty() {
                                break;
                            }
                            app.req_text.clear();
                        }
                        KeyCode::Enter => {
                            let req = app.req_text.trim().to_string();
                            if req.is_empty() {
                                continue;
                            }
                            app.req_text.clear();
                            app.push(format!("你：{req}"));
                            app.phase = Phase::Busy("思考中…".into());

                            let session = Arc::clone(&session);
                            let client = Arc::clone(&client);
                            let registry = Arc::clone(&registry);
                            let host = Arc::clone(&host);
                            let log = Arc::clone(&log);
                            tokio::spawn(async move {
                                let mut s = session.lock().await;
                                let result = agent::run_turn(
                                    &client,
                                    &registry,
                                    host.as_ref(),
                                    &mut s,
                                    &req,
                                    &log,
                                )
                                .await;
                                match result {
                                    Ok(text) => host.send(AppMsg::Done(text)),
                                    Err(e) => {
                                        log.line("ERROR", &format!("{e:#}"));
                                        host.send(AppMsg::Failed(format!("{e:#}")))
                                    }
                                }
                            });
                        }
                        _ => {}
                    },
                    Phase::Busy(_) => match k.code {
                        KeyCode::Up => app.scroll = app.scroll.saturating_sub(1),
                        KeyCode::Down => app.scroll = app.scroll.saturating_add(1),
                        _ => {}
                    },
                }
            }
        }
    }
    Ok(())
}
