//! `tower console` — the ratatui TUI.
//!
//! One synchronous loop owns the serial port: it polls the keyboard (short timeout),
//! drains all incoming frames through one `FrameDecoder`, and redraws. Everything is
//! async-via-drain — TAB sends a `ShellComplete` and Enter sends a `ShellCommand`; the
//! responses (`ShellCompletions` / `ShellResponse`) are handled when they arrive in the
//! drain, so nothing blocks. Layout/keys follow `docs/console.md` (in the firmware repo).
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
use tower_protocol::msg::{
    Dropped, Event as EvMsg, Hello, Level, Log, Print, ShellCommand, ShellComplete,
    ShellCompletions, ShellResponse,
};
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
    resp_buf: String, // accumulates a chunked shell response until `last`
    /// The `cmd_id` of the command whose `ShellResponse` we're currently reassembling.
    /// Chunks for any other `cmd_id` are dropped (a stale/overlapping response must not
    /// bleed into the current one). Cleared when the response completes or the port reopens.
    pending_cmd: Option<u16>,
    /// Expected `chunk` index of the next `ShellResponse` frame for `pending_cmd`; a gap
    /// means a middle chunk was CRC-dropped, so the reassembled text is truncated.
    next_chunk: u16,
    pending_req: Option<u16>,
    hint: String, // transient completion / status hint
    /// Whether we've already warned about a protocol-version mismatch (warn once per session).
    warned_mismatch: bool,
    /// Last frame `seq` seen (any type), for gap detection; re-baselined on every `Hello`.
    last_seq: Option<u16>,
    /// Frames the codec refused to decode (`BadVersion`/`BadCrc`/`BadType`/…). A burst of these
    /// on a live link is the classic tag-mismatch symptom — shown in the header, not swallowed.
    decode_failures: u64,
    /// Count of `seq` discontinuities (a decoded frame whose `seq` wasn't `prev + 1`).
    seq_gaps: u64,
    /// Frames that decoded at the frame layer but whose postcard payload failed to parse.
    payload_errors: u64,
    /// The version byte from the first header-level `BadVersion` seen (for the header hint).
    mismatch_got: Option<u8>,
    /// Last failed (re)connect error, shown next to the reconnect indicator. `None` while
    /// connected or before the first failed attempt.
    last_open_error: Option<String>,
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
            resp_buf: String::new(),
            pending_cmd: None,
            next_chunk: 0,
            pending_req: None,
            hint: String::new(),
            warned_mismatch: false,
            last_seq: None,
            decode_failures: 0,
            seq_gaps: 0,
            payload_errors: 0,
            mismatch_got: None,
            last_open_error: None,
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

