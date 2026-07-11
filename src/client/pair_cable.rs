//! Cable pairing (`tower nodes add --port <SERIAL>`): the one client flow that
//! touches BOTH transports — only this process can reach the node's USB port, so the
//! provisioning runs here, not on the gateway:
//!
//! 1. probe the node's port (`Describe` → role must be Node),
//! 2. learn the gateway's network parameters (RPC `describe` over MQTT),
//! 3. **mint the AES key host-side** (the only CSPRNG in the system),
//! 4. register the node on the gateway (RPC `node_add` with id+key),
//! 5. install the credentials on the node (`Provision` over its serial port — a
//!    typed frame, never a shell line, so the key stays out of history/logs);
//!    the node acks and reboots into its new identity.
//!
//! A failure at step 5 rolls the gateway registration back (step 4′), so no phantom
//! peer is left behind. Steps 1–3 touch nothing.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tower_protocol::FrameDecoder;
use tower_protocol::mgmt::{DeviceRole, MgmtOp, Provision};

use crate::gateway::topics;
use crate::mgmt::{DescribeOutcome, MgmtOutcome, describe, mgmt_roundtrip};
use crate::port::open_console;
use crate::render::warn_protocol_mismatch;
use crate::{EXIT_DEVICE_TIMEOUT, EXIT_OK, EXIT_PROTOCOL_MISMATCH, EXIT_WRONG_FIRMWARE};

use super::{Session, key_hex, mint_key};

pub(crate) fn run(s: &mut Session, node_port: &str, name: Option<&str>) -> Result<u8> {
    // 1. Probe the node.
    eprintln!("[tower] probing {node_port}…");
    let mut sp = open_console(node_port, false)?;
    let mut dec = FrameDecoder::new();
    let node_info = match describe(&mut *sp, &mut dec, 0x7100, Duration::from_secs(4)) {
        DescribeOutcome::Info(info) => info,
        DescribeOutcome::Refused(code) => {
            eprintln!("[tower] {node_port}: refused the role probe (code {code})");
            return Ok(EXIT_WRONG_FIRMWARE);
        }
        DescribeOutcome::Timeout {
            bad_version: Some(got),
        } => {
            warn_protocol_mismatch(got);
            return Ok(EXIT_PROTOCOL_MISMATCH);
        }
        DescribeOutcome::Timeout { bad_version: None } => {
            eprintln!("[tower] {node_port}: no answer — is this a v3 TOWER node?");
            return Ok(EXIT_DEVICE_TIMEOUT);
        }
    };
    if node_info.role != DeviceRole::Node {
        eprintln!(
            "[tower] {node_port} runs \"{}\" (role {:?}) — that's not a node; use the gateway's own port with `tower gateway`",
            node_info.firmware_name, node_info.role
        );
        return Ok(EXIT_WRONG_FIRMWARE);
    }
    let node_id = node_info.net_id;
    eprintln!(
        "[tower] node {} (\"{}\"){}",
        topics::node_hex(node_id),
        node_info.firmware_name,
        if node_info.provisioned {
            " — already provisioned, re-pairing"
        } else {
            ""
        }
    );

    // 2. The gateway's network parameters (band/channel/id).
    let gw = s.rpc_ok("describe", serde_json::Value::Null)?;
    let gw_hex = gw
        .get("gateway")
        .and_then(|v| v.as_str())
        .context("gateway describe: missing id")?
        .to_string();
    let gw_id = topics::parse_node_hex(&gw_hex).context("gateway describe: bad id")?;
    let band = gw.get("band").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
    let channel = gw.get("channel").and_then(|v| v.as_u64()).unwrap_or(0) as u8;

    // 3. Mint the key.
    let key = mint_key()?;

    // 4. Register on the gateway first (a node provisioned toward a gateway that
    //    doesn't know it would uplink into rejection).
    s.rpc_ok(
        "node_add",
        serde_json::json!({
            "id": topics::node_hex(node_id),
            "key": key_hex(&key),
            "name": name.unwrap_or(""),
            "sleeping": true,
        }),
    )
    .context("registering the node on the gateway")?;

    // 5. Install the credentials on the node. It reboots on success.
    let outcome = mgmt_roundtrip(
        &mut *sp,
        &mut dec,
        0x7101,
        &MgmtOp::Provision(Provision {
            my_id: None,
            gw_id,
            key,
            band,
            channel,
        }),
        Duration::from_secs(4),
    );
    match outcome {
        MgmtOutcome::Reply(r) if r.result == tower_protocol::mgmt::MGMT_OK => {
            println!(
                "paired {} → gateway {gw_hex}{}",
                topics::node_hex(node_id),
                name.map(|n| format!(" as \"{n}\"")).unwrap_or_default()
            );
            eprintln!("[tower] node is rebooting into its new identity");
            Ok(EXIT_OK)
        }
        other => {
            // Roll the gateway registration back — no phantom peers.
            let _ = s.rpc(
                "node_remove",
                serde_json::json!({ "node": topics::node_hex(node_id) }),
            );
            match other {
                MgmtOutcome::Reply(r) => {
                    bail!(
                        "node refused provisioning ({}) — gateway registration rolled back",
                        crate::gateway::payload::mgmt_error_str(r.result).unwrap_or("unknown")
                    )
                }
                MgmtOutcome::Timeout { .. } => {
                    bail!("node did not ack provisioning — gateway registration rolled back")
                }
            }
        }
    }
}
