//! The gateway TUI — the interactive frontend of `tower gateway` (the default mode).
//! Same DNA as the `tower console` TUI (`src/tui.rs`): a single synchronous 33 ms
//! poll loop, gray header/footer with chip toggles, Cyan focused border — but a
//! layout built for network coordination:
//!
//! ```text
//!  Nodes table (select/remove/rename/reveal) │ Radio chart (ambient + RX/TX marks)
//!  ─────────────────────────────────────────────┼─────────────────────────────────────
//!  per-node remote-shell dialog (⌛ pending)     │ gateway log feed
//! ```
//!
//! All actions ride the engine's own surfaces (`FrontendCmd` → the same code paths
//! the MQTT clients hit), so the TUI is a *view* over the bridge, never a second
//! implementation. Cable pairing needs a second serial port and stays a CLI flow
//! (`tower nodes add --port …`); the F2 modal runs OTA pairing and lists the ports
//! for the cable hint.

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
    Axis, Block, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table, Wrap,
};

use super::engine::{Event, FrontendCmd, Input, NodeView, RadioSample};
use super::payload::{Pairing, ShellReq};
use super::topics;
use crate::EXIT_OK;

const LOG_CAP: usize = 1000;
const DIALOG_CAP: usize = 500;
/// Radio-graph window, seconds.
const GRAPH_WINDOW_S: f64 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Nodes,
    Shell,
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
    Pairing {
        ports: Vec<String>,
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

struct App {
    events: Receiver<Event>,
    input: Sender<Input>,
    prefix: String,
    port: String,
    quit: bool,
    focus: Pane,
    paused: bool,
    serial_up: bool,
    mqtt_up: bool,
    nodes: Vec<NodeView>,
    selected: usize,
    /// The node whose dialog the shell pane shows (index into `nodes`).
    shell_node: usize,
    dialogs: HashMap<u32, VecDeque<DialogLine>>,
    line: String,
    cursor: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
    log: VecDeque<String>,
    log_scroll: usize,
    started: Instant,
    ambient: VecDeque<(f64, f64)>,
    rx_marks: VecDeque<(f64, f64)>,
    tx_marks: VecDeque<(f64, f64)>,
    pairing: Option<Pairing>,
    modal: Option<Modal>,
    next_req: u64,
}

impl App {
    fn now_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

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
                let sel_id = self.selected_node().map(|n| n.id);
                let dlg_id = self.shell_target().map(|n| n.id);
                self.nodes = nodes;
                if let Some(id) = sel_id
                    && let Some(i) = self.nodes.iter().position(|n| n.id == id)
                {
                    self.selected = i;
                } else {
                    self.selected = self.selected.min(self.nodes.len().saturating_sub(1));
                }
                if let Some(id) = dlg_id
                    && let Some(i) = self.nodes.iter().position(|n| n.id == id)
                {
                    self.shell_node = i;
                } else {
                    self.shell_node = self.shell_node.min(self.nodes.len().saturating_sub(1));
                }
            }
            Event::Shell { node, rsp } => {
                if let Some(err) = rsp.error {
                    self.dialog_push(node, DialogLine::Err(err));
                } else {
                    if !rsp.text.is_empty() {
                        for l in rsp.text.lines() {
                            self.dialog_push(node, DialogLine::Text(l.to_string()));
                        }
                    }
                    if rsp.done {
                        self.dialog_push(node, DialogLine::Done(rsp.result));
                    }
                }
            }
            Event::Radio(sample) => {
                let t = self.now_s();
                let cap = 1024;
                match sample {
                    RadioSample::Ambient { dbm, .. } => {
                        if self.ambient.len() >= cap {
                            self.ambient.pop_front();
                        }
                        self.ambient.push_back((t, dbm as f64));
                    }
                    RadioSample::Rx { rssi_dbm, .. } => {
                        if self.rx_marks.len() >= cap {
                            self.rx_marks.pop_front();
                        }
                        self.rx_marks.push_back((t, rssi_dbm as f64));
                    }
                    RadioSample::Tx { .. } => {
                        if self.tx_marks.len() >= cap {
                            self.tx_marks.pop_front();
                        }
                        // TX has no receive RSSI — pin the marks to a fixed lane so
                        // they read as activity ticks, not measurements.
                        self.tx_marks.push_back((t, -35.0));
                    }
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
                if !rsp.ok {
                    self.push_log(format!(
                        "action failed: {}",
                        rsp.error.unwrap_or_else(|| "gateway refused".into())
                    ));
                } else if let Some(key) = rsp.data.get("key").and_then(|v| v.as_str()) {
                    // A reveal answered — swap the confirm modal for the key view.
                    let node = rsp
                        .data
                        .get("node")
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
) -> Result<u8> {
    let mut app = App {
        events,
        input,
        prefix,
        port,
        quit: false,
        focus: Pane::Nodes,
        paused: false,
        serial_up: true,
        mqtt_up: false,
        nodes: Vec::new(),
        selected: 0,
        shell_node: 0,
        dialogs: HashMap::new(),
        line: String::new(),
        cursor: 0,
        history: Vec::new(),
        hist_idx: None,
        log: VecDeque::new(),
        log_scroll: 0,
        started: Instant::now(),
        ambient: VecDeque::new(),
        rx_marks: VecDeque::new(),
        tx_marks: VecDeque::new(),
        pairing: None,
        modal: None,
        next_req: 0,
    };
    let mut terminal = ratatui::init();
    let res = run_loop(&mut terminal, &mut app);
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
        terminal.draw(|f| ui(f, app))?;
        if event::poll(std::time::Duration::from_millis(33))?
            && let TermEvent::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            handle_key(app, key.code, key.modifiers);
        }
    }
    Ok(())
}

// ---- key handling -------------------------------------------------------------

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    if app.modal.is_some() {
        return handle_modal_key(app, code);
    }
    match (code, mods) {
        (KeyCode::F(10), _) => app.quit = true,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.quit = true,
        (KeyCode::BackTab, _) => {
            app.focus = match app.focus {
                Pane::Nodes => Pane::Shell,
                Pane::Shell => Pane::Log,
                Pane::Log => Pane::Nodes,
            };
        }
        (KeyCode::F(5), _) => app.paused = !app.paused,
        (KeyCode::F(8), _) => {
            app.log.clear();
            app.log_scroll = 0;
        }
        (KeyCode::F(2), _) => {
            let ports = crate::port::usb_ports().unwrap_or_default();
            app.modal = Some(Modal::Pairing { ports });
        }
        _ => match app.focus {
            Pane::Nodes => handle_nodes_key(app, code),
            Pane::Shell => handle_shell_key(app, code, mods),
            Pane::Log => match code {
                KeyCode::PageUp => app.log_scroll = (app.log_scroll + 10).min(app.log.len()),
                KeyCode::PageDown => app.log_scroll = app.log_scroll.saturating_sub(10),
                KeyCode::Char('q') => app.quit = true,
                _ => {}
            },
        },
    }
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
                app.modal = Some(Modal::ConfirmRemove(n.id));
            }
        }
        KeyCode::F(6) | KeyCode::Char('r') => {
            if let Some(n) = app.selected_node() {
                app.modal = Some(Modal::Rename {
                    node: n.id,
                    buf: n.name.clone(),
                });
            }
        }
        KeyCode::F(4) | KeyCode::Char('k') => {
            if let Some(n) = app.selected_node() {
                app.modal = Some(Modal::ConfirmReveal(n.id));
            }
        }
        KeyCode::Char('p') => {
            if let Some(n) = app.selected_node() {
                app.modal = Some(Modal::Pending {
                    node: n.id,
                    selected: 0,
                });
            }
        }
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
    let Some(target) = app.shell_target().map(|n| n.id) else {
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
        ttl_s: 0,
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
                    serde_json::json!({ "node": topics::node_hex(node) }),
                );
            }
            _ => {}
        },
        Modal::Rename { node, mut buf } => match code {
            KeyCode::Enter => {
                if !buf.is_empty() {
                    app.rpc(
                        "node_rename",
                        serde_json::json!({ "node": topics::node_hex(node), "name": buf }),
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
                    serde_json::json!({ "node": topics::node_hex(node) }),
                );
                // The Key modal opens when the RPC answers (Event::Rpc).
            }
            _ => {}
        },
        Modal::Key { .. } => {} // any key hides it
        Modal::Pairing { ports } => match code {
            KeyCode::Char('o') | KeyCode::Enter => {
                app.rpc("node_add_ota", serde_json::json!({ "window_s": 60 }));
            }
            KeyCode::Esc => {}
            _ => app.modal = Some(Modal::Pairing { ports }),
        },
        Modal::Pending { node, selected } => {
            let count = app
                .nodes
                .iter()
                .find(|n| n.id == node)
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
                        .find(|n| n.id == node)
                        .and_then(|n| n.pending.get(selected))
                    {
                        let r = entry.r#ref;
                        app.rpc(
                            "queue_drop",
                            serde_json::json!({ "node": topics::node_hex(node), "ref": r }),
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
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
    let [nodes_area, chart_area] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(top);
    let [shell_area, log_area] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(bottom);
    render_nodes(f, app, nodes_area);
    render_chart(f, app, chart_area);
    render_shell(f, app, shell_area);
    render_log(f, app, log_area);
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
            format!("  PAIRING {}s ", p.remaining_s.unwrap_or(0)),
            Style::new()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line).style(bar), area);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let bar = Style::new().bg(Color::Gray).fg(Color::Black);
    let chip = |label: &str, active: bool| -> Span<'static> {
        if active {
            Span::styled(
                format!(" {label} "),
                Style::new()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), bar)
        }
    };
    let line = Line::from(vec![
        chip("S-Tab Focus", false),
        chip(
            "F2 Pair",
            app.pairing.as_ref().is_some_and(|p| p.state == "open"),
        ),
        chip("F4 Key", false),
        chip("F5 Pause", app.paused),
        chip("F6 Rename", false),
        chip("p Pending", false),
        chip("F8 Clear", false),
        chip("F10 Quit", false),
    ]);
    f.render_widget(Paragraph::new(line).style(bar), area);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let mut b = Block::bordered().title(title);
    if focused {
        b = b.border_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    }
    b
}