pub fn run(port: String, reset: bool) -> Result<()> {
    // The FIRST open is fatal — same contract as the streaming commands: a bad `--device`
    // (typo, EBUSY, EACCES) must exit 1 with the real OS error, not silently spin in the
    // reconnect loop behind an empty four-pane UI. Open BEFORE ratatui grabs the terminal so
    // the error prints normally. `--reset` is consumed here, on the initial attach; the
    // reconnect path never re-resets (a flaky link mustn't become a reboot loop).
    let sp = crate::port::open_console_responsive(&port, reset)?;
    let mut app = App::new(port);
    app.sp = Some(sp);
    let mut terminal = ratatui::init(); // raw mode + alt screen + panic-restore hook
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
            && key.kind != KeyEventKind::Release
        {
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
    // Reconnect without re-resetting (never re-reset on a flaky link). Unlike the fatal first
    // open in `run`, a later failure isn't fatal — we keep retrying — but we no longer drop it
    // on the floor: stash it so the header can show *why* (EBUSY, EACCES, ENOENT, …) next to
    // the reconnect indicator instead of a bare "reconnecting…" with a green-looking UI.
    match crate::port::open_console_responsive(&app.port_name, false) {
        Ok(sp) => {
            app.sp = Some(sp);
            app.last_open_error = None;
            app.dec.reset();
            // Drop any half-reassembled response from the previous connection: its remaining
            // chunks will never arrive, and the new session restarts `cmd_id`/`chunk` at 1/0.
            app.resp_buf.clear();
            app.pending_cmd = None;
            app.pending_req = None;
        }
        Err(e) => app.last_open_error = Some(format!("{e:#}")),
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
    // Capture the decode error instead of dropping the frame silently: a wall of these behind a
    // green "connected" dot is exactly the tag-mismatch symptom that used to show four empty
    // panes. Count them (shown in the header) and, on the first header-level version mismatch,
    // shout a red line into the log pane naming the version and the remedy.
    let (mt, seq, payload) = match decode_frame(inner) {
        Ok(t) => t,
        Err(e) => {
            app.decode_failures += 1;
            if let tower_protocol::Error::BadVersion { got } = e {
                app.mismatch_got = Some(got);
                if !app.warned_mismatch {
                    push_cap(
                        &mut app.logs,
                        (
                            format!(
                                "✖ PROTOCOL MISMATCH: device frames are tagged v{got}, but this \
                                 `tower` was built for v{} — every frame will mis-decode. Rebuild \
                                 the firmware and `tower` against the SAME tower-protocol tag \
                                 (the lockstep rule).",
                                tower_protocol::PROTOCOL_VERSION
                            ),
                            Color::Red,
                        ),
                    );
                    app.warned_mismatch = true;
                }
            }
            return;
        }
    };

    // Sequence-gap accounting (mirrors the plain-CLI renderer): a `Hello` marks a new device
    // session and re-baselines `seq` (the console re-emits `Hello` on every USB plug-in), so a
    // fresh 0 isn't a spurious gap. Done before the pause check because frames still arrive
    // while paused — only their *display* is frozen.
    if matches!(mt, MsgType::Hello) {
        app.last_seq = None;
    }
    if let Some(prev) = app.last_seq {
        // Wrapping u16 distance minus one: nonzero means at least one frame went missing.
        if seq.wrapping_sub(prev).wrapping_sub(1) != 0 {
            app.seq_gaps += 1;
        }
    }
    app.last_seq = Some(seq);

    // While paused (F5), freeze the streaming panes — keep draining the port (so its
    // buffer can't overflow) but don't append. Interactive traffic (Hello, shell
    // responses/completions) still flows so the shell stays usable.
    let streaming = matches!(
        mt,
        MsgType::Log | MsgType::Print | MsgType::Event | MsgType::Dropped
    );
    if app.paused && streaming {
        return;
    }
    match mt {
        // Decode the Hello (the TUI otherwise ignores it) purely to enforce the lockstep rule:
        // a payload-version mismatch is a secondary guard — a *real* tag mismatch is caught at
        // the frame header above and never parses a Hello.
        MsgType::Hello => match postcard::from_bytes::<Hello>(payload) {
            Ok(h) => {
                if h.protocol_version != tower_protocol::PROTOCOL_VERSION && !app.warned_mismatch {
                    push_cap(
                        &mut app.logs,
                        (
                            format!(
                                "⚠ PROTOCOL MISMATCH: device v{}, tower built for v{} — frames \
                                 will mis-decode; rebuild both against the same tower-protocol \
                                 tag.",
                                h.protocol_version,
                                tower_protocol::PROTOCOL_VERSION
                            ),
                            Color::Red,
                        ),
                    );
                    app.warned_mismatch = true;
                    app.mismatch_got = Some(h.protocol_version);
                }
            }
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Log => match postcard::from_bytes::<Log>(payload) {
            // Reuse the CLI's shared layout (`[uptime] LEVEL module: message`) so both
            // frontends render a Log identically; the TUI prepends its own clock + color.
            Ok(l) => {
                let line = format!("{} {}", now(), crate::render::log_line(&l));
                push_cap(&mut app.logs, (line, level_color(l.level)));
            }
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Print => match postcard::from_bytes::<Print>(payload) {
            Ok(p) => push_cap(&mut app.logs, (p.text.trim_end().to_string(), Color::Reset)),
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Event => match postcard::from_bytes::<EvMsg>(payload) {
            Ok(e) => push_cap(
                &mut app.events,
                format!("{} {}  {}", now(), e.name, crate::render::event_fields(&e)),
            ),
            Err(_) => app.payload_errors += 1,
        },
        MsgType::Dropped => match postcard::from_bytes::<Dropped>(payload) {
            Ok(d) => push_cap(
                &mut app.logs,
                (format!("⚠ {} log frame(s) dropped", d.count), Color::Yellow),
            ),
            Err(_) => app.payload_errors += 1,
        },
        // Reassemble chunks (`chunk`/`last`) into one response before splitting it into lines,
        // so a chunk boundary mid-line doesn't fragment the display.
        MsgType::ShellResponse => match postcard::from_bytes::<ShellResponse>(payload) {
            // Only reassemble chunks for the command we're currently awaiting — a stale or
            // overlapping response (different cmd_id) must not bleed into this one.
            Ok(r) if app.pending_cmd == Some(r.cmd_id) => {
                // A `chunk` gap means a middle chunk was CRC-dropped (the decoder silently
                // discards corrupt frames): flag the truncation instead of emitting a
                // seemingly-complete response.
                if r.chunk != app.next_chunk {
                    push_cap(
                        &mut app.responses,
                        format!(
                            "[tower] response chunk dropped (expected #{}, got #{}) — output truncated",
                            app.next_chunk, r.chunk
                        ),
                    );
                }
                app.next_chunk = r.chunk.wrapping_add(1);
                app.resp_buf.push_str(r.text);
                if r.last {
                    for line in app.resp_buf.lines() {
                        push_cap(&mut app.responses, line.to_string());
                    }
                    if r.result != 0 {
                        push_cap(&mut app.responses, format!("[result {}]", r.result));
                    }
                    app.resp_buf.clear();
                    app.pending_cmd = None;
                }
            }
            Ok(_) => {} // a response for a different cmd_id — ignore
            Err(_) => app.payload_errors += 1,
        },
        MsgType::ShellCompletions => match postcard::from_bytes::<ShellCompletions>(payload) {
            Ok(c) if Some(c.req_id) == app.pending_req => {
                apply_completion(app, &c);
                app.pending_req = None;
            }
            Ok(_) => {} // a completion for a stale req_id — ignore
            Err(_) => app.payload_errors += 1,
        },
        _ => {}
    }
}

fn apply_completion(app: &mut App, c: &ShellCompletions) {
    let start = (c.token_start as usize).min(app.input.len());
    // `token_start` is device-supplied: validate it's a char boundary and doesn't run past
    // the cursor before using it in `replace_range` (a mid-UTF-8-scalar index would panic).
    // `app.cursor` is always kept on a boundary by the key handlers, so it needs no check.
    if start > app.cursor || !app.input.is_char_boundary(start) {
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
    // Ctrl+C always quits (any focus). F10 works too, but many terminals swallow F10, so a
    // universally-available quit key matters — and without this, Ctrl+C used to fall through
    // to `handle_command_key` and insert a literal 'c'.
    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }
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
        // In a non-input pane, bare 'q' quits (a common TUI convention); the command pane
        // needs the character, so it's handled here rather than globally.
        KeyCode::Char('q') => app.quit = true,
        _ => {}
    }
}

fn handle_command_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    // Control/Alt-modified keys are commands, not text: handle the readline-style editing
    // bindings and drop the rest, so e.g. a stray Ctrl+<x> can't insert a literal control
    // character into the command line. (Plain Shift is fine — it just gives capitals.)
    if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        match code {
            KeyCode::Char('u') => kill_to_start(app), // Ctrl+U: clear to line start
            KeyCode::Char('w') => kill_word(app),     // Ctrl+W: delete previous word
            KeyCode::Char('a') => app.cursor = 0,     // Ctrl+A: line start
            KeyCode::Char('e') => app.cursor = app.input.len(), // Ctrl+E: line end
            _ => {}
        }
        return;
    }
    match code {
        KeyCode::Char(ch) => {
            app.input.insert(app.cursor, ch);
            app.cursor += ch.len_utf8();
            app.hint.clear();
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let prev = app.input[..app.cursor]
                    .chars()
                    .next_back()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1);
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

/// Ctrl+U: delete everything left of the cursor.
fn kill_to_start(app: &mut App) {
    app.input.replace_range(0..app.cursor, "");
    app.cursor = 0;
    app.hint.clear();
}

/// Ctrl+W: delete the whitespace-delimited word before the cursor (trailing spaces first,
/// then the word). Operates on char boundaries so multibyte input can't panic.
fn kill_word(app: &mut App) {
    let mut start = app.cursor;
    // Skip any spaces immediately left of the cursor.
    while start > 0 {
        let p = prev_char(&app.input, start);
        if app.input[start - p..start].chars().all(char::is_whitespace) {
            start -= p;
        } else {
            break;
        }
    }
    // Then skip the word itself.
    while start > 0 {
        let p = prev_char(&app.input, start);
        if app.input[start - p..start].chars().any(char::is_whitespace) {
            break;
        }
        start -= p;
    }
    app.input.replace_range(start..app.cursor, "");
    app.cursor = start;
    app.hint.clear();
}

fn prev_char(s: &str, cur: usize) -> usize {
    s[..cur]
        .chars()
        .next_back()
        .map(|c| c.len_utf8())
        .unwrap_or(0)
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
    if send_frame(
        app,
        MsgType::ShellComplete,
        &ShellComplete {
            req_id,
            line: &line,
            cursor,
        },
    ) {
        app.pending_req = Some(req_id);
    }
}

fn send_command(app: &mut App) {
    let line = app.input.trim().to_string();
    if line.is_empty() {
        return;
    }
    let cmd_id = app.cmd_id;
    // Try to send FIRST; only commit UI state (echo, arm reassembly, history, clear the input)
    // if the frame actually went out. A too-long line fails to encode and a dropped link fails
    // to write — in either case keep the typed line so the user can edit/retry, don't arm a
    // reassembly for a command that never left, and explain why in the hint.
    if !send_frame(
        app,
        MsgType::ShellCommand,
        &ShellCommand {
            cmd_id,
            line: &line,
        },
    ) {
        app.hint = if app.connected() {
            format!(
                "line too long (max ~{} bytes)",
                tower_protocol::MAX_FRAME - 12
            )
        } else {
            "send failed: link lost — reconnecting…".to_string()
        };
        return;
    }
    app.cmd_id = app.cmd_id.wrapping_add(1);
    push_cap(&mut app.responses, format!("> {line}"));
    app.resp_buf.clear(); // discard any incomplete prior response
    // Track this command so `handle_frame` only reassembles chunks tagged with *its*
    // cmd_id (a late chunk from a prior command must not bleed in), starting at chunk 0.
    app.pending_cmd = Some(cmd_id);
    app.next_chunk = 0;
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

/// The header's decode/seq/payload diagnostics string, or `None` when everything's clean.
/// e.g. `"✖ 37 bad frames (v2?) · 4 seq gaps · 2 payload errs"`.
fn header_diagnostics(app: &App) -> Option<String> {
    if app.decode_failures == 0 && app.seq_gaps == 0 && app.payload_errors == 0 {
        return None;
    }
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    let mut parts: Vec<String> = Vec::new();
    if app.decode_failures > 0 {
        let mut d = format!(
            "{} bad frame{}",
            app.decode_failures,
            plural(app.decode_failures)
        );
        if let Some(v) = app.mismatch_got {
            d.push_str(&format!(" (v{v}?)"));
        }
        parts.push(d);
    }
    if app.seq_gaps > 0 {
        parts.push(format!("{} seq gap{}", app.seq_gaps, plural(app.seq_gaps)));
    }
    if app.payload_errors > 0 {
        parts.push(format!(
            "{} payload err{}",
            app.payload_errors,
            plural(app.payload_errors)
        ));
    }
    Some(format!("  ✖ {}", parts.join(" · ")))
}

fn ui(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    let bar = Style::new().bg(Color::Gray).fg(Color::Black);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    // Header. Built as spans so the connection dot, a reconnect error, and the decode/seq
    // diagnostics can each carry their own color over the gray bar.
    let on_bar = |fg: Color| Style::new().bg(Color::Gray).fg(fg);
    let mut spans = vec![Span::styled(
        format!(
            " HARDWARIO TOWER Console v{} — {} ",
            env!("CARGO_PKG_VERSION"),
            app.port_name
        ),
        bar,
    )];
    if app.connected() {
        spans.push(Span::styled("●", on_bar(Color::Green)));
    } else {
        spans.push(Span::styled("○ reconnecting…", bar));
        // Surface *why* the (re)connect is failing instead of an endless bare "reconnecting…".
        if let Some(err) = &app.last_open_error {
            spans.push(Span::styled(
                format!("  {}: {err}", app.port_name),
                on_bar(Color::Red),
            ));
        }
    }
    // Decode-failure / seq-gap / payload-error tally — the tag-mismatch smoking gun, kept in
    // view so a live-but-garbled link can't masquerade as healthy.
    if let Some(diag) = header_diagnostics(app) {
        spans.push(Span::styled(
            diag,
            on_bar(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(bar), rows[0]);

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
        Span::raw("  F8 Clear  F10/^C Quit   "),
        Span::raw(if app.hint.is_empty() {
            String::new()
        } else {
            format!("[{}] ", app.hint)
        }),
    ]);
    let footer_area = rows[2];
    f.render_widget(Paragraph::new(footer).style(bar), footer_area);
    // Clock, right-aligned.
    let clock = now_date();
    let cw = clock.len() as u16;
    if footer_area.width > cw + 1 {
        let clock_rect = Rect::new(
            footer_area.x + footer_area.width - cw - 1,
            footer_area.y,
            cw,
            1,
        );
        f.render_widget(Paragraph::new(clock).style(bar), clock_rect);
    }
}

fn render_split(f: &mut ratatui::Frame, app: &App, body: Rect) {
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body);
    let left = Layout::vertical([
        Constraint::Percentage(25),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .split(cols[0]);

    let plain = |s: &String| Line::raw(s.clone());
    let colored =
        |(s, c): &(String, Color)| Line::from(Span::styled(s.clone(), Style::new().fg(*c)));
    render_text_pane(
        f,
        left[0],
        "Device Events",
        app.focus == Pane::Events,
        &app.events,
        app.scroll[1],
        plain,
    );
    render_command(f, left[1], app);
    render_text_pane(
        f,
        left[2],
        "Shell Responses",
        app.focus == Pane::Responses,
        &app.responses,
        app.scroll[2],
        plain,
    );
    render_text_pane(
        f,
        cols[1],
        "Device Logs",
        app.focus == Pane::Logs,
        &app.logs,
        app.scroll[0],
        colored,
    );
}

fn render_zoom(f: &mut ratatui::Frame, app: &App, body: Rect) {
    let plain = |s: &String| Line::raw(s.clone());
    let colored =
        |(s, c): &(String, Color)| Line::from(Span::styled(s.clone(), Style::new().fg(*c)));
    match app.focus {
        Pane::Logs => render_text_pane(f, body, "", false, &app.logs, app.scroll[0], colored),
        Pane::Events => render_text_pane(f, body, "", false, &app.events, app.scroll[1], plain),
        Pane::Responses => {
            render_text_pane(f, body, "", false, &app.responses, app.scroll[2], plain)
        }
        Pane::Command => render_command(f, body, app),
    }
}

/// How many terminal rows a line of display width `w` occupies once wrapped to `width`
/// columns (matching `Wrap { trim: false }`): `ceil(w / width)`, at least one row.
fn wrapped_rows(w: usize, width: usize) -> usize {
    if width == 0 {
        1
    } else {
        w.div_ceil(width).max(1)
    }
}

/// Render a bottom-anchored scrollback pane.
///
/// The window is anchored by **visual rows, not item count**: with `Wrap { trim: false }` a
/// long item spans several rows, so counting items would push the newest lines below the
/// clip and hide them (they were only reachable by scrolling *up*). Instead we walk items
/// from the back, summing each one's wrapped row count, until we've gathered `inner_h`
/// rows (plus the user's `scrollback`), then render that slice with a Paragraph `scroll`
/// offset that trims the partial item at the top so the newest line sits exactly on the
/// bottom edge. Only the visible items are materialized, not the whole `CAP`-deep deque.
fn render_text_pane<T>(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    focused: bool,
    items: &VecDeque<T>,
    scrollback: usize,
    to_line: impl Fn(&T) -> Line<'static>,
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
    let bordered = !title.is_empty();
    let inner_h = area.height.saturating_sub(if bordered { 2 } else { 0 }) as usize;
    // Wrap width = inner content width (both borders removed when bordered).
    let inner_w = area.width.saturating_sub(if bordered { 2 } else { 0 }) as usize;
    if inner_h == 0 {
        f.render_widget(Paragraph::new(Vec::<Line>::new()).block(block), area);
        return;
    }

    // Materialize each candidate line lazily from the back, accumulating its wrapped row
    // count until we've covered the visible window plus the requested scroll-up. `want` can
    // exceed what exists (short buffer) — that's fine, we just render everything.
    let want = inner_h + scrollback;
    let mut collected: Vec<Line> = Vec::new();
    let mut rows_by_line: Vec<usize> = Vec::new();
    let mut total_rows = 0usize;
    for item in items.iter().rev() {
        let line = to_line(item);
        let rows = wrapped_rows(line.width(), inner_w);
        total_rows += rows;
        rows_by_line.push(rows);
        collected.push(line);
        if total_rows >= want {
            break;
        }
    }
    collected.reverse();
    rows_by_line.reverse();

    // Bottom of the collected block sits at row `total_rows`; the visible window's top row
    // (from the top of `collected`) is `total_rows - inner_h - scrollback`, clamped to 0.
    let top_row = total_rows.saturating_sub(inner_h + scrollback);
    // Scrolling by whole rows can land mid-item; Paragraph's `scroll` skips exactly that many
    // rendered rows, so we can pass `top_row` directly (it trims the partial top item).
    let p = Paragraph::new(collected)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((top_row as u16, 0));
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
    f.render_widget(
        Paragraph::new(format!("/ {}", app.input)).block(block),
        area,
    );
    if focused {
        // Cursor after the "/ " prefix.
        let cx = inner.x + 2 + app.input[..app.cursor].chars().count() as u16;
        f.set_cursor_position((cx.min(inner.x + inner.width.saturating_sub(1)), inner.y));
    }
}

/// The ratatui color a Log line is tinted with, by severity. (The label text itself comes
/// from the shared `render::level_label` via `render::log_line`, so it can't drift.)
fn level_color(l: Level) -> Color {
    match l {
        Level::Error => Color::Red,
        Level::Warn => Color::Yellow,
        Level::Info => Color::Green,
        Level::Debug => Color::Cyan,
        Level::Trace => Color::DarkGray,
    }
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}
fn now_date() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

// ===========================================================================
// Tests
//
// The TUI can't be driven interactively here (no TTY), but its two pure halves *can* be:
// `ui()` renders into ratatui's `TestBackend` (an in-memory cell grid we can read back as
// text), and `handle_key`/`handle_frame` are ordinary state transitions over `App`. These
// cover the input-handling fixes (C22 char-boundary completion, C24 modifier handling) and
// the frame-reassembly fixes (C18 cmd_id filtering, C19 chunk-gap flagging) without hardware.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tower_protocol::encode_frame;
    use tower_protocol::msg::{Candidate, CandidateKind, ShellResponse};

    fn test_app() -> App {
        App::new("/dev/mock".to_string())
    }

    /// A COBS-framed wire frame whose header advertises protocol version `ver` (top 3 bits of
    /// `ver_type`) — i.e. what a firmware built against a *different* tower-protocol tag puts on
    /// the wire. `decode_frame` rejects it at the version check (before CRC), so `handle_frame`
    /// sees `Error::BadVersion`. Returns the deframed *inner* bytes (what `handle_frame` takes).
    fn bad_version_inner(ver: u8) -> Vec<u8> {
        // inner = ver_type(1) | seq(2) | payload | crc(4); min length is HDR+CRC = 7. The
        // version check fires before CRC, so the payload/crc contents are irrelevant here.
        let mut inner = vec![(ver << 5) | (MsgType::Hello as u8 & 0x1F), 0, 0];
        inner.extend_from_slice(&[0, 0, 0, 0]);
        inner
    }

    /// A deframed inner frame with a *valid* header version + CRC but a caller-supplied raw
    /// payload — so `decode_frame` succeeds and the postcard parse is what's exercised (e.g.
    /// an empty payload for a `Log` fails to parse → a payload error, not a frame error).
    fn valid_inner(mt: MsgType, seq: u16, payload: &[u8]) -> Vec<u8> {
        let mut inner = vec![(tower_protocol::PROTOCOL_VERSION << 5) | (mt as u8 & 0x1F)];
        inner.extend_from_slice(&seq.to_le_bytes());
        inner.extend_from_slice(payload);
        let crc = tower_protocol::crc::crc32_ieee(&inner);
        inner.extend_from_slice(&crc.to_le_bytes());
        inner
    }

    /// The deframed inner bytes of a well-formed `Log` frame at sequence `seq`.
    fn log_inner(seq: u16) -> Vec<u8> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(
            MsgType::Log,
            seq,
            &Log {
                level: Level::Info,
                uptime_us: 0,
                module: "t",
                message: "m",
            },
            &mut buf,
        )
        .unwrap();
        let mut dec = FrameDecoder::new();
        let mut inner = Vec::new();
        for &b in &buf[..n] {
            if let Some(f) = dec.push(b) {
                inner = f.to_vec();
            }
        }
        inner
    }

    /// Render `ui()` into an 80x24 test terminal and return the whole screen as text (rows
    /// joined by newlines), trailing spaces trimmed — enough to assert on visible content.
    fn render_to_text(app: &App) -> String {
        render_to_text_sized(app, 80, 24)
    }

    /// Like [`render_to_text`] but at an explicit size — some assertions (the footer hint)
    /// need a wide terminal so the right-aligned clock doesn't overwrite the region.
    fn render_to_text_sized(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    // ---- ui() snapshots for canned states ----

    #[test]
    fn ui_disconnected_shows_reconnecting() {
        let app = test_app(); // no sp → disconnected
        let text = render_to_text(&app);
        assert!(text.contains("HARDWARIO TOWER Console"));
        assert!(text.contains("reconnecting"));
        assert!(text.contains("/dev/mock"));
        // Footer hint mentions the new quit binding.
        assert!(text.contains("^C"));
    }

    #[test]
    fn ui_split_shows_all_pane_titles() {
        let mut app = test_app();
        push_cap(&mut app.logs, ("boot ok".to_string(), Color::Green));
        push_cap(&mut app.events, "sensor temp=21".to_string());
        push_cap(&mut app.responses, "> /help".to_string());
        let text = render_to_text(&app);
        assert!(text.contains("Device Logs"));
        assert!(text.contains("Device Events"));
        assert!(text.contains("Shell Responses"));
        assert!(text.contains("Shell Command"));
        assert!(text.contains("boot ok"));
    }

    #[test]
    fn ui_zoom_shows_only_focused_pane() {
        let mut app = test_app();
        app.zoom = true;
        app.focus = Pane::Logs;
        push_cap(&mut app.logs, ("zoomed log line".to_string(), Color::Reset));
        let text = render_to_text(&app);
        assert!(text.contains("zoomed log line"));
        // In zoom, the events/responses pane borders aren't drawn.
        assert!(!text.contains("Device Events"));
    }

    #[test]
    fn ui_paused_and_hint_render() {
        let mut app = test_app();
        app.paused = true;
        app.hint = "system  radio  gpio".to_string();
        // Wide terminal so the right-aligned footer clock doesn't overwrite the hint region.
        let text = render_to_text_sized(&app, 140, 24);
        assert!(text.contains("F5 Pause"));
        assert!(text.contains("[system  radio  gpio]"));
    }

    #[test]
    fn ui_keeps_newest_line_visible_when_lines_wrap() {
        // Regression for C6: with a pane full of over-wide lines, the *newest* line must be
        // on screen (previously wrapped items pushed it below the clip).
        let mut app = test_app();
        app.zoom = true;
        app.focus = Pane::Logs;
        let wide = "X".repeat(200); // wraps to several rows at width 80
        for i in 0..40 {
            push_cap(&mut app.logs, (format!("{i}-{wide}"), Color::Reset));
        }
        push_cap(&mut app.logs, ("NEWEST-MARKER".to_string(), Color::Reset));
        let text = render_to_text(&app);
        assert!(
            text.contains("NEWEST-MARKER"),
            "the most recent line must stay visible even when earlier lines wrap"
        );
    }

    // ---- handle_key: multibyte input, modifiers (C24) ----

    #[test]
    fn typing_multibyte_chars_keeps_cursor_on_boundary() {
        let mut app = test_app();
        for ch in "über→π".chars() {
            handle_key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(app.input, "über→π");
        assert_eq!(app.cursor, app.input.len());
        assert!(app.input.is_char_boundary(app.cursor));
    }

    #[test]
    fn ctrl_c_quits_from_any_pane() {
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit);
        // And it must NOT have inserted a literal 'c' into the command line.
        assert_eq!(app.input, "");
    }

    #[test]
    fn ctrl_modified_char_is_not_inserted() {
        let mut app = test_app();
        handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(app.input, ""); // Ctrl+X is not text
    }

    #[test]
    fn q_quits_only_in_non_command_panes() {
        let mut app = test_app();
        app.focus = Pane::Logs;
        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit);

        let mut app2 = test_app(); // focus defaults to Command
        handle_key(&mut app2, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app2.quit);
        assert_eq!(app2.input, "q"); // in the command pane, 'q' is just text
    }

    #[test]
    fn ctrl_u_and_ctrl_w_edit_the_line() {
        let mut app = test_app();
        for ch in "/system radio".chars() {
            handle_key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        handle_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL); // kill "radio"
        assert_eq!(app.input, "/system ");
        handle_key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL); // kill to start
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
    }

    // ---- apply_completion: device-supplied token_start can't panic (C22) ----

    fn completions(
        token_start: u16,
        prefix: &str,
        cands: &[(&str, CandidateKind)],
    ) -> ShellCompletions<'static> {
        // Leak the strings so the returned struct is 'static for the test (small, one-shot).
        let mut v: heapless::Vec<Candidate<'static>, 16> = heapless::Vec::new();
        for (t, k) in cands {
            v.push(Candidate {
                text: Box::leak(t.to_string().into_boxed_str()),
                kind: *k,
            })
            .unwrap();
        }
        ShellCompletions {
            req_id: 1,
            token_start,
            common_prefix: Box::leak(prefix.to_string().into_boxed_str()),
            candidates: v,
            more: false,
        }
    }

    #[test]
    fn completion_with_mid_char_token_start_is_ignored() {
        // "π" is 2 bytes; token_start=1 falls inside it. apply_completion must bail, not panic.
        let mut app = test_app();
        app.input = "π".to_string();
        app.cursor = app.input.len();
        let c = completions(1, "", &[("system", CandidateKind::Command)]);
        apply_completion(&mut app, &c); // must not panic
        assert_eq!(app.input, "π"); // unchanged
    }

    #[test]
    fn completion_past_cursor_is_ignored() {
        let mut app = test_app();
        app.input = "ab".to_string();
        app.cursor = 1;
        // token_start beyond the cursor is nonsensical → ignore.
        let c = completions(2, "", &[("x", CandidateKind::Command)]);
        apply_completion(&mut app, &c);
        assert_eq!(app.input, "ab");
    }

    #[test]
    fn single_completion_is_inserted_with_separator() {
        let mut app = test_app();
        app.input = "/sys".to_string();
        app.cursor = app.input.len();
        let c = completions(1, "system", &[("system", CandidateKind::Menu)]);
        apply_completion(&mut app, &c);
        assert_eq!(app.input, "/system/"); // Menu kind appends '/'
        assert_eq!(app.cursor, app.input.len());
    }

    // ---- handle_frame: cmd_id filtering (C18) + chunk-gap flag (C19) ----

    fn resp_frame(cmd_id: u16, chunk: u16, last: bool, text: &str) -> Vec<u8> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(
            MsgType::ShellResponse,
            0,
            &ShellResponse {
                cmd_id,
                result: 0,
                chunk,
                last,
                text,
            },
            &mut buf,
        )
        .unwrap();
        // decode_frame in handle_frame wants the *inner* bytes; feed via a decoder to deframe.
        let mut dec = FrameDecoder::new();
        let mut inner = Vec::new();
        for &b in &buf[..n] {
            if let Some(f) = dec.push(b) {
                inner = f.to_vec();
            }
        }
        inner
    }

    #[test]
    fn handle_frame_ignores_wrong_cmd_id() {
        let mut app = test_app();
        app.pending_cmd = Some(5);
        app.next_chunk = 0;
        // A response for a different command must not populate this one (C18).
        handle_frame(&mut app, &resp_frame(9, 0, true, "stale"));
        assert!(app.responses.is_empty());
        assert!(app.resp_buf.is_empty());
    }

    #[test]
    fn handle_frame_reassembles_matching_cmd_id() {
        let mut app = test_app();
        app.pending_cmd = Some(5);
        app.next_chunk = 0;
        handle_frame(&mut app, &resp_frame(5, 0, false, "line one\n"));
        handle_frame(&mut app, &resp_frame(5, 1, true, "line two"));
        let joined: Vec<String> = app.responses.iter().cloned().collect();
        assert_eq!(joined, vec!["line one".to_string(), "line two".to_string()]);
        assert_eq!(app.pending_cmd, None); // cleared on `last`
    }

    #[test]
    fn handle_frame_flags_chunk_gap() {
        let mut app = test_app();
        app.pending_cmd = Some(2);
        app.next_chunk = 0;
        handle_frame(&mut app, &resp_frame(2, 0, false, "a"));
        handle_frame(&mut app, &resp_frame(2, 2, true, "c")); // chunk 1 missing → gap (C19)
        let joined: String = app.responses.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(joined.contains("chunk dropped"));
    }

    // ---- handle_frame: decode-failure surfacing + seq gaps + payload errors ----

    #[test]
    fn handle_frame_surfaces_bad_version() {
        // A frame tagged with a different protocol version must be counted (not silently
        // dropped) and, on the first one, shout a red banner naming the version + remedy.
        let mut app = test_app();
        handle_frame(&mut app, &bad_version_inner(1));
        assert_eq!(app.decode_failures, 1);
        assert_eq!(app.mismatch_got, Some(1));
        assert!(app.warned_mismatch);
        assert!(
            app.logs
                .iter()
                .any(|(s, c)| *c == Color::Red && s.contains("PROTOCOL MISMATCH")),
            "the first bad-version frame must push a red mismatch line"
        );
        // A second one bumps the count but doesn't spam a second banner.
        handle_frame(&mut app, &bad_version_inner(2));
        assert_eq!(app.decode_failures, 2);
        assert_eq!(
            app.logs
                .iter()
                .filter(|(s, _)| s.contains("PROTOCOL MISMATCH"))
                .count(),
            1
        );
    }

    #[test]
    fn handle_frame_counts_seq_gaps() {
        let mut app = test_app();
        handle_frame(&mut app, &log_inner(10));
        handle_frame(&mut app, &log_inner(11)); // contiguous — no gap
        handle_frame(&mut app, &log_inner(20)); // jump — one gap
        assert_eq!(app.seq_gaps, 1);
        assert_eq!(app.last_seq, Some(20));
    }

    #[test]
    fn handle_frame_rebaselines_seq_on_hello() {
        // A Hello re-baselines seq tracking, so a fresh session at seq 0 after an earlier
        // stream isn't reported as a spurious gap.
        let mut app = test_app();
        handle_frame(&mut app, &log_inner(100));
        handle_frame(&mut app, &valid_inner(MsgType::Hello, 0, &[])); // empty Hello payload
        // The empty-payload Hello counts as a payload error, but the seq re-baseline still ran.
        assert_eq!(app.seq_gaps, 0);
        assert_eq!(app.last_seq, Some(0));
    }

    #[test]
    fn handle_frame_counts_payload_errors() {
        // A frame that decodes at the frame layer (version + CRC ok) but whose payload isn't a
        // valid Log is counted, not silently dropped.
        let mut app = test_app();
        handle_frame(&mut app, &valid_inner(MsgType::Log, 0, &[]));
        assert_eq!(app.payload_errors, 1);
        assert_eq!(app.decode_failures, 0);
    }

    // ---- ui(): decode diagnostics + reconnect error in the header (C-tui-1, C-tui-5) ----

    #[test]
    fn ui_header_shows_decode_diagnostics() {
        let mut app = test_app();
        app.decode_failures = 37;
        app.mismatch_got = Some(2);
        app.seq_gaps = 4;
        let text = render_to_text_sized(&app, 140, 24);
        assert!(text.contains("37 bad frames"));
        assert!(text.contains("(v2?)"));
        assert!(text.contains("4 seq gaps"));
    }

    #[test]
    fn ui_header_shows_reconnect_error() {
        let mut app = test_app(); // disconnected (sp = None)
        app.last_open_error = Some("Device or resource busy".to_string());
        let text = render_to_text_sized(&app, 140, 24);
        assert!(text.contains("reconnecting"));
        assert!(text.contains("Device or resource busy"));
    }

    // ---- send_command: a failed send must not tear down / desync the UI (C-tui-4) ----

    #[test]
    fn send_command_keeps_input_when_send_fails() {
        // With no connection the send fails; the typed line must survive (for retry), no
        // reassembly is armed, nothing is echoed, cmd_id doesn't advance, and a hint explains.
        let mut app = test_app(); // sp = None
        app.input = "/system radio".to_string();
        app.cursor = app.input.len();
        send_command(&mut app);
        assert_eq!(app.input, "/system radio");
        assert_eq!(app.pending_cmd, None);
        assert!(app.responses.is_empty());
        assert_eq!(app.cmd_id, 1);
        assert!(!app.hint.is_empty());
    }
}
