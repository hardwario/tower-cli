//! `tower gateway` — the radio↔MQTT bridge process. One engine (a pure state
//! machine, `engine.rs`), plain threads around it:
//!
//! ```text
//!  serial thread ──┐                       ┌── engine outputs ──▶ serial writer queue
//!  mqtt conn loop ─┼─▶ Input mpsc ─▶ engine┼── publishes ───────▶ rumqttc Client
//!  1 Hz ticker ────┘      (engine thread)  └── events ──────────▶ frontend (service/TUI)
//!  [embedded rumqttd broker thread]
//! ```
//!
//! Startup is synchronous and fail-fast, *before* any thread or alt-screen: open the
//! port, `Describe` the device (the role probe — see `crate::mgmt`), and refuse
//! non-gateways with the documented exit codes (124 mute / 125 protocol mismatch /
//! 126 wrong firmware role).

pub(crate) mod engine;
pub(crate) mod mqtt;
pub(crate) mod payload;
pub(crate) mod serial;
pub(crate) mod service;
pub(crate) mod topics;
pub(crate) mod tui;

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use rumqttc::QoS;
use tower_protocol::FrameDecoder;
use tower_protocol::mgmt::DeviceRole;

use crate::mgmt::{DescribeOutcome, DeviceInfoOwned, describe};
use crate::port::{open_console_responsive, pick_port};
use crate::render::warn_protocol_mismatch;
use crate::{EXIT_DEVICE_TIMEOUT, EXIT_OK, EXIT_PROTOCOL_MISMATCH, EXIT_WRONG_FIRMWARE};

use engine::{Engine, Event, Input, Output};

/// `tower gateway` arguments.
#[derive(Args, Debug)]
pub(crate) struct GatewayOpts {
    /// Host the embedded MQTT broker on ADDR:PORT (default 127.0.0.1:1883 unless
    /// --mqtt picks an external broker instead).
    #[cfg(feature = "embedded-broker")]
    #[arg(long, value_name = "ADDR:PORT", conflicts_with = "mqtt")]
    pub broker: Option<Option<std::net::SocketAddr>>,
    /// Connect to an existing MQTT broker at HOST or HOST:PORT instead of hosting one.
    #[arg(long, value_name = "HOST[:PORT]")]
    pub mqtt: Option<String>,
    /// External-broker username.
    #[arg(long, requires = "mqtt")]
    pub mqtt_user: Option<String>,
    /// External-broker password (prefer the TOWER_MQTT_PASSWORD env var — argv is
    /// visible in `ps`).
    #[arg(
        long,
        requires = "mqtt",
        env = "TOWER_MQTT_PASSWORD",
        hide_env_values = true
    )]
    pub mqtt_password: Option<String>,
    /// MQTT topic prefix.
    #[arg(long, default_value = "tower/")]
    pub prefix: String,
    /// Run headless (no TUI): log lines to stderr, MQTT as the only interface.
    #[arg(long)]
    pub service: bool,
    /// Reboot the dongle on attach (NRST pulse). Clears its RAM downlink queue.
    #[arg(long)]
    pub reset: bool,
}

/// Split a `HOST[:PORT]` argument (default port 1883).
fn split_host_port(s: &str) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(1883))
        }
        _ => (s.to_string(), 1883),
    }
}

/// A verified, open gateway attach — or the exit code refusing it.
type Verified = std::result::Result<(Box<dyn serialport::SerialPort>, DeviceInfoOwned), u8>;

/// The terminal decision from one `Describe` role probe, factored out of [`verify_gateway`]
/// so the role gate is unit-testable without a serial port (the port open + boot-window
/// retry loop stays in `verify_gateway`). `Accept` = a real gateway; `Reject(code)` =
/// refuse the attach with that exit code; `Retry` = an inconclusive (mute) probe — the
/// caller tries once more before giving up as a timeout.
enum GateDecision {
    Accept(DeviceInfoOwned),
    Reject(u8),
    Retry,
}

