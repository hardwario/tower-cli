//! The gateway TUI — the interactive frontend of `tower gateway` (the default mode).
//! Same DNA as the `tower console` TUI (`src/tui.rs`): a single synchronous 33 ms
//! poll loop, gray header/footer with chip toggles, LightRed focused border — but a
//! layout built for network coordination:
//!
//! ```text
//!  Nodes table (select/remove/rename/reveal) │ Radio chart (ambient bars + RX/TX)
//!  ──────────────────────────────────────────┼──────────────────────────────────────
//!  per-node remote-shell dialog (⌛ pending)  │ MQTT feed (JSON-highlighted; F7 = per-
//!                                            │ node filter, view-only — nothing lost)
//!  ──────────────────────────────────────────┴──────────────────────────────────────
//!  gateway log strip (full width)
//! ```
//!
//! All actions ride the engine's own surfaces (`FrontendCmd` → the same code paths
//! the MQTT clients hit), so the TUI is a *view* over the bridge, never a second
//! implementation. The F2 pairing menu offers OTA (countdown popup, Esc cancels),
//! "catch the cable-connected device" (pairs the first newly plugged port), and a
//! grow-only port picker — the cable flows run `gateway::pair` on a worker thread
//! (the same five steps as `tower nodes add --port`). Panes focus by **F1 / Shift-F1**
//! (clockwise / counter-clockwise), a mouse click (when capture is on), or **F3** to zoom
//! the focused pane borderless; **F9** toggles mouse capture so terminal text selection
//! works. While capture is on, the footer chips are clickable buttons for their keys.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Chart, Clear, Dataset, GraphType, LegendPosition, Paragraph, Row, Table, Wrap,
};

use super::engine::{Event, FrontendCmd, Input, MqttDir, MqttMsg, NodeView, RadioSample};
use super::pair::{self, CableMsg, GwParams};
use super::payload::{Pairing, ShellReq};
use super::topics;
use crate::EXIT_OK;

const LOG_CAP: usize = 1000;
const DIALOG_CAP: usize = 500;
/// MQTT-feed retention (messages, both directions). The per-node filter is a view
/// predicate over this deque — filtering never drops a message.
const MQTT_CAP: usize = 500;
/// Radio-graph retention, in readings (the visible window is the pane width — one
/// bar per reading; this only bounds the deques for the widest terminals).
const CHART_HISTORY: usize = 600;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Nodes,
    Radio,
    Shell,
    Mqtt,
    Log,
}

enum Modal {
    ConfirmRemove(u32),
    Rename {
        node: u32,
        buf: String,
    },
    ConfirmReveal(u32),
    Key {
        node: u32,
        key: String,
        shown: Instant,
    },
    /// F2 — the pairing submenu (OTA / catch-the-cable / pick-a-port).
    PairMenu {
        selected: usize,
    },
    /// OTA window open: live countdown from `App::pairing`; Esc cancels.
    PairOta {
        opened_at: Instant,
        seen_open: bool,
    },
    /// Waiting for a NEW serial device to appear (baseline = ports at entry);
    /// the first newcomer is paired automatically. 60 s or Esc.
    PairCatch {
        baseline: Vec<String>,
        deadline: Instant,
    },
    /// Pick a port from the live list. The list only GROWS (new ports append at
    /// the end — positions never shuffle under the cursor); a removed port is
    /// the one exception and shrinks it.
    PairPorts {
        ports: Vec<String>,
        selected: usize,
    },
    /// A cable-pairing worker is running against `port` — progress streams in.
    /// Success auto-closes; a failure stays until dismissed.
    PairRun {
        port: String,
        lines: Vec<String>,
        failed: Option<String>,
    },
    /// A transient outcome notice (e.g. catch-window timeout); any key closes.
    Info {
        title: String,
        text: String,
    },
    Pending {
        node: u32,
        selected: usize,
    },
}

enum DialogLine {
    Sent(String),
    Text(String),
    Err(String),
    Done(u8),
}

/// A live cable-pairing worker: its progress feed, and the return path for the
/// engine RPC responses it is waiting on (`tui-cbl-*` ids are forwarded here).
struct CableSession {
    rx: std::sync::mpsc::Receiver<CableMsg>,
    rpc_tx: Sender<super::payload::RpcResponse>,
}

/// Where the panes landed in the last frame — the mouse hit-test targets.
#[derive(Default, Clone, Copy)]
struct PaneZones {
    nodes: Rect,
    radio: Rect,
    shell: Rect,
    mqtt: Rect,
    log: Rect,
}

struct App {
    events: Receiver<Event>,
    input: Sender<Input>,
    prefix: String,
    port: String,
    quit: bool,
    focus: Pane,
    /// F3: the focused pane fills the whole body, borderless.
    zoom: bool,
    /// F9: mouse capture (click-to-focus + footer-chip buttons). Off = the
    /// terminal's native text selection/copy works again.
    mouse: bool,
    paused: bool,
    serial_up: bool,
    mqtt_up: bool,
    nodes: Vec<NodeView>,
    selected: usize,
    /// The node whose dialog the shell pane shows (index into `nodes`).
    shell_node: usize,
    dialogs: HashMap<u32, VecDeque<DialogLine>>,
    /// Per-node partial shell line — the tail of a chunk that ended mid-line,
    /// keyed by the response id it belongs to (chunk reassembly, docs above).
    shell_partial: HashMap<u32, (String, String)>,
    line: String,
    cursor: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
    log: VecDeque<String>,
    log_scroll: usize,
    /// The broker traffic feed (both directions, capped). Never filtered at
    /// ingest — `mqtt_filter` is applied at render time only.
    mqtt: VecDeque<MqttMsg>,
    mqtt_scroll: usize,
    /// `Some(addr)` = show only that node's `nodes/{addr}/…` topics (visual only).
    mqtt_filter: Option<u32>,
    /// Ambient readings, newest last — the chart's shift register. One reading =
    /// one column; the graph moves only when a reading arrives (no wall clock).
    ambient: VecDeque<f64>,
    /// Monotonic count of ambient readings ever received (the newest one's seq).
    ambient_seq: u64,
    /// Measured reading cadence (EMA, ms) — keeps the chart's time labels honest
    /// for any device `stats-period` (500 ms by default).
    ambient_ema_ms: f64,
    ambient_prev: Option<Instant>,
    /// RX/TX marks, each pinned to the ambient reading (`seq`) it arrived during —
    /// so marks scroll in lockstep with the trace instead of by wall clock.
    rx_marks: VecDeque<(u64, f64)>,
    tx_marks: VecDeque<(u64, f64)>,
    /// Last ambient sample, for the chart title's live readout.
    last_ambient: Option<(i16, u8)>,
    pairing: Option<Pairing>,
    modal: Option<Modal>,
    next_req: u64,
    /// This gateway's network parameters (for the cable-pairing worker).
    gw: GwParams,
    /// The running cable-pairing worker, if any.
    cable: Option<CableSession>,
    /// Last serial-port enumeration (the pairing modals poll at 1 Hz).
    last_port_poll: Instant,
    /// Last-frame pane rectangles for click-to-focus.
    zones: PaneZones,
    /// Last-frame footer-chip rectangles — each chip is a button; a click
    /// presses the key it carries.
    chips: Vec<(Rect, KeyCode)>,
}

impl App {
    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn selected_node(&self) -> Option<&NodeView> {
        self.nodes.get(self.selected)
    }

    fn shell_target(&self) -> Option<&NodeView> {
        self.nodes.get(self.shell_node)
    }

    fn dialog_push(&mut self, node: u32, line: DialogLine) {
        let d = self.dialogs.entry(node).or_default();
        if d.len() >= DIALOG_CAP {
            d.pop_front();
        }
        d.push_back(line);
    }

    /// Flush a node's pending partial shell line (a terminal event ends the stream).
    fn shell_flush(&mut self, node: u32) {
        if let Some((_, p)) = self.shell_partial.remove(&node)
            && !p.is_empty()
        {
            self.dialog_push(node, DialogLine::Text(p));
        }
    }

