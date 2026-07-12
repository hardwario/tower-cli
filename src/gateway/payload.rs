//! JSON payloads for every topic in `topics.rs` — the other half of the gateway's
//! public MQTT API. Golden-string tests pin the serialized shapes (the MQTT analog of
//! tower-protocol's golden vectors): renaming or reordering a field here breaks
//! external subscribers, so it must be a conscious change. `Status.schema` versions
//! the whole tree — clients refuse a mismatch like the serial side refuses a
//! `PROTOCOL_VERSION` mismatch.

use serde::{Deserialize, Serialize};

/// Version of this topic/payload tree, carried in the retained `gateway/status`.
/// Bumped 1→2 when the radio-address terminology was unified (`nodes` payload key
/// `id`→`addr`; RPC node-address param `id`/`node`→`addr`).
pub(crate) const SCHEMA: u32 = 2;

/// `gateway/status` (retained). The LWT publishes only `{state:"offline", schema}` —
/// the broker can't know the rest once we're gone — so every other field is
/// `#[serde(default)]`: without that, `net status` could not even *parse* the one
/// payload that means "gateway died", which is the state the command exists to report.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default)]
pub(crate) struct Status {
    /// `online` (serial + mqtt up) | `degraded` (serial down) | `offline` (LWT).
    pub state: String,
    pub schema: u32,
    pub gateway: String,
    pub firmware: String,
    pub firmware_version: String,
    pub session_id: u32,
    pub protocol: u8,
    pub serial_port: String,
    pub serial_up: bool,
}

impl Status {
    /// The last-will payload: everything a broker can say once we're gone.
    pub(crate) fn offline() -> serde_json::Value {
        serde_json::json!({ "state": "offline", "schema": SCHEMA })
    }
}

/// `gateway/stats` (retained, refreshed on a slow tick).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Stats {
    pub uptime_s: u64,
    pub nodes: u32,
    pub uplinks: u64,
    pub queued: u32,
    /// Last ambient channel-RSSI sample, if any.
    pub rssi_dbm: Option<i16>,
    pub channel: Option<u8>,
}

/// `gateway/pairing` (retained).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Pairing {
    /// `idle` | `open`.
    pub state: String,
    /// Seconds until the open window closes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_s: Option<u64>,
    /// The node that joined (set on the final `idle` transition after a join).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joined: Option<String>,
}

/// `nodes/{addr}` (retained; an empty MQTT payload — not JSON — clears a removed node).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Node {
    pub addr: String,
    pub name: String,
    /// Device kind derived from the node's reported firmware name (e.g. `push-button`);
    /// empty until the first `NodeInfo` heartbeat.
    pub kind: String,
    pub sleeping: bool,
    /// No operator/auto name assigned yet.
    pub unnamed: bool,
    /// Seconds since the last uplink at publish time; `null` = never (since gateway boot).
    pub last_seen_s: Option<u32>,
    pub rssi_dbm: Option<i8>,
    pub uplinks: u32,
    /// Downlink items currently queued on the gateway.
    pub queued: u8,
}

/// `nodes/{addr}/event/button`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ButtonEvent {
    /// `press` | `release` | `click` | `hold`.
    pub event: String,
    /// The node's running count for this event kind since its boot.
    pub count: u32,
    /// Net-layer frame counter (dedup key for QoS-1 redelivery).
    pub counter: u32,
    pub ts: u64,
}

/// `nodes/{addr}/event/accel`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct AccelEvent {
    /// `motion` | `orientation`.
    pub event: String,
    /// Die face up 1..=6; 0 = unknown/moving.
    pub face: u8,
    pub counter: u32,
    pub ts: u64,
}

/// `nodes/{addr}/measure/temperature` (retained last value).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Temperature {
    pub celsius: f64,
    pub millic: i32,
    pub ts: u64,
}

/// `nodes/{addr}/uplink` — the raw decoded envelope (debug feed).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct UplinkDebug {
    pub counter: u32,
    pub rssi_dbm: i16,
    pub lqi: u8,
    pub len: usize,
    pub hex: String,
    pub ts: u64,
}

/// `nodes/{addr}/shell/req` (client → gateway): enqueue one remote-shell line.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ShellReq {
    /// Client-minted correlation id (uuid) echoed in every `ShellRsp` chunk.
    pub id: String,
    pub line: String,
    /// Queue TTL; 0/absent = the gateway default (3600 s).
    #[serde(default)]
    pub ttl_s: u16,
}

/// `nodes/{addr}/shell/rsp` (gateway → clients): one response chunk.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ShellRsp {
    pub id: String,
    pub chunk: u16,
    pub text: String,
    pub done: bool,
    /// Authoritative only when `done` (mirrors the shell-response discipline).
    pub result: u8,
    /// A chunk gap was detected — the text is incomplete.
    #[serde(default)]
    pub truncated: bool,
    /// Set on gateway-side failures (queue expiry, undeliverable) instead of a node reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One entry of `nodes/{addr}/shell/pending` (retained; whole queue mirrored per node).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct PendingEntry {
    /// The gateway's queue item id — the `nodes dequeue` handle.
    pub r#ref: u16,
    /// The originating request's correlation id.
    pub id: String,
    pub line: String,
}

