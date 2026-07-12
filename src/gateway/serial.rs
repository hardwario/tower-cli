//! The gateway's serial thread: owns the dongle's port (sole reader AND writer —
//! serial handles don't share), decodes target→host frames into owned
//! [`SerialMsg`]s for the engine, drains the engine's outbound frame queue between
//! reads (≤10 ms latency at the responsive read timeout), and reconnects with the
//! console commands' 800 ms throttle. The first open already happened in `mod.rs`
//! (startup verification), so this thread never invents the "first open is fatal"
//! policy — it inherits an open, verified handle.

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use tower_protocol::msg::{Hello, Log, MgmtResponse, Print, RadioStat, Uplink};
use tower_protocol::{Error, FrameDecoder, MsgType, decode_frame};

use super::engine::{Input, SerialMsg};
use crate::port::open_console_responsive;

/// Decode one inner frame into an owned engine message (`None` = not engine-relevant).
pub(crate) fn to_serial_msg(inner: &[u8]) -> Option<SerialMsg> {
    let (mt, _seq, payload) = match decode_frame(inner) {
        Ok(t) => t,
        // Should be impossible after startup verification, but a mid-session reflash
        // to an older firmware would surface exactly here.
        Err(Error::BadVersion { got }) => {
            return Some(SerialMsg::Log {
                line: format!("PROTOCOL MISMATCH: device now speaks v{got} — reflash or rebuild"),
            });
        }
        Err(_) => return None,
    };
    match mt {
        MsgType::Hello => postcard::from_bytes::<Hello>(payload)
            .ok()
            .map(|h| SerialMsg::Hello {
                firmware_name: h.firmware_name.to_string(),
                firmware_version: h.firmware_version.to_string(),
                session_id: h.session_id,
            }),
        MsgType::Log => postcard::from_bytes::<Log>(payload)
            .ok()
            .map(|l| SerialMsg::Log {
                line: format!(
                    "[{:5}.{:03}] {} {}: {}",
                    l.uptime_us / 1_000_000,
                    (l.uptime_us % 1_000_000) / 1000,
                    level_tag(l.level),
                    l.module,
                    l.message
                ),
            }),
        MsgType::Print => postcard::from_bytes::<Print>(payload)
            .ok()
            .map(|p| SerialMsg::Log {
                line: p.text.trim_end().to_string(),
            }),
        MsgType::Uplink => {
            postcard::from_bytes::<Uplink>(payload)
                .ok()
                .map(|u| SerialMsg::Uplink {
                    src: u.src,
                    counter: u.counter,
                    rssi: u.rssi,
                    lqi: u.lqi,
                    data: u.data.to_vec(),
                })
        }
        MsgType::MgmtResponse => {
            postcard::from_bytes::<MgmtResponse>(payload)
                .ok()
                .map(|m| SerialMsg::Mgmt {
                    req_id: m.req_id,
                    result: m.result,
                    chunk: m.chunk,
                    last: m.last,
                    data: m.data.to_vec(),
                })
        }
        MsgType::RadioStat => postcard::from_bytes::<RadioStat>(payload)
            .ok()
            .map(SerialMsg::Stat),
        MsgType::Dropped => postcard::from_bytes::<tower_protocol::msg::Dropped>(payload)
            .ok()
            .map(|d| SerialMsg::Dropped(d.count)),
        // Shell/completions traffic belongs to `tower shell`, not the gateway; the
        // host→target types never arrive here.
        _ => None,
    }
}

