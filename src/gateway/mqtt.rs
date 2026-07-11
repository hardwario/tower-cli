//! The gateway's MQTT side: the rumqttc **synchronous** client (its `Connection`
//! iterator drives the whole event loop on a plain thread — no async code in this
//! crate; tokio exists only inside these dependencies) and, behind the default-on
//! `embedded-broker` feature, an in-process rumqttd broker so bare `tower gateway`
//! needs zero infrastructure. In embedded mode the gateway's own client loops back
//! over localhost TCP — one MQTT code path regardless of mode.

use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use rumqttc::{Client, Connection, Event as MqttEvent, Incoming, LastWill, MqttOptions, QoS};

use super::engine::Input;
use super::payload;
use super::topics;

/// Broker connection parameters (resolved from the `tower gateway` flags).
pub(crate) struct MqttParams {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub client_id: String,
}

/// Build the client. The LWT publishes `gateway/status = offline` (retained) so
/// subscribers see a dead gateway without any timeout logic of their own.
pub(crate) fn client(params: &MqttParams, prefix: &str) -> (Client, Connection) {
    let mut opts = MqttOptions::new(params.client_id.clone(), params.host.clone(), params.port);
    opts.set_keep_alive(Duration::from_secs(15));
    if let (Some(u), Some(p)) = (&params.username, &params.password) {
        opts.set_credentials(u.clone(), p.clone());
    }
    opts.set_last_will(LastWill::new(
        topics::gateway_status(prefix),
        serde_json::to_vec(&payload::Status::offline()).unwrap_or_default(),
        QoS::AtLeastOnce,
        true,
    ));
    Client::new(opts, 100)
}

/// Drive the rumqttc connection loop, translating broker events into engine inputs.
/// rumqttc reconnects internally; we only report edges. Runs until the engine hangs up.
pub(crate) fn run(mut connection: Connection, input: Sender<Input>) {
    let mut was_up = false;
    for event in connection.iter() {
        let ok = match event {
            Ok(MqttEvent::Incoming(Incoming::ConnAck(_))) => {
                was_up = true;
                input.send(Input::MqttUp).is_ok()
            }
            Ok(MqttEvent::Incoming(Incoming::Publish(p))) => input
                .send(Input::MqttIn {
                    topic: p.topic.clone(),
                    payload: p.payload.to_vec(),
                })
                .is_ok(),
            Ok(_) => true,
            Err(e) => {
                let ok = if was_up {
                    was_up = false;
                    input
                        .send(Input::MqttDown {
                            error: e.to_string(),
                        })
                        .is_ok()
                } else {
                    true
                };
                // Don't spin on a refused broker: rumqttc retries on the next iteration.
                std::thread::sleep(Duration::from_secs(1));
                ok
            }
        };
        if !ok {
            return;
        }
    }
}

/// Start the embedded rumqttd broker on `listen` (blocking — run on its own thread).
#[cfg(feature = "embedded-broker")]
pub(crate) fn embedded_broker(listen: std::net::SocketAddr) -> Result<std::thread::JoinHandle<()>> {
    use rumqttd::{Broker, Config, ConnectionSettings, RouterConfig, ServerSettings};

    let router = RouterConfig {
        max_connections: 128,
        max_outgoing_packet_count: 200,
        max_segment_size: 104857600,
        max_segment_count: 10,
        ..Default::default()
    };
    let server = ServerSettings {
        name: "tower".to_string(),
        listen,
        tls: None,
        next_connection_delay_ms: 1,
        connections: ConnectionSettings {
            connection_timeout_ms: 60000,
            max_payload_size: 1024 * 1024,
            max_inflight_count: 100,
            auth: None,
            external_auth: None,
            dynamic_filters: false,
        },
    };
    let config = Config {
        id: 0,
        router,
        v4: Some([("tower".to_string(), server)].into_iter().collect()),
        ..Default::default()
    };
    let mut broker = Broker::new(config);
    let handle = std::thread::Builder::new()
        .name("mqtt-broker".into())
        .spawn(move || {
            if let Err(e) = broker.start() {
                eprintln!("[tower] embedded broker died: {e}");
            }
        })
        .context("spawning the embedded broker thread")?;
    Ok(handle)
}

#[cfg(all(test, feature = "embedded-broker"))]
mod tests {
    use super::*;
    use rumqttc::{Client, MqttOptions};

    /// End-to-end through the real embedded rumqttd: bind, connect, retained
    /// publish/subscribe round-trip. The one integration point unit tests can't fake.
    #[test]
    fn embedded_broker_roundtrip() {
        // A fixed high port would collide across CI runs; probe for a free one.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        embedded_broker(addr).expect("broker thread");

        let mut opts = MqttOptions::new("test-client", "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut connection) = Client::new(opts, 10);

        let mut connected = false;
        let mut got: Option<Vec<u8>> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match connection.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(MqttEvent::Incoming(Incoming::ConnAck(_)))) => {
                    connected = true;
                    client
                        .subscribe("tower/test", rumqttc::QoS::AtLeastOnce)
                        .unwrap();
                    client
                        .publish(
                            "tower/test",
                            rumqttc::QoS::AtLeastOnce,
                            true,
                            b"hi".as_slice(),
                        )
                        .unwrap();
                }
                Ok(Ok(MqttEvent::Incoming(Incoming::Publish(p)))) if p.topic == "tower/test" => {
                    got = Some(p.payload.to_vec());
                    break;
                }
                _ => {}
            }
        }
        assert!(connected, "client never connected to the embedded broker");
        assert_eq!(got.as_deref(), Some(b"hi".as_slice()));
    }
}
