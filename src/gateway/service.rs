//! `tower gateway --service` — the headless frontend: engine events become
//! timestamped stderr lines (systemd/journald-friendly), MQTT is the only real
//! interface. Runs until the process is signalled (Ctrl-C kills us; the LWT then
//! flips `gateway/status` to `offline` on the broker's timeout).

use std::sync::mpsc::Receiver;

use super::engine::{Event, RadioSample};

fn line(msg: &str) {
    eprintln!("[tower] {} {msg}", chrono::Local::now().format("%H:%M:%S"));
}

pub(crate) fn run(events: Receiver<Event>) {
    line("gateway service up (Ctrl-C to stop)");
    while let Ok(ev) = events.recv() {
        match ev {
            Event::Log(l) => line(&l),
            Event::Link { serial_up, mqtt_up } => line(&format!(
                "link: serial {} · mqtt {}",
                if serial_up { "up" } else { "DOWN" },
                if mqtt_up { "up" } else { "DOWN" }
            )),
            Event::Registry(nodes) => line(&format!(
                "registry: {} node(s){}",
                nodes.len(),
                nodes
                    .iter()
                    .map(|n| format!(
                        " · {}",
                        if n.name.is_empty() {
                            n.id.to_string()
                        } else {
                            n.name.clone()
                        }
                    ))
                    .collect::<String>()
            )),
            Event::Shell { node, rsp } => {
                if let Some(err) = &rsp.error {
                    line(&format!("shell {node:08x} #{}: error: {err}", rsp.id));
                } else if rsp.done {
                    line(&format!(
                        "shell {node:08x} #{}: done (result {})",
                        rsp.id, rsp.result
                    ));
                }
            }
            // The graph feed is TUI food — too chatty for a service log.
            Event::Radio(RadioSample::Ambient { .. }) => {}
            Event::Radio(RadioSample::Rx { src, rssi_dbm }) => {
                line(&format!("rx {src:08x} {rssi_dbm} dBm"));
            }
            Event::Radio(RadioSample::Tx { dest, delivered }) => {
                line(&format!(
                    "tx {dest:08x} {}",
                    if delivered { "delivered" } else { "pending" }
                ));
            }
            // In-process RPCs only exist in TUI mode.
            Event::Rpc(_) => {}
            Event::Pairing(p) => match p.remaining_s {
                Some(s) if p.state == "open" => line(&format!("pairing window open ({s}s left)")),
                _ => {
                    if let Some(j) = p.joined {
                        line(&format!("paired {j}"));
                    }
                }
            },
        }
    }
}
