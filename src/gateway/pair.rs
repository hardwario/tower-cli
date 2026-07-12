//! Cable pairing from inside the gateway TUI — the same five-step flow as
//! `tower nodes add --port` (`client/pair_cable.rs`), reshaped for the bridge
//! process: the node's serial port is driven directly on a worker thread, while
//! the gateway-side steps (`node_add`, rollback `node_remove`) ride the engine's
//! own in-process RPC surface — the exact code path an MQTT client would hit.
//!
//! Wiring: the worker sends `FrontendCmd::Rpc` with `tui-cbl-*` ids into the
//! engine; the TUI event loop forwards the matching `Event::Rpc` responses back
//! over `rpc_rx`. Progress and the terminal outcome stream over `progress`.
//! The gateway's network parameters come from its startup `Describe` (settings
//! are boot-applied, so they cannot drift while the bridge runs).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use tower_protocol::FrameDecoder;
use tower_protocol::mgmt::{self, DeviceRole, MgmtOp, Provision};

use super::engine::{FrontendCmd, Input};
use super::payload::{RpcRequest, RpcResponse};
use super::topics;
use crate::client::{key_hex, mint_key};
use crate::mgmt::{DescribeOutcome, MgmtOutcome, describe, mgmt_roundtrip};
use crate::port::open_console;

/// The gateway's network parameters a provisioned node must be told.
#[derive(Clone, Copy)]
pub(crate) struct GwParams {
    pub addr: u32,
    pub band: u8,
    pub channel: u8,
}

/// Worker → TUI progress stream.
pub(crate) enum CableMsg {
    /// One human-readable step line for the modal.
    Progress(String),
    /// Paired — the node's address (the modal closes; the log records it).
    Done { addr: u32 },
    /// Terminal failure — shown in the modal until dismissed.
    Failed(String),
}

/// Distinct RPC ids across workers (a retry must never collide with a stale reply).
static RPC_NONCE: AtomicU64 = AtomicU64::new(0);

/// Spawn the cable-pairing worker for `node_port`. Detached: the thread owns its
/// channel ends and simply runs the flow to a terminal `Done`/`Failed` message.
pub(crate) fn spawn(
    node_port: String,
    gw: GwParams,
    input: Sender<Input>,
    rpc_rx: Receiver<RpcResponse>,
    progress: Sender<CableMsg>,
) {
    std::thread::Builder::new()
        .name("cable-pair".into())
        .spawn(move || {
            let outcome = run(&node_port, gw, &input, &rpc_rx, &progress);
            let _ = match outcome {
                Ok(addr) => progress.send(CableMsg::Done { addr }),
                Err(e) => progress.send(CableMsg::Failed(e)),
            };
        })
        .ok();
}

/// One engine RPC round-trip (send via the frontend surface, await the forwarded
/// response). The 5 s ceiling covers the engine's own device timeout.
fn rpc(
    input: &Sender<Input>,
    rpc_rx: &Receiver<RpcResponse>,
    op: &str,
    params: serde_json::Value,
) -> Result<RpcResponse, String> {
    let id = format!("tui-cbl-{}", RPC_NONCE.fetch_add(1, Ordering::Relaxed));
    input
        .send(Input::Frontend(FrontendCmd::Rpc(RpcRequest {
            id: id.clone(),
            op: op.into(),
            params,
        })))
        .map_err(|_| "engine gone".to_string())?;
    // Drain until OUR id answers (a stale reply from an abandoned worker is skipped).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rpc_rx.recv_timeout(remaining) {
            Ok(rsp) if rsp.id == id => return Ok(rsp),
            Ok(_stale) => continue,
            Err(_) => return Err(format!("gateway did not answer {op}")),
        }
    }
}

fn run(
    node_port: &str,
    gw: GwParams,
    input: &Sender<Input>,
    rpc_rx: &Receiver<RpcResponse>,
    progress: &Sender<CableMsg>,
) -> Result<u32, String> {
    let say = |s: String| {
        let _ = progress.send(CableMsg::Progress(s));
    };

    // 1. Probe the node's port (role must be Node).
    say(format!("probing {node_port}…"));
    let mut sp = open_console(node_port, false).map_err(|e| format!("open {node_port}: {e}"))?;
    let mut dec = FrameDecoder::new();
    let node_info = match describe(&mut *sp, &mut dec, 0x7100, Duration::from_secs(4)) {
        DescribeOutcome::Info(info) => info,
        DescribeOutcome::Refused(code) => return Err(format!("role probe refused (code {code})")),
        DescribeOutcome::Timeout {
            bad_version: Some(got),
        } => {
            return Err(format!(
                "protocol mismatch: device speaks v{got} — rebuild/reflash"
            ));
        }
        DescribeOutcome::Timeout { bad_version: None } => {
            return Err("no answer — is this a v3 TOWER node?".into());
        }
    };
    if node_info.role != DeviceRole::Node {
        return Err(format!(
            "\"{}\" is a {:?}, not a node",
            node_info.firmware_name, node_info.role
        ));
    }
    let node_addr = node_info.addr;
    say(format!(
        "node {} (\"{}\"){}",
        topics::node_hex(node_addr),
        node_info.firmware_name,
        if node_info.provisioned {
            " — already provisioned, re-pairing"
        } else {
            ""
        }
    ));

    // 2. Mint the AES key (host CSPRNG — the only legitimate key source).
    let key = mint_key().map_err(|e| e.to_string())?;

    // 3. Register on the gateway FIRST (a node provisioned toward a gateway that
    //    doesn't know it would uplink into rejection).
    say("registering on the gateway…".into());
    let rsp = rpc(
        input,
        rpc_rx,
        "node_add",
        serde_json::json!({
            "addr": topics::node_hex(node_addr),
            "key": key_hex(&key),
            "name": "",
            "sleeping": true,
        }),
    )?;
    if !rsp.ok {
        return Err(rsp
            .error
            .unwrap_or_else(|| "gateway refused node_add".into()));
    }

    // 4. Install the credentials on the node — it acks and reboots. A failure
    //    rolls the gateway registration back (no phantom peers).
    say("provisioning the node…".into());
    let outcome = mgmt_roundtrip(
        &mut *sp,
        &mut dec,
        0x7101,
        &MgmtOp::Provision(Provision {
            addr: None,
            gw_addr: gw.addr,
            key,
            band: gw.band,
            channel: gw.channel,
        }),
        Duration::from_secs(4),
    );
    match outcome {
        MgmtOutcome::Reply(r) if r.result == mgmt::MGMT_OK => Ok(node_addr),
        other => {
            let _ = rpc(
                input,
                rpc_rx,
                "node_remove",
                serde_json::json!({ "addr": topics::node_hex(node_addr) }),
            );
            Err(match other {
                MgmtOutcome::Reply(r) => format!(
                    "node refused provisioning ({}) — registration rolled back",
                    super::payload::mgmt_error_str(r.result).unwrap_or("unknown")
                ),
                MgmtOutcome::Timeout { .. } => {
                    "node did not ack provisioning — registration rolled back".into()
                }
            })
        }
    }
}
