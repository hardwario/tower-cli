//! `tower nodes …` — node management against a running gateway, over MQTT.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use rumqttc::{Event as MqttEvent, Incoming, QoS};

use crate::gateway::payload::{Node, PendingEntry, ShellReq, ShellRsp};
use crate::gateway::topics;
use crate::{EXIT_DEVICE_TIMEOUT, EXIT_ERROR, EXIT_OK};

use super::{MqttOpts, RpcTimeout, Session, connect};

#[derive(Subcommand, Debug)]
pub(crate) enum NodesCmd {
    /// List the gateway's registered nodes.
    List {
        /// Emit the raw JSON registry instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show one node (add --keys to reveal its AES key).
    Show {
        /// Node id (0xHHHHHHHH) or name.
        node: String,
        /// Also reveal the node's AES key (it is never shown otherwise).
        #[arg(long)]
        keys: bool,
        #[arg(long)]
        json: bool,
    },
    /// Pair a new node: over the air (--ota) or over its USB cable (--port).
    Add {
        /// Open the gateway's OTA pairing window (put the node in join mode: hold
        /// its button).
        #[arg(long, conflicts_with = "port")]
        ota: bool,
        /// Cable pairing: the serial port of the NODE to provision.
        #[arg(long, value_name = "SERIAL")]
        port: Option<String>,
        /// OTA window length in seconds.
        #[arg(long, value_name = "S", default_value_t = 60, requires = "ota")]
        window: u16,
        /// Friendly name to assign (default: auto-named from the node's firmware).
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove (unpair) a node.
    Remove { node: String },
    /// Rename a node.
    Rename { node: String, name: String },
    /// Run a remote shell command on a node (queued until it wakes).
    Shell {
        node: String,
        /// The command line, e.g. "/system settings set temp-period=30".
        line: String,
        /// Queue TTL in seconds (0 = gateway default, 1 h).
        #[arg(long, value_name = "S", default_value_t = 0)]
        ttl: u16,
        /// Idle wait for the node's reply in ms (sleeping nodes answer on their
        /// next wake — size this to the node's heartbeat if it rarely stirs).
        #[arg(long, value_name = "MS", default_value_t = 120_000)]
        timeout: u64,
        /// Enqueue and exit without waiting for the reply.
        #[arg(long)]
        no_wait: bool,
    },
    /// List a node's queued (not yet delivered) remote commands.
    Pending {
        node: String,
        #[arg(long)]
        json: bool,
    },
    /// Drop a queued remote command by its ref (see `nodes pending`).
    Dequeue { node: String, r#ref: u16 },
}

/// Map an anyhow error to the exit-code contract (RpcTimeout → 124).
fn exit_of(e: &anyhow::Error) -> u8 {
    if e.downcast_ref::<RpcTimeout>().is_some() {
        EXIT_DEVICE_TIMEOUT
    } else {
        EXIT_ERROR
    }
}

pub(crate) fn run(cmd: NodesCmd, opts: MqttOpts) -> Result<u8> {
    match dispatch(cmd, &opts) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("[tower] error: {e:#}");
            Ok(exit_of(&e))
        }
    }
}

fn dispatch(cmd: NodesCmd, opts: &MqttOpts) -> Result<u8> {
    let mut s = connect(opts)?;
    match cmd {
        NodesCmd::List { json } => {
            let nodes = node_list(&mut s)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&nodes)?);
            } else {
                print_table(&nodes);
            }
            Ok(EXIT_OK)
        }
        NodesCmd::Show { node, keys, json } => {
            let id = s.resolve_node(&node)?;
            let nodes = node_list(&mut s)?;
            let Some(n) = nodes.iter().find(|n| n.addr == topics::node_hex(id)) else {
                bail!("node vanished from the registry");
            };
            let key = if keys {
                let data = s.rpc_ok(
                    "reveal_key",
                    serde_json::json!({ "addr": topics::node_hex(id) }),
                )?;
                data.get("key").and_then(|v| v.as_str()).map(String::from)
            } else {
                None
            };
            if json {
                let mut v = serde_json::to_value(n)?;
                if let Some(k) = &key {
                    v["key"] = serde_json::Value::String(k.clone());
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_table(std::slice::from_ref(n));
                if let Some(k) = &key {
                    eprintln!("[tower] key material follows — treat this line as a secret");
                    println!("key: {k}");
                }
            }
            Ok(EXIT_OK)
        }
        NodesCmd::Add {
            ota: _,
            port: Some(port),
            name,
            ..
        } => super::pair_cable::run(&mut s, &port, name.as_deref()),
        NodesCmd::Add {
            ota: true,
            port: None,
            window,
            name,
        } => add_ota(&mut s, window, name.as_deref()),
        NodesCmd::Add { .. } => bail!("pick a pairing path: --ota or --port <SERIAL>"),
        NodesCmd::Remove { node } => {
            let id = s.resolve_node(&node)?;
            s.rpc_ok(
                "node_remove",
                serde_json::json!({ "addr": topics::node_hex(id) }),
            )?;
            println!("removed {}", topics::node_hex(id));
            Ok(EXIT_OK)
        }
        NodesCmd::Rename { node, name } => {
            let id = s.resolve_node(&node)?;
            s.rpc_ok(
                "node_rename",
                serde_json::json!({ "addr": topics::node_hex(id), "name": name }),
            )?;
            println!("renamed {} to \"{name}\"", topics::node_hex(id));
            Ok(EXIT_OK)
        }
        NodesCmd::Shell {
            node,
            line,
            ttl,
            timeout,
            no_wait,
        } => shell(
            &mut s,
            &node,
            &line,
            ttl,
            Duration::from_millis(timeout),
            no_wait,
        ),
        NodesCmd::Pending { node, json } => {
            let id = s.resolve_node(&node)?;
            let raw = s
                .read_retained(
                    &topics::node_shell_pending(&s.prefix.clone(), id),
                    Duration::from_secs(2),
                )?
                .unwrap_or_else(|| b"[]".to_vec());
            let entries: Vec<PendingEntry> = serde_json::from_slice(&raw).unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("no queued commands");
            } else {
                println!("{:<6} {:<38} LINE", "REF", "ID");
                for e in &entries {
                    println!("{:<6} {:<38} {}", e.r#ref, e.id, e.line);
                }
            }
            Ok(EXIT_OK)
        }
        NodesCmd::Dequeue { node, r#ref } => {
            let id = s.resolve_node(&node)?;
            s.rpc_ok(
                "queue_drop",
                serde_json::json!({ "addr": topics::node_hex(id), "ref": r#ref }),
            )?;
            println!("dequeued #{ref}", ref = r#ref);
            Ok(EXIT_OK)
        }
    }
}

fn node_list(s: &mut Session) -> Result<Vec<Node>> {
    let data = s.rpc_ok("node_list", serde_json::Value::Null)?;
    Ok(serde_json::from_value(data)?)
}

fn print_table(nodes: &[Node]) {
    if nodes.is_empty() {
        println!("no nodes paired (try `tower nodes add --ota` or `--port <SERIAL>`)");
        return;
    }
    println!(
        "{:<12} {:<16} {:<12} {:>9} {:>6} {:>8} {:>5} {:<5}",
        "ADDR", "NAME", "TYPE", "LAST-SEEN", "RSSI", "UPLINKS", "PEND", "SLEEP"
    );
    for n in nodes {
        println!(
            "{:<12} {:<16} {:<12} {:>9} {:>6} {:>8} {:>5} {:<5}",
            n.addr,
            if n.name.is_empty() { "—" } else { &n.name },
            if n.kind.is_empty() { "?" } else { &n.kind },
            n.last_seen_s
                .map(|s| format!("{s}s"))
                .unwrap_or_else(|| "never".into()),
            n.rssi_dbm
                .map(|r| format!("{r}dBm"))
                .unwrap_or_else(|| "—".into()),
            n.uplinks,
            n.queued,
            if n.sleeping { "yes" } else { "no" },
        );
    }
}

fn add_ota(s: &mut Session, window: u16, name: Option<&str>) -> Result<u8> {
    eprintln!("[tower] pairing window open for {window}s — hold the node's button to join…");
    // The RPC resolves when the window does (join / expiry), so give it the window
    // plus slack over the generic RPC timeout.
    let saved = std::mem::replace(&mut s.timeout, Duration::from_secs(window as u64 + 10));
    let rsp = s.rpc("node_add_ota", serde_json::json!({ "window_s": window }));
    s.timeout = saved;
    let data = match rsp {
        Ok(r) if r.ok => r.data,
        Ok(r) => bail!("{}", r.error.unwrap_or_else(|| "gateway refused".into())),
        Err(e) => return Err(e),
    };
    match data.get("joined").and_then(|v| v.as_str()) {
        Some(hex) => {
            println!("paired {hex}");
            if let Some(n) = name {
                s.rpc_ok("node_rename", serde_json::json!({ "addr": hex, "name": n }))?;
                println!("named \"{n}\"");
            }
            Ok(EXIT_OK)
        }
        None => {
            eprintln!("[tower] window expired — no node joined");
            Ok(EXIT_ERROR)
        }
    }
}

fn shell(
    s: &mut Session,
    node: &str,
    line: &str,
    ttl: u16,
    timeout: Duration,
    no_wait: bool,
) -> Result<u8> {
    let id = s.resolve_node(node)?;
    let req_id = uuid::Uuid::new_v4().to_string();
    let rsp_topic = topics::node_shell_rsp(&s.prefix.clone(), id);
    if !no_wait {
        s.client
            .subscribe(&rsp_topic, QoS::AtLeastOnce)
            .context("subscribing to the response topic")?;
    }
    s.client
        .publish(
            topics::node_shell_req(&s.prefix.clone(), id),
            QoS::AtLeastOnce,
            false,
            serde_json::to_vec(&ShellReq {
                id: req_id.clone(),
                line: line.to_string(),
                ttl_s: ttl,
            })?,
        )
        .context("publishing the shell request")?;
    if no_wait {
        println!(
            "queued for {} (reply will publish on {rsp_topic})",
            topics::node_hex(id)
        );
        return Ok(EXIT_OK);
    }
    eprintln!("[tower] queued — waiting for the node to wake (Ctrl-C to stop waiting)…");
    let mut result = 0u8;
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!("[tower] no reply within the wait window (the command stays queued)");
            return Ok(EXIT_DEVICE_TIMEOUT);
        }
        match s.connection.recv_timeout(remaining) {
            Ok(Ok(MqttEvent::Incoming(Incoming::Publish(p)))) if p.topic == rsp_topic => {
                let Ok(rsp) = serde_json::from_slice::<ShellRsp>(&p.payload) else {
                    continue;
                };
                if rsp.id != req_id {
                    continue; // someone else's dialog with the same node
                }
                if let Some(err) = &rsp.error {
                    eprintln!("[tower] {err}");
                    return Ok(EXIT_ERROR);
                }
                print!("{}", rsp.text);
                if rsp.truncated {
                    eprintln!("[tower] response truncated (chunk lost over the air)");
                    result = result.max(1);
                }
                if rsp.done {
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                    // Clamp like `tower exec`: device results own 1..=123.
                    return Ok(match rsp.result.max(result) {
                        0 => EXIT_OK,
                        r => r.clamp(1, 123),
                    });
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => bail!("broker connection lost: {e}"),
            Err(_) => continue, // recv_timeout tick — loop decides via the deadline
        }
    }
}