    fn rpc(&mut self, op: &str, params: serde_json::Value) {
        self.next_req += 1;
        let _ = self.input.send(Input::Frontend(FrontendCmd::Rpc(
            super::payload::RpcRequest {
                id: format!("tui-{}", self.next_req),
                op: op.into(),
                params,
            },
        )));
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Log(l) => {
                if !self.paused {
                    self.push_log(l);
                }
            }
            Event::Link { serial_up, mqtt_up } => {
                self.serial_up = serial_up;
                self.mqtt_up = mqtt_up;
            }
            Event::Registry(nodes) => {
                // Keep selection/dialog anchored to the same node across refreshes.
                let sel_id = self.selected_node().map(|n| n.addr);
                let dlg_id = self.shell_target().map(|n| n.addr);
                self.nodes = nodes;
                if let Some(id) = sel_id
                    && let Some(i) = self.nodes.iter().position(|n| n.addr == id)
                {
                    self.selected = i;
                } else {
                    self.selected = self.selected.min(self.nodes.len().saturating_sub(1));
                }
                if let Some(id) = dlg_id
                    && let Some(i) = self.nodes.iter().position(|n| n.addr == id)
                {
                    self.shell_node = i;
                } else {
                    self.shell_node = self.shell_node.min(self.nodes.len().saturating_sub(1));
                }
            }
            Event::Shell { node, rsp } => {
                if let Some(err) = rsp.error {
                    self.shell_flush(node);
                    self.dialog_push(node, DialogLine::Err(err));
                } else {
                    // Chunks split at BYTE offsets (the ≤56 B radio payload), not at
                    // line breaks — a line can straddle two chunks. Accumulate into a
                    // per-node partial buffer and only break on real newlines; the
                    // remainder waits for the next chunk (or the done flush).
                    let mut buf = match self.shell_partial.remove(&node) {
                        Some((id, p)) if id == rsp.id => p,
                        // A different command's tail was left behind — flush it.
                        Some((_, p)) => {
                            if !p.is_empty() {
                                self.dialog_push(node, DialogLine::Text(p));
                            }
                            String::new()
                        }
                        None => String::new(),
                    };
                    buf.push_str(&rsp.text);
                    while let Some(i) = buf.find('\n') {
                        let line: String = buf.drain(..=i).collect();
                        let line = line.trim_end_matches(['\n', '\r']);
                        self.dialog_push(node, DialogLine::Text(line.to_string()));
                    }
                    if rsp.done {
                        if !buf.is_empty() {
                            self.dialog_push(node, DialogLine::Text(buf));
                        }
                        self.dialog_push(node, DialogLine::Done(rsp.result));
                    } else {
                        self.shell_partial.insert(node, (rsp.id, buf));
                    }
                }
            }
            Event::Radio(sample) => {
                // Same pause discipline as the log/MQTT feeds. Unlike those (where pause
                // freezes a view over retained data), the sample deques ARE the chart's
                // data — so pause simply stops ingest, and the oscilloscope holds still.
                if self.paused {
                    return;
                }
                match sample {
                    RadioSample::Ambient { dbm, channel } => {
                        self.last_ambient = Some((dbm, channel));
                        // Measure the actual cadence so the chart's time labels
                        // stay honest whatever the device's stats-period is.
                        let now = Instant::now();
                        if let Some(prev) = self.ambient_prev {
                            let d = now.duration_since(prev).as_millis() as f64;
                            if (50.0..=60_000.0).contains(&d) {
                                self.ambient_ema_ms = 0.8 * self.ambient_ema_ms + 0.2 * d;
                            }
                        }
                        self.ambient_prev = Some(now);
                        self.ambient_seq += 1;
                        if self.ambient.len() >= CHART_HISTORY {
                            self.ambient.pop_front();
                        }
                        self.ambient.push_back(dbm as f64);
                    }
                    RadioSample::Rx { rssi, .. } => {
                        if self.rx_marks.len() >= CHART_HISTORY {
                            self.rx_marks.pop_front();
                        }
                        self.rx_marks.push_back((self.ambient_seq, rssi as f64));
                    }
                    RadioSample::Tx { .. } => {
                        if self.tx_marks.len() >= CHART_HISTORY {
                            self.tx_marks.pop_front();
                        }
                        // TX has no receive RSSI — pin the marks to a fixed lane so
                        // they read as activity ticks, not measurements.
                        self.tx_marks.push_back((self.ambient_seq, -35.0));
                    }
                }
            }
            Event::Mqtt(m) => {
                // Same pause discipline as the log; the filter never gates ingest.
                if !self.paused {
                    if self.mqtt.len() >= MQTT_CAP {
                        self.mqtt.pop_front();
                    }
                    self.mqtt.push_back(m);
                }
            }
            Event::Pairing(p) => {
                if p.state == "idle" && self.pairing.as_ref().is_some_and(|o| o.state == "open") {
                    match &p.joined {
                        Some(j) => self.push_log(format!("paired {j}")),
                        None => self.push_log("pairing window closed".into()),
                    }
                }
                self.pairing = Some(p);
            }
            Event::Rpc(rsp) => {
                // A cable-pairing worker's RPC — hand the response through; it is
                // not for the interactive surfaces.
                if rsp.id.starts_with("tui-cbl-") {
                    if let Some(c) = &self.cable {
                        let _ = c.rpc_tx.send(rsp);
                    }
                    return;
                }
                if !rsp.ok {
                    self.push_log(format!(
                        "action failed: {}",
                        rsp.error.unwrap_or_else(|| "gateway refused".into())
                    ));
                } else if let Some(key) = rsp.data.get("key").and_then(|v| v.as_str()) {
                    // A reveal answered — swap the confirm modal for the key view.
                    let node = rsp
                        .data
                        .get("addr")
                        .and_then(|v| v.as_str())
                        .and_then(topics::parse_node_hex)
                        .unwrap_or(0);
                    self.modal = Some(Modal::Key {
                        node,
                        key: key.to_string(),
                        shown: Instant::now(),
                    });
                }
            }
        }
    }
}

pub(crate) fn run(
    events: Receiver<Event>,
    input: Sender<Input>,
    prefix: String,
    port: String,
    gw: GwParams,
) -> Result<u8> {
    let mut app = App {
        events,
        input,
        prefix,
        port,
        quit: false,
        focus: Pane::Nodes,
        zoom: false,
        mouse: true,
        paused: false,
        serial_up: true,
        mqtt_up: false,
        nodes: Vec::new(),
        selected: 0,
        shell_node: 0,
        dialogs: HashMap::new(),
        shell_partial: HashMap::new(),
        line: String::new(),
        cursor: 0,
        history: Vec::new(),
        hist_idx: None,
        log: VecDeque::new(),
        log_scroll: 0,
        mqtt: VecDeque::new(),
        mqtt_scroll: 0,
        mqtt_filter: None,
        ambient: VecDeque::new(),
        ambient_seq: 0,
        ambient_ema_ms: 500.0,
        ambient_prev: None,
        rx_marks: VecDeque::new(),
        tx_marks: VecDeque::new(),
        last_ambient: None,
        pairing: None,
        modal: None,
        next_req: 0,
        gw,
        cable: None,
        last_port_poll: Instant::now(),
        zones: PaneZones::default(),
        chips: Vec::new(),
    };
    let mut terminal = ratatui::init();
    // Click-to-focus needs mouse events; released with the terminal below.
    let _ = ratatui::crossterm::execute!(std::io::stdout(), event::EnableMouseCapture);
    let res = run_loop(&mut terminal, &mut app);
    let _ = ratatui::crossterm::execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
    res.map(|()| EXIT_OK)
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        while let Ok(ev) = app.events.try_recv() {
            app.on_event(ev);
        }
        // Auto-hide a revealed key after 30 s — screens get walked away from.
        if let Some(Modal::Key { shown, .. }) = &app.modal
            && shown.elapsed().as_secs() >= 30
        {
            app.modal = None;
        }
        tick_pairing(app);
        terminal.draw(|f| ui(f, app))?;
        if event::poll(std::time::Duration::from_millis(33))? {
            match event::read()? {
                TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                    handle_key(app, key.code, key.modifiers);
                }
                TermEvent::Mouse(m) => handle_mouse(app, m),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Click-to-focus: a left click inside a pane focuses it (modals keep the
/// keyboard-only flow — a click just falls through while one is open). The
/// footer chips double as buttons: a click presses the chip's key, and a
/// Shift-click on the F1 chip walks focus counter-clockwise like Shift-F1.
fn handle_mouse(app: &mut App, m: event::MouseEvent) {
    if !app.mouse
        || app.modal.is_some()
        || m.kind != event::MouseEventKind::Down(event::MouseButton::Left)
    {
        return;
    }
    let hit = |r: Rect| -> bool {
        m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
    };
    let chip = app.chips.iter().find(|(r, _)| hit(*r)).map(|&(_, k)| k);
    if let Some(key) = chip {
        match key {
            // Node-scoped chips go straight to the Nodes key map so they act
            // on the selected node no matter which pane holds focus.
            KeyCode::F(4) | KeyCode::F(6) | KeyCode::Delete | KeyCode::Char('p') => {
                handle_nodes_key(app, key);
            }
            _ => handle_key(app, key, m.modifiers),
        }
        return;
    }
    for (rect, pane) in [
        (app.zones.nodes, Pane::Nodes),
        (app.zones.radio, Pane::Radio),
        (app.zones.shell, Pane::Shell),
        (app.zones.mqtt, Pane::Mqtt),
        (app.zones.log, Pane::Log),
    ] {
        if hit(rect) {
            app.focus = pane;
            return;
        }
    }
}

/// USB serial ports minus the gateway's own (a node can't be on OUR port).
fn poll_ports(own: &str) -> Vec<String> {
    crate::port::usb_ports()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p != own)
        .collect()
}

/// Launch the cable-pairing worker against `port` and switch to the progress view.
fn start_cable(app: &mut App, port: String) {
    let (ptx, prx) = std::sync::mpsc::channel();
    let (rtx, rrx) = std::sync::mpsc::channel();
    pair::spawn(port.clone(), app.gw, app.input.clone(), rrx, ptx);
    app.cable = Some(CableSession {
        rx: prx,
        rpc_tx: rtx,
    });
    app.modal = Some(Modal::PairRun {
        port,
        lines: Vec::new(),
        failed: None,
    });
}

/// Per-frame pairing housekeeping: cable-worker progress, the OTA popup's
/// lifecycle, the catch-window detection/timeout, and the port-list refresh.
fn tick_pairing(app: &mut App) {
    // Drain the cable worker's stream (progress → modal; outcome → log + modal).
    let mut msgs = Vec::new();
    if let Some(c) = &app.cable {
        while let Ok(m) = c.rx.try_recv() {
            msgs.push(m);
        }
    }
    for m in msgs {
        match m {
            CableMsg::Progress(s) => {
                if let Some(Modal::PairRun { lines, .. }) = &mut app.modal {
                    lines.push(s);
                }
            }
            CableMsg::Done { addr } => {
                app.cable = None;
                app.push_log(format!("cable-paired {}", topics::node_hex(addr)));
                // Success auto-closes ("when done, it automatically ends").
                if matches!(app.modal, Some(Modal::PairRun { .. })) {
                    app.modal = None;
                }
            }
            CableMsg::Failed(e) => {
                app.cable = None;
                app.push_log(format!("cable pairing failed: {e}"));
                if let Some(Modal::PairRun { failed, .. }) = &mut app.modal {
                    *failed = Some(e);
                }
            }
        }
    }
    // OTA popup lifecycle: close once the window resolved (join/timeout/cancel
    // publish the idle state), or if it never opened (RPC refused) within 5 s.
    if let Some(Modal::PairOta {
        opened_at,
        seen_open,
    }) = &mut app.modal
    {
        let open = app.pairing.as_ref().is_some_and(|p| p.state == "open");
        if open {
            *seen_open = true;
        }
        if (*seen_open && !open) || (!*seen_open && opened_at.elapsed().as_secs() >= 5) {
            app.modal = None; // the Pairing event already logged the outcome
        }
    }
    // Catch-window: timeout, else look for a newcomer once a second.
    let poll_due = app.last_port_poll.elapsed().as_secs() >= 1;
    let mut catch_found: Option<String> = None;
    let mut catch_timeout = false;
    if let Some(Modal::PairCatch { baseline, deadline }) = &app.modal {
        if Instant::now() >= *deadline {
            catch_timeout = true;
        } else if poll_due {
            catch_found = poll_ports(&app.port)
                .into_iter()
                .find(|p| !baseline.contains(p));
        }
    }
    if catch_timeout {
        app.modal = Some(Modal::Info {
            title: " Catch cable device ".into(),
            text: "no new USB device appeared within 60 s".into(),
        });
    } else if let Some(p) = catch_found {
        start_cable(app, p);
    }
    // Port picker: refresh grow-only (append newcomers at the END — positions
    // never shuffle under the cursor); a removed port is the one exception.
    if poll_due {
        if let Some(Modal::PairPorts { ports, selected }) = &mut app.modal {
            let current = poll_ports(&app.port);
            ports.retain(|p| current.contains(p));
            for p in current {
                if !ports.contains(&p) {
                    ports.push(p);
                }
            }
            if *selected >= ports.len() {
                *selected = ports.len().saturating_sub(1);
            }
        }
        app.last_port_poll = Instant::now();
    }
}

// ---- key handling -------------------------------------------------------------

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    if app.modal.is_some() {
        return handle_modal_key(app, code);
    }
    match (code, mods) {
        (KeyCode::F(10), _) => app.quit = true,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.quit = true,
        // F1 walks the panes clockwise around the layout (Nodes → MQTT → Log →
        // Shell); Shift-F1 walks counter-clockwise. (Some terminals report
        // Shift-F1 as F13 — accept both.)
        (KeyCode::F(1), m) if !m.contains(KeyModifiers::SHIFT) => {
            app.focus = match app.focus {
                Pane::Nodes => Pane::Radio,
                Pane::Radio => Pane::Mqtt,
                Pane::Mqtt => Pane::Log,
                Pane::Log => Pane::Shell,
                Pane::Shell => Pane::Nodes,
            };
        }
        (KeyCode::F(1), _) | (KeyCode::F(13), _) => {
            app.focus = match app.focus {
                Pane::Nodes => Pane::Shell,
                Pane::Shell => Pane::Log,
                Pane::Log => Pane::Mqtt,
                Pane::Mqtt => Pane::Radio,
                Pane::Radio => Pane::Nodes,
            };
        }
        // F3 zooms the focused pane to the whole body, borderless (header/footer stay).
        (KeyCode::F(3), _) => app.zoom = !app.zoom,
        // F9 toggles mouse capture: ON = click-to-focus; OFF = the terminal's
        // native text selection/copy works (capture swallows it otherwise).
        (KeyCode::F(9), _) => {
            app.mouse = !app.mouse;
            let _ = if app.mouse {
                ratatui::crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)
            } else {
                ratatui::crossterm::execute!(std::io::stdout(), event::DisableMouseCapture)
            };
        }
        (KeyCode::F(5), _) => app.paused = !app.paused,
        // F7 toggles the per-node MQTT filter from any pane; `f` on the MQTT and
        // Nodes panes is the pane-local alias (kept off the footer, like `r`/`k`).
        (KeyCode::F(7), _) => toggle_mqtt_filter(app),
        // The node-action keys advertised on the footer work from any pane —
        // they act on the SELECTED node, exactly like their chips.
        (KeyCode::F(4) | KeyCode::F(6), _) => handle_nodes_key(app, code),
        // Del = remove (with confirmation). macOS keyboards send Backspace for
        // the key labeled "delete", so accept both — except on the Shell pane,
        // where Backspace belongs to line editing.
        (KeyCode::Delete | KeyCode::Backspace, _) if app.focus != Pane::Shell => {
            handle_nodes_key(app, KeyCode::Delete);
        }
        // F8 clears the focused feed (MQTT when its pane is focused, else the log).
        (KeyCode::F(8), _) => match app.focus {
            Pane::Mqtt => {
                app.mqtt.clear();
                app.mqtt_scroll = 0;
            }
            Pane::Radio => {
                app.ambient.clear();
                app.rx_marks.clear();
                app.tx_marks.clear();
            }
            _ => {
                app.log.clear();
                app.log_scroll = 0;
            }
        },
        (KeyCode::F(2), _) => {
            app.modal = Some(Modal::PairMenu { selected: 0 });
        }
        _ => match app.focus {
            Pane::Nodes => handle_nodes_key(app, code),
            Pane::Radio => {
                if let KeyCode::Char('q') = code {
                    app.quit = true;
                }
            }
            Pane::Shell => handle_shell_key(app, code, mods),
            Pane::Mqtt => match code {
                KeyCode::PageUp => app.mqtt_scroll = (app.mqtt_scroll + 10).min(app.mqtt.len()),
                KeyCode::PageDown => app.mqtt_scroll = app.mqtt_scroll.saturating_sub(10),
                KeyCode::Char('f') => toggle_mqtt_filter(app),
                KeyCode::Char('q') => app.quit = true,
                _ => {}
            },
            Pane::Log => match code {
                KeyCode::PageUp => app.log_scroll = (app.log_scroll + 10).min(app.log.len()),
                KeyCode::PageDown => app.log_scroll = app.log_scroll.saturating_sub(10),
                KeyCode::Char('q') => app.quit = true,
                _ => {}
            },
        },
    }
}

