//! The gateway engine — a **pure, synchronous state machine**: `handle(Input) ->
//! Vec<Output>`. No I/O, no threads, no clocks beyond the injected 1 Hz `Tick`; the
//! surrounding threads (serial reader/writer, rumqttc connection loop, frontend) feed
//! `Input`s in and execute `Output`s. That seam is what makes the whole
//! serial↔MQTT bridge unit-testable without hardware or a broker — the same trick as
//! `session::Transport`, one level up.
//!
//! Responsibilities: mirror the dongle's registry into retained MQTT topics, translate
//! uplinks into telemetry topics (the gateway firmware forwards payloads verbatim —
//! *this* is where `tower_protocol::radio` is decoded), run the remote-shell
//! queue/response correlation (MQTT uuid ↔ radio `cmd_id` ↔ queue item), serve the
//! `gateway/cmd` RPC surface, stream radio diagnostics, and keep `gateway/status`
//! truthful across serial/MQTT link changes and gateway reboots (session_id).

use std::time::{SystemTime, UNIX_EPOCH};

use tower_protocol::mgmt::{self, MgmtOp, NodeEntry, NodeKey, Paired, QueueEntry, QueueId};
use tower_protocol::msg::RadioStat;
use tower_protocol::radio::{self, NodeCmd, NodeMsg};

use crate::mgmt::{DeviceInfoOwned, NodeEntryOwned, encode_mgmt, parse_records};

use super::payload::{self, PendingEntry, RpcRequest, RpcResponse};
use super::topics;

/// Idle ticks (1 Hz) before an in-flight management op is failed as a device timeout.
const MGMT_TIMEOUT_TICKS: u64 = 8;
/// Idle ticks before a delivered-but-unanswered remote shell command is failed (the
/// node executed it on its wake cycle; a healthy reply arrives within its RX window).
const SHELL_REPLY_TIMEOUT_TICKS: u64 = 30;
/// Ticks between retained `gateway/stats` refreshes.
const STATS_EVERY_TICKS: u64 = 10;

/// One decoded target→host frame, owned (the serial thread copies out of its buffer).
#[derive(Debug, Clone)]
pub(crate) enum SerialMsg {
    Hello {
        firmware_name: String,
        firmware_version: String,
        session_id: u32,
    },
    Log {
        line: String,
    },
    Uplink {
        src: u32,
        counter: u32,
        rssi_dbm: i16,
        lqi: u8,
        data: Vec<u8>,
    },
    Mgmt {
        req_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        data: Vec<u8>,
    },
    Stat(RadioStat),
    Dropped(u32),
}

/// Everything that can happen to the engine.
#[derive(Debug)]
pub(crate) enum Input {
    SerialUp,
    SerialDown {
        error: String,
    },
    SerialFrame(SerialMsg),
    MqttUp,
    MqttDown {
        error: String,
    },
    MqttIn {
        topic: String,
        payload: Vec<u8>,
    },
    /// An in-process frontend action (the TUI drives the same surfaces the MQTT
    /// clients do; RPC ids prefixed "tui-" route their responses back as events).
    Frontend(FrontendCmd),
    /// 1 Hz housekeeping (timeouts, stats refresh, pairing countdown).
    Tick,
}

/// What the TUI can ask for.
#[derive(Debug)]
pub(crate) enum FrontendCmd {
    Rpc(RpcRequest),
    Shell { node: u32, req: payload::ShellReq },
}

/// Everything the engine wants done. Executed by the runner thread.
#[derive(Debug, PartialEq)]
pub(crate) enum Output {
    /// Write one pre-encoded frame to the dongle.
    SerialSend(Vec<u8>),
    Publish {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
    Subscribe(String),
    /// A frontend event (service log line / TUI view-model update).
    Event(Event),
}

/// Frontend feed — the service prints these; the TUI folds them into its view-model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    /// A human-readable line (device logs, engine milestones, warnings).
    Log(String),
    /// Link state changed.
    Link { serial_up: bool, mqtt_up: bool },
    /// The registry mirror changed (full snapshot — ≤32 nodes, cheap).
    Registry(Vec<NodeView>),
    /// A remote-shell response chunk (or terminal error) for a node's dialog.
    Shell { node: u32, rsp: payload::ShellRsp },
    /// One radio-graph sample.
    Radio(RadioSample),
    /// Pairing window state changed.
    Pairing(payload::Pairing),
    /// The response to a frontend-initiated RPC (id prefixed "tui-").
    Rpc(RpcResponse),
}

/// The frontend's view of one node (mirror row + live pendings).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeView {
    pub id: u32,
    pub name: String,
    pub kind: String,
    pub sleeping: bool,
    pub unnamed: bool,
    pub last_seen_s: Option<u32>,
    pub rssi_dbm: Option<i8>,
    pub uplinks: u32,
    pub queued: u8,
    pub pending: Vec<PendingEntry>,
}

/// One radio-graph sample (TUI chart food).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RadioSample {
    Ambient { dbm: i16, channel: u8 },
    Rx { src: u32, rssi_dbm: i16 },
    Tx { dest: u32, delivered: bool },
}

// ---- internal state ---------------------------------------------------------------

/// What an in-flight `MgmtRequest` was for (drives reply decoding + follow-ups).
#[derive(Debug, Clone)]
enum OpKind {
    /// `resync`: re-describe after a serial reconnect / gateway reboot.
    Describe,
    NodeList,
    NodeAdd {
        id: u32,
        name: String,
    },
    NodeRemove {
        id: u32,
    },
    NodeUpdate,
    RevealKey {
        id: u32,
    },
    PairingOpen,
    PairingCancel,
    QueuePush {
        node: u32,
        shell: Option<ShellOrigin>,
    },
    QueueList,
    QueueDrop {
        node: u32,
    },
    StatsConfig,
}

/// Where a remote-shell command came from (for response routing).
#[derive(Debug, Clone)]
struct ShellOrigin {
    rpc: String,
    line: String,
    cmd_id: u16,
}

/// One in-flight management request.
#[derive(Debug)]
struct PendingMgmt {
    req_id: u16,
    op: OpKind,
    /// RPC correlation id to answer on `gateway/rsp/{id}` (None = engine-internal).
    rpc: Option<String>,
    data: Vec<u8>,
    next_chunk: u16,
    truncated: bool,
    /// Tick deadline (delayed ops — pairing — get their window + slack).
    deadline: u64,
}

/// The registry mirror row (device truth + host-side enrichment).
#[derive(Debug, Clone)]
struct NodeMirror {
    entry: NodeEntryOwned,
    /// From the node's `NodeInfo` heartbeat: firmware name mapped to a kind.
    kind: String,
    /// Last `NodeInfo.session_id` (count-reset disambiguation for subscribers).
    session_id: Option<u32>,
    /// Live remote-shell queue entries (ref = gateway queue item id).
    pending: Vec<PendingShell>,
}

#[derive(Debug, Clone)]
struct PendingShell {
    item: u16,
    rpc: String,
    line: String,
    cmd_id: u16,
    /// Set once the gateway reports TX_DELIVERED (awaiting the node's reply chunks).
    delivered_tick: Option<u64>,
    next_chunk: u16,
    truncated: bool,
}

pub(crate) struct Engine {
    prefix: String,
    port_name: String,
    describe: DeviceInfoOwned,
    hello_session: Option<u32>,
    firmware_version: String,
    serial_up: bool,
    mqtt_up: bool,
    seq: u16,
    next_req: u16,
    next_cmd: u16,
    pending_mgmt: Vec<PendingMgmt>,
    nodes: Vec<NodeMirror>,
    pairing_rpc: Option<String>,
    pairing_until: Option<u64>,
    tick: u64,
    uplinks: u64,
    last_ambient: Option<(i16, u8)>,
}

