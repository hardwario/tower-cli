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
                    rssi_dbm: u.rssi_dbm,
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