/// Flip the MQTT feed between "everything" and "the selected node only". A pure
/// view predicate — the underlying deque keeps every message either way.
fn toggle_mqtt_filter(app: &mut App) {
    app.mqtt_filter = match app.mqtt_filter {
        Some(_) => None,
        None => app.selected_node().map(|n| n.addr),
    };
    app.mqtt_scroll = 0; // the visible tail changed shape — re-anchor
}

fn handle_nodes_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Up => app.selected = app.selected.saturating_sub(1),
        KeyCode::Down => {
            app.selected = (app.selected + 1).min(app.nodes.len().saturating_sub(1));
        }
        KeyCode::Enter => {
            if !app.nodes.is_empty() {
                app.shell_node = app.selected;
                app.focus = Pane::Shell;
            }
        }
        KeyCode::Delete | KeyCode::Char('x') => {
            if let Some(n) = app.selected_node() {
                app.modal = Some(Modal::ConfirmRemove(n.addr));
            }
        }
        KeyCode::F(6) | KeyCode::Char('r') => {
            if let Some(n) = app.selected_node() {
                app.modal = Some(Modal::Rename {
                    node: n.addr,
                    buf: n.name.clone(),
                });
            }
        }
        KeyCode::F(4) | KeyCode::Char('k') => {
            if let Some(n) = app.selected_node() {
                app.modal = Some(Modal::ConfirmReveal(n.addr));
            }
        }
        KeyCode::Char('p') => {
            if let Some(n) = app.selected_node() {
                app.modal = Some(Modal::Pending {
                    node: n.addr,
                    selected: 0,
                });
            }
        }
        // Filter the MQTT feed to the selected node (toggle) without leaving the table.
        KeyCode::Char('f') => toggle_mqtt_filter(app),
        KeyCode::Char('q') => app.quit = true,
        _ => {}
    }
}

fn handle_shell_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    match (code, mods) {
        // Node-tab switching (Alt+arrows, or brackets on an empty line).
        (KeyCode::Left, KeyModifiers::ALT) => {
            app.shell_node = app.shell_node.saturating_sub(1);
        }
        (KeyCode::Right, KeyModifiers::ALT) => {
            app.shell_node = (app.shell_node + 1).min(app.nodes.len().saturating_sub(1));
        }
        (KeyCode::Char('['), _) if app.line.is_empty() => {
            app.shell_node = app.shell_node.saturating_sub(1);
        }
        (KeyCode::Char(']'), _) if app.line.is_empty() => {
            app.shell_node = (app.shell_node + 1).min(app.nodes.len().saturating_sub(1));
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            app.line.clear();
            app.cursor = 0;
        }
        (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => {
            app.line.insert(app.cursor, c);
            app.cursor += c.len_utf8();
        }
        (KeyCode::Backspace, _) => {
            if app.cursor > 0 {
                let mut i = app.cursor - 1;
                while !app.line.is_char_boundary(i) {
                    i -= 1;
                }
                app.line.remove(i);
                app.cursor = i;
            }
        }
        (KeyCode::Left, _) => {
            if app.cursor > 0 {
                let mut i = app.cursor - 1;
                while !app.line.is_char_boundary(i) {
                    i -= 1;
                }
                app.cursor = i;
            }
        }
        (KeyCode::Right, _) => {
            if app.cursor < app.line.len() {
                let mut i = app.cursor + 1;
                while i < app.line.len() && !app.line.is_char_boundary(i) {
                    i += 1;
                }
                app.cursor = i;
            }
        }
        (KeyCode::Up, _) if app.line.is_empty() || app.hist_idx.is_some() => {
            let idx = match app.hist_idx {
                Some(0) | None if app.history.is_empty() => return,
                None => app.history.len() - 1,
                Some(0) => 0,
                Some(i) => i - 1,
            };
            app.hist_idx = Some(idx);
            app.line = app.history[idx].clone();
            app.cursor = app.line.len();
        }
        (KeyCode::Down, _) if app.hist_idx.is_some() => {
            let i = app.hist_idx.unwrap();
            if i + 1 < app.history.len() {
                app.hist_idx = Some(i + 1);
                app.line = app.history[i + 1].clone();
            } else {
                app.hist_idx = None;
                app.line.clear();
            }
            app.cursor = app.line.len();
        }
        (KeyCode::Enter, _) => submit_line(app),
        _ => {}
    }
}