fn render_nodes(f: &mut Frame, app: &App, area: Rect) {
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
                topics::node_hex(n.id),
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
                n.last_seen_s
                    .map(|s| format!("{s}s"))
                    .unwrap_or_else(|| "never".into()),
                n.rssi_dbm
                    .map(|r| format!("{r}"))
                    .unwrap_or_else(|| "—".into()),
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
    .header(header)
    .block(pane_block(
        format!(" Nodes ({}) ", app.nodes.len()),
        app.focus == Pane::Nodes,
    ));
    f.render_widget(table, area);
}

fn render_chart(f: &mut Frame, app: &App, area: Rect) {
    let now = app.now_s();
    let x_min = now - GRAPH_WINDOW_S;
    let shift = |points: &VecDeque<(f64, f64)>| -> Vec<(f64, f64)> {
        points
            .iter()
            .filter(|(t, _)| *t >= x_min)
            .map(|(t, v)| (t - now, *v))
            .collect()
    };
    let ambient = shift(&app.ambient);
    let rx = shift(&app.rx_marks);
    let tx = shift(&app.tx_marks);
    let datasets = vec![
        Dataset::default()
            .name("rssi")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(Color::DarkGray))
            .data(&ambient),
        Dataset::default()
            .name("rx")
            .marker(symbols::Marker::Dot)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(Color::Green))
            .data(&rx),
        Dataset::default()
            .name("tx")
            .marker(symbols::Marker::Dot)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(Color::Yellow))
            .data(&tx),
    ];
    let chart = Chart::new(datasets)
        .block(pane_block(" Radio ".into(), false))
        .x_axis(
            Axis::default()
                .bounds([-GRAPH_WINDOW_S, 0.0])
                .labels(["-60s", "-30s", "now"]),
        )
        .y_axis(
            Axis::default()
                .bounds([-110.0, -30.0])
                .labels(["-110", "-70", "-30"])
                .title("dBm"),
        );
    f.render_widget(chart, area);
}

