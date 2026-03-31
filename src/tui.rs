use crate::youtube::{LiveChatMessage, YouTube};
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, event};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use std::collections::VecDeque;
use std::io::{self, Stdout};
use tokio::sync::mpsc;

const HISTORY_LINES: usize = 30;
const MAX_LINES: usize = 1000;
const FALLBACK_POLL_MS: u64 = 5000;

struct AppState {
    title: String,
    status: String,
    input: String,
    lines: VecDeque<Line<'static>>,
}

fn push_line(state: &mut AppState, line: Line<'static>) {
    state.lines.push_back(line);
    while state.lines.len() > MAX_LINES {
        state.lines.pop_front();
    }
}

fn format_message(msg: &LiveChatMessage) -> Line<'static> {
    let name = msg
        .author_details
        .display_name
        .clone()
        .unwrap_or_else(|| "Unknown".to_string());
    let body = msg
        .snippet
        .display_message
        .clone()
        .unwrap_or_default();

    let msg_type = msg.snippet.message_type.clone().unwrap_or_default();

    let is_owner = msg.author_details.is_chat_owner.unwrap_or(false)
        || msg.author_details.is_verified.unwrap_or(false);
    let is_mod = msg.author_details.is_chat_moderator.unwrap_or(false);
    let is_member = msg.author_details.is_chat_sponsor.unwrap_or(false);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Event badge
    let (badge, badge_color) = match msg_type.as_str() {
        "superChatEvent" => ("💰 SUPERCHAT", Color::Yellow),
        "superStickerEvent" => ("🎉 STICKER", Color::Magenta),
        "memberMilestoneChatEvent" => ("⭐ MEMBER", Color::Green),
        "newSponsorEvent" => ("🌟 NEW MEMBER", Color::Green),
        "membershipGiftingEvent" => ("🎁 GIFTING", Color::Blue),
        "giftMembershipReceivedEvent" => ("🎁 GIFT", Color::Blue),
        _ => ("", Color::Reset),
    };
    if !badge.is_empty() {
        spans.push(Span::styled(
            format!("{badge} "),
            Style::default().fg(badge_color).add_modifier(Modifier::BOLD),
        ));
    }

    // Role icon
    if is_owner {
        spans.push(Span::styled("👑 ", Style::default().fg(Color::Yellow)));
    } else if is_mod {
        spans.push(Span::styled("🔧 ", Style::default().fg(Color::Cyan)));
    } else if is_member {
        spans.push(Span::styled("⭐ ", Style::default().fg(Color::Blue)));
    }

    // Name color
    let name_style = if is_owner {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else if is_mod {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if is_member {
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    };
    spans.push(Span::styled(name, name_style));
    spans.push(Span::styled(": ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::raw(body));

    if msg_type == "superChatEvent" {
        if let Some(amt) = msg
            .snippet
            .super_chat_details
            .as_ref()
            .and_then(|d| d.amount_display_string.clone())
        {
            spans.push(Span::styled(
                format!("  [{amt}]"),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
    }

    Line::from(spans)
}

enum UiEvent {
    Key(KeyEvent),
}

pub async fn run_tui(
    yt: YouTube,
    live_chat_id: String,
    video_title: String,
    channel: String,
) -> Result<()> {
    let (msg_tx, mut msg_rx) = mpsc::channel::<Vec<LiveChatMessage>>(100);
    let (status_tx, mut status_rx) = mpsc::channel::<String>(20);
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(200);

    // Poller
    {
        let yt = yt.clone();
        let live_chat_id = live_chat_id.clone();
        let msg_tx = msg_tx.clone();
        let status_tx = status_tx.clone();
        tokio::spawn(async move {
            let mut page_token: Option<String> = None;
            let mut is_first = true;
            let mut backoff_ms: u64 = 10_000;

            loop {
                match yt
                    .list_messages(&live_chat_id, page_token.as_deref())
                    .await
                {
                    Ok(resp) => {
                        page_token = resp.next_page_token.clone();
                        let poll_ms = resp.polling_interval_millis.unwrap_or(FALLBACK_POLL_MS);

                        let items = if is_first {
                            let n = resp.items.len();
                            resp.items
                                .into_iter()
                                .skip(n.saturating_sub(HISTORY_LINES))
                                .collect::<Vec<_>>()
                        } else {
                            resp.items
                        };

                        if msg_tx.send(items).await.is_ok() {
                            let _ = status_tx
                                .send(format!(
                                    "Connected · next poll in {}s · Ctrl+C to quit",
                                    poll_ms / 1000
                                ))
                                .await;
                        }

                        if is_first {
                            is_first = false;
                        }

                        backoff_ms = 10_000;
                        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
                    }
                    Err(e) => {
                        let _ = status_tx
                            .send(format!("API error — retrying in {}s… ({e})", backoff_ms / 1000))
                            .await;
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms.saturating_mul(2)).min(60_000);
                    }
                }
            }
        });
    }

    // Keyboard reader thread -> async channel
    {
        let ui_tx = ui_tx.clone();
        std::thread::spawn(move || loop {
            if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    if let Event::Key(k) = ev {
                        let _ = ui_tx.blocking_send(UiEvent::Key(k));
                    }
                }
            }
        });
    }

    // Terminal setup
    let mut terminal = setup_terminal().context("setup terminal")?;
    let mut state = AppState {
        title: format!("▶  {}  ·  {}", video_title, channel),
        status: "Connecting…".to_string(),
        input: String::new(),
        lines: VecDeque::new(),
    };

    // Main loop
    let tick = tokio::time::interval(std::time::Duration::from_millis(33));
    tokio::pin!(tick);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                draw(&mut terminal, &state).ok();
            }
            Some(status) = status_rx.recv() => {
                state.status = status;
            }
            Some(batch) = msg_rx.recv() => {
                for msg in batch {
                    push_line(&mut state, format_message(&msg));
                }
                // Add a separator after first history load (rough equivalent of Python script).
                if state.lines.len() == HISTORY_LINES {
                    push_line(&mut state, Line::from(Span::styled("──────────────────  history loaded  ──────────────────", Style::default().fg(Color::DarkGray))));
                }
            }
            Some(UiEvent::Key(k)) = ui_rx.recv() => {
                if should_quit(&k) {
                    break;
                }
                match k.code {
                    KeyCode::Enter => {
                        let msg = state.input.trim().to_string();
                        state.input.clear();
                        if !msg.is_empty() {
                            let yt = yt.clone();
                            let live_chat_id = live_chat_id.clone();
                            tokio::spawn(async move {
                                let _ = yt.send_message(&live_chat_id, &msg).await;
                            });
                        }
                    }
                    KeyCode::Backspace => { state.input.pop(); }
                    KeyCode::Char(c) => {
                        if !k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT) {
                            state.input.push(c);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    restore_terminal(&mut terminal).ok();
    Ok(())
}

fn should_quit(k: &KeyEvent) -> bool {
    (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
        || (k.code == KeyCode::Char('q') && k.modifiers.contains(KeyModifiers::CONTROL))
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("create terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    Ok(())
}

fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, state: &AppState) -> Result<()> {
    terminal
        .draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(3),
                ])
                .split(f.area());

            let title = Paragraph::new(state.title.clone())
                .style(Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD));
            f.render_widget(title, chunks[0]);

            let text = Text::from(state.lines.iter().cloned().collect::<Vec<_>>());
            let chat = Paragraph::new(text)
                .block(Block::default().borders(Borders::NONE))
                .wrap(Wrap { trim: false })
                .scroll((state.lines.len().saturating_sub(chunks[1].height as usize) as u16, 0));
            f.render_widget(chat, chunks[1]);

            let status = Paragraph::new(format!(" {}", state.status))
                .style(Style::default().fg(Color::Gray).bg(Color::Black));
            f.render_widget(status, chunks[2]);

            let input = Paragraph::new(state.input.clone())
                .block(Block::default().borders(Borders::ALL).title("💬 Message"));
            f.render_widget(input, chunks[3]);
            f.set_cursor_position((chunks[3].x + 1 + state.input.len() as u16, chunks[3].y + 1));
        })
        .context("terminal draw")?;
    Ok(())
}