fn level_tag(l: tower_protocol::msg::Level) -> &'static str {
    use tower_protocol::msg::Level;
    match l {
        Level::Error => "ERROR",
        Level::Warn => "WARN ",
        Level::Info => "INFO ",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

/// Run the serial loop until the input channel's receiver hangs up. `sp` is the
/// already-open, already-verified handle from startup.
pub(crate) fn run(
    mut sp: Box<dyn serialport::SerialPort>,
    port: String,
    input: Sender<Input>,
    frames: Receiver<Vec<u8>>,
) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 512];
    let mut up = true;
    loop {
        // Outbound first: mgmt requests are latency-sensitive (the TUI is waiting).
        loop {
            match frames.try_recv() {
                Ok(frame) => {
                    if up && (sp.write_all(&frame).is_err() || sp.flush().is_err()) {
                        up = false;
                        dec.reset();
                        let _ = input.send(Input::SerialDown {
                            error: "write failed".into(),
                        });
                    }
                    // A frame issued while down is dropped; the engine's op timeout
                    // + resync-on-reconnect own the recovery story.
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return, // engine gone — shut down
            }
        }

        if !up {
            std::thread::sleep(Duration::from_millis(800));
            match open_console_responsive(&port, false) {
                Ok(reopened) => {
                    sp = reopened;
                    dec.reset();
                    up = true;
                    if input.send(Input::SerialUp).is_err() {
                        return;
                    }
                }
                Err(_) => continue,
            }
            continue;
        }

        match sp.read(&mut buf) {
            Ok(n) => {
                for &b in &buf[..n] {
                    if let Some(inner) = dec.push(b)
                        && let Some(msg) = to_serial_msg(inner)
                        && input.send(Input::SerialFrame(msg)).is_err()
                    {
                        return;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                up = false;
                dec.reset();
                if input
                    .send(Input::SerialDown {
                        error: e.to_string(),
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_protocol::mgmt::MGMT_OK;
    use tower_protocol::msg::{Dropped, Level, ShellResponse};
    use tower_protocol::{MAX_WIRE, encode_frame};

    /// Encode a real wire frame with `encode_frame`, then deframe it back to the inner bytes
    /// `to_serial_msg` consumes (what `FrameDecoder::push` yields in the serial loop).
    fn inner_of<T: serde::Serialize>(mt: MsgType, payload: &T) -> Vec<u8> {
        let mut buf = [0u8; MAX_WIRE];
        let n = encode_frame(mt, 0, payload, &mut buf).unwrap();
        let mut dec = FrameDecoder::new();
        buf[..n]
            .iter()
            .find_map(|&b| dec.push(b).map(|s| s.to_vec()))
            .expect("one complete frame")
    }

    #[test]
    fn uplink_frame_maps_to_uplink() {
        let inner = inner_of(
            MsgType::Uplink,
            &Uplink {
                src: 0xAB12,
                counter: 42,
                rssi: -67,
                lqi: 30,
                data: &[1, 2, 3],
            },
        );
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Uplink {
                src,
                counter,
                rssi,
                lqi,
                data,
            } => {
                assert_eq!((src, counter, rssi, lqi), (0xAB12, 42, -67, 30));
                assert_eq!(data, vec![1, 2, 3]);
            }
            other => panic!("expected Uplink, got {other:?}"),
        }
    }

    #[test]
    fn mgmt_response_frame_maps_to_mgmt() {
        let inner = inner_of(
            MsgType::MgmtResponse,
            &MgmtResponse {
                req_id: 7,
                result: MGMT_OK,
                chunk: 1,
                last: true,
                data: &[9, 9],
            },
        );
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Mgmt {
                req_id,
                result,
                chunk,
                last,
                data,
            } => {
                assert_eq!((req_id, result, chunk, last), (7, MGMT_OK, 1, true));
                assert_eq!(data, vec![9, 9]);
            }
            other => panic!("expected Mgmt, got {other:?}"),
        }
    }

    #[test]
    fn radio_stat_frames_map_to_stat() {
        let inner = inner_of(
            MsgType::RadioStat,
            &RadioStat::Tx {
                dest: 0xAB12,
                item: 5,
                outcome: 0,
                ack_rssi: Some(-40),
            },
        );
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Stat(RadioStat::Tx {
                dest,
                item,
                outcome,
                ack_rssi,
            }) => assert_eq!((dest, item, outcome, ack_rssi), (0xAB12, 5, 0, Some(-40))),
            other => panic!("expected Stat(Tx), got {other:?}"),
        }
        let inner = inner_of(
            MsgType::RadioStat,
            &RadioStat::Channel {
                channel: 3,
                rssi: -98,
            },
        );
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Stat(RadioStat::Channel { channel, rssi }) => {
                assert_eq!((channel, rssi), (3, -98))
            }
            other => panic!("expected Stat(Channel), got {other:?}"),
        }
    }

    #[test]
    fn print_frame_maps_to_trimmed_log() {
        let inner = inner_of(
            MsgType::Print,
            &Print {
                text: "hello world\n  ",
            },
        );
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Log { line } => assert_eq!(line, "hello world"),
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn log_frame_maps_to_formatted_log() {
        let inner = inner_of(
            MsgType::Log,
            &Log {
                level: Level::Info,
                uptime_us: 1_234_567,
                module: "radio",
                message: "up",
            },
        );
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Log { line } => {
                assert!(line.contains("INFO"), "{line}");
                assert!(line.ends_with("radio: up"), "{line}");
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn hello_and_dropped_frames_map() {
        let inner = inner_of(
            MsgType::Hello,
            &Hello {
                protocol_version: tower_protocol::PROTOCOL_VERSION,
                firmware_name: "radio_dongle_gateway",
                firmware_version: "v0.1.0",
                session_id: 9,
            },
        );
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Hello {
                firmware_name,
                firmware_version,
                session_id,
            } => assert_eq!(
                (
                    firmware_name.as_str(),
                    firmware_version.as_str(),
                    session_id
                ),
                ("radio_dongle_gateway", "v0.1.0", 9)
            ),
            other => panic!("expected Hello, got {other:?}"),
        }
        let inner = inner_of(MsgType::Dropped, &Dropped { count: 4 });
        match to_serial_msg(&inner).expect("engine-relevant") {
            SerialMsg::Dropped(n) => assert_eq!(n, 4),
            other => panic!("expected Dropped, got {other:?}"),
        }
    }

    #[test]
    fn bad_version_frame_becomes_protocol_mismatch_log() {
        // A deframed inner whose header advertises v1 (not the current v3): `decode_frame`
        // rejects it at the version check (before CRC), and `to_serial_msg` surfaces the
        // mismatch as a Log line rather than silently dropping it — the mid-session reflash
        // to older firmware the doc comment calls out. Build the inner directly (the host's
        // own encoder always stamps the current version).
        let mut inner = vec![(1u8 << 5) | (MsgType::Hello as u8 & 0x1F), 0, 0];
        inner.extend_from_slice(&[0, 0, 0, 0]); // filler CRC — version fails before it's read
        match to_serial_msg(&inner).expect("a mismatch surfaces as a Log") {
            SerialMsg::Log { line } => {
                assert!(line.contains("PROTOCOL MISMATCH"), "{line}");
                assert!(line.contains("v1"), "{line}");
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn non_engine_and_corrupt_frames_are_ignored() {
        // A ShellResponse belongs to `tower shell`, not the gateway → the `_ => None` arm.
        let inner = inner_of(
            MsgType::ShellResponse,
            &ShellResponse {
                cmd_id: 1,
                result: 0,
                chunk: 0,
                last: true,
                text: "hi",
            },
        );
        assert!(to_serial_msg(&inner).is_none());
        // A CRC-corrupt frame decodes to `Err(_)` → dropped (None).
        let mut inner = inner_of(
            MsgType::Log,
            &Log {
                level: Level::Info,
                uptime_us: 0,
                module: "m",
                message: "x",
            },
        );
        inner[3] ^= 0xFF; // flip a payload byte → CRC mismatch
        assert!(to_serial_msg(&inner).is_none());
    }
}