/// Map one `Describe` outcome onto the gateway-verification decision (see the exit-code
/// contract in `main`): a non-gateway role or a refused probe is [`EXIT_WRONG_FIRMWARE`], a
/// header version mismatch is [`EXIT_PROTOCOL_MISMATCH`], and a mute probe is retryable.
/// Pure but for the operator-facing diagnostics it prints on a refusal.
fn gate_describe(port: &str, outcome: DescribeOutcome) -> GateDecision {
    match outcome {
        DescribeOutcome::Info(info) => {
            if info.role != DeviceRole::Gateway {
                eprintln!(
                    "[tower] {port} runs \"{}\" (role {:?}), not the gateway firmware — flash apps/radio_dongle_gateway first",
                    info.firmware_name, info.role
                );
                GateDecision::Reject(EXIT_WRONG_FIRMWARE)
            } else {
                GateDecision::Accept(info)
            }
        }
        DescribeOutcome::Refused(code) => {
            eprintln!("[tower] {port}: device refused the role probe (code {code})");
            GateDecision::Reject(EXIT_WRONG_FIRMWARE)
        }
        DescribeOutcome::Timeout {
            bad_version: Some(got),
        } => {
            warn_protocol_mismatch(got);
            GateDecision::Reject(EXIT_PROTOCOL_MISMATCH)
        }
        DescribeOutcome::Timeout { bad_version: None } => GateDecision::Retry,
    }
}

/// The verified gateway attach: open + `Describe` + role gate. Shared with the TUI
/// path so both fail before any screen takeover.
pub(crate) fn verify_gateway(port: &str, reset: bool) -> Result<Verified> {
    let mut sp = open_console_responsive(port, reset)?;
    if reset {
        // Let the freshly-reset dongle boot before probing (Hello + settle happen
        // inside the wait: describe just retries over the boot window).
        std::thread::sleep(Duration::from_millis(300));
    }
    let mut dec = FrameDecoder::new();
    // Two attempts: the first can land in a boot burst / compaction stall.
    for attempt in 0..2 {
        match gate_describe(
            port,
            describe(&mut *sp, &mut dec, 0x7000 + attempt, Duration::from_secs(4)),
        ) {
            GateDecision::Accept(info) => return Ok(Ok((sp, info))),
            GateDecision::Reject(code) => return Ok(Err(code)),
            GateDecision::Retry => {}
        }
    }
    eprintln!(
        "[tower] {port}: no answer to the management probe — not a gateway (or pre-v3 firmware)?"
    );
    Ok(Err(EXIT_DEVICE_TIMEOUT))
}