fn submit_line(app: &mut App) {
    let line = app.line.trim().to_string();
    if line.is_empty() {
        return;
    }
    let Some(target) = app.shell_target().map(|n| n.addr) else {
        app.push_log("no node selected — pair one first (F2)".into());
        return;
    };
    app.history.push(line.clone());
    app.hist_idx = None;
    app.line.clear();
    app.cursor = 0;
    app.next_req += 1;
    let req = ShellReq {
        id: format!("tui-sh-{}", app.next_req),
        line: line.clone(),
        ttl: 0,
    };
    app.dialog_push(target, DialogLine::Sent(line));
    let _ = app
        .input
        .send(Input::Frontend(FrontendCmd::Shell { node: target, req }));
}

fn handle_modal_key(app: &mut App, code: KeyCode) {
    let Some(modal) = app.modal.take() else {
        return;
    };
    match modal {
        Modal::ConfirmRemove(node) => match code {
            KeyCode::Enter | KeyCode::Char('y') => {
                app.rpc(
                    "node_remove",
                    serde_json::json!({ "addr": topics::node_hex(node) }),
                );
            }
            _ => {}
        },
        Modal::Rename { node, mut buf } => match code {
            KeyCode::Enter => {
                if !buf.is_empty() {
                    app.rpc(
                        "node_rename",
                        serde_json::json!({ "addr": topics::node_hex(node), "name": buf }),
                    );
                }
            }
            KeyCode::Esc => {}
            KeyCode::Backspace => {
                buf.pop();
                app.modal = Some(Modal::Rename { node, buf });
            }
            KeyCode::Char(c) if buf.len() < 16 => {
                buf.push(c);
                app.modal = Some(Modal::Rename { node, buf });
            }
            _ => app.modal = Some(Modal::Rename { node, buf }),
        },
        Modal::ConfirmReveal(node) => match code {
            KeyCode::Enter | KeyCode::Char('y') => {
                app.rpc(
                    "reveal_key",
                    serde_json::json!({ "addr": topics::node_hex(node) }),
                );
                // The Key modal opens when the RPC answers (Event::Rpc).
            }
            _ => {}
        },
        Modal::Key { .. } => {} // any key hides it
        Modal::PairMenu { selected } => match code {
            KeyCode::Up => {
                app.modal = Some(Modal::PairMenu {
                    selected: selected.saturating_sub(1),
                });
            }
            KeyCode::Down => {
                app.modal = Some(Modal::PairMenu {
                    selected: (selected + 1).min(2),
                });
            }
            KeyCode::Enter => match selected {
                0 => {
                    app.rpc("node_add_ota", serde_json::json!({ "window": 60 }));
                    app.modal = Some(Modal::PairOta {
                        opened_at: Instant::now(),
                        seen_open: false,
                    });
                }
                1 => {
                    app.last_port_poll = Instant::now();
                    app.modal = Some(Modal::PairCatch {
                        baseline: poll_ports(&app.port),
                        deadline: Instant::now() + std::time::Duration::from_secs(60),
                    });
                }
                _ => {
                    app.modal = Some(Modal::PairPorts {
                        ports: poll_ports(&app.port),
                        selected: 0,
                    });
                }
            },
            KeyCode::Esc | KeyCode::Char('q') => {}
            _ => app.modal = Some(Modal::PairMenu { selected }),
        },
        Modal::PairOta {
            opened_at,
            seen_open,
        } => match code {
            // Esc = cancel NOW: the engine cancels the device window and
            // publishes the idle pairing state; the modal just closes.
            KeyCode::Esc => app.rpc("pairing_cancel", serde_json::Value::Null),
            _ => {
                app.modal = Some(Modal::PairOta {
                    opened_at,
                    seen_open,
                })
            }
        },
        Modal::PairCatch { baseline, deadline } => match code {
            KeyCode::Esc => {}
            _ => app.modal = Some(Modal::PairCatch { baseline, deadline }),
        },
        Modal::PairPorts { ports, selected } => match code {
            KeyCode::Up => {
                app.modal = Some(Modal::PairPorts {
                    ports,
                    selected: selected.saturating_sub(1),
                });
            }
            KeyCode::Down => {
                let s = (selected + 1).min(ports.len().saturating_sub(1));
                app.modal = Some(Modal::PairPorts { ports, selected: s });
            }
            KeyCode::Enter => match ports.get(selected).cloned() {
                Some(p) => start_cable(app, p),
                None => app.modal = Some(Modal::PairPorts { ports, selected }),
            },
            KeyCode::Esc | KeyCode::Char('q') => {}
            _ => app.modal = Some(Modal::PairPorts { ports, selected }),
        },
        Modal::PairRun {
            port,
            lines,
            failed,
        } => {
            // A failure waits for any key; while running, Esc hides the modal
            // (the worker finishes on its own and the outcome lands in the log).
            if failed.is_none() && code != KeyCode::Esc {
                app.modal = Some(Modal::PairRun {
                    port,
                    lines,
                    failed,
                });
            }
        }
        Modal::Info { .. } => {} // any key closes
        Modal::Pending { node, selected } => {
            let count = app
                .nodes
                .iter()
                .find(|n| n.addr == node)
                .map(|n| n.pending.len())
                .unwrap_or(0);
            match code {
                KeyCode::Up => {
                    app.modal = Some(Modal::Pending {
                        node,
                        selected: selected.saturating_sub(1),
                    })
                }
                KeyCode::Down => {
                    app.modal = Some(Modal::Pending {
                        node,
                        selected: (selected + 1).min(count.saturating_sub(1)),
                    })
                }
                KeyCode::Delete | KeyCode::Char('x') => {
                    if let Some(entry) = app
                        .nodes
                        .iter()
                        .find(|n| n.addr == node)
                        .and_then(|n| n.pending.get(selected))
                    {
                        let r = entry.r#ref;
                        app.rpc(
                            "queue_drop",
                            serde_json::json!({ "addr": topics::node_hex(node), "ref": r }),
                        );
                    }
                    app.modal = Some(Modal::Pending { node, selected });
                }
                KeyCode::Esc | KeyCode::Char('q') => {}
                _ => app.modal = Some(Modal::Pending { node, selected }),
            }
        }
    }
}

// ---- rendering -------------------------------------------------------------------

fn ui(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());
    render_header(f, app, header);
    if app.zoom {
        // F3 zoom: the focused pane owns the whole body, borderless (the
        // header/footer stay). Clicks outside it have nothing to hit.
        app.zones = PaneZones::default();
        match app.focus {
            Pane::Nodes => {
                app.zones.nodes = body;
                render_nodes(f, app, body, true);
            }
            Pane::Radio => {
                app.zones.radio = body;
                render_chart(f, app, body, true);
            }
            Pane::Shell => {
                app.zones.shell = body;
                render_shell(f, app, body, true);
            }
            Pane::Mqtt => {
                app.zones.mqtt = body;
                render_mqtt(f, app, body, true);
            }
            Pane::Log => {
                app.zones.log = body;
                render_log(f, app, body, true);
            }
        }
    } else {
        // Three rows: Nodes|Radio on top, Shell|MQTT in the middle, and the gateway
        // log as a full-width strip at the bottom (always visible, never dominant).
        let [top, middle, log_area] = Layout::vertical([
            Constraint::Fill(5),
            Constraint::Fill(6),
            Constraint::Length(8),
        ])
        .areas(body);
        let [nodes_area, chart_area] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(top);
        let [shell_area, mqtt_area] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(middle);
        // Remember where the panes are for click-to-focus.
        app.zones = PaneZones {
            nodes: nodes_area,
            radio: chart_area,
            shell: shell_area,
            mqtt: mqtt_area,
            log: log_area,
        };
        render_nodes(f, app, nodes_area, false);
        render_chart(f, app, chart_area, false);
        render_shell(f, app, shell_area, false);
        render_mqtt(f, app, mqtt_area, false);
        render_log(f, app, log_area, false);
    }
    render_footer(f, app, footer);
    if app.modal.is_some() {
        render_modal(f, app, body);
    }
}