/// Parse a 32-hex-digit AES key.
fn parse_key_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, k) in key.iter_mut().enumerate() {
        *k = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn json<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// Firmware name → device kind: `radio_push_button` → `push-button`.
fn kind_of(firmware_name: &str) -> String {
    firmware_name
        .strip_prefix("radio_")
        .unwrap_or(firmware_name)
        .replace('_', "-")
}

impl Engine {
    pub(crate) fn new(prefix: String, port_name: String, describe: DeviceInfoOwned) -> Self {
        Self {
            prefix,
            port_name,
            describe,
            hello_session: None,
            firmware_version: String::new(),
            serial_up: true,
            mqtt_up: false,
            seq: 0,
            next_req: 1,
            next_cmd: 1,
            pending_mgmt: Vec::new(),
            nodes: Vec::new(),
            pairing_rpc: None,
            pairing_until: None,
            tick: 0,
            uplinks: 0,
            last_ambient: None,
        }
    }

    /// The initial output batch: subscriptions + a registry/queue sync.
    pub(crate) fn start(&mut self) -> Vec<Output> {
        let mut out = vec![
            Output::Subscribe(topics::gateway_cmd(&self.prefix)),
            Output::Subscribe(format!("{}nodes/+/shell/req", self.prefix)),
        ];
        self.issue(&mut out, OpKind::NodeList, MgmtOp::NodeList, None);
        out.push(Output::Event(Event::Log(format!(
            "gateway {} on {} ({} node slots)",
            topics::node_hex(self.describe.net_id),
            self.port_name,
            self.describe.node_capacity
        ))));
        out
    }

    pub(crate) fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::SerialUp => {
                self.serial_up = true;
                self.publish_status(&mut out);
                out.push(Output::Event(Event::Link {
                    serial_up: true,
                    mqtt_up: self.mqtt_up,
                }));
                // Resync: the dongle may have rebooted while we were away.
                self.issue(&mut out, OpKind::Describe, MgmtOp::Describe, None);
            }
            Input::SerialDown { error } => {
                self.serial_up = false;
                self.publish_status(&mut out);
                out.push(Output::Event(Event::Link {
                    serial_up: false,
                    mqtt_up: self.mqtt_up,
                }));
                out.push(Output::Event(Event::Log(format!("serial lost: {error}"))));
            }
            Input::SerialFrame(msg) => self.on_serial(msg, &mut out),
            Input::MqttUp => {
                self.mqtt_up = true;
                out.push(Output::Subscribe(topics::gateway_cmd(&self.prefix)));
                out.push(Output::Subscribe(format!(
                    "{}nodes/+/shell/req",
                    self.prefix
                )));
                // Retained world re-publish: reconnect-resync is free by design.
                self.publish_status(&mut out);
                self.publish_stats(&mut out);
                self.publish_pairing(&mut out, None);
                for i in 0..self.nodes.len() {
                    self.publish_node(&mut out, i);
                    self.publish_pending(&mut out, i);
                }
                out.push(Output::Event(Event::Link {
                    serial_up: self.serial_up,
                    mqtt_up: true,
                }));
            }
            Input::MqttDown { error } => {
                self.mqtt_up = false;
                out.push(Output::Event(Event::Link {
                    serial_up: self.serial_up,
                    mqtt_up: false,
                }));
                out.push(Output::Event(Event::Log(format!("mqtt lost: {error}"))));
            }
            Input::MqttIn { topic, payload } => self.on_mqtt(&topic, &payload, &mut out),
            Input::Frontend(FrontendCmd::Rpc(req)) => self.on_rpc(req, &mut out),
            Input::Frontend(FrontendCmd::Shell { node, req }) => {
                self.on_shell_req(node, req, &mut out)
            }
            Input::Tick => self.on_tick(&mut out),
        }
        out
    }

    // ---- serial → engine ------------------------------------------------------

    fn on_serial(&mut self, msg: SerialMsg, out: &mut Vec<Output>) {
        match msg {
            SerialMsg::Hello {
                firmware_name,
                firmware_version,
                session_id,
            } => {
                let rebooted = self.hello_session.is_some_and(|s| s != session_id);
                self.hello_session = Some(session_id);
                self.firmware_version = firmware_version;
                self.describe.firmware_name = firmware_name;
                if rebooted {
                    out.push(Output::Event(Event::Log(format!(
                        "gateway rebooted (session {session_id}) — resyncing; queued downlinks were lost"
                    ))));
                    // The RAM queue died with the old session: fail every pending
                    // command loudly, then resync the registry.
                    for i in 0..self.nodes.len() {
                        let node = self.nodes[i].entry.id;
                        for p in std::mem::take(&mut self.nodes[i].pending) {
                            self.shell_error(out, node, &p, "gateway rebooted — command lost");
                        }
                        self.publish_pending(out, i);
                    }
                    self.issue(out, OpKind::Describe, MgmtOp::Describe, None);
                    self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
                }
                self.publish_status(out);
            }
            SerialMsg::Log { line } => out.push(Output::Event(Event::Log(line))),
            SerialMsg::Uplink {
                src,
                counter,
                rssi_dbm,
                lqi,
                data,
            } => {
                self.uplinks += 1;
                self.touch_node(src, rssi_dbm);
                out.push(Output::Event(Event::Radio(RadioSample::Rx {
                    src,
                    rssi_dbm,
                })));
                out.push(Output::Publish {
                    topic: topics::radio_rx(&self.prefix),
                    payload: json(&payload::RadioRx {
                        src: topics::node_hex(src),
                        rssi_dbm,
                        lqi,
                        len: data.len(),
                        ts: now_ts(),
                    }),
                    retain: false,
                });
                out.push(Output::Publish {
                    topic: topics::node_uplink(&self.prefix, src),
                    payload: json(&payload::UplinkDebug {
                        counter,
                        rssi_dbm,
                        lqi,
                        len: data.len(),
                        hex: data.iter().map(|b| format!("{b:02x}")).collect(),
                        ts: now_ts(),
                    }),
                    retain: false,
                });
                self.on_node_msg(src, counter, &data, out);
            }
            SerialMsg::Mgmt {
                req_id,
                result,
                chunk,
                last,
                data,
            } => self.on_mgmt_chunk(req_id, result, chunk, last, data, out),
            SerialMsg::Stat(stat) => self.on_radio_stat(stat, out),
            SerialMsg::Dropped(count) => {
                out.push(Output::Event(Event::Log(format!(
                    "gateway console dropped {count} frame(s) (link congestion)"
                ))));
            }
        }
    }

    /// Decode one forwarded radio-application payload — the host side of the
    /// "gateway is a transparent bridge" contract.
    fn on_node_msg(&mut self, src: u32, counter: u32, data: &[u8], out: &mut Vec<Output>) {
        let ts = now_ts();
        match radio::decode_node_msg(data) {
            Ok(NodeMsg::Button { kind, count }) => {
                let event = match kind {
                    radio::ButtonKind::Press => "press",
                    radio::ButtonKind::Release => "release",
                    radio::ButtonKind::Click => "click",
                    radio::ButtonKind::Hold => "hold",
                };
                out.push(Output::Publish {
                    topic: topics::node_button(&self.prefix, src),
                    payload: json(&payload::ButtonEvent {
                        event: event.into(),
                        count,
                        counter,
                        ts,
                    }),
                    retain: false,
                });
            }
            Ok(NodeMsg::Temperature { millic }) => {
                out.push(Output::Publish {
                    topic: topics::node_temperature(&self.prefix, src),
                    payload: json(&payload::Temperature {
                        celsius: millic as f64 / 1000.0,
                        millic,
                        ts,
                    }),
                    retain: true,
                });
            }
            Ok(NodeMsg::Accel { kind, face }) => {
                let event = match kind {
                    radio::AccelKind::Motion => "motion",
                    radio::AccelKind::Orientation => "orientation",
                };
                out.push(Output::Publish {
                    topic: topics::node_accel(&self.prefix, src),
                    payload: json(&payload::AccelEvent {
                        event: event.into(),
                        face,
                        counter,
                        ts,
                    }),
                    retain: false,
                });
            }
            Ok(NodeMsg::Info(info)) => self.on_node_info(src, &info, out),
            Ok(NodeMsg::Shell(chunk)) => self.on_shell_chunk(src, &chunk, out),
            Err(tower_protocol::Error::BadVersion { got }) => {
                out.push(Output::Event(Event::Log(format!(
                    "node {} speaks radio schema v{got}, this build v{} — update tower-cli or the node",
                    topics::node_hex(src),
                    radio::RADIO_SCHEMA_VERSION
                ))));
            }
            Err(_) => {
                out.push(Output::Event(Event::Log(format!(
                    "node {}: undecodable application payload ({} B)",
                    topics::node_hex(src),
                    data.len()
                ))));
            }
        }
    }

    /// A `NodeInfo` heartbeat: enrich the mirror (kind/sleeping) and auto-name
    /// unnamed nodes — `push-button:0` style, host-side by design (the gateway
    /// firmware never interprets payloads).
    fn on_node_info(&mut self, src: u32, info: &radio::NodeInfo<'_>, out: &mut Vec<Output>) {
        let kind = kind_of(info.firmware_name);
        let Some(i) = self.nodes.iter().position(|n| n.entry.id == src) else {
            return; // unregistered node — forwarded but not ours to manage
        };
        self.nodes[i].kind = kind.clone();
        self.nodes[i].session_id = Some(info.session_id);

        let mut flags = self.nodes[i].entry.flags;
        if info.sleeping {
            flags |= mgmt::NODE_FLAG_SLEEPING;
        } else {
            flags &= !mgmt::NODE_FLAG_SLEEPING;
        }

        if self.nodes[i].entry.flags & mgmt::NODE_FLAG_UNNAMED != 0 {
            // Auto-name: smallest free index for this kind.
            let name = (0..)
                .map(|n| format!("{kind}:{n}"))
                .find(|cand| !self.nodes.iter().any(|m| &m.entry.name == cand))
                .unwrap();
            flags &= !mgmt::NODE_FLAG_UNNAMED;
            out.push(Output::Event(Event::Log(format!(
                "auto-naming node {} \"{name}\"",
                topics::node_hex(src)
            ))));
            // `op` borrows the local; `issue` encodes it immediately, so no leak/copy.
            let op = MgmtOp::NodeUpdate {
                id: src,
                name: Some(name.as_str()),
                flags: Some(flags),
            };
            self.issue(out, OpKind::NodeUpdate, op, None);
        } else if flags != self.nodes[i].entry.flags {
            let op = MgmtOp::NodeUpdate {
                id: src,
                name: None,
                flags: Some(flags),
            };
            self.issue(out, OpKind::NodeUpdate, op, None);
        }
        self.publish_node(out, i);
    }

    /// One remote-shell response chunk from a node → the MQTT dialog + frontend.
    fn on_shell_chunk(
        &mut self,
        src: u32,
        chunk: &radio::NodeShellChunk<'_>,
        out: &mut Vec<Output>,
    ) {
        let Some(i) = self.nodes.iter().position(|n| n.entry.id == src) else {
            return;
        };
        let Some(pi) = self.nodes[i]
            .pending
            .iter()
            .position(|p| p.cmd_id == chunk.cmd_id)
        else {
            out.push(Output::Event(Event::Log(format!(
                "node {}: shell chunk for unknown cmd_id {}",
                topics::node_hex(src),
                chunk.cmd_id
            ))));
            return;
        };
        let gap = self.nodes[i].pending[pi].next_chunk != chunk.chunk;
        if gap {
            self.nodes[i].pending[pi].truncated = true;
        }
        self.nodes[i].pending[pi].next_chunk = chunk.chunk.wrapping_add(1);
        let rsp = payload::ShellRsp {
            id: self.nodes[i].pending[pi].rpc.clone(),
            chunk: chunk.chunk,
            text: chunk.text.to_string(),
            done: chunk.last,
            result: chunk.result,
            truncated: self.nodes[i].pending[pi].truncated,
            error: None,
        };
        out.push(Output::Publish {
            topic: topics::node_shell_rsp(&self.prefix, src),
            payload: json(&rsp),
            retain: false,
        });
        out.push(Output::Event(Event::Shell { node: src, rsp }));
        if chunk.last {
            self.nodes[i].pending.remove(pi);
            self.publish_pending(out, i);
        }
    }

    fn on_radio_stat(&mut self, stat: RadioStat, out: &mut Vec<Output>) {
        let ts = now_ts();
        match stat {
            RadioStat::Channel { channel, rssi_dbm } => {
                self.last_ambient = Some((rssi_dbm, channel));
                out.push(Output::Event(Event::Radio(RadioSample::Ambient {
                    dbm: rssi_dbm,
                    channel,
                })));
                out.push(Output::Publish {
                    topic: topics::radio_rssi(&self.prefix),
                    payload: json(&payload::RadioRssi {
                        dbm: rssi_dbm,
                        channel,
                        ts,
                    }),
                    retain: false,
                });
            }
            RadioStat::Tx {
                dest,
                item,
                outcome,
                ack_rssi_dbm,
            } => {
                out.push(Output::Event(Event::Radio(RadioSample::Tx {
                    dest,
                    delivered: outcome == mgmt::TX_DELIVERED,
                })));
                out.push(Output::Publish {
                    topic: topics::radio_tx(&self.prefix),
                    payload: json(&payload::RadioTx {
                        dest: topics::node_hex(dest),
                        item,
                        outcome: payload::tx_outcome_str(outcome).into(),
                        ack_rssi_dbm,
                        ts,
                    }),
                    retain: false,
                });
                if item != 0 {
                    self.on_queue_outcome(dest, item, outcome, out);
                }
            }
        }
    }

    /// A queue item left the gateway (delivered) or died (expired / kept for retry).
    fn on_queue_outcome(&mut self, node: u32, item: u16, outcome: u8, out: &mut Vec<Output>) {
        let Some(i) = self.nodes.iter().position(|n| n.entry.id == node) else {
            return;
        };
        match outcome {
            mgmt::TX_DELIVERED => {
                if let Some(p) = self.nodes[i].pending.iter_mut().find(|p| p.item == item) {
                    p.delivered_tick = Some(self.tick);
                }
                // Stays in `pending` until the node's reply completes (the dialog's
                // "pending" marker covers queued *and* awaiting-reply).
            }
            mgmt::TX_EXPIRED => {
                if let Some(pi) = self.nodes[i].pending.iter().position(|p| p.item == item) {
                    let p = self.nodes[i].pending.remove(pi);
                    self.shell_error(out, node, &p, "queue TTL expired before delivery");
                    self.publish_pending(out, i);
                }
            }
            _ => {} // not-delivered/busy/duty: the gateway retries on the next uplink
        }
    }

    // ---- management replies -----------------------------------------------------

    fn on_mgmt_chunk(
        &mut self,
        req_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        data: Vec<u8>,
        out: &mut Vec<Output>,
    ) {
        let Some(idx) = self.pending_mgmt.iter().position(|p| p.req_id == req_id) else {
            return; // stale/unknown (e.g. a timed-out op answering late)
        };
        {
            let p = &mut self.pending_mgmt[idx];
            if p.next_chunk != chunk {
                p.truncated = true;
            }
            p.next_chunk = chunk.wrapping_add(1);
            p.data.extend_from_slice(&data);
        }
        if !last {
            return;
        }
        let done = self.pending_mgmt.remove(idx);
        self.on_mgmt_done(done, result, out);
    }

    fn on_mgmt_done(&mut self, p: PendingMgmt, result: u8, out: &mut Vec<Output>) {
        let err = payload::mgmt_error_str(result);
        match (&p.op, err) {
            // ---- happy paths ----
            (OpKind::Describe, None) => {
                if let Ok(info) = postcard::from_bytes::<tower_protocol::mgmt::DeviceInfo>(&p.data)
                {
                    self.describe = info.into();
                    self.publish_status(out);
                }
                self.rpc_ok(
                    out,
                    &p,
                    serde_json::json!({ "gateway": topics::node_hex(self.describe.net_id) }),
                );
            }
            (OpKind::NodeList, None) => {
                let entries: Vec<NodeEntry> = parse_records(&p.data);
                let owned: Vec<NodeEntryOwned> = entries.into_iter().map(Into::into).collect();
                self.merge_registry(owned, out);
                let data = serde_json::to_value(self.node_payloads()).unwrap_or_default();
                self.rpc_ok(out, &p, data);
            }
            (OpKind::NodeAdd { id, name }, None) => {
                out.push(Output::Event(Event::Log(format!(
                    "node {} added ({name})",
                    topics::node_hex(*id)
                ))));
                self.rpc_ok(out, &p, serde_json::Value::Null);
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (OpKind::NodeRemove { id }, None) => {
                let id = *id;
                if let Some(i) = self.nodes.iter().position(|n| n.entry.id == id) {
                    for pd in std::mem::take(&mut self.nodes[i].pending) {
                        self.shell_error(out, id, &pd, "node removed");
                    }
                    self.nodes.remove(i);
                }
                // Clear the retained topics for the removed node.
                for topic in [
                    topics::node(&self.prefix, id),
                    topics::node_temperature(&self.prefix, id),
                    topics::node_shell_pending(&self.prefix, id),
                ] {
                    out.push(Output::Publish {
                        topic,
                        payload: Vec::new(),
                        retain: true,
                    });
                }
                self.emit_registry(out);
                self.rpc_ok(out, &p, serde_json::Value::Null);
            }
            (OpKind::NodeUpdate, None) => {
                self.rpc_ok(out, &p, serde_json::Value::Null);
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (OpKind::RevealKey { id }, None) => match postcard::from_bytes::<NodeKey>(&p.data) {
                Ok(k) => {
                    let hex: String = k.key.iter().map(|b| format!("{b:02x}")).collect();
                    self.rpc_ok(
                        out,
                        &p,
                        serde_json::json!({ "node": topics::node_hex(*id), "key": hex }),
                    );
                }
                Err(_) => self.rpc_err(out, &p, "malformed key record"),
            },
            (OpKind::PairingOpen, None) => {
                // Delayed resolution: a node joined.
                self.pairing_rpc = None;
                self.pairing_until = None;
                let joined = postcard::from_bytes::<Paired>(&p.data).ok();
                let hex = joined.map(|j| topics::node_hex(j.node_id));
                out.push(Output::Event(Event::Log(match &hex {
                    Some(h) => format!("node {h} paired"),
                    None => "pairing window resolved".into(),
                })));
                self.publish_pairing(out, hex.clone());
                self.rpc_ok(out, &p, serde_json::json!({ "joined": hex }));
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (OpKind::PairingCancel, None) => self.rpc_ok(out, &p, serde_json::Value::Null),
            (OpKind::QueuePush { node, shell }, None) => {
                let node = *node;
                match postcard::from_bytes::<QueueId>(&p.data) {
                    Ok(qid) => {
                        if let Some(origin) = shell
                            && let Some(i) = self.nodes.iter().position(|n| n.entry.id == node)
                        {
                            self.nodes[i].pending.push(PendingShell {
                                item: qid.item,
                                rpc: origin.rpc.clone(),
                                line: origin.line.clone(),
                                cmd_id: origin.cmd_id,
                                delivered_tick: None,
                                next_chunk: 0,
                                truncated: false,
                            });
                            self.publish_pending(out, i);
                            self.publish_node(out, i);
                        }
                        self.rpc_ok(out, &p, serde_json::json!({ "ref": qid.item }));
                    }
                    Err(_) => self.rpc_err(out, &p, "malformed queue id"),
                }
            }
            (OpKind::QueueList, None) => {
                let entries: Vec<QueueEntry> = parse_records(&p.data);
                let list: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "node": topics::node_hex(e.node),
                            "ref": e.item,
                            "age_s": e.age_s,
                            "ttl_s": e.ttl_s,
                        })
                    })
                    .collect();
                self.rpc_ok(out, &p, serde_json::Value::Array(list));
            }
            (OpKind::QueueDrop { node }, None) => {
                let node = *node;
                // The dequeue succeeded on the device; drop the mirror entry too. We
                // don't know which item without echoing it — the caller's ref is in
                // the RPC params, so re-list pending from the mirror by pruning the
                // ref recorded at issue time (see rpc handling; the ref rides in op).
                self.rpc_ok(out, &p, serde_json::Value::Null);
                if let Some(i) = self.nodes.iter().position(|n| n.entry.id == node) {
                    self.publish_pending(out, i);
                }
                self.issue(out, OpKind::NodeList, MgmtOp::NodeList, None);
            }
            (OpKind::StatsConfig, None) => self.rpc_ok(out, &p, serde_json::Value::Null),
            // ---- refusals ----
            (OpKind::PairingOpen, Some(e)) => {
                self.pairing_rpc = None;
                self.pairing_until = None;
                self.publish_pairing(out, None);
                if result == mgmt::MGMT_TIMEOUT {
                    // Window expired unjoined — a normal outcome, not an error.
                    self.rpc_ok(out, &p, serde_json::json!({ "joined": null }));
                } else {
                    self.rpc_err(out, &p, e);
                }
            }
            (OpKind::QueuePush { node, shell }, Some(e)) => {
                let node = *node;
                if let Some(origin) = shell {
                    let rsp = payload::ShellRsp {
                        id: origin.rpc.clone(),
                        chunk: 0,
                        text: String::new(),
                        done: true,
                        result: result.max(1),
                        truncated: false,
                        error: Some(e.to_string()),
                    };
                    out.push(Output::Publish {
                        topic: topics::node_shell_rsp(&self.prefix, node),
                        payload: json(&rsp),
                        retain: false,
                    });
                    out.push(Output::Event(Event::Shell { node, rsp }));
                }
                self.rpc_err(out, &p, e);
            }
            (_, Some(e)) => self.rpc_err(out, &p, e),
        }
        if p.truncated {
            out.push(Output::Event(Event::Log(format!(
                "management reply {} arrived truncated (chunk gap)",
                p.req_id
            ))));
        }
    }

    // ---- MQTT → engine ----------------------------------------------------------

    fn on_mqtt(&mut self, topic: &str, data: &[u8], out: &mut Vec<Output>) {
        match topics::classify(&self.prefix, topic) {
            topics::Inbound::Cmd => match serde_json::from_slice::<RpcRequest>(data) {
                Ok(req) => self.on_rpc(req, out),
                Err(e) => out.push(Output::Event(Event::Log(format!("bad RPC payload: {e}")))),
            },
            topics::Inbound::ShellReq(node) => {
                match serde_json::from_slice::<payload::ShellReq>(data) {
                    Ok(req) => self.on_shell_req(node, req, out),
                    Err(e) => {
                        out.push(Output::Event(Event::Log(format!("bad shell request: {e}"))))
                    }
                }
            }
            topics::Inbound::Other => {}
        }
    }

    /// Enqueue one remote-shell line for `node` (from `nodes/{id}/shell/req`).
    pub(crate) fn on_shell_req(
        &mut self,
        node: u32,
        req: payload::ShellReq,
        out: &mut Vec<Output>,
    ) {
        let fail = |engine: &Engine, out: &mut Vec<Output>, error: &str| {
            let rsp = payload::ShellRsp {
                id: req.id.clone(),
                chunk: 0,
                text: String::new(),
                done: true,
                result: 1,
                truncated: false,
                error: Some(error.to_string()),
            };
            out.push(Output::Publish {
                topic: topics::node_shell_rsp(&engine.prefix, node),
                payload: json(&rsp),
                retain: false,
            });
            out.push(Output::Event(Event::Shell { node, rsp }));
        };
        if self.nodes.iter().all(|n| n.entry.id != node) {
            return fail(self, out, "unknown node");
        }
        if req.line.len() > radio::RADIO_SHELL_CHUNK {
            return fail(
                self,
                out,
                &format!(
                    "line too long for the radio MTU (max {} bytes)",
                    radio::RADIO_SHELL_CHUNK
                ),
            );
        }
        let cmd_id = self.next_cmd;
        self.next_cmd = self
            .next_cmd
            .checked_add(1)
            .filter(|&v| v != 0)
            .unwrap_or(1);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let Ok(n) = radio::encode_node_cmd(
            &NodeCmd::Shell {
                cmd_id,
                line: &req.line,
            },
            &mut env,
        ) else {
            return fail(self, out, "envelope encode failed");
        };
        let data = env[..n].to_vec();
        let origin = ShellOrigin {
            rpc: req.id.clone(),
            line: req.line.clone(),
            cmd_id,
        };
        // The borrow-carrying op is encoded immediately by `issue`.
        let op = MgmtOp::QueuePush {
            node,
            ttl_s: req.ttl_s,
            data: &data,
        };
        self.issue(
            out,
            OpKind::QueuePush {
                node,
                shell: Some(origin),
            },
            op,
            None,
        );
    }

    /// The `gateway/cmd` RPC surface — what the `tower nodes`/`net` client commands
    /// (and the TUI, in-process) drive.
    pub(crate) fn on_rpc(&mut self, req: RpcRequest, out: &mut Vec<Output>) {
        let rpc = Some(req.id.clone());
        let params = &req.params;
        let node_param = || -> Option<u32> {
            let v = params.get("node")?.as_str()?;
            topics::parse_node_hex(v).or_else(|| {
                self.nodes
                    .iter()
                    .find(|n| n.entry.name == v)
                    .map(|n| n.entry.id)
            })
        };
        match req.op.as_str() {
            "describe" => self.issue(out, OpKind::Describe, MgmtOp::Describe, rpc),
            "node_list" => self.issue(out, OpKind::NodeList, MgmtOp::NodeList, rpc),
            "node_add" => {
                // Cable pairing: the client provisioned the node on its own serial
                // port and registers (id, key, name) here.
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .and_then(topics::parse_node_hex);
                let key = params
                    .get("key")
                    .and_then(|v| v.as_str())
                    .and_then(parse_key_hex);
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let sleeping = params
                    .get("sleeping")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let (Some(id), Some(key)) = (id, key) else {
                    return self.rpc_err_direct(out, &req.id, "node_add needs id + key (hex)");
                };
                let mut flags = if sleeping {
                    mgmt::NODE_FLAG_SLEEPING
                } else {
                    0
                };
                if name.is_empty() {
                    flags |= mgmt::NODE_FLAG_UNNAMED;
                }
                let op = MgmtOp::NodeAdd {
                    id,
                    key,
                    name: name.as_str(),
                    flags,
                };
                self.issue(
                    out,
                    OpKind::NodeAdd {
                        id,
                        name: name.clone(),
                    },
                    op,
                    rpc,
                );
            }
            "node_remove" => match node_param() {
                Some(id) => self.issue(
                    out,
                    OpKind::NodeRemove { id },
                    MgmtOp::NodeRemove { id },
                    rpc,
                ),
                None => self.rpc_err_direct(out, &req.id, "unknown node"),
            },
            "node_rename" => {
                let Some(id) = node_param() else {
                    return self.rpc_err_direct(out, &req.id, "unknown node");
                };
                let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                    return self.rpc_err_direct(out, &req.id, "missing name");
                };
                if name.is_empty() || name.len() > mgmt::MAX_NODE_NAME {
                    return self.rpc_err_direct(
                        out,
                        &req.id,
                        &format!("name must be 1..={} bytes", mgmt::MAX_NODE_NAME),
                    );
                }
                let flags = self
                    .nodes
                    .iter()
                    .find(|n| n.entry.id == id)
                    .map(|n| n.entry.flags & !mgmt::NODE_FLAG_UNNAMED);
                let op = MgmtOp::NodeUpdate {
                    id,
                    name: Some(name),
                    flags,
                };
                self.issue(out, OpKind::NodeUpdate, op, rpc);
            }
            "reveal_key" => match node_param() {
                Some(id) => self.issue(
                    out,
                    OpKind::RevealKey { id },
                    MgmtOp::NodeRevealKey { id },
                    rpc,
                ),
                None => self.rpc_err_direct(out, &req.id, "unknown node"),
            },
            "node_add_ota" => {
                if self.pairing_rpc.is_some() {
                    return self.rpc_err_direct(out, &req.id, "a pairing window is already open");
                }
                let window_s = params
                    .get("window_s")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60)
                    .clamp(1, 600) as u16;
                // The host mints the AES key — the only CSPRNG in the system.
                let mut key = [0u8; 16];
                if getrandom::fill(&mut key).is_err() {
                    return self.rpc_err_direct(out, &req.id, "no entropy source for the node key");
                }
                self.pairing_rpc = Some(req.id.clone());
                self.pairing_until = Some(self.tick + window_s as u64);
                self.publish_pairing(out, None);
                self.issue_delayed(
                    out,
                    OpKind::PairingOpen,
                    MgmtOp::PairingOpen { window_s, key },
                    rpc,
                    window_s as u64 + 5,
                );
            }
            "pairing_cancel" => {
                self.pairing_rpc = None;
                self.pairing_until = None;
                self.publish_pairing(out, None);
                self.issue(out, OpKind::PairingCancel, MgmtOp::PairingCancel, rpc);
            }
            "queue_list" => {
                let node = node_param().unwrap_or(0);
                self.issue(out, OpKind::QueueList, MgmtOp::QueueList { node }, rpc);
            }
            "queue_drop" => {
                let Some(node) = node_param() else {
                    return self.rpc_err_direct(out, &req.id, "unknown node");
                };
                let item = params.get("ref").and_then(|v| v.as_u64()).map(|v| v as u16);
                // Prune the mirror entry now (the device answer confirms; a NotFound
                // still clears our side, which matches "it isn't queued anymore").
                if let (Some(it), Some(i)) =
                    (item, self.nodes.iter().position(|n| n.entry.id == node))
                    && let Some(pi) = self.nodes[i].pending.iter().position(|pd| pd.item == it)
                {
                    let pd = self.nodes[i].pending.remove(pi);
                    self.shell_error(out, node, &pd, "dequeued");
                    self.publish_pending(out, i);
                }
                self.issue(
                    out,
                    OpKind::QueueDrop { node },
                    MgmtOp::QueueDrop { node, item },
                    rpc,
                );
            }
            "stats_config" => {
                let period = params
                    .get("period_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1000)
                    .min(60_000) as u32;
                self.issue(
                    out,
                    OpKind::StatsConfig,
                    MgmtOp::StatsConfig {
                        channel_period_ms: period,
                    },
                    rpc,
                );
            }
            other => self.rpc_err_direct(out, &req.id, &format!("unknown op \"{other}\"")),
        }
    }

    // ---- housekeeping -------------------------------------------------------------

    fn on_tick(&mut self, out: &mut Vec<Output>) {
        self.tick += 1;
        // Management timeouts.
        let expired: Vec<usize> = self
            .pending_mgmt
            .iter()
            .enumerate()
            .filter(|(_, p)| self.tick >= p.deadline)
            .map(|(i, _)| i)
            .collect();
        for i in expired.into_iter().rev() {
            let p = self.pending_mgmt.remove(i);
            if matches!(p.op, OpKind::PairingOpen) {
                self.pairing_rpc = None;
                self.pairing_until = None;
                self.publish_pairing(out, None);
            }
            out.push(Output::Event(Event::Log(format!(
                "management op timed out (req {})",
                p.req_id
            ))));
            self.rpc_err(out, &p, "device did not answer (timeout)");
        }
        // Remote-shell reply timeouts (delivered but never answered).
        for i in 0..self.nodes.len() {
            let node = self.nodes[i].entry.id;
            let dead: Vec<usize> = self.nodes[i]
                .pending
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.delivered_tick
                        .is_some_and(|t| self.tick >= t + SHELL_REPLY_TIMEOUT_TICKS)
                })
                .map(|(pi, _)| pi)
                .collect();
            let changed = !dead.is_empty();
            for pi in dead.into_iter().rev() {
                let p = self.nodes[i].pending.remove(pi);
                self.shell_error(out, node, &p, "delivered but no reply from the node");
            }
            if changed {
                self.publish_pending(out, i);
            }
        }
        // Pairing countdown surface.
        if self.pairing_until.is_some() {
            self.publish_pairing(out, None);
        }
        if self.tick % STATS_EVERY_TICKS == 0 {
            self.publish_stats(out);
        }
    }

    // ---- helpers --------------------------------------------------------------------

    /// Allocate a req_id, encode and send `op`, and track the reply.
    fn issue(&mut self, out: &mut Vec<Output>, kind: OpKind, op: MgmtOp<'_>, rpc: Option<String>) {
        self.issue_delayed(out, kind, op, rpc, MGMT_TIMEOUT_TICKS);
    }

    fn issue_delayed(
        &mut self,
        out: &mut Vec<Output>,
        kind: OpKind,
        op: MgmtOp<'_>,
        rpc: Option<String>,
        timeout_ticks: u64,
    ) {
        let req_id = self.next_req;
        self.next_req = self
            .next_req
            .checked_add(1)
            .filter(|&v| v != 0)
            .unwrap_or(1);
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        match encode_mgmt(seq, req_id, &op) {
            Some(frame) => {
                self.pending_mgmt.push(PendingMgmt {
                    req_id,
                    op: kind,
                    rpc,
                    data: Vec::new(),
                    next_chunk: 0,
                    truncated: false,
                    deadline: self.tick + timeout_ticks,
                });
                out.push(Output::SerialSend(frame));
            }
            None => {
                if let Some(id) = rpc {
                    self.rpc_err_direct(out, &id, "request encode failed");
                }
            }
        }
    }

    fn rpc_ok(&self, out: &mut Vec<Output>, p: &PendingMgmt, data: serde_json::Value) {
        if let Some(id) = &p.rpc {
            self.rpc_respond(
                out,
                RpcResponse {
                    id: id.clone(),
                    ok: true,
                    error: None,
                    data,
                },
            );
        }
    }

    fn rpc_err(&self, out: &mut Vec<Output>, p: &PendingMgmt, error: &str) {
        if let Some(id) = &p.rpc {
            self.rpc_err_direct(out, id, error);
        }
    }

    fn rpc_err_direct(&self, out: &mut Vec<Output>, rpc_id: &str, error: &str) {
        self.rpc_respond(
            out,
            RpcResponse {
                id: rpc_id.to_string(),
                ok: false,
                error: Some(error.to_string()),
                data: serde_json::Value::Null,
            },
        );
    }

    /// Route an RPC response to its origin: in-process (TUI) ids go back as events,
    /// everything else to the response topic.
    fn rpc_respond(&self, out: &mut Vec<Output>, rsp: RpcResponse) {
        if rsp.id.starts_with("tui-") {
            out.push(Output::Event(Event::Rpc(rsp)));
        } else {
            out.push(Output::Publish {
                topic: topics::gateway_rsp(&self.prefix, &rsp.id),
                payload: json(&rsp),
                retain: false,
            });
        }
    }

    /// Publish a terminal error for a pending shell command (dialog + rsp topic).
    fn shell_error(&self, out: &mut Vec<Output>, node: u32, p: &PendingShell, error: &str) {
        let rsp = payload::ShellRsp {
            id: p.rpc.clone(),
            chunk: p.next_chunk,
            text: String::new(),
            done: true,
            result: 1,
            truncated: p.truncated,
            error: Some(error.to_string()),
        };
        out.push(Output::Publish {
            topic: topics::node_shell_rsp(&self.prefix, node),
            payload: json(&rsp),
            retain: false,
        });
        out.push(Output::Event(Event::Shell { node, rsp }));
    }

    /// Merge a fresh `NodeList` into the mirror: keep host-side enrichment (kind,
    /// session, pendings) for surviving nodes, publish adds/changes, clear removals.
    fn merge_registry(&mut self, fresh: Vec<NodeEntryOwned>, out: &mut Vec<Output>) {
        let mut next: Vec<NodeMirror> = Vec::with_capacity(fresh.len());
        for entry in fresh {
            let old = self.nodes.iter_mut().find(|n| n.entry.id == entry.id);
            match old {
                Some(o) => next.push(NodeMirror {
                    entry,
                    kind: o.kind.clone(),
                    session_id: o.session_id,
                    pending: std::mem::take(&mut o.pending),
                }),
                None => next.push(NodeMirror {
                    entry,
                    kind: String::new(),
                    session_id: None,
                    pending: Vec::new(),
                }),
            }
        }
        // Anything left in the old mirror was removed on the device.
        for gone in self
            .nodes
            .iter()
            .filter(|o| next.iter().all(|n| n.entry.id != o.entry.id))
        {
            out.push(Output::Publish {
                topic: topics::node(&self.prefix, gone.entry.id),
                payload: Vec::new(),
                retain: true,
            });
        }
        self.nodes = next;
        for i in 0..self.nodes.len() {
            self.publish_node(out, i);
        }
        self.emit_registry(out);
    }

    /// Refresh a node's RAM-side liveliness on an uplink (the device tracks this too;
    /// mirroring it keeps the TUI live without polling NodeList).
    fn touch_node(&mut self, id: u32, rssi_dbm: i16) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.entry.id == id) {
            n.entry.last_seen_s = 0;
            n.entry.rssi_dbm = rssi_dbm.clamp(i8::MIN as i16, i8::MAX as i16) as i8;
            n.entry.uplinks = n.entry.uplinks.saturating_add(1);
        }
    }

    fn node_payloads(&self) -> Vec<payload::Node> {
        self.nodes
            .iter()
            .map(|n| payload::Node {
                id: topics::node_hex(n.entry.id),
                name: n.entry.name.clone(),
                kind: n.kind.clone(),
                sleeping: n.entry.flags & mgmt::NODE_FLAG_SLEEPING != 0,
                unnamed: n.entry.flags & mgmt::NODE_FLAG_UNNAMED != 0,
                last_seen_s: (n.entry.last_seen_s != mgmt::LAST_SEEN_NEVER)
                    .then_some(n.entry.last_seen_s),
                rssi_dbm: (n.entry.rssi_dbm != mgmt::RSSI_NONE).then_some(n.entry.rssi_dbm),
                uplinks: n.entry.uplinks,
                queued: n.entry.queued.max(n.pending.len() as u8),
            })
            .collect()
    }

    fn publish_node(&self, out: &mut Vec<Output>, i: usize) {
        let payloads = self.node_payloads();
        out.push(Output::Publish {
            topic: topics::node(&self.prefix, self.nodes[i].entry.id),
            payload: json(&payloads[i]),
            retain: true,
        });
    }

    fn publish_pending(&self, out: &mut Vec<Output>, i: usize) {
        let entries: Vec<PendingEntry> = self.nodes[i]
            .pending
            .iter()
            .map(|p| PendingEntry {
                r#ref: p.item,
                id: p.rpc.clone(),
                line: p.line.clone(),
            })
            .collect();
        out.push(Output::Publish {
            topic: topics::node_shell_pending(&self.prefix, self.nodes[i].entry.id),
            payload: json(&entries),
            retain: true,
        });
        self.emit_registry(out);
    }

    fn emit_registry(&self, out: &mut Vec<Output>) {
        let views: Vec<NodeView> = self
            .nodes
            .iter()
            .map(|n| NodeView {
                id: n.entry.id,
                name: n.entry.name.clone(),
                kind: n.kind.clone(),
                sleeping: n.entry.flags & mgmt::NODE_FLAG_SLEEPING != 0,
                unnamed: n.entry.flags & mgmt::NODE_FLAG_UNNAMED != 0,
                last_seen_s: (n.entry.last_seen_s != mgmt::LAST_SEEN_NEVER)
                    .then_some(n.entry.last_seen_s),
                rssi_dbm: (n.entry.rssi_dbm != mgmt::RSSI_NONE).then_some(n.entry.rssi_dbm),
                uplinks: n.entry.uplinks,
                queued: n.entry.queued.max(n.pending.len() as u8),
                pending: n
                    .pending
                    .iter()
                    .map(|p| PendingEntry {
                        r#ref: p.item,
                        id: p.rpc.clone(),
                        line: p.line.clone(),
                    })
                    .collect(),
            })
            .collect();
        out.push(Output::Event(Event::Registry(views)));
    }

    fn publish_status(&self, out: &mut Vec<Output>) {
        let state = if self.serial_up { "online" } else { "degraded" };
        out.push(Output::Publish {
            topic: topics::gateway_status(&self.prefix),
            payload: json(&payload::Status {
                state: state.into(),
                schema: payload::SCHEMA,
                gateway: topics::node_hex(self.describe.net_id),
                firmware: self.describe.firmware_name.clone(),
                firmware_version: self.firmware_version.clone(),
                session_id: self.hello_session.unwrap_or(0),
                protocol: tower_protocol::PROTOCOL_VERSION,
                serial_port: self.port_name.clone(),
                serial_up: self.serial_up,
            }),
            retain: true,
        });
    }

    fn publish_stats(&self, out: &mut Vec<Output>) {
        out.push(Output::Publish {
            topic: topics::gateway_stats(&self.prefix),
            payload: json(&payload::Stats {
                uptime_s: self.tick,
                nodes: self.nodes.len() as u32,
                uplinks: self.uplinks,
                queued: self.nodes.iter().map(|n| n.pending.len() as u32).sum(),
                rssi_dbm: self.last_ambient.map(|(d, _)| d),
                channel: self.last_ambient.map(|(_, c)| c),
            }),
            retain: true,
        });
    }

    fn publish_pairing(&self, out: &mut Vec<Output>, joined: Option<String>) {
        let open = self.pairing_until.is_some();
        let p = payload::Pairing {
            state: if open { "open" } else { "idle" }.into(),
            remaining_s: self.pairing_until.map(|u| u.saturating_sub(self.tick)),
            joined,
        };
        out.push(Output::Publish {
            topic: topics::gateway_pairing(&self.prefix),
            payload: json(&p),
            retain: true,
        });
        out.push(Output::Event(Event::Pairing(p)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_protocol::mgmt::{DeviceRole, MGMT_OK, NODE_FLAG_SLEEPING, NODE_FLAG_UNNAMED};
    use tower_protocol::{MsgType, decode_frame};

    fn engine() -> Engine {
        Engine::new(
            "tower/".into(),
            "/dev/ttyTEST".into(),
            crate::mgmt::DeviceInfoOwned {
                role: DeviceRole::Gateway,
                radio_schema_version: radio::RADIO_SCHEMA_VERSION,
                net_id: 0x0000_0001,
                band: 0,
                channel: 0,
                node_capacity: 32,
                node_count: 0,
                provisioned: true,
                gw_id: 0x0000_0001,
                firmware_name: "radio_dongle_gateway".into(),
            },
        )
    }

    /// Decode the MgmtRequest inside an Output::SerialSend.
    fn sent_op(out: &Output) -> Option<(u16, String)> {
        let Output::SerialSend(frame) = out else {
            return None;
        };
        let mut dec = tower_protocol::FrameDecoder::new();
        let inner: Vec<u8> = frame
            .iter()
            .find_map(|&b| dec.push(b).map(|s| s.to_vec()))?;
        let (mt, _, payload) = decode_frame(&inner).ok()?;
        if mt != MsgType::MgmtRequest {
            return None;
        }
        let req: tower_protocol::msg::MgmtRequest = postcard::from_bytes(payload).ok()?;
        Some((req.req_id, format!("{:?}", req.op)))
    }

    fn publishes<'a>(out: &'a [Output], topic: &str) -> Vec<&'a [u8]> {
        out.iter()
            .filter_map(|o| match o {
                Output::Publish {
                    topic: t, payload, ..
                } if t == topic => Some(payload.as_slice()),
                _ => None,
            })
            .collect()
    }

    /// Feed a complete single-chunk mgmt reply for the most recent request.
    fn reply(e: &mut Engine, req_id: u16, result: u8, data: &[u8]) -> Vec<Output> {
        e.handle(Input::SerialFrame(SerialMsg::Mgmt {
            req_id,
            result,
            chunk: 0,
            last: true,
            data: data.to_vec(),
        }))
    }

    fn node_entry_record(id: u32, name: &str, flags: u8) -> Vec<u8> {
        postcard::to_stdvec(&NodeEntry {
            id,
            name,
            flags,
            last_seen_s: 3,
            rssi_dbm: -60,
            uplinks: 5,
            queued: 0,
        })
        .unwrap()
    }

    /// Install one node into the engine mirror via a NodeList round-trip.
    fn with_node(e: &mut Engine, id: u32, name: &str, flags: u8) {
        let start = e.start();
        let (req_id, op) = start
            .iter()
            .find_map(sent_op)
            .expect("start issues NodeList");
        assert!(op.contains("NodeList"));
        let out = reply(e, req_id, MGMT_OK, &node_entry_record(id, name, flags));
        assert!(!publishes(&out, &format!("tower/nodes/{}", topics::node_hex(id))).is_empty());
    }

    #[test]
    fn start_subscribes_and_lists() {
        let mut e = engine();
        let out = e.start();
        let subs: Vec<&String> = out
            .iter()
            .filter_map(|o| match o {
                Output::Subscribe(f) => Some(f),
                _ => None,
            })
            .collect();
        assert!(subs.iter().any(|f| *f == "tower/gateway/cmd"));
        assert!(subs.iter().any(|f| *f == "tower/nodes/+/shell/req"));
        assert!(
            out.iter().find_map(sent_op).is_some(),
            "a NodeList goes out"
        );
    }

    #[test]
    fn node_list_reply_publishes_retained_registry() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", NODE_FLAG_SLEEPING);
        // The retained node payload carries the name and the sleeping flag.
        let out = reply_dummy_registry(&mut e);
        let raw = publishes(&out, "tower/nodes/0x0000ab12");
        assert!(!raw.is_empty());
        let n: payload::Node = serde_json::from_slice(raw[0]).unwrap();
        assert_eq!(n.name, "kitchen");
        assert!(n.sleeping);
    }

    /// Re-list helper so assertions read the publish of a fresh merge.
    fn reply_dummy_registry(e: &mut Engine) -> Vec<Output> {
        let mut out = Vec::new();
        e.issue(&mut out, OpKind::NodeList, MgmtOp::NodeList, None);
        let (req_id, _) = out.iter().find_map(sent_op).unwrap();
        reply(
            e,
            req_id,
            MGMT_OK,
            &node_entry_record(0xAB12, "kitchen", NODE_FLAG_SLEEPING),
        )
    }

    #[test]
    fn button_uplink_maps_to_topic() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Button {
                kind: radio::ButtonKind::Click,
                count: 7,
            },
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 42,
            rssi_dbm: -67,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let raw = publishes(&out, "tower/nodes/0x0000ab12/event/button");
        assert_eq!(raw.len(), 1);
        let ev: payload::ButtonEvent = serde_json::from_slice(raw[0]).unwrap();
        assert_eq!((ev.event.as_str(), ev.count, ev.counter), ("click", 7, 42));
        // The graph feed got an RX mark too.
        assert!(!publishes(&out, "tower/radio/rx").is_empty());
    }

    #[test]
    fn unnamed_node_is_auto_named_from_info() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "", NODE_FLAG_UNNAMED);
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Info(radio::NodeInfo {
                firmware_name: "radio_push_button",
                firmware_version: "v0.1.0",
                session_id: 1,
                sleeping: true,
                battery_mv: None,
            }),
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 1,
            rssi_dbm: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let op = out.iter().find_map(sent_op).expect("a NodeUpdate goes out");
        assert!(op.1.contains("NodeUpdate"), "{}", op.1);
        assert!(op.1.contains("push-button:0"), "auto-name in {}", op.1);
    }

    #[test]
    fn shell_req_roundtrip_queue_and_chunks() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", NODE_FLAG_SLEEPING);
        // 1. MQTT shell request → QueuePush.
        let out = e.handle(Input::MqttIn {
            topic: "tower/nodes/0x0000ab12/shell/req".into(),
            payload: serde_json::to_vec(&payload::ShellReq {
                id: "u-1".into(),
                line: "/led on".into(),
                ttl_s: 0,
            })
            .unwrap(),
        });
        let (req_id, op) = out.iter().find_map(sent_op).expect("QueuePush goes out");
        assert!(op.contains("QueuePush"), "{op}");
        // 2. QueueId reply → retained pending mirror.
        let qid = postcard::to_stdvec(&QueueId { item: 9 }).unwrap();
        let out = reply(&mut e, req_id, MGMT_OK, &qid);
        let pend = publishes(&out, "tower/nodes/0x0000ab12/shell/pending");
        assert!(!pend.is_empty());
        let entries: Vec<payload::PendingEntry> = serde_json::from_slice(pend[0]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].r#ref, 9);
        assert_eq!(entries[0].line, "/led on");
        // 3. The node's reply chunks → shell/rsp; done clears pending.
        let mut env = [0u8; radio::MAX_RADIO_PAYLOAD];
        let n = radio::encode_node_msg(
            &NodeMsg::Shell(radio::NodeShellChunk {
                cmd_id: 1, // first allocated cmd_id
                result: 0,
                chunk: 0,
                last: true,
                text: "ok",
            }),
            &mut env,
        )
        .unwrap();
        let out = e.handle(Input::SerialFrame(SerialMsg::Uplink {
            src: 0xAB12,
            counter: 2,
            rssi_dbm: -60,
            lqi: 30,
            data: env[..n].to_vec(),
        }));
        let rsp_raw = publishes(&out, "tower/nodes/0x0000ab12/shell/rsp");
        assert_eq!(rsp_raw.len(), 1);
        let rsp: payload::ShellRsp = serde_json::from_slice(rsp_raw[0]).unwrap();
        assert_eq!(
            (rsp.id.as_str(), rsp.text.as_str(), rsp.done),
            ("u-1", "ok", true)
        );
        // Pending mirror emptied.
        let pend = publishes(&out, "tower/nodes/0x0000ab12/shell/pending");
        assert!(!pend.is_empty());
        let entries: Vec<payload::PendingEntry> = serde_json::from_slice(pend[0]).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn unknown_rpc_answers_error_on_rsp_topic() {
        let mut e = engine();
        let out = e.handle(Input::MqttIn {
            topic: "tower/gateway/cmd".into(),
            payload: serde_json::to_vec(&RpcRequest {
                id: "u-9".into(),
                op: "frobnicate".into(),
                params: serde_json::Value::Null,
            })
            .unwrap(),
        });
        let raw = publishes(&out, "tower/gateway/rsp/u-9");
        assert_eq!(raw.len(), 1);
        let rsp: RpcResponse = serde_json::from_slice(raw[0]).unwrap();
        assert!(!rsp.ok);
    }

    #[test]
    fn gateway_reboot_fails_pendings_and_resyncs() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        // First Hello pins the session.
        let _ = e.handle(Input::SerialFrame(SerialMsg::Hello {
            firmware_name: "radio_dongle_gateway".into(),
            firmware_version: "v0.1.0".into(),
            session_id: 7,
        }));
        // Queue a shell command (request + QueueId reply).
        let out = e.handle(Input::MqttIn {
            topic: "tower/nodes/0x0000ab12/shell/req".into(),
            payload: br#"{"id":"u-2","line":"/x"}"#.to_vec(),
        });
        let (req_id, _) = out.iter().find_map(sent_op).unwrap();
        let qid = postcard::to_stdvec(&QueueId { item: 3 }).unwrap();
        let _ = reply(&mut e, req_id, MGMT_OK, &qid);
        // A new session id = the gateway rebooted; its RAM queue is gone.
        let out = e.handle(Input::SerialFrame(SerialMsg::Hello {
            firmware_name: "radio_dongle_gateway".into(),
            firmware_version: "v0.1.0".into(),
            session_id: 8,
        }));
        let rsp_raw = publishes(&out, "tower/nodes/0x0000ab12/shell/rsp");
        assert_eq!(rsp_raw.len(), 1);
        let rsp: payload::ShellRsp = serde_json::from_slice(rsp_raw[0]).unwrap();
        assert!(rsp.error.is_some());
        // And a resync went out (Describe + NodeList).
        let ops: Vec<String> = out.iter().filter_map(sent_op).map(|(_, op)| op).collect();
        assert!(ops.iter().any(|o| o.contains("Describe")));
        assert!(ops.iter().any(|o| o.contains("NodeList")));
    }

    #[test]
    fn mgmt_timeout_fails_the_rpc() {
        let mut e = engine();
        let out = e.handle(Input::MqttIn {
            topic: "tower/gateway/cmd".into(),
            payload: br#"{"id":"u-3","op":"node_list"}"#.to_vec(),
        });
        assert!(out.iter().find_map(sent_op).is_some());
        let mut failed = false;
        for _ in 0..(MGMT_TIMEOUT_TICKS + 1) {
            let out = e.handle(Input::Tick);
            if !publishes(&out, "tower/gateway/rsp/u-3").is_empty() {
                failed = true;
            }
        }
        assert!(failed, "the pending RPC must fail after the timeout ticks");
    }

    #[test]
    fn ambient_stat_publishes_and_feeds_the_graph() {
        let mut e = engine();
        let out = e.handle(Input::SerialFrame(SerialMsg::Stat(RadioStat::Channel {
            channel: 0,
            rssi_dbm: -98,
        })));
        assert!(!publishes(&out, "tower/radio/rssi").is_empty());
        assert!(out.iter().any(|o| matches!(
            o,
            Output::Event(Event::Radio(RadioSample::Ambient { dbm: -98, .. }))
        )));
    }

    #[test]
    fn mqtt_reconnect_republishes_retained_world() {
        let mut e = engine();
        with_node(&mut e, 0xAB12, "kitchen", 0);
        let out = e.handle(Input::MqttUp);
        assert!(!publishes(&out, "tower/gateway/status").is_empty());
        assert!(!publishes(&out, "tower/nodes/0x0000ab12").is_empty());
        assert!(!publishes(&out, "tower/nodes/0x0000ab12/shell/pending").is_empty());
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Subscribe(f) if f == "tower/gateway/cmd"))
        );
    }
}