pub(crate) fn run(port: Option<String>, opts: GatewayOpts) -> Result<u8> {
    let port = pick_port(port)?;
    let prefix = topics::normalize_prefix(&opts.prefix);

    // ---- fail-fast verification (before threads, before any TUI) ----
    let (sp, info) = match verify_gateway(&port, opts.reset)? {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    eprintln!(
        "[tower] gateway {} on {port} ({} of {} node slots)",
        topics::node_hex(info.net_id),
        info.node_count,
        info.node_capacity
    );

    // ---- broker resolution ----
    let (mqtt_host, mqtt_port) = match &opts.mqtt {
        Some(hp) => split_host_port(hp),
        None => {
            #[cfg(feature = "embedded-broker")]
            {
                let listen = opts
                    .broker
                    .flatten()
                    .unwrap_or_else(|| "127.0.0.1:1883".parse().unwrap());
                mqtt::embedded_broker(listen)?;
                eprintln!("[tower] embedded MQTT broker on {listen}");
                (listen.ip().to_string(), listen.port())
            }
            #[cfg(not(feature = "embedded-broker"))]
            {
                anyhow::bail!("built without the embedded broker; pass --mqtt HOST[:PORT]");
            }
        }
    };

    let params = mqtt::MqttParams {
        host: mqtt_host,
        port: mqtt_port,
        username: opts.mqtt_user.clone(),
        password: opts.mqtt_password.clone(),
        client_id: format!("tower-gateway-{}", topics::node_hex(info.net_id)),
    };
    let (client, connection) = mqtt::client(&params, &prefix);

    // ---- threads ----
    let (input_tx, input_rx) = mpsc::channel::<Input>();
    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>();
    let (event_tx, event_rx) = mpsc::channel::<Event>();

    {
        let input = input_tx.clone();
        let port = port.clone();
        std::thread::Builder::new()
            .name("serial".into())
            .spawn(move || serial::run(sp, port, input, frame_rx))
            .context("spawning the serial thread")?;
    }
    {
        let input = input_tx.clone();
        std::thread::Builder::new()
            .name("mqtt".into())
            .spawn(move || mqtt::run(connection, input))
            .context("spawning the mqtt thread")?;
    }
    {
        let input = input_tx.clone();
        std::thread::Builder::new()
            .name("ticker".into())
            .spawn(move || {
                while input.send(Input::Tick).is_ok() {
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .context("spawning the ticker thread")?;
    }

    // ---- engine thread: state machine + output execution ----
    let mut engine = Engine::new(prefix.clone(), port.clone(), info);
    {
        let client = client.clone();
        std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                let execute = |outputs: Vec<Output>| {
                    for o in outputs {
                        match o {
                            Output::SerialSend(frame) => {
                                let _ = frame_tx.send(frame);
                            }
                            Output::Publish {
                                topic,
                                payload,
                                retain,
                            } => {
                                let _ = client.publish(topic, QoS::AtLeastOnce, retain, payload);
                            }
                            Output::Subscribe(filter) => {
                                let _ = client.subscribe(filter, QoS::AtLeastOnce);
                            }
                            Output::Event(ev) => {
                                let _ = event_tx.send(ev);
                            }
                        }
                    }
                };
                execute(engine.start());
                while let Ok(input) = input_rx.recv() {
                    execute(engine.handle(input));
                }
            })
            .context("spawning the engine thread")?;
    }

    // ---- frontend (owns the main thread) ----
    if opts.service {
        service::run(event_rx);
        Ok(EXIT_OK)
    } else {
        tui::run(event_rx, input_tx, prefix, port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};

    use tower_protocol::mgmt::{DeviceInfo, MGMT_OK, MGMT_UNSUPPORTED};
    use tower_protocol::msg::{Hello, MgmtResponse};
    use tower_protocol::{MAX_WIRE, MsgType, encode_frame};

    /// In-memory duplex port, same contract as the `MockPort`s in `mgmt.rs` / `main.rs`
    /// tests: a drained read returns `TimedOut` exactly like a real serial port past its
    /// read timeout, so `describe`'s read loop exercises its real timeout path.
    struct Mock {
        to_read: VecDeque<u8>,
        written: Vec<u8>,
    }
    impl Mock {
        fn new() -> Self {
            Self {
                to_read: VecDeque::new(),
                written: Vec::new(),
            }
        }
        fn feed(&mut self, bytes: &[u8]) {
            self.to_read.extend(bytes.iter().copied());
        }
    }
    impl Read for Mock {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.to_read.is_empty() {
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
            }
            let n = buf.len().min(self.to_read.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.to_read.pop_front().unwrap();
            }
            Ok(n)
        }
    }
    impl Write for Mock {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A postcard `DeviceInfo` record for the given role (the `Describe` reply body).
    fn device_info(role: DeviceRole) -> Vec<u8> {
        postcard::to_stdvec(&DeviceInfo {
            role,
            radio_schema_version: 1,
            net_id: 0xAB12,
            band: 0,
            channel: 0,
            node_capacity: 32,
            node_count: 0,
            provisioned: true,
            gw_id: 0xAB12,
            firmware_name: "radio_push_button",
        })
        .unwrap()
    }

    /// A single-chunk `MgmtResponse` wire frame answering `req_id`.
    fn mgmt_response(req_id: u16, result: u8, data: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; MAX_WIRE];
        let n = encode_frame(
            MsgType::MgmtResponse,
            0,
            &MgmtResponse {
                req_id,
                result,
                chunk: 0,
                last: true,
                data,
            },
            &mut buf,
        )
        .unwrap();
        buf[..n].to_vec()
    }

    fn hello_frame() -> Vec<u8> {
        let mut buf = [0u8; MAX_WIRE];
        let n = encode_frame(
            MsgType::Hello,
            0,
            &Hello {
                protocol_version: tower_protocol::PROTOCOL_VERSION,
                firmware_name: "radio_push_button",
                firmware_version: "v0.1.0",
                session_id: 1,
            },
            &mut buf,
        )
        .unwrap();
        buf[..n].to_vec()
    }

    /// Run the real `describe()` role probe over a port scripted with a Hello (which the
    /// probe ignores) followed by a `Describe` reply, then classify it through the gate.
    fn gate_scripted(result: u8, data: &[u8]) -> GateDecision {
        let mut m = Mock::new();
        m.feed(&hello_frame());
        m.feed(&mgmt_response(0x7000, result, data));
        let mut dec = FrameDecoder::new();
        let outcome = describe(&mut m, &mut dec, 0x7000, Duration::from_millis(300));
        gate_describe("/dev/ttyTEST", outcome)
    }

    #[test]
    fn wrong_role_node_yields_exit_126() {
        // The role-gate contract: a dongle running node firmware answers the probe honestly
        // with role Node, and the gate refuses it with EXIT_WRONG_FIRMWARE (126) — distinct
        // from 124/125 so a script can say "flash the gateway firmware", not "check the cable".
        assert_eq!(EXIT_WRONG_FIRMWARE, 126);
        match gate_scripted(MGMT_OK, &device_info(DeviceRole::Node)) {
            GateDecision::Reject(code) => assert_eq!(code, EXIT_WRONG_FIRMWARE),
            _ => panic!("a node-role device must be refused"),
        }
    }

    #[test]
    fn role_other_yields_exit_126() {
        match gate_scripted(MGMT_OK, &device_info(DeviceRole::Other)) {
            GateDecision::Reject(code) => assert_eq!(code, EXIT_WRONG_FIRMWARE),
            _ => panic!("a non-gateway role must be refused"),
        }
    }

    #[test]
    fn refused_probe_yields_exit_126() {
        // The device answered the management channel but declined the op (a wrong/older role
        // returns a non-OK result, no DeviceInfo record) — still "wrong firmware", exit 126.
        match gate_scripted(MGMT_UNSUPPORTED, &[]) {
            GateDecision::Reject(code) => assert_eq!(code, EXIT_WRONG_FIRMWARE),
            _ => panic!("a refused role probe must be rejected"),
        }
    }

    #[test]
    fn gateway_role_is_accepted() {
        match gate_scripted(MGMT_OK, &device_info(DeviceRole::Gateway)) {
            GateDecision::Accept(info) => {
                assert_eq!(info.role, DeviceRole::Gateway);
                assert_eq!(info.net_id, 0xAB12);
            }
            _ => panic!("a gateway must be accepted"),
        }
    }

    #[test]
    fn version_mismatch_yields_exit_125_and_mute_retries() {
        // The two non-Describe outcomes of the gate: a header version mismatch is the lockstep
        // failure (125, not retryable); a mute probe is inconclusive and asks for a retry.
        assert_eq!(EXIT_PROTOCOL_MISMATCH, 125);
        match gate_describe(
            "/dev/ttyTEST",
            DescribeOutcome::Timeout {
                bad_version: Some(1),
            },
        ) {
            GateDecision::Reject(code) => assert_eq!(code, EXIT_PROTOCOL_MISMATCH),
            _ => panic!("a version mismatch must be refused as 125"),
        }
        assert!(matches!(
            gate_describe(
                "/dev/ttyTEST",
                DescribeOutcome::Timeout { bad_version: None }
            ),
            GateDecision::Retry
        ));
    }
}