/// `radio/rssi` — ambient channel sample.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct RadioRssi {
    pub dbm: i16,
    pub channel: u8,
    pub ts: u64,
}

/// `radio/rx` — one received packet (graph marker).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct RadioRx {
    pub src: String,
    pub rssi_dbm: i16,
    pub lqi: u8,
    pub len: usize,
    pub ts: u64,
}

/// `radio/tx` — one gateway TX attempt (graph marker + queue outcome).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct RadioTx {
    pub dest: String,
    /// Queue item id (0 = not a queue item).
    pub item: u16,
    /// `delivered` | `not-delivered` | `busy` | `duty-limited` | `error` | `expired`.
    pub outcome: String,
    pub ack_rssi_dbm: Option<i8>,
    pub ts: u64,
}

/// `gateway/cmd` — an RPC request from a client command.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct RpcRequest {
    /// Client-minted uuid; the response arrives on `gateway/rsp/{id}`.
    pub id: String,
    pub op: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// `gateway/rsp/{uuid}` — the RPC response.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct RpcResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// Map a `mgmt::TX_*` outcome code to its wire string.
pub(crate) fn tx_outcome_str(code: u8) -> &'static str {
    match code {
        tower_protocol::mgmt::TX_DELIVERED => "delivered",
        tower_protocol::mgmt::TX_NOT_DELIVERED => "not-delivered",
        tower_protocol::mgmt::TX_BUSY => "busy",
        tower_protocol::mgmt::TX_DUTY_LIMITED => "duty-limited",
        tower_protocol::mgmt::TX_EXPIRED => "expired",
        _ => "error",
    }
}

