//! The gateway's MQTT **clients**: `tower nodes …` and `tower net …` talk to any
//! running `tower gateway` (local TUI or remote service) through the broker — never
//! the serial port (except `nodes add --port`, which additionally drives the *node's*
//! port for cable provisioning). One RPC = subscribe `gateway/rsp/{uuid}`, publish
//! `gateway/cmd`, wait. Synchronous rumqttc throughout.

pub(crate) mod net;
pub(crate) mod nodes;
pub(crate) mod pair_cable;

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use rumqttc::{Client, Connection, Event as MqttEvent, Incoming, MqttOptions, QoS};

use crate::gateway::payload::{RpcRequest, RpcResponse};
use crate::gateway::topics;

/// Shared broker flags for every client command.
#[derive(Args, Debug, Clone)]
pub(crate) struct MqttOpts {
    /// MQTT broker (where a `tower gateway` is bridging).
    #[arg(
        long,
        value_name = "HOST[:PORT]",
        default_value = "localhost:1883",
        global = true
    )]
    pub mqtt: String,
    /// Broker username.
    #[arg(long, global = true)]
    pub mqtt_user: Option<String>,
    /// Broker password (prefer the TOWER_MQTT_PASSWORD env var).
    #[arg(
        long,
        env = "TOWER_MQTT_PASSWORD",
        hide_env_values = true,
        global = true
    )]
    pub mqtt_password: Option<String>,
    /// MQTT topic prefix (must match the gateway's).
    #[arg(long, default_value = "tower/", global = true)]
    pub prefix: String,
    /// Gateway response timeout in ms.
    #[arg(long, value_name = "MS", default_value_t = 5000, global = true)]
    pub timeout: u64,
}

impl MqttOpts {
    pub(crate) fn prefix(&self) -> String {
        topics::normalize_prefix(&self.prefix)
    }
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout)
    }
}

/// A connected client session.
pub(crate) struct Session {
    pub client: Client,
    pub connection: Connection,
    pub prefix: String,
    timeout: Duration,
}

/// The gateway didn't answer an RPC in time — exit 124 territory (mirrors the serial
/// device-timeout code so scripts branch the same way on both transports).
#[derive(Debug)]
pub(crate) struct RpcTimeout;

impl std::fmt::Display for RpcTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no gateway response (is `tower gateway` running against this broker?)")
    }
}
impl std::error::Error for RpcTimeout {}

pub(crate) fn connect(opts: &MqttOpts) -> Result<Session> {
    let (host, port) = match opts.mqtt.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(1883))
        }
        _ => (opts.mqtt.clone(), 1883),
    };
    let mut mo = MqttOptions::new(
        format!("tower-cli-{}", std::process::id()),
        host.clone(),
        port,
    );
    mo.set_keep_alive(Duration::from_secs(15));
    if let (Some(u), Some(p)) = (&opts.mqtt_user, &opts.mqtt_password) {
        mo.set_credentials(u.clone(), p.clone());
    }
    let (client, mut connection) = Client::new(mo, 32);
    // Wait for the ConnAck so a refused/unreachable broker fails fast and clean.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match connection.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(MqttEvent::Incoming(Incoming::ConnAck(_)))) => break,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => bail!("broker {host}:{port}: {e}"),
            Err(_) => bail!("broker {host}:{port}: connect timeout"),
        }
    }
    Ok(Session {
        client,
        connection,
        prefix: opts.prefix(),
        timeout: opts.timeout(),
    })
}

impl Session {
    /// One RPC round-trip. `Err(RpcTimeout)` is downcastable for the 124 exit path.
    pub(crate) fn rpc(&mut self, op: &str, params: serde_json::Value) -> Result<RpcResponse> {
        let id = uuid::Uuid::new_v4().to_string();
        let rsp_topic = topics::gateway_rsp(&self.prefix, &id);
        self.client
            .subscribe(&rsp_topic, QoS::AtLeastOnce)
            .context("subscribing to the response topic")?;
        self.client
            .publish(
                topics::gateway_cmd(&self.prefix),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&RpcRequest {
                    id: id.clone(),
                    op: op.into(),
                    params,
                })?,
            )
            .context("publishing the request")?;
        let deadline = Instant::now() + self.timeout;
        let rsp = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RpcTimeout.into());
            }
            match self.connection.recv_timeout(remaining) {
                Ok(Ok(MqttEvent::Incoming(Incoming::Publish(p)))) if p.topic == rsp_topic => {
                    match serde_json::from_slice::<RpcResponse>(&p.payload) {
                        Ok(r) if r.id == id => break r,
                        _ => continue,
                    }
                }
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => bail!("broker connection lost: {e}"),
                Err(_) => return Err(RpcTimeout.into()),
            }
        };
        let _ = self.client.unsubscribe(&rsp_topic);
        Ok(rsp)
    }

    /// An RPC that must succeed: `ok:false` becomes an `Err` with the gateway's message.
    pub(crate) fn rpc_ok(
        &mut self,
        op: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let rsp = self.rpc(op, params)?;
        if rsp.ok {
            Ok(rsp.data)
        } else {
            bail!("{}", rsp.error.unwrap_or_else(|| "gateway refused".into()))
        }
    }

    /// Read one retained message from `topic` (None = nothing retained there).
    pub(crate) fn read_retained(
        &mut self,
        topic: &str,
        window: Duration,
    ) -> Result<Option<Vec<u8>>> {
        self.client
            .subscribe(topic, QoS::AtLeastOnce)
            .context("subscribing")?;
        let deadline = Instant::now() + window;
        let got = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break None;
            }
            match self.connection.recv_timeout(remaining) {
                Ok(Ok(MqttEvent::Incoming(Incoming::Publish(p)))) if p.topic == topic => {
                    break Some(p.payload.to_vec());
                }
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => bail!("broker connection lost: {e}"),
                Err(_) => break None,
            }
        };
        let _ = self.client.unsubscribe(topic);
        Ok(got)
    }

    /// Resolve a `<node>` argument (canonical hex or friendly name) to its id, via
    /// the gateway's authoritative registry.
    pub(crate) fn resolve_node(&mut self, arg: &str) -> Result<u32> {
        if let Some(id) = topics::parse_node_hex(arg) {
            return Ok(id);
        }
        let data = self.rpc_ok("node_list", serde_json::Value::Null)?;
        let nodes: Vec<crate::gateway::payload::Node> = serde_json::from_value(data)?;
        for n in &nodes {
            if n.name == arg
                && let Some(id) = topics::parse_node_hex(&n.id)
            {
                return Ok(id);
            }
        }
        bail!(
            "no node named \"{arg}\" (known: {})",
            nodes
                .iter()
                .map(|n| if n.name.is_empty() {
                    n.id.clone()
                } else {
                    n.name.clone()
                })
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Format an id/hex-key pair for terminal display.
pub(crate) fn key_hex(key: &[u8; 16]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// Mint a fresh AES-128 node key — host-side CSPRNG, the only legitimate key source
/// in the ecosystem (the device PRNGs are deterministic).
pub(crate) fn mint_key() -> Result<[u8; 16]> {
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).context("no entropy source for the node key")?;
    Ok(key)
}