fn render_shell(f: &mut Frame, app: &App, area: Rect) {
    let (title, node_id) = match app.shell_target() {
        Some(n) => (
            format!(
                " Shell: {} ‹{}/{}› ",
                if n.name.is_empty() {
                    topics::node_hex(n.id)
                } else {
                    n.name.clone()
                },
                app.shell_node + 1,
                app.nodes.len()
            ),
            Some(n.id),
        ),
        None => (" Shell (no nodes) ".to_string(), None),
    };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(id) = node_id {
        if let Some(d) = app.dialogs.get(&id) {
            for l in d {
                lines.push(match l {
                    DialogLine::Sent(s) => Line::from(vec![
                        "> ".fg(Color::DarkGray),
                        Span::styled(s.clone(), Style::new().fg(Color::Yellow)),
                    ]),
                    DialogLine::Text(s) => Line::from(s.clone()),
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
        if let Some(n) = app.nodes.iter().find(|n| n.id == id) {
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
    // Bottom-anchor: show the tail that fits.
    let inner_h = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(inner_h);
    let visible: Vec<Line> = lines.into_iter().skip(skip).collect();
    let p = Paragraph::new(visible)
        .wrap(Wrap { trim: false })
        .block(pane_block(title, app.focus == Pane::Shell));
    f.render_widget(p, area);
}

fn render_log(f: &mut Frame, app: &App, area: Rect) {
    let inner_h = area.height.saturating_sub(2) as usize;
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
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(pane_block(title.into(), app.focus == Pane::Log));
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
        Modal::Rename { node, buf } => (
            " Rename node ".into(),
            vec![
                Line::from(format!("{} → \"{buf}\"", topics::node_hex(*node))),
                Line::from(""),
                Line::from("type the name (≤16 bytes) — Enter = save, Esc = cancel"),
            ],
        ),
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
        Modal::Key { node, key, .. } => (
            " AES key (auto-hides in 30 s) ".into(),
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
        Modal::Pairing { ports } => {
            let mut lines = vec![
                Line::from("o/Enter = open a 60 s OTA window (hold the node's button)"),
                Line::from(""),
                Line::from("Cable pairing runs where the node is plugged in:"),
            ];
            if ports.is_empty() {
                lines.push(Line::from("  (no serial ports visible)"));
            }
            for p in ports {
                lines.push(Line::from(format!("  tower nodes add --port {p}")));
            }
            lines.push(Line::from(""));
            lines.push(Line::from("Esc = close"));
            (" Pair a node ".into(), lines)
        }
        Modal::Pending { node, selected } => {
            let mut lines = Vec::new();
            let pend = app
                .nodes
                .iter()
                .find(|n| n.id == *node)
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
            paused: false,
            serial_up: true,
            mqtt_up: true,
            nodes: Vec::new(),
            selected: 0,
            shell_node: 0,
            dialogs: HashMap::new(),
            line: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_idx: None,
            log: VecDeque::new(),
            log_scroll: 0,
            started: Instant::now(),
            ambient: VecDeque::new(),
            rx_marks: VecDeque::new(),
            tx_marks: VecDeque::new(),
            pairing: None,
            modal: None,
            next_req: 0,
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
            id,
            name: name.into(),
            kind: "push-button".into(),
            sleeping: true,
            unnamed: false,
            last_seen_s: Some(3),
            rssi_dbm: Some(-67),
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
            data: serde_json::json!({ "node": "0x0000ab12", "key": "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf" }),
        }));
        let grid = render(&mut app);
        assert!(grid.contains("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"));
        assert!(grid.contains("auto-hides"));
    }
}
