//! MQTT topic construction/parsing — the single source of truth for the gateway's
//! topic tree (the public API surface of `tower gateway`). Everything lives under a
//! configurable prefix (default `tower/`); node topics address nodes by **fixed hex
//! id** (`nodes/0x0000ab12/…`) — names are mutable metadata and live only in payloads.
//!
//! | topic (under prefix)            | dir | retain | payload (`payload.rs`)        |
//! |---------------------------------|-----|--------|-------------------------------|
//! | `gateway/status`                | gw→ | yes+LWT| `Status`                      |
//! | `gateway/stats`                 | gw→ | yes    | `Stats`                       |
//! | `gateway/pairing`               | gw→ | yes    | `Pairing`                     |
//! | `gateway/cmd`                   | →gw | no     | `RpcRequest`                  |
//! | `gateway/rsp/{uuid}`            | gw→ | no     | `RpcResponse`                 |
//! | `nodes/{id}`                    | gw→ | yes    | `Node` (empty = removed)      |
//! | `nodes/{id}/event/button`       | gw→ | no     | `ButtonEvent`                 |
//! | `nodes/{id}/event/accel`        | gw→ | no     | `AccelEvent`                  |
//! | `nodes/{id}/measure/temperature`| gw→ | yes    | `Temperature`                 |
//! | `nodes/{id}/uplink`             | gw→ | no     | `UplinkDebug`                 |
//! | `nodes/{id}/shell/req`          | →gw | no     | `ShellReq`                    |
//! | `nodes/{id}/shell/rsp`          | gw→ | no     | `ShellRsp`                    |
//! | `nodes/{id}/shell/pending`      | gw→ | yes    | `Pending`                     |
//! | `radio/rssi`                    | gw→ | no     | `RadioRssi`                   |
//! | `radio/rx` / `radio/tx`         | gw→ | no     | `RadioRx` / `RadioTx`         |

/// Normalize a user-supplied prefix: trailing `/` guaranteed, empty allowed ("" = no
/// prefix). `tower` and `tower/` both become `tower/`.
pub(crate) fn normalize_prefix(prefix: &str) -> String {
    let p = prefix.trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

/// Canonical hex form of a node id, as used in topics: `0x` + 8 lowercase hex digits.
pub(crate) fn node_hex(id: u32) -> String {
    format!("0x{id:08x}")
}

/// Parse the canonical hex form (strict: exactly what [`node_hex`] emits, case-insensitive).
pub(crate) fn parse_node_hex(s: &str) -> Option<u32> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if hex.len() != 8 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

pub(crate) fn gateway_status(prefix: &str) -> String {
    format!("{prefix}gateway/status")
}
pub(crate) fn gateway_stats(prefix: &str) -> String {
    format!("{prefix}gateway/stats")
}
pub(crate) fn gateway_pairing(prefix: &str) -> String {
    format!("{prefix}gateway/pairing")
}
pub(crate) fn gateway_cmd(prefix: &str) -> String {
    format!("{prefix}gateway/cmd")
}
pub(crate) fn gateway_rsp(prefix: &str, rpc_id: &str) -> String {
    format!("{prefix}gateway/rsp/{rpc_id}")
}
pub(crate) fn node(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}", node_hex(id))
}
pub(crate) fn node_button(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}/event/button", node_hex(id))
}
pub(crate) fn node_accel(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}/event/accel", node_hex(id))
}
pub(crate) fn node_temperature(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}/measure/temperature", node_hex(id))
}
pub(crate) fn node_uplink(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}/uplink", node_hex(id))
}
pub(crate) fn node_shell_req(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}/shell/req", node_hex(id))
}
pub(crate) fn node_shell_rsp(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}/shell/rsp", node_hex(id))
}
pub(crate) fn node_shell_pending(prefix: &str, id: u32) -> String {
    format!("{prefix}nodes/{}/shell/pending", node_hex(id))
}
pub(crate) fn radio_rssi(prefix: &str) -> String {
    format!("{prefix}radio/rssi")
}
pub(crate) fn radio_rx(prefix: &str) -> String {
    format!("{prefix}radio/rx")
}
pub(crate) fn radio_tx(prefix: &str) -> String {
    format!("{prefix}radio/tx")
}

/// What an inbound (subscribed) topic means to the gateway engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inbound {
    /// `gateway/cmd` — an RPC request.
    Cmd,
    /// `nodes/{id}/shell/req` — a remote-shell enqueue for `id`.
    ShellReq(u32),
    /// Anything else (our own publishes echoed back, foreign topics) — ignore.
    Other,
}

/// Classify an inbound topic (the engine subscribes to `gateway/cmd` and
/// `nodes/+/shell/req`).
pub(crate) fn classify(prefix: &str, topic: &str) -> Inbound {
    let Some(rest) = topic.strip_prefix(prefix) else {
        return Inbound::Other;
    };
    if rest == "gateway/cmd" {
        return Inbound::Cmd;
    }
    if let Some(tail) = rest.strip_prefix("nodes/")
        && let Some(hex) = tail.strip_suffix("/shell/req")
        && let Some(id) = parse_node_hex(hex)
    {
        return Inbound::ShellReq(id);
    }
    Inbound::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_normalization() {
        assert_eq!(normalize_prefix("tower"), "tower/");
        assert_eq!(normalize_prefix("tower/"), "tower/");
        assert_eq!(normalize_prefix("a/b"), "a/b/");
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
    }

    #[test]
    fn node_hex_roundtrip() {
        assert_eq!(node_hex(0xAB12), "0x0000ab12");
        assert_eq!(parse_node_hex("0x0000ab12"), Some(0xAB12));
        assert_eq!(parse_node_hex("0X0000AB12"), Some(0xAB12));
        assert_eq!(parse_node_hex("0xab12"), None, "must be 8 digits");
        assert_eq!(parse_node_hex("0000ab12"), None, "must carry 0x");
        assert_eq!(parse_node_hex("0x0000zz12"), None);
    }

    #[test]
    fn topic_shapes() {
        let p = "tower/";
        assert_eq!(gateway_status(p), "tower/gateway/status");
        assert_eq!(node(p, 1), "tower/nodes/0x00000001");
        assert_eq!(node_shell_req(p, 0xAB), "tower/nodes/0x000000ab/shell/req");
        assert_eq!(gateway_rsp(p, "xyz"), "tower/gateway/rsp/xyz");
        // No prefix works too (empty prefix = broker root).
        assert_eq!(gateway_cmd(""), "gateway/cmd");
    }

    #[test]
    fn classify_inbound() {
        let p = "tower/";
        assert_eq!(classify(p, "tower/gateway/cmd"), Inbound::Cmd);
        assert_eq!(
            classify(p, "tower/nodes/0x0000ab12/shell/req"),
            Inbound::ShellReq(0xAB12)
        );
        assert_eq!(
            classify(p, "tower/nodes/0x0000ab12/shell/rsp"),
            Inbound::Other
        );
        assert_eq!(classify(p, "other/gateway/cmd"), Inbound::Other);
        assert_eq!(classify(p, "tower/nodes/garbage/shell/req"), Inbound::Other);
    }
}
