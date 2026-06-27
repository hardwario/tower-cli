//! `tower console` — the ratatui TUI (Phase 5).
//!
//! One synchronous loop owns the serial port: it polls the keyboard (short timeout),
//! drains all incoming frames through one `FrameDecoder`, and redraws. Everything is
//! async-via-drain — TAB sends a `ShellComplete` and Enter sends a `ShellCommand`; the
//! responses (`ShellCompletions` / `ShellResponse`) are handled when they arrive in the
//! drain, so nothing blocks. Layout/keys follow `CONSOLE.md`.
//!
//! Caveat: a TUI needs a real terminal — this is build- + clippy-verified, but NOT
//! interactively driven here (no TTY available). The completion/command round-trips
//! reuse the protocol paths proven in the non-TUI commands, so behaviour is by
//! construction; run `tower console` on a real terminal to confirm the UI.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use tower_protocol::msg::{Dropped, Event as EvMsg, Hello, Level, Log, Print, ShellCommand, ShellComplete, ShellCompletions, ShellResponse};
use tower_protocol::{FrameDecoder, MsgType, decode_frame, encode_frame};

const CAP: usize = 5000; // scrollback per pane

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Command,
    Responses,
    Events,
    Logs,
}

struct App {
    port_name: String,
    sp: Option<Box<dyn serialport::SerialPort>>,
    dec: FrameDecoder,
    logs: VecDeque<(String, Color)>,
    events: VecDeque<String>,
    responses: VecDeque<String>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
    focus: Pane,
    zoom: bool,
    paused: bool,
    scroll: [usize; 3], // [logs, events, responses] lines scrolled up from bottom
    cmd_id: u16,
    req_id: u16,
    seq: u16,
    fw: String,
    pending_req: Option<u16>,
    hint: String, // transient completion / status hint
    last_open_attempt: Instant,
    quit: bool,
}

impl App {
    fn new(port_name: String) -> Self {
        App {
            port_name,
            sp: None,
            dec: FrameDecoder::new(),
            logs: VecDeque::new(),
            events: VecDeque::new(),
            responses: VecDeque::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_idx: None,
            focus: Pane::Command,
            zoom: false,
            paused: false,
            scroll: [0; 3],
            cmd_id: 1,
            req_id: 1,
            seq: 0,
            fw: "?".into(),
            pending_req: None,
            hint: String::new(),
            last_open_attempt: Instant::now() - Duration::from_secs(10),
            quit: false,
        }
    }

    fn connected(&self) -> bool {
        self.sp.is_some()
    }
}

fn push_cap<T>(buf: &mut VecDeque<T>, item: T) {
    if buf.len() >= CAP {
        buf.pop_front();
    }
    buf.push_back(item);
}

pub fn run(port: String) -> Result<()> {
    let mut terminal = ratatui::init(); // raw mode + alt screen + panic-restore hook
    let app = App::new(port);
    let res = run_loop(&mut terminal, app);
    ratatui::restore();
    res
}

fn run_loop(terminal: &mut DefaultTerminal, mut app: App) -> Result<()> {
    while !app.quit {
        ensure_connected(&mut app);
        terminal.draw(|f| ui(f, &app))?;

        if event::poll(Duration::from_millis(33))?
            && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release {
                    handle_key(&mut app, key.code, key.modifiers);
                }
        drain_serial(&mut app);
    }
    Ok(())
}

fn ensure_connected(app: &mut App) {
    if app.sp.is_some() {
        return;
    }
    if app.last_open_attempt.elapsed() < Duration::from_millis(800) {
        return;
    }
    app.last_open_attempt = Instant::now();
    if let Ok(sp) = serialport::new(&app.port_name, 115_200)
        .timeout(Duration::from_millis(10))
        .open()
    {
        app.sp = Some(sp);
        app.dec.reset();
    }
}