/// Map a `mgmt::MGMT_*` result code to a human error string (`None` = ok).
pub(crate) fn mgmt_error_str(code: u8) -> Option<&'static str> {
    use tower_protocol::mgmt as m;
    match code {
        m::MGMT_OK => None,
        m::MGMT_UNSUPPORTED => Some("operation not supported by this device"),
        m::MGMT_BAD_ARG => Some("invalid argument"),
        m::MGMT_NOT_FOUND => Some("not found"),
        m::MGMT_FULL => Some("capacity exhausted"),
        m::MGMT_BUSY => Some("busy (conflicting operation in flight)"),
        m::MGMT_STORAGE => Some("device storage error"),
        m::MGMT_TIMEOUT => Some("window expired"),
        _ => Some("unknown device error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden JSON strings — the public API. Field names/order are load-bearing for
    /// external subscribers; change them consciously (and bump `SCHEMA`).
    #[test]
    fn golden_status() {
        let s = Status {
            state: "online".into(),
            schema: SCHEMA,
            gateway: "0x0000ab12".into(),
            firmware: "radio_dongle_gateway".into(),
            firmware_version: "v0.1.0".into(),
            session_id: 7,
            protocol: 3,
            serial_port: "/dev/ttyACM0".into(),
            serial_up: true,
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"state":"online","schema":2,"gateway":"0x0000ab12","firmware":"radio_dongle_gateway","firmware_version":"v0.1.0","session_id":7,"protocol":3,"serial_port":"/dev/ttyACM0","serial_up":true}"#
        );
    }

    #[test]
    fn golden_button_event() {
        let e = ButtonEvent {
            event: "click".into(),
            count: 42,
            counter: 1234,
            ts: 1_700_000_000,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"click","count":42,"counter":1234,"ts":1700000000}"#
        );
    }

    #[test]
    fn golden_node() {
        let n = Node {
            addr: "0x0000ab12".into(),
            name: "kitchen".into(),
            kind: "push-button".into(),
            sleeping: true,
            unnamed: false,
            last_seen_s: Some(3),
            rssi_dbm: Some(-67),
            uplinks: 12,
            queued: 1,
        };
        assert_eq!(
            serde_json::to_string(&n).unwrap(),
            r#"{"addr":"0x0000ab12","name":"kitchen","kind":"push-button","sleeping":true,"unnamed":false,"last_seen_s":3,"rssi_dbm":-67,"uplinks":12,"queued":1}"#
        );
    }

    #[test]
    fn golden_shell_rsp() {
        let r = ShellRsp {
            id: "abc".into(),
            chunk: 0,
            text: "ok".into(),
            done: true,
            result: 0,
            truncated: false,
            error: None,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"id":"abc","chunk":0,"text":"ok","done":true,"result":0,"truncated":false}"#
        );
    }

    #[test]
    fn golden_stats() {
        // `Stats` has no `skip_serializing_if`: absent ambient samples serialize as `null`
        // (present as the value) — pin both so a subscriber sees stable keys.
        let s = Stats {
            uptime_s: 3600,
            nodes: 2,
            uplinks: 42,
            queued: 1,
            rssi_dbm: None,
            channel: None,
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"uptime_s":3600,"nodes":2,"uplinks":42,"queued":1,"rssi_dbm":null,"channel":null}"#
        );
        let s = Stats {
            rssi_dbm: Some(-98),
            channel: Some(3),
            ..s
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"uptime_s":3600,"nodes":2,"uplinks":42,"queued":1,"rssi_dbm":-98,"channel":3}"#
        );
    }

    #[test]
    fn golden_pairing() {
        // `remaining_s`/`joined` are `skip_serializing_if = Option::is_none`: the open window
        // carries the countdown, the idle state omits both keys entirely.
        let open = Pairing {
            state: "open".into(),
            remaining_s: Some(45),
            joined: None,
        };
        assert_eq!(
            serde_json::to_string(&open).unwrap(),
            r#"{"state":"open","remaining_s":45}"#
        );
        let idle = Pairing {
            state: "idle".into(),
            remaining_s: None,
            joined: None,
        };
        assert_eq!(serde_json::to_string(&idle).unwrap(), r#"{"state":"idle"}"#);
        let joined = Pairing {
            state: "idle".into(),
            remaining_s: None,
            joined: Some("0x0000ab12".into()),
        };
        assert_eq!(
            serde_json::to_string(&joined).unwrap(),
            r#"{"state":"idle","joined":"0x0000ab12"}"#
        );
    }

    #[test]
    fn golden_temperature() {
        let t = Temperature {
            celsius: 21.5,
            millic: 21500,
            ts: 1_700_000_000,
        };
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"{"celsius":21.5,"millic":21500,"ts":1700000000}"#
        );
    }

    #[test]
    fn golden_accel_event() {
        let e = AccelEvent {
            event: "orientation".into(),
            face: 3,
            counter: 7,
            ts: 1_700_000_000,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"orientation","face":3,"counter":7,"ts":1700000000}"#
        );
    }

    #[test]
    fn golden_uplink_debug() {
        let u = UplinkDebug {
            counter: 5,
            rssi_dbm: -67,
            lqi: 30,
            len: 3,
            hex: "aabbcc".into(),
            ts: 1_700_000_000,
        };
        assert_eq!(
            serde_json::to_string(&u).unwrap(),
            r#"{"counter":5,"rssi_dbm":-67,"lqi":30,"len":3,"hex":"aabbcc","ts":1700000000}"#
        );
    }

    #[test]
    fn golden_radio_rssi() {
        let r = RadioRssi {
            dbm: -98,
            channel: 0,
            ts: 1_700_000_000,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"dbm":-98,"channel":0,"ts":1700000000}"#
        );
    }

    #[test]
    fn golden_radio_rx() {
        let r = RadioRx {
            src: "0x0000ab12".into(),
            rssi_dbm: -67,
            lqi: 30,
            len: 3,
            ts: 1_700_000_000,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"src":"0x0000ab12","rssi_dbm":-67,"lqi":30,"len":3,"ts":1700000000}"#
        );
    }

    #[test]
    fn golden_radio_tx() {
        // `ack_rssi_dbm` has no skip: a missing ACK serializes as `null`.
        let t = RadioTx {
            dest: "0x0000ab12".into(),
            item: 9,
            outcome: "delivered".into(),
            ack_rssi_dbm: Some(-42),
            ts: 1_700_000_000,
        };
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"{"dest":"0x0000ab12","item":9,"outcome":"delivered","ack_rssi_dbm":-42,"ts":1700000000}"#
        );
        let t = RadioTx {
            ack_rssi_dbm: None,
            ..t
        };
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"{"dest":"0x0000ab12","item":9,"outcome":"delivered","ack_rssi_dbm":null,"ts":1700000000}"#
        );
    }

    #[test]
    fn golden_pending_entry() {
        // The `r#ref` field serializes as the bare key `ref` (its raw-identifier prefix is
        // a Rust keyword escape, not part of the wire name).
        let e = PendingEntry {
            r#ref: 9,
            id: "u-1".into(),
            line: "/led on".into(),
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"ref":9,"id":"u-1","line":"/led on"}"#
        );
    }

    #[test]
    fn shell_req_defaults() {
        let r: ShellReq = serde_json::from_str(r#"{"id":"x","line":"/led on"}"#).unwrap();
        assert_eq!(r.ttl_s, 0, "absent ttl_s defaults to 0 (gateway default)");
    }

    #[test]
    fn rpc_roundtrip() {
        let req: RpcRequest = serde_json::from_str(r#"{"id":"u1","op":"node_list"}"#).unwrap();
        assert_eq!(req.op, "node_list");
        assert!(req.params.is_null());

        let rsp = RpcResponse {
            id: "u1".into(),
            ok: false,
            error: Some("not found".into()),
            data: serde_json::Value::Null,
        };
        assert_eq!(
            serde_json::to_string(&rsp).unwrap(),
            r#"{"id":"u1","ok":false,"error":"not found"}"#
        );
    }

    #[test]
    fn outcome_strings() {
        assert_eq!(
            tx_outcome_str(tower_protocol::mgmt::TX_DELIVERED),
            "delivered"
        );
        assert_eq!(tx_outcome_str(tower_protocol::mgmt::TX_EXPIRED), "expired");
        assert_eq!(mgmt_error_str(tower_protocol::mgmt::MGMT_OK), None);
        assert!(mgmt_error_str(tower_protocol::mgmt::MGMT_FULL).is_some());
    }
}
