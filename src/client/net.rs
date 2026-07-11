//! `tower net …` — gateway/network health, over MQTT (retained-topic reads; scripts
//! get a liveness exit code without owning any serial port).

use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;

use crate::gateway::payload::{self, Stats, Status};
use crate::gateway::topics;
use crate::{EXIT_ERROR, EXIT_OK, EXIT_PROTOCOL_MISMATCH};

use super::{MqttOpts, connect};

#[derive(Subcommand, Debug)]
pub(crate) enum NetCmd {
    /// Gateway health: link state, identity, node count, radio snapshot.
    Status {
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn run(cmd: NetCmd, opts: MqttOpts) -> Result<u8> {
    match cmd {
        NetCmd::Status { json } => status(&opts, json),
    }
}

fn status(opts: &MqttOpts, json: bool) -> Result<u8> {
    let mut s = connect(opts)?;
    let prefix = s.prefix.clone();
    let window = Duration::from_secs(2);
    let Some(raw) = s.read_retained(&topics::gateway_status(&prefix), window)? else {
        eprintln!(
            "[tower] no retained gateway status under \"{prefix}\" — no gateway has ever bridged to this broker"
        );
        return Ok(EXIT_ERROR);
    };
    let status: Status = serde_json::from_slice(&raw)?;
    if status.schema != payload::SCHEMA {
        eprintln!(
            "[tower] gateway publishes MQTT schema v{}, this build speaks v{} — update one side",
            status.schema,
            payload::SCHEMA
        );
        return Ok(EXIT_PROTOCOL_MISMATCH);
    }
    let stats: Option<Stats> = s
        .read_retained(&topics::gateway_stats(&prefix), window)?
        .and_then(|raw| serde_json::from_slice(&raw).ok());

    if json {
        let mut v = serde_json::to_value(&status)?;
        if let Some(st) = &stats {
            v["stats"] = serde_json::to_value(st)?;
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        if status.state == "offline" {
            // The LWT fired: the broker only knows we're gone — the other fields
            // are its defaults, not data. Say the one thing that's true.
            println!(
                "state:    offline (the gateway's last-will fired — the bridge process is gone)"
            );
        } else {
            println!(
                "gateway:  {} ({} {})",
                status.gateway, status.firmware, status.firmware_version
            );
            println!(
                "state:    {} (serial {} on {})",
                status.state,
                if status.serial_up { "up" } else { "DOWN" },
                status.serial_port
            );
        }
        println!(
            "protocol: v{} · mqtt schema v{}",
            status.protocol, status.schema
        );
        if let Some(st) = &stats {
            println!(
                "network:  {} node(s), {} uplink(s), {} queued downlink(s)",
                st.nodes, st.uplinks, st.queued
            );
            if let (Some(dbm), Some(ch)) = (st.rssi_dbm, st.channel) {
                println!("radio:    ch {ch} ambient {dbm} dBm");
            }
        }
    }
    // Scripts branch on liveness: offline (LWT fired) exits non-zero.
    Ok(if status.state == "offline" {
        EXIT_ERROR
    } else {
        EXIT_OK
    })
}