fn drain_serial(app: &mut App) {
    let mut buf = [0u8; 512];
    let Some(mut sp) = app.sp.take() else {
        return;
    };
    match sp.read(&mut buf) {
        Ok(0) => app.sp = Some(sp),
        Ok(n) => {
            app.sp = Some(sp);
            for &b in &buf[..n] {
                // Copy the deframed bytes out so the `app.dec` borrow ends before we
                // borrow `app` mutably in `handle_frame`.
                if let Some(frame) = app.dec.push(b).map(|inner| inner.to_vec()) {
                    handle_frame(app, &frame);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => app.sp = Some(sp),
        Err(_) => { /* drop sp → reconnect on next tick */ }
    }
}

fn handle_frame(app: &mut App, inner: &[u8]) {
    let Ok((mt, _seq, payload)) = decode_frame(inner) else {
        return;
    };
    match mt {
        MsgType::Hello => {
            if let Ok(h) = postcard::from_bytes::<Hello>(payload) {
                app.fw = h.firmware_version.to_string();
            }
        }
        MsgType::Log => {
            if let Ok(l) = postcard::from_bytes::<Log>(payload) {
                let (lbl, color) = level_style(l.level);
                let secs = l.uptime_us / 1_000_000;
                let ms = (l.uptime_us % 1_000_000) / 1_000;
                let line = format!("{} [{secs:>5}.{ms:03}] {lbl} {}: {}", now(), l.module, l.message);
                push_cap(&mut app.logs, (line, color));
            }
        }
        MsgType::Print => {
            if let Ok(p) = postcard::from_bytes::<Print>(payload) {
                push_cap(&mut app.logs, (p.text.trim_end().to_string(), Color::Reset));
            }
        }
        MsgType::Event => {
            if let Ok(e) = postcard::from_bytes::<EvMsg>(payload) {
                let fields: Vec<String> = e.fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
                push_cap(&mut app.events, format!("{} {}  {}", now(), e.name, fields.join(" ")));
            }
        }
        MsgType::Dropped => {
            if let Ok(d) = postcard::from_bytes::<Dropped>(payload) {
                push_cap(&mut app.logs, (format!("⚠ {} log frame(s) dropped", d.count), Color::Yellow));
            }
        }
        MsgType::ShellResponse => {
            if let Ok(r) = postcard::from_bytes::<ShellResponse>(payload) {
                for line in r.text.lines() {
                    push_cap(&mut app.responses, line.to_string());
                }
                if r.result != 0 {
                    push_cap(&mut app.responses, format!("[result {}]", r.result));
                }
            }
        }
        MsgType::ShellCompletions => {
            if let Ok(c) = postcard::from_bytes::<ShellCompletions>(payload)
                && Some(c.req_id) == app.pending_req {
                    apply_completion(app, &c);
                    app.pending_req = None;
                }
        }
        _ => {}
    }
}

fn apply_completion(app: &mut App, c: &ShellCompletions) {
    let start = (c.token_start as usize).min(app.input.len());
    if app.cursor < start {
        return;
    }
    if c.candidates.len() == 1 {
        let cand = &c.candidates[0];
        let sep = match cand.kind {
            tower_protocol::msg::CandidateKind::Menu => "/",
            tower_protocol::msg::CandidateKind::Command => " ",
            tower_protocol::msg::CandidateKind::Arg => "=",
            tower_protocol::msg::CandidateKind::Value => "",
        };
        let repl = format!("{}{sep}", cand.text);
        app.input.replace_range(start..app.cursor, &repl);
        app.cursor = start + repl.len();
        app.hint.clear();
    } else if c.candidates.is_empty() {
        app.hint = "(no completions)".into();
    } else {
        if !c.common_prefix.is_empty() {
            app.input.replace_range(start..app.cursor, c.common_prefix);
            app.cursor = start + c.common_prefix.len();
        }
        let list: Vec<String> = c.candidates.iter().map(|cd| cd.text.to_string()).collect();
        app.hint = list.join("  ");
        if c.more {
            app.hint.push_str("  …");
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    // Global function keys (any focus).
    match code {
        KeyCode::F(10) => {
            app.quit = true;
            return;
        }
        KeyCode::F(3) => {
            app.zoom = !app.zoom;
            return;
        }
        KeyCode::F(5) => {
            app.paused = !app.paused;
            return;
        }
        KeyCode::F(8) => {
            match app.focus {
                Pane::Logs => app.logs.clear(),
                Pane::Events => app.events.clear(),
                Pane::Responses => app.responses.clear(),
                Pane::Command => {}
            }
            return;
        }
        KeyCode::BackTab => {
            app.focus = match app.focus {
                Pane::Command => Pane::Responses,
                Pane::Responses => Pane::Events,
                Pane::Events => Pane::Logs,
                Pane::Logs => Pane::Command,
            };
            return;
        }
        _ => {}
    }

    match app.focus {
        Pane::Command => handle_command_key(app, code, mods),
        pane => handle_scroll_key(app, pane, code),
    }
}

fn handle_scroll_key(app: &mut App, pane: Pane, code: KeyCode) {
    let idx = match pane {
        Pane::Logs => 0,
        Pane::Events => 1,
        Pane::Responses => 2,
        Pane::Command => return,
    };
    match code {
        KeyCode::PageUp => app.scroll[idx] = app.scroll[idx].saturating_add(10),
        KeyCode::PageDown => app.scroll[idx] = app.scroll[idx].saturating_sub(10),
        KeyCode::Up => app.scroll[idx] = app.scroll[idx].saturating_add(1),
        KeyCode::Down => app.scroll[idx] = app.scroll[idx].saturating_sub(1),
        _ => {}
    }
}

fn handle_command_key(app: &mut App, code: KeyCode, _mods: KeyModifiers) {
    match code {
        KeyCode::Char(ch) => {
            app.input.insert(app.cursor, ch);
            app.cursor += ch.len_utf8();
            app.hint.clear();
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let prev = app.input[..app.cursor].chars().next_back().map(|c| c.len_utf8()).unwrap_or(1);
                app.cursor -= prev;
                app.input.remove(app.cursor);
            }
        }
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(prev_char(&app.input, app.cursor)),
        KeyCode::Right => {
            if app.cursor < app.input.len() {
                app.cursor += next_char(&app.input, app.cursor);
            }
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.len(),
        KeyCode::Up => history_prev(app),
        KeyCode::Down => history_next(app),
        KeyCode::Tab => send_complete(app),
        KeyCode::Enter => send_command(app),
        _ => {}
    }
}

fn prev_char(s: &str, cur: usize) -> usize {
    s[..cur].chars().next_back().map(|c| c.len_utf8()).unwrap_or(0)
}
fn next_char(s: &str, cur: usize) -> usize {
    s[cur..].chars().next().map(|c| c.len_utf8()).unwrap_or(0)
}

fn history_prev(app: &mut App) {
    if app.history.is_empty() {
        return;
    }
    let i = match app.hist_idx {
        None => app.history.len() - 1,
        Some(0) => 0,
        Some(i) => i - 1,
    };
    app.hist_idx = Some(i);
    app.input = app.history[i].clone();
    app.cursor = app.input.len();
}

fn history_next(app: &mut App) {
    match app.hist_idx {
        Some(i) if i + 1 < app.history.len() => {
            app.hist_idx = Some(i + 1);
            app.input = app.history[i + 1].clone();
            app.cursor = app.input.len();
        }
        _ => {
            app.hist_idx = None;
            app.input.clear();
            app.cursor = 0;
        }
    }
}

fn send_complete(app: &mut App) {
    let req_id = app.req_id;
    app.req_id = app.req_id.wrapping_add(1);
    let line = app.input.clone();
    let cursor = app.cursor as u16;
    if send_frame(app, MsgType::ShellComplete, &ShellComplete { req_id, line: &line, cursor }) {
        app.pending_req = Some(req_id);
    }
}

fn send_command(app: &mut App) {
    let line = app.input.trim().to_string();
    if line.is_empty() {
        return;
    }
    let cmd_id = app.cmd_id;
    app.cmd_id = app.cmd_id.wrapping_add(1);
    push_cap(&mut app.responses, format!("> {line}"));
    let _ = send_frame(app, MsgType::ShellCommand, &ShellCommand { cmd_id, line: &line });
    if app.history.last().map(|h| h.as_str()) != Some(line.as_str()) {
        app.history.push(line);
    }
    app.hist_idx = None;
    app.input.clear();
    app.cursor = 0;
    app.hint.clear();
}

fn send_frame<T: serde::Serialize>(app: &mut App, mt: MsgType, payload: &T) -> bool {
    let Some(sp) = app.sp.as_mut() else {
        return false;
    };
    let mut buf = [0u8; tower_protocol::MAX_WIRE];
    let Ok(n) = encode_frame(mt, app.seq, payload, &mut buf) else {
        return false;
    };
    app.seq = app.seq.wrapping_add(1);
    if sp.write_all(&buf[..n]).and_then(|_| sp.flush()).is_err() {
        app.sp = None; // trigger reconnect
        return false;
    }
    true
}

// ---- rendering ----

fn ui(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    let bar = Style::new().bg(Color::Gray).fg(Color::Black);

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // Header.
    let conn = if app.connected() { "●" } else { "○ reconnecting…" };
    let header = format!(" HARDWARIO TOWER Console — fw {} — {} {}", app.fw, app.port_name, conn);
    f.render_widget(Paragraph::new(header).style(bar), rows[0]);

    if app.zoom {
        render_zoom(f, app, rows[1]);
    } else {
        render_split(f, app, rows[1]);
    }

    // Footer.
    let y = |on: bool| if on { Color::Yellow } else { Color::Black };
    let footer = Line::from(vec![
        Span::raw(" Shift-Tab Focus  "),
        Span::styled("F3 Zoom", Style::new().fg(y(app.zoom)).bg(Color::Gray)),
        Span::raw("  "),
        Span::styled("F5 Pause", Style::new().fg(y(app.paused)).bg(Color::Gray)),
        Span::raw("  F8 Clear  F10 Quit   "),
        Span::raw(if app.hint.is_empty() { String::new() } else { format!("[{}] ", app.hint) }),
    ]);
    let footer_area = rows[2];
    f.render_widget(Paragraph::new(footer).style(bar), footer_area);
    // Clock, right-aligned.
    let clock = now_date();
    let cw = clock.len() as u16;
    if footer_area.width > cw + 1 {
        let clock_rect = Rect::new(footer_area.x + footer_area.width - cw - 1, footer_area.y, cw, 1);
        f.render_widget(Paragraph::new(clock).style(bar), clock_rect);
    }
}

fn render_split(f: &mut ratatui::Frame, app: &App, body: Rect) {
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body);
    let left = Layout::vertical([
        Constraint::Percentage(25),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .split(cols[0]);

    render_text_pane(f, left[0], "Device Events", app.focus == Pane::Events, &events_lines(app), app.scroll[1], false);
    render_command(f, left[1], app);
    render_text_pane(f, left[2], "Shell Responses", app.focus == Pane::Responses, &resp_lines(app), app.scroll[2], true);
    render_text_pane(f, cols[1], "Device Logs", app.focus == Pane::Logs, &log_lines(app), app.scroll[0], false);
}

fn render_zoom(f: &mut ratatui::Frame, app: &App, body: Rect) {
    match app.focus {
        Pane::Logs => render_text_pane(f, body, "", false, &log_lines(app), app.scroll[0], false),
        Pane::Events => render_text_pane(f, body, "", false, &events_lines(app), app.scroll[1], false),
        Pane::Responses => render_text_pane(f, body, "", false, &resp_lines(app), app.scroll[2], true),
        Pane::Command => render_command(f, body, app),
    }
}

fn log_lines(app: &App) -> Vec<Line<'static>> {
    app.logs
        .iter()
        .map(|(s, c)| Line::from(Span::styled(s.clone(), Style::new().fg(*c))))
        .collect()
}
fn events_lines(app: &App) -> Vec<Line<'static>> {
    app.events.iter().map(|s| Line::raw(s.clone())).collect()
}
fn resp_lines(app: &App) -> Vec<Line<'static>> {
    app.responses.iter().map(|s| Line::raw(s.clone())).collect()
}

fn render_text_pane(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    focused: bool,
    lines: &[Line<'static>],
    scrollback: usize,
    _bottom: bool,
) {
    let block = if title.is_empty() {
        Block::default()
    } else {
        let style = if focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        Block::bordered().title(title).border_style(style)
    };
    let inner_h = area.height.saturating_sub(if title.is_empty() { 0 } else { 2 }) as usize;
    let total = lines.len();
    let max_off = total.saturating_sub(inner_h);
    let off = max_off.saturating_sub(scrollback); // bottom-anchored minus user scrollback
    let text: Vec<Line> = lines.to_vec();
    let p = Paragraph::new(text).block(block).scroll((off as u16, 0)).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_command(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let focused = app.focus == Pane::Command;
    let style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let block = Block::bordered().title("Shell Command").border_style(style);
    let inner = block.inner(area);
    f.render_widget(Paragraph::new(format!("/ {}", app.input)).block(block), area);
    if focused {
        // Cursor after the "/ " prefix.
        let cx = inner.x + 2 + app.input[..app.cursor].chars().count() as u16;
        f.set_cursor_position((cx.min(inner.x + inner.width.saturating_sub(1)), inner.y));
    }
}

fn level_style(l: Level) -> (&'static str, Color) {
    match l {
        Level::Error => ("ERROR", Color::Red),
        Level::Warn => ("WARN ", Color::Yellow),
        Level::Info => ("INFO ", Color::Green),
        Level::Debug => ("DEBUG", Color::Cyan),
        Level::Trace => ("TRACE", Color::DarkGray),
    }
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}
fn now_date() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