fn dot(up: bool) -> Span<'static> {
    if up {
        "●".fg(Color::Green)
    } else {
        "●".fg(Color::Red)
    }
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let bar = Style::new().bg(Color::Gray).fg(Color::Black);
    let mut spans = vec![
        Span::styled(
            " HARDWARIO TOWER Gateway ",
            bar.add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{} ", app.port), bar),
        dot(app.serial_up),
        Span::styled(" serial  ", bar),
        dot(app.mqtt_up),
        Span::styled(format!(" mqtt  prefix {}", app.prefix), bar),
    ];
    if let Some(p) = &app.pairing
        && p.state == "open"
    {
        spans.push(Span::styled(
            format!("  PAIRING {}s ", p.remaining.unwrap_or(0)),
            Style::new()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line).style(bar), area);
}

fn render_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let bar = Style::new().bg(Color::Gray).fg(Color::Black);
    // Label, lit-when-active flag, and the key a click on the chip presses.
    let chips: [(&str, bool, KeyCode); 12] = [
        ("[Shift-]F1 Focus", false, KeyCode::F(1)),
        (
            "F2 Pair",
            app.pairing.as_ref().is_some_and(|p| p.state == "open"),
            KeyCode::F(2),
        ),
        ("F3 Zoom", app.zoom, KeyCode::F(3)),
        ("F4 Key", false, KeyCode::F(4)),
        ("F5 Pause", app.paused, KeyCode::F(5)),
        ("F6 Rename", false, KeyCode::F(6)),
        ("Del Remove", false, KeyCode::Delete),
        ("p Pending", false, KeyCode::Char('p')),
        ("F7 Filter", app.mqtt_filter.is_some(), KeyCode::F(7)),
        ("F8 Clear", false, KeyCode::F(8)),
        ("F9 Mouse", app.mouse, KeyCode::F(9)),
        ("F10 Quit", false, KeyCode::F(10)),
    ];
    app.chips.clear();
    let mut spans = Vec::with_capacity(chips.len());
    let mut x = area.x;
    for (label, active, key) in chips {
        let text = format!(" {label} ");
        let w = text.chars().count() as u16;
        // Remember where the chip landed (clipped to the visible row) —
        // `handle_mouse` treats these rectangles as buttons.
        if x < area.right() {
            let rect = Rect::new(x, area.y, w.min(area.right() - x), 1);
            app.chips.push((rect, key));
        }
        x = x.saturating_add(w);
        spans.push(if active {
            Span::styled(
                text,
                Style::new()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(text, bar)
        });
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let mut b = Block::bordered().title(title);
    if focused {
        b = b.border_style(
            Style::new()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        );
    }
    b
}

fn render_nodes(f: &mut Frame, app: &App, area: Rect, zoomed: bool) {
    let header = Row::new(vec!["ADDR", "NAME", "TYPE", "SEEN", "RSSI", "PEND", "SLP"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let style = if i == app.selected {
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Row::new(vec![
                topics::node_hex(n.addr),
                if n.name.is_empty() {
                    "—".into()
                } else {
                    n.name.clone()
                },
                if n.kind.is_empty() {
                    "?".into()
                } else {
                    n.kind.clone()
                },
                n.last_seen
                    .map(|s| format!("{s}s"))
                    .unwrap_or_else(|| "never".into()),
                n.rssi.map(|r| format!("{r}")).unwrap_or_else(|| "—".into()),
                if n.pending.is_empty() {
                    "—".into()
                } else {
                    format!("⌛{}", n.pending.len())
                },
                if n.sleeping {
                    "●".into()
                } else {
                    "○".into()
                },
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Min(10),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(3),
        ],
    )
    .header(header);
    let table = if zoomed {
        table
    } else {
        table.block(pane_block(
            format!(" Nodes ({}) ", app.nodes.len()),
            app.focus == Pane::Nodes,
        ))
    };
    f.render_widget(table, area);
}

/// Ambient bars turn from "floor" to "busy" to "hot" at these levels (dBm).
const CHART_BUSY_DBM: f64 = -85.0;
const CHART_HOT_DBM: f64 = -70.0;
/// The chart's y range (dBm) — every bar rises from this floor to its reading.
const CHART_MIN_DBM: f64 = -110.0;
const CHART_MAX_DBM: f64 = -30.0;
/// Bar geometry in plot columns: 2 wide + 1 gap (a 2:1 ratio), so each reading
/// owns BAR_STRIDE columns.
const BAR_W: usize = 2;
const BAR_STRIDE: usize = BAR_W + 1;

fn render_chart(f: &mut Frame, app: &App, area: Rect, zoomed: bool) {
    // The x axis is the READING INDEX (newest = 0), not wall-clock time. Clock-based
    // placement re-quantized jittery sample timestamps into columns differently as
    // the window slid, so history appeared to blend and jump. Indexed slots give
    // every reading one stable bar for its whole life, the level stays
    // pixel-identical while it scrolls, and the graph moves exactly one bar per
    // reading — nothing animates between readings.
    //
    // Slots = the plot's column count; each reading owns BAR_STRIDE columns of it
    // (bar + gap), newest at the right edge. MUST equal the Chart's graph width
    // EXACTLY: the canvas maps x → column as round((x−left)·(width−1)/span), which is
    // 1:1 only when the bounds span slots−1 units over exactly `slots` columns — one
    // column off and the rounding drifts, rendering ragged bars/gaps (seen 2026-07-13).
    // ratatui-widgets 0.3 layout: left gutter = max(y-label width, first-x-label
    // width − 1) + 1 for the axis line — both terms held at 4 here ("-110" is 4 chars;
    // the time labels are clamped to ≤5 below) — plus the pane's 2 border columns when
    // not zoomed. The x-label/axis rows cost height only, never columns.
    let slots = (area.width.saturating_sub(if zoomed { 5 } else { 7 }) as usize).max(2);
    // Bar `back` fills columns x = -(back·BAR_STRIDE) .. and BAR_W-1 more to the
    // left; only bars whose left column still fits inside the plot are drawn.
    let bars = (slots + BAR_STRIDE - BAR_W) / BAR_STRIDE;
    let visible = app.ambient.len().min(bars);
    // Ambient as bottom-up bars: each reading's column pair is FILLED from the
    // noise floor to its level (stacked half-block points ~1 dBm apart), split into
    // level buckets so channel energy is readable as color, not just height (one
    // style per dataset). Scatter on a Chart rather than ratatui's BarChart: the
    // Chart keeps the dBm axis, the measured time labels and the rx/tx overlay.
    let mut quiet: Vec<(f64, f64)> = Vec::new();
    let mut busy: Vec<(f64, f64)> = Vec::new();
    let mut hot: Vec<(f64, f64)> = Vec::new();
    for (back, v) in app.ambient.iter().rev().take(visible).enumerate() {
        let bucket = if *v <= CHART_BUSY_DBM {
            &mut quiet
        } else if *v <= CHART_HOT_DBM {
            &mut busy
        } else {
            &mut hot
        };
        let top = v.clamp(CHART_MIN_DBM, CHART_MAX_DBM);
        for dx in 0..BAR_W {
            let x = -((back * BAR_STRIDE + dx) as f64);
            let mut y = CHART_MIN_DBM;
            while y < top {
                bucket.push((x, y));
                y += 1.0;
            }
            bucket.push((x, top)); // the exact top, wherever the 1 dBm steps landed
        }
    }
    // Marks ride the reading they arrived during, so they scroll in lockstep —
    // widened to the bar's columns so they cap the bar they belong to.
    let newest = app.ambient_seq;
    let marks = |points: &VecDeque<(u64, f64)>| -> Vec<(f64, f64)> {
        points
            .iter()
            .filter(|(seq, _)| newest - seq < bars as u64)
            .flat_map(|(seq, v)| {
                let back = (newest - seq) as usize;
                (0..BAR_W).map(move |dx| (-((back * BAR_STRIDE + dx) as f64), *v))
            })
            .collect()
    };
    let rx = marks(&app.rx_marks);
    let tx = marks(&app.tx_marks);
    // Unnamed: the level buckets are one visual series — keep the legend to rx/tx.
    fn trace(data: &[(f64, f64)], color: Color) -> Dataset<'_> {
        Dataset::default()
            .marker(symbols::Marker::HalfBlock)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(color))
            .data(data)
    }
    let datasets = vec![
        trace(&quiet, Color::Blue),
        trace(&busy, Color::Cyan),
        trace(&hot, Color::LightRed),
        Dataset::default()
            .name("rx")
            .marker(symbols::Marker::HalfBlock)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
            .data(&rx),
        Dataset::default()
            .name("tx")
            .marker(symbols::Marker::HalfBlock)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            .data(&tx),
    ];
    // Live readout in the title — the number you'd otherwise squint at the bars for.
    let paused = if app.paused { " (paused)" } else { "" };
    let title = match app.last_ambient {
        Some((dbm, ch)) => format!(" Radio · {dbm} dBm · ch {ch}{paused} "),
        None => format!(" Radio{paused} "),
    };
    let dim = |s: String| Span::styled(s, Style::new().fg(Color::DarkGray));
    // One reading per BAR_STRIDE columns; the time labels come from the MEASURED
    // cadence (`ambient_ema_ms`), so they stay honest for any device stats-period.
    // Clamped to ≤5 chars — the FIRST x label's width sets the chart's left gutter,
    // and a wider gutter would break the exact column mapping `slots` relies on.
    let window_s = (slots - 1) as f64 * app.ambient_ema_ms / (BAR_STRIDE as f64 * 1000.0);
    let ago = |s: f64| -> String {
        if s < 1000.0 {
            format!("-{s:.0}s")
        } else if s < 36_000.0 {
            format!("-{:.0}m", s / 60.0)
        } else {
            format!("-{:.0}h", s / 3600.0)
        }
    };
    let mut chart = Chart::new(datasets)
        .legend_position(Some(LegendPosition::TopLeft))
        .x_axis(
            Axis::default()
                .bounds([-((slots - 1) as f64), 0.0])
                .labels([
                    dim(ago(window_s)),
                    dim(ago(window_s / 2.0)),
                    dim("now".into()),
                ]),
        )
        .y_axis(
            Axis::default()
                .bounds([CHART_MIN_DBM, CHART_MAX_DBM])
                .labels([dim("-110".into()), dim("-70".into()), dim("-30".into())])
                .title("dBm"),
        );
    if !zoomed {
        chart = chart.block(pane_block(title, app.focus == Pane::Radio));
    }
    f.render_widget(chart, area);
}

fn render_shell(f: &mut Frame, app: &App, area: Rect, zoomed: bool) {
    let (title, node_addr) = match app.shell_target() {
        Some(n) => (
            format!(
                " Shell: {} ‹{}/{}› ",
                if n.name.is_empty() {
                    topics::node_hex(n.addr)
                } else {
                    n.name.clone()
                },
                app.shell_node + 1,
                app.nodes.len()
            ),
            Some(n.addr),
        ),
        None => (" Shell (no nodes) ".to_string(), None),
    };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(addr) = node_addr {
        if let Some(d) = app.dialogs.get(&addr) {
            for l in d {
                lines.push(match l {
                    // Same syntax highlighting as the `tower console` shell (shared
                    // `crate::highlight`): the echoed command, then each response line.
                    DialogLine::Sent(s) => {
                        let mut spans = vec!["> ".fg(Color::DarkGray)];
                        spans.extend(crate::highlight::command(s));
                        Line::from(spans)
                    }
                    DialogLine::Text(s) => crate::highlight::response(s),
                    DialogLine::Err(e) => {
                        Line::from(Span::styled(format!("✖ {e}"), Style::new().fg(Color::Red)))
                    }
                    DialogLine::Done(0) => Line::from("✓".fg(Color::Green)),
                    DialogLine::Done(r) => Line::from(Span::styled(
                        format!("✖ result {r}"),
                        Style::new().fg(Color::Red),
                    )),
                });
            }
        }
        // Queue state: every still-pending command as a ⌛ line above the prompt.
        if let Some(n) = app.nodes.iter().find(|n| n.addr == addr) {
            for p in &n.pending {
                lines.push(Line::from(Span::styled(
                    format!("⌛ {} (ref {} — p→Del dequeues)", p.line, p.r#ref),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM),
                )));
            }
        }
        lines.push(Line::from(vec![
            "> ".fg(Color::Cyan),
            Span::raw(app.line.clone()),
            Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
        ]));
    } else {
        lines.push(Line::from(
            "pair a node first — F2 opens the pairing window",
        ));
    }
    // Bottom-anchor: show the tail that fits (no border rows when zoomed).
    let inner_h = area.height.saturating_sub(if zoomed { 0 } else { 2 }) as usize;
    let skip = lines.len().saturating_sub(inner_h);
    let visible: Vec<Line> = lines.into_iter().skip(skip).collect();
    let mut p = Paragraph::new(visible).wrap(Wrap { trim: false });
    if !zoomed {
        p = p.block(pane_block(title, app.focus == Pane::Shell));
    }
    f.render_widget(p, area);
}

/// One feed entry: direction arrow ‖ retained mark ‖ prefix-stripped topic ‖ payload.
fn mqtt_line<'a>(prefix: &str, m: &'a MqttMsg) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::with_capacity(8);
    spans.push(match m.dir {
        MqttDir::Out => Span::styled("▲ ", Style::new().fg(Color::Cyan)),
        MqttDir::In => Span::styled(
            "▼ ",
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
    });
    let topic = m.topic.strip_prefix(prefix).unwrap_or(&m.topic);
    spans.push(Span::styled(
        topic,
        Style::new().add_modifier(Modifier::BOLD),
    ));
    if m.retain {
        spans.push(Span::styled(" ®", Style::new().fg(Color::DarkGray)));
    }
    spans.push(Span::raw(" "));
    spans.extend(json_spans(&m.payload));
    Line::from(spans)
}

/// Syntax-highlight a JSON payload into spans — a tiny byte lexer over the RAW
/// bytes (never re-serialized), so the display shows exactly what crossed the
/// wire, key order included. Total: anything non-JSON still renders, just dim.
/// Byte-index slicing is UTF-8-safe: every boundary sits before an ASCII byte
/// (quote/punct/digit), which never occurs inside a multi-byte sequence.
fn json_spans(payload: &[u8]) -> Vec<Span<'_>> {
    const KEY: Color = Color::Cyan;
    const STR: Color = Color::Green;
    const NUM: Color = Color::Magenta;
    const LIT: Color = Color::Yellow;
    const PUNCT: Color = Color::DarkGray;
    if payload.is_empty() {
        // An empty MQTT publish clears a retained topic (node removed).
        return vec![Span::styled(
            "(cleared)",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )];
    }
    let Ok(raw) = std::str::from_utf8(payload) else {
        return vec![Span::styled(
            format!("({} B binary)", payload.len()),
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )];
    };
    let b = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let i = i.min(b.len());
                // A string followed by ':' is an object key — color it apart.
                let mut j = i;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let color = if j < b.len() && b[j] == b':' {
                    KEY
                } else {
                    STR
                };
                out.push(Span::styled(&raw[start..i], Style::new().fg(color)));
            }
            b'0'..=b'9' | b'-' => {
                while i < b.len() && matches!(b[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    i += 1;
                }
                out.push(Span::styled(&raw[start..i], Style::new().fg(NUM)));
            }
            b'a'..=b'z' | b'A'..=b'Z' => {
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let word = &raw[start..i];
                let style = if matches!(word, "true" | "false" | "null") {
                    Style::new().fg(LIT)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                out.push(Span::styled(word, style));
            }
            // Structural run: braces/brackets/colons/commas + whitespace, one dim span.
            b'{' | b'}' | b'[' | b']' | b':' | b',' | b' ' | b'\t' | b'\r' | b'\n' => {
                while i < b.len()
                    && matches!(
                        b[i],
                        b'{' | b'}' | b'[' | b']' | b':' | b',' | b' ' | b'\t' | b'\r' | b'\n'
                    )
                {
                    i += 1;
                }
                out.push(Span::styled(&raw[start..i], Style::new().fg(PUNCT)));
            }
            _ => {
                // Anything else (multi-byte text outside strings — not valid JSON,
                // but render it): advance to the next ASCII-interesting byte.
                while i < b.len() && !b[i].is_ascii() {
                    i += 1;
                }
                i = i.max(start + 1); // guarantee progress on stray ASCII
                out.push(Span::raw(&raw[start..i]));
            }
        }
    }
    out
}

fn render_mqtt(f: &mut Frame, app: &App, area: Rect, zoomed: bool) {
    let inner_h = area.height.saturating_sub(if zoomed { 0 } else { 2 }) as usize;
    // The filter is applied HERE and only here — the deque retains everything.
    let shown: Vec<&MqttMsg> = app
        .mqtt
        .iter()
        .filter(|m| {
            app.mqtt_filter
                .is_none_or(|n| topics::node_of(&app.prefix, &m.topic) == Some(n))
        })
        .collect();
    let end = shown.len().saturating_sub(app.mqtt_scroll);
    let start = end.saturating_sub(inner_h);
    let lines: Vec<Line> = shown[start..end]
        .iter()
        .map(|m| mqtt_line(&app.prefix, m))
        .collect();
    let title = match app.mqtt_filter {
        Some(n) => format!(
            " MQTT ▶ {} · {}/{} msgs{} — F7 = all ",
            topics::node_hex(n),
            shown.len(),
            app.mqtt.len(),
            if app.paused { " (paused)" } else { "" },
        ),
        None => format!(
            " MQTT · {} msgs{} — F7 = filter node ",
            app.mqtt.len(),
            if app.paused { " (paused)" } else { "" },
        ),
    };
    let mut p = Paragraph::new(lines).wrap(Wrap { trim: false });
    if !zoomed {
        p = p.block(pane_block(title, app.focus == Pane::Mqtt));
    }
    f.render_widget(p, area);
}

fn render_log(f: &mut Frame, app: &App, area: Rect, zoomed: bool) {
    let inner_h = area.height.saturating_sub(if zoomed { 0 } else { 2 }) as usize;
    let end = app.log.len().saturating_sub(app.log_scroll);
    let start = end.saturating_sub(inner_h);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| {
            let style = if l.contains("ERROR") {
                Style::new().fg(Color::Red)
            } else if l.contains("WARN") {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .collect();
    let title = if app.paused {
        " Gateway Log (paused) "
    } else {
        " Gateway Log "
    };
    let mut p = Paragraph::new(lines).wrap(Wrap { trim: false });
    if !zoomed {
        p = p.block(pane_block(title.into(), app.focus == Pane::Log));
    }
    f.render_widget(p, area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn render_modal(f: &mut Frame, app: &App, body: Rect) {
    let Some(modal) = &app.modal else { return };
    let (title, lines): (String, Vec<Line>) = match modal {
        Modal::ConfirmRemove(node) => (
            " Remove node ".into(),
            vec![
                Line::from(format!("Unpair {}?", topics::node_hex(*node))),
                Line::from("Its key, name and queued commands are forgotten."),
                Line::from(""),
                Line::from("Enter/y = remove   any other key = cancel"),
            ],
        ),
        Modal::Rename { node, buf } => {
            // A classic input field: a shaded fixed-width strip with a block
            // cursor — the earlier bare quoted string read as a label, and the
            // missing cursor made it non-obvious this is an editor.
            let field = Style::new().bg(Color::DarkGray).fg(Color::White);
            let pad = 17usize.saturating_sub(buf.chars().count());
            (
                " Rename node ".into(),
                vec![
                    Line::from(format!("New name for {}:", topics::node_hex(*node))),
                    Line::from(""),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(" ", field),
                        Span::styled(buf.clone(), field.add_modifier(Modifier::BOLD)),
                        // The cursor: reversed cell inside the field strip.
                        Span::styled(" ", field.add_modifier(Modifier::REVERSED)),
                        Span::styled(" ".repeat(pad), field),
                    ]),
                    Line::from(""),
                    Line::from("≤16 bytes — Enter = save, Esc = cancel"),
                ],
            )
        }
        Modal::ConfirmReveal(node) => (
            " Reveal AES key ".into(),
            vec![
                Line::from(format!(
                    "Show {}'s network key on screen?",
                    topics::node_hex(*node)
                )),
                Line::from("Anyone reading this terminal can then impersonate the node."),
                Line::from(""),
                Line::from("Enter/y = reveal   any other key = cancel"),
            ],
        ),
        Modal::Key { node, key, shown } => (
            // Live countdown to the auto-hide (the 33 ms redraw keeps it ticking).
            format!(
                " AES key (auto-hides in {} s) ",
                30u64.saturating_sub(shown.elapsed().as_secs())
            ),
            vec![
                Line::from(format!("node {}", topics::node_hex(*node))),
                Line::from(Span::styled(
                    key.clone(),
                    Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("any key hides it"),
            ],
        ),
        Modal::PairMenu { selected } => {
            let items: [(&str, &str); 3] = [
                (
                    "Start over-the-air pairing",
                    "60 s window — the node joins by holding its button",
                ),
                (
                    "Catch the cable-connected device",
                    "plug the node in over USB; it pairs automatically",
                ),
                (
                    "Pair with the existing device",
                    "pick the node's serial port from the list",
                ),
            ];
            let mut lines = Vec::new();
            for (i, (label, hint)) in items.iter().enumerate() {
                let (marker, style) = if i == *selected {
                    (
                        "▸ ",
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("  ", Style::new())
                };
                lines.push(Line::from(vec![
                    Span::raw(marker),
                    Span::styled(*label, style),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("    {hint}"),
                    Style::new().fg(Color::DarkGray),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from("↑/↓ select — Enter = run, Esc = close"));
            (" Pair a node ".into(), lines)
        }
        Modal::PairOta { seen_open, .. } => {
            let mut lines = Vec::new();
            match &app.pairing {
                Some(p) if p.state == "open" => {
                    lines.push(Line::from(vec![
                        Span::raw("window open — "),
                        Span::styled(
                            format!("{} s", p.remaining.unwrap_or(0)),
                            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" left"),
                    ]));
                    lines.push(Line::from(""));
                    lines.push(Line::from("hold the node's button ≥ 1 s to join;"));
                    lines.push(Line::from("the window closes itself on the first join"));
                }
                _ if !seen_open => {
                    lines.push(Line::from("opening the pairing window…"));
                }
                _ => lines.push(Line::from("window resolved")),
            }
            lines.push(Line::from(""));
            lines.push(Line::from("Esc = cancel now"));
            (" OTA pairing ".into(), lines)
        }
        Modal::PairCatch { deadline, .. } => {
            let left = deadline.saturating_duration_since(Instant::now()).as_secs();
            (
                " Catch cable device ".into(),
                vec![
                    Line::from("connect the node over USB now —"),
                    Line::from("the first new device pairs automatically"),
                    Line::from(""),
                    Line::from(vec![
                        Span::raw("watching for new ports… "),
                        Span::styled(
                            format!("{left} s"),
                            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(""),
                    Line::from("Esc = cancel"),
                ],
            )
        }
        Modal::PairPorts { ports, selected } => {
            let mut lines = Vec::new();
            if ports.is_empty() {
                lines.push(Line::from("(no serial ports visible — plug the node in)"));
            }
            for (i, p) in ports.iter().enumerate() {
                let (marker, style) = if i == *selected {
                    (
                        "▸ ",
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("  ", Style::new())
                };
                lines.push(Line::from(vec![
                    Span::raw(marker),
                    Span::styled(p.clone(), style),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from("↑/↓ select — Enter = pair, Esc = close"));
            (" Pair via serial port ".into(), lines)
        }
        Modal::PairRun {
            port,
            lines: steps,
            failed,
        } => {
            let mut lines = vec![Line::from(format!("pairing over {port}…")), Line::from("")];
            for s in steps {
                lines.push(Line::from(format!("  {s}")));
            }
            match failed {
                Some(e) => {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        format!("✖ {e}"),
                        Style::new().fg(Color::Red),
                    )));
                    lines.push(Line::from("any key closes"));
                }
                None => {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "…",
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }
            (" Cable pairing ".into(), lines)
        }
        Modal::Info { title, text } => (
            title.clone(),
            vec![
                Line::from(text.clone()),
                Line::from(""),
                Line::from("any key closes"),
            ],
        ),
        Modal::Pending { node, selected } => {
            let mut lines = Vec::new();
            let pend = app
                .nodes
                .iter()
                .find(|n| n.addr == *node)
                .map(|n| n.pending.as_slice())
                .unwrap_or(&[]);
            if pend.is_empty() {
                lines.push(Line::from("nothing queued"));
            }
            for (i, p) in pend.iter().enumerate() {
                let style = if i == *selected {
                    Style::new()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                lines.push(Line::from(Span::styled(
                    format!("#{} {}", p.r#ref, p.line),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from("↑/↓ select · Del = dequeue · Esc = close"));
            (format!(" Queued for {} ", topics::node_hex(*node)), lines)
        }
    };
    let h = (lines.len() as u16 + 2).min(body.height);
    let area = centered(body, 64, h);
    f.render_widget(Clear, area);
    let p = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().fg(Color::Yellow)),
    );
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn test_app() -> (App, std::sync::mpsc::Sender<Event>, Receiver<Input>) {
        let (etx, erx) = std::sync::mpsc::channel();
        let (itx, irx) = std::sync::mpsc::channel();
        let app = App {
            events: erx,
            input: itx,
            prefix: "tower/".into(),
            port: "/dev/ttyTEST".into(),
            quit: false,
            focus: Pane::Nodes,
            zoom: false,
            mouse: true,
            paused: false,
            serial_up: true,
            mqtt_up: true,
            nodes: Vec::new(),
            selected: 0,
            shell_node: 0,
            dialogs: HashMap::new(),
            shell_partial: HashMap::new(),
            line: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_idx: None,
            log: VecDeque::new(),
            log_scroll: 0,
            mqtt: VecDeque::new(),
            mqtt_scroll: 0,
            mqtt_filter: None,
            ambient: VecDeque::new(),
            ambient_seq: 0,
            ambient_ema_ms: 500.0,
            ambient_prev: None,
            rx_marks: VecDeque::new(),
            tx_marks: VecDeque::new(),
            last_ambient: None,
            pairing: None,
            modal: None,
            next_req: 0,
            gw: GwParams {
                addr: 1,
                band: 0,
                channel: 0,
            },
            cable: None,
            last_port_poll: Instant::now(),
            zones: PaneZones::default(),
            chips: Vec::new(),
        };
        (app, etx, irx)
    }

    fn render(app: &mut App) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn node(id: u32, name: &str, pending: usize) -> NodeView {
        NodeView {
            addr: id,
            name: name.into(),
            kind: "push-button".into(),
            sleeping: true,
            unnamed: false,
            last_seen: Some(3),
            rssi: Some(-67),
            uplinks: 12,
            queued: pending as u8,
            pending: (0..pending)
                .map(|i| super::super::payload::PendingEntry {
                    r#ref: i as u16 + 1,
                    id: format!("u-{i}"),
                    line: "/led on".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn renders_empty_state_with_pairing_hint() {
        let (mut app, _etx, _irx) = test_app();
        let grid = render(&mut app);
        assert!(grid.contains("Nodes (0)"));
        assert!(grid.contains("pair a node first"));
        assert!(grid.contains("Gateway Log"));
        assert!(grid.contains("F2 Pair"));
    }

    #[test]
    fn renders_node_row_and_pending_marker() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Registry(vec![node(0xAB12, "kitchen", 1)]));
        let grid = render(&mut app);
        assert!(grid.contains("0x0000ab12"));
        assert!(grid.contains("kitchen"));
        assert!(grid.contains("push-button"));
        assert!(grid.contains("⌛"), "pending marker in table + dialog");
        assert!(grid.contains("Shell: kitchen"));
    }

    #[test]
    fn shell_submit_sends_frontend_cmd_and_echoes() {
        let (mut app, _etx, irx) = test_app();
        app.on_event(Event::Registry(vec![node(0xAB12, "kitchen", 0)]));
        app.focus = Pane::Shell;
        for c in "/led on".chars() {
            handle_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        match irx.try_recv() {
            Ok(Input::Frontend(FrontendCmd::Shell { node, req })) => {
                assert_eq!(node, 0xAB12);
                assert_eq!(req.line, "/led on");
                assert!(req.id.starts_with("tui-"));
            }
            other => panic!("expected a shell FrontendCmd, got {other:?}"),
        }
        let grid = render(&mut app);
        assert!(grid.contains("/led on"), "echoed into the dialog");
    }

    #[test]
    fn key_reveal_flow_needs_confirm_and_autohides() {
        let (mut app, _etx, irx) = test_app();
        app.on_event(Event::Registry(vec![node(0xAB12, "kitchen", 0)]));
        // F4 opens the confirm modal — nothing sent yet.
        handle_key(&mut app, KeyCode::F(4), KeyModifiers::NONE);
        assert!(matches!(app.modal, Some(Modal::ConfirmReveal(0xAB12))));
        assert!(irx.try_recv().is_err(), "no RPC before confirmation");
        // Enter confirms → the reveal RPC goes out.
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            irx.try_recv(),
            Ok(Input::Frontend(FrontendCmd::Rpc(r))) if r.op == "reveal_key"
        ));
        // The key arrives → modal shows it (and the grid contains it).
        app.on_event(Event::Rpc(super::super::payload::RpcResponse {
            id: "tui-1".into(),
            ok: true,
            error: None,
            data: serde_json::json!({ "addr": "0x0000ab12", "key": "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf" }),
        }));
        let grid = render(&mut app);
        assert!(grid.contains("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"));
        assert!(grid.contains("auto-hides"));
    }

    fn mqtt(topic: &str, payload: &str) -> Event {
        Event::Mqtt(MqttMsg {
            dir: MqttDir::Out,
            topic: topic.into(),
            payload: payload.as_bytes().to_vec(),
            retain: false,
        })
    }

    #[test]
    fn mqtt_feed_renders_json_payloads() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(mqtt(
            "tower/nodes/0x0000ab12/event/button",
            r#"{"event":"click","count":42,"ts":1700000000}"#,
        ));
        let grid = render(&mut app);
        assert!(grid.contains("▲"), "direction arrow");
        assert!(grid.contains("nodes/0x0000ab12"), "prefix-stripped topic");
        assert!(grid.contains("\"click\""), "JSON string value");
        assert!(grid.contains("42"), "JSON number");
    }

    #[test]
    fn mqtt_filter_is_visual_only_and_toggles() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Registry(vec![
            node(0xAB12, "kitchen", 0),
            node(0xBB34, "garage", 0),
        ]));
        app.on_event(mqtt("tower/nodes/0x0000ab12", r#"{"name":"kitchen"}"#));
        app.on_event(mqtt("tower/nodes/0x0000bb34", r#"{"name":"garage"}"#));
        app.on_event(mqtt("tower/gateway/stats", r#"{"nodes":2}"#));
        // Unfiltered: everything visible.
        let grid = render(&mut app);
        assert!(grid.contains("nodes/0x0000ab12"));
        assert!(grid.contains("nodes/0x0000bb34"));
        assert!(grid.contains("gateway/stats"));
        // `f` on the Nodes pane (selection = kitchen) filters the view…
        handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
        assert_eq!(app.mqtt_filter, Some(0xAB12));
        let grid = render(&mut app);
        assert!(grid.contains("nodes/0x0000ab12"));
        assert!(!grid.contains("nodes/0x0000bb34"), "other node hidden");
        assert!(!grid.contains("gateway/stats"), "gateway topics hidden");
        assert!(
            grid.contains("MQTT ▶ 0x0000ab12"),
            "filter shown in the title"
        );
        // …but the deque still holds every message (filtering loses nothing)…
        assert_eq!(app.mqtt.len(), 3);
        // …and `f` again flips straight back to all.
        handle_key(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
        assert_eq!(app.mqtt_filter, None);
        let grid = render(&mut app);
        assert!(grid.contains("nodes/0x0000bb34"));
        // F7 is the global binding — it toggles from any pane, not just MQTT/Nodes.
        app.focus = Pane::Log;
        handle_key(&mut app, KeyCode::F(7), KeyModifiers::NONE);
        assert_eq!(app.mqtt_filter, Some(0xAB12));
        handle_key(&mut app, KeyCode::F(7), KeyModifiers::NONE);
        assert_eq!(app.mqtt_filter, None);
    }

    #[test]
    fn radio_bars_fill_bottom_up() {
        let (mut app, _etx, _irx) = test_app();
        for _ in 0..4 {
            app.on_event(Event::Radio(RadioSample::Ambient {
                dbm: -60,
                channel: 5,
            }));
        }
        let grid = render(&mut app);
        // A -60 dBm reading spans most of the -110..-30 range: the filled column
        // pairs must produce full-block cells (dots never did).
        assert!(grid.contains('█'), "bars are filled from the floor up");
        assert!(grid.contains("-60 dBm"), "live readout in the title");
    }

    #[test]
    fn radio_bars_are_evenly_spaced() {
        let (mut app, _etx, _irx) = test_app();
        for _ in 0..40 {
            app.on_event(Event::Radio(RadioSample::Ambient {
                dbm: -60,
                channel: 5,
            }));
        }
        let grid = render(&mut app);
        // Uniform readings ⇒ every bar is a full "██" column pair. The 2:1 rhythm must
        // be exact across the whole plot: a lone "█", a "███" run, or a double gap
        // means the x→column mapping aliased (slots ≠ the chart's true graph width).
        let row = grid
            .lines()
            .find(|l| l.contains('█'))
            .expect("a bar row renders");
        let start = row.find('█').unwrap();
        let end = row.rfind('█').unwrap() + '█'.len_utf8();
        let bars: Vec<&str> = row[start..end].split(' ').collect();
        assert!(bars.len() > 5, "several bars fit the test pane: {row:?}");
        assert!(
            bars.iter().all(|s| *s == "██"),
            "bars 2 wide, gaps 1 wide, everywhere: {:?}",
            &row[start..end]
        );
    }

    #[test]
    fn pause_freezes_the_radio_chart() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Radio(RadioSample::Ambient {
            dbm: -60,
            channel: 5,
        }));
        assert_eq!(app.ambient.len(), 1);
        // F5: samples and marks stop ingesting (the deques ARE the chart), title readout freezes.
        handle_key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        app.on_event(Event::Radio(RadioSample::Ambient {
            dbm: -50,
            channel: 5,
        }));
        app.on_event(Event::Radio(RadioSample::Rx { src: 1, rssi: -60 }));
        assert_eq!(app.ambient.len(), 1, "paused: no ambient ingest");
        assert!(app.rx_marks.is_empty(), "paused: no mark ingest");
        assert_eq!(app.last_ambient, Some((-60, 5)), "paused: readout frozen");
        assert!(render(&mut app).contains("Radio · -60 dBm · ch 5 (paused)"));
        // Unpause: ingest resumes.
        handle_key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        app.on_event(Event::Radio(RadioSample::Ambient {
            dbm: -50,
            channel: 5,
        }));
        assert_eq!(app.ambient.len(), 2);
    }

    #[test]
    fn node_action_keys_are_global_except_shell_editing() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Registry(vec![node(0xAB12, "kitchen", 0)]));
        // Delete works from any pane — it acts on the selected node…
        app.focus = Pane::Log;
        handle_key(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        assert!(matches!(app.modal, Some(Modal::ConfirmRemove(0xAB12))));
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE); // cancel
        // …and so does Backspace (the key macOS keyboards label "delete").
        app.focus = Pane::Nodes;
        handle_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(app.modal, Some(Modal::ConfirmRemove(0xAB12))));
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        // On the Shell pane both stay line-editing keys — no modal.
        app.focus = Pane::Shell;
        app.line = "ab".into();
        app.cursor = 2;
        handle_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.line, "a");
        assert!(app.modal.is_none());
        // F6 Rename is global too, matching its footer chip.
        app.focus = Pane::Mqtt;
        handle_key(&mut app, KeyCode::F(6), KeyModifiers::NONE);
        assert!(matches!(
            app.modal,
            Some(Modal::Rename { node: 0xAB12, .. })
        ));
    }

    #[test]
    fn footer_chips_click_as_buttons() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Registry(vec![node(0xAB12, "kitchen", 0)]));
        render(&mut app); // populates app.chips
        let click = |app: &mut App, key: KeyCode, mods: KeyModifiers| {
            let (r, _) = *app.chips.iter().find(|(_, k)| *k == key).unwrap();
            handle_mouse(
                app,
                event::MouseEvent {
                    kind: event::MouseEventKind::Down(event::MouseButton::Left),
                    column: r.x,
                    row: r.y,
                    modifiers: mods,
                },
            );
        };
        // A chip click presses its key: F5 toggles pause.
        click(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        assert!(app.paused);
        // Node-scoped chips act on the selected node from ANY focus: the Del
        // chip opens the remove confirmation even while the log pane is focused.
        app.focus = Pane::Log;
        click(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        assert!(matches!(app.modal, Some(Modal::ConfirmRemove(0xAB12))));
        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE); // cancel
        assert!(app.modal.is_none());
        // Shift-click on the F1 chip = Shift-F1 (counter-clockwise walk).
        click(&mut app, KeyCode::F(1), KeyModifiers::SHIFT);
        assert!(app.focus == Pane::Mqtt, "Log walks back to MQTT");
        // With mouse capture off, chip clicks are inert.
        app.mouse = false;
        click(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        assert!(app.paused, "click ignored while capture is off");
    }

    #[test]
    fn f8_clears_the_focused_feed_only() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Log("hello log".into()));
        app.on_event(mqtt("tower/gateway/stats", r#"{"nodes":0}"#));
        // Focus MQTT → F8 clears the MQTT feed, not the log.
        app.focus = Pane::Mqtt;
        handle_key(&mut app, KeyCode::F(8), KeyModifiers::NONE);
        assert!(app.mqtt.is_empty());
        assert_eq!(app.log.len(), 1);
        // Focus elsewhere → F8 clears the log.
        app.focus = Pane::Log;
        handle_key(&mut app, KeyCode::F(8), KeyModifiers::NONE);
        assert!(app.log.is_empty());
    }

    #[test]
    fn json_lexer_shapes() {
        // Key vs string-value distinction, numbers, literals.
        let spans = json_spans(br#"{"k":"v","n":-1.5e3,"b":true,"z":null}"#);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(texts.contains(&"\"k\""));
        assert!(texts.contains(&"\"v\""));
        assert!(texts.contains(&"-1.5e3"));
        assert!(texts.contains(&"true"));
        assert!(texts.contains(&"null"));
        // The whole payload survives the lexer verbatim.
        assert_eq!(texts.concat(), r#"{"k":"v","n":-1.5e3,"b":true,"z":null}"#);
        // Empty payload = a retained-topic clear.
        let spans = json_spans(b"");
        assert_eq!(spans[0].content.as_ref(), "(cleared)");
        // Non-UTF-8 stays safe.
        let spans = json_spans(&[0xFF, 0xFE]);
        assert!(spans[0].content.contains("binary"));
    }

    #[test]
    fn shell_chunks_reassemble_across_line_breaks() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Registry(vec![node(0xAB12, "hello", 0)]));
        let rsp = |chunk: u16, text: &str, done: bool| super::super::payload::ShellRsp {
            id: "u-1".into(),
            chunk,
            text: text.into(),
            done,
            result: 0,
            truncated: false,
            error: None,
        };
        // "therm-delta = 50" straddles the chunk boundary — the ≤56 B radio chunks
        // split at byte offsets, not line breaks (the broken-lines screenshot bug).
        app.on_event(Event::Shell {
            node: 0xAB12,
            rsp: rsp(0, "therm-period = 60\r\nth", false),
        });
        app.on_event(Event::Shell {
            node: 0xAB12,
            rsp: rsp(1, "erm-delta = 50\r\naccel = medium\r\n", true),
        });
        let grid = render(&mut app);
        assert!(grid.contains("therm-period = 60"));
        assert!(
            grid.contains("therm-delta = 50"),
            "straddling line reassembled"
        );
        assert!(grid.contains("accel = medium"));
    }

    #[test]
    fn f1_cycles_panes_both_ways() {
        let (mut app, _etx, _irx) = test_app();
        assert!(app.focus == Pane::Nodes);
        handle_key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(app.focus == Pane::Radio, "F1 = clockwise (chart first)");
        handle_key(&mut app, KeyCode::F(1), KeyModifiers::SHIFT);
        assert!(app.focus == Pane::Nodes, "Shift-F1 = counter-clockwise");
        handle_key(&mut app, KeyCode::F(13), KeyModifiers::NONE);
        assert!(app.focus == Pane::Shell, "F13 = Shift-F1 fallback");
    }

    #[test]
    fn f3_zooms_focused_pane_borderless() {
        let (mut app, _etx, _irx) = test_app();
        app.on_event(Event::Log("zoomed log line".into()));
        app.focus = Pane::Log;
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE);
        assert!(app.zoom);
        let grid = render(&mut app);
        assert!(grid.contains("zoomed log line"));
        assert!(!grid.contains("Nodes (0)"), "other panes hidden in zoom");
        assert!(!grid.contains("Gateway Log"), "borderless: no pane title");
        handle_key(&mut app, KeyCode::F(3), KeyModifiers::NONE);
        assert!(!app.zoom, "F3 toggles back");
    }
}
