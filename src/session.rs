//! The framed console **session** over a byte transport: the boot-`Hello` handshake, the
//! `ShellResponse` reassembler, and the `ShellCompletions` request/response. Everything here
//! is generic over [`Transport`] (`Read + Write`), so
//! it runs unchanged over a real `Box<dyn serialport::SerialPort>` in production and over an
//! in-memory mock in tests — which is what makes the session logic testable without hardware.
//!
//! What is deliberately *not* here: opening/enumerating ports and the reset pulse (that's
//! `port`), rendering the stream (that's `render`), and the rustyline/clap glue for the
//! interactive shell (that stays in the command layer in `main`).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use tower_protocol::msg::{CandidateKind, Hello, ShellComplete, ShellCompletions, ShellResponse};
use tower_protocol::{Error, FrameDecoder, MAX_WIRE, MsgType, decode_frame, encode_frame};

/// The byte transport the session runs over: just `Read + Write`. A real
/// `Box<dyn serialport::SerialPort>` already satisfies this (so production callers are
/// unchanged), and a test can supply an in-memory duplex mock. A drained mock returns
/// `ErrorKind::TimedOut`, exactly as a serial port does on its read timeout, so the loops
/// exercise their real timeout paths.
pub(crate) trait Transport: Read + Write {}
impl<T: Read + Write + ?Sized> Transport for T {}

/// How long to wait for the boot `Hello` before falling back to `--delay`.
///
/// Sized to the measured worst case, not the typical one: a warm boot announces in
/// ~150 ms, but when the LSE 32 kHz crystal restarts cold the same firmware takes
/// **~5.2 s** to its Hello (measured 2026-07-05, same board, same pulse — bimodal).
/// The wait is event-driven (returns the moment the Hello decodes), so the ceiling
/// costs nothing on fast boots; a short ceiling silently degraded `exec --reset`
/// into firing the command at a device that hadn't even booted yet.
const HELLO_WAIT: Duration = Duration::from_millis(8000);
/// Fallback settle when `--reset` is used on a send path but no `Hello` arrives
/// and no explicit `--delay` was given.
const DEFAULT_SETTLE: Duration = Duration::from_millis(250);
/// Guard settle after a post-reset `Hello` before the first host→device send.
///
/// Right after the boot `Hello`, a chatty firmware drains its whole boot log backlog
/// in one burst; while that flood saturates the UART ISR, host→device bytes can be
/// lost to receiver overrun, so a command sent immediately after the Hello vanishes
/// without a trace (measured window ≤ ~60 ms on console_full; zero on quiet
/// firmwares like blinky — 2026-07-05 wire probe). 150 ms is 2.5× the measured
/// worst case and imperceptible next to the reset itself. An explicit `--delay`
/// extends (never shortens) this guard.
const POST_HELLO_GUARD: Duration = Duration::from_millis(150);

/// The outcome of waiting for a freshly-reset device to announce itself. Tri-state on purpose:
/// a *real* protocol-version mismatch is rejected by `decode_frame` at the frame **header**
/// (top 3 bits of `ver_type`) — it returns [`Error::BadVersion`] and the `Hello` payload never
/// parses — so a Hello-payload comparison alone would miss it and the caller would just time
/// out. Surfacing [`Readiness::BadVersion`] lets `exec` fail fast with a clear diagnostic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Readiness {
    /// A `Hello` decoded cleanly; carries the `protocol_version` it announced (which the
    /// caller still checks against its own, as a secondary guard — see `exec`).
    Hello(u8),
    /// `decode_frame` rejected a frame's header version: the peer speaks a different
    /// `tower-protocol` tag. Carries the version byte actually seen on the wire.
    BadVersion(u8),
    /// No `Hello` (and no version-tagged frame) arrived before the timeout.
    Timeout,
}

/// Block until the device announces itself with a `Hello` frame (so its shell is up before we
/// send), or a header-level version mismatch is seen, or `timeout`. Bytes seen meanwhile feed
/// `dec`. Only meaningful right after a reset.
pub(crate) fn wait_for_hello(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    timeout: Duration,
) -> Readiness {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        let n = match sp.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(_) => return Readiness::Timeout,
        };
        for &b in &buf[..n] {
            if let Some(inner) = dec.push(b) {
                match decode_frame(inner) {
                    Ok((MsgType::Hello, _, payload)) => {
                        if let Ok(h) = postcard::from_bytes::<Hello>(payload) {
                            return Readiness::Hello(h.protocol_version);
                        }
                    }
                    // The smoking gun for a tag mismatch: the header version didn't match, so
                    // this (and every) frame is rejected before its payload is even read.
                    Err(Error::BadVersion { got }) => return Readiness::BadVersion(got),
                    _ => {}
                }
            }
        }
    }
    Readiness::Timeout
}

/// Get a freshly reset device ready to accept a command: wait for the boot
/// `Hello` (self-calibrating to real boot time), then honor an explicit `--delay`
/// as extra settle. If no `Hello` arrives, fall back to `--delay` (or a default)
/// so we don't send into a link that isn't up yet. Returns the [`Readiness`] so callers can
/// enforce the lockstep rule (and, for `BadVersion`, bail immediately without settling).
pub(crate) fn await_ready(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    delay: Option<u64>,
) -> Readiness {
    let readiness = wait_for_hello(sp, dec, HELLO_WAIT);
    match readiness {
        // A Hello arrived: guard against the boot-burst deaf window (see POST_HELLO_GUARD),
        // extended by an explicit `--delay` if the caller asked for more.
        Readiness::Hello(_) => {
            std::thread::sleep(POST_HELLO_GUARD.max(Duration::from_millis(delay.unwrap_or(0))));
        }
        // Mismatch: don't settle — the caller is about to bail.
        Readiness::BadVersion(_) => {}
        // No Hello even at the worst-case ceiling: the device may be wedged, held in reset,
        // or running firmware with no console. Say so — the send that follows is a best-effort
        // shot in the dark, and the old silent fallback made this indistinguishable from a
        // healthy send that got no reply.
        Readiness::Timeout => {
            eprintln!(
                "[tower] no boot Hello within {}s of the reset — device wedged or console-less? sending anyway",
                HELLO_WAIT.as_secs()
            );
            std::thread::sleep(delay.map_or(DEFAULT_SETTLE, Duration::from_millis));
        }
    }
    readiness
}

// ---- shell command response ------------------------------------------------

/// The reassembled outcome of a shell command.
pub(crate) struct Response {
    /// Device-reported result byte (authoritative only on the `last` chunk).
    pub(crate) result: u8,
    /// Concatenated chunk text.
    pub(crate) text: String,
    /// A `chunk` index gap was seen: a middle chunk was CRC-dropped, so `text` is
    /// silently truncated. Callers should warn and (for `exec`) exit non-zero.
    pub(crate) incomplete: bool,
}

/// The result of waiting for a shell response. The timeout variant carries the lockstep
/// smoking gun: if frames DID arrive but every one was rejected at the header for its
/// protocol version, the link isn't dead — the two ends speak different `tower-protocol`
/// tags — and callers must say so (exit 125) instead of reporting a mute device (124).
pub(crate) enum ReadOutcome {
    Response(Response),
    Timeout { bad_version: Option<u8> },
}

impl ReadOutcome {
    /// Test convenience mirroring the previous `Option` API.
    #[cfg(test)]
    pub(crate) fn unwrap(self) -> Response {
        match self {
            ReadOutcome::Response(r) => r,
            ReadOutcome::Timeout { .. } => panic!("read_response timed out"),
        }
    }
    /// Test convenience mirroring the previous `Option` API ("timed out?").
    #[cfg(test)]
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, ReadOutcome::Timeout { .. })
    }
}

/// Read frames until the `ShellResponse` for `cmd_id` completes (`last`), or the idle
/// `timeout` elapses. Non-matching frames (logs/events, other `cmd_id`s) are ignored.
///
/// `timeout` is an *idle* deadline: it is reset every time a matching chunk arrives, so a
/// long command that keeps producing output isn't cut off — only a genuinely silent link
/// times out. The `chunk` index is tracked per response: a gap means a middle chunk was
/// CRC-dropped (the frame decoder silently discards corrupt frames), which would otherwise
/// yield silently-truncated output with result 0 — so we flag it `incomplete` instead.
///
/// Times out only on a hard read error or if no matching chunk ever arrives before the
/// idle deadline; version-rejected frames seen while waiting are reported in the
/// [`ReadOutcome::Timeout`] so the caller can diagnose a lockstep mismatch.
pub(crate) fn read_response(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    cmd_id: u16,
    timeout: Duration,
) -> ReadOutcome {
    let mut deadline = Instant::now() + timeout;
    let mut text = String::new();
    let mut next_chunk: u16 = 0; // expected `chunk` index of the next matching frame
    let mut incomplete = false;
    let mut bad_version: Option<u8> = None;
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        let nread = match sp.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(_) => return ReadOutcome::Timeout { bad_version },
        };
        for &b in &buf[..nread] {
            let Some(inner) = dec.push(b) else { continue };
            let (mt, _, payload) = match decode_frame(inner) {
                Ok(t) => t,
                Err(Error::BadVersion { got }) => {
                    // The device IS talking — its frames just carry a different protocol
                    // version. Remember it so a "timeout" can be diagnosed as lockstep.
                    bad_version = Some(got);
                    continue;
                }
                Err(_) => continue,
            };
            if mt != MsgType::ShellResponse {
                continue;
            }
            let Ok(r) = postcard::from_bytes::<ShellResponse>(payload) else {
                continue;
            };
            if r.cmd_id != cmd_id {
                continue;
            }
            // A matching chunk: extend the idle window.
            deadline = Instant::now() + timeout;
            if r.chunk != next_chunk {
                // A middle chunk was dropped (or reordered) — the output is truncated.
                eprintln!(
                    "[tower] response chunk dropped (expected #{next_chunk}, got #{}) — output truncated",
                    r.chunk
                );
                incomplete = true;
            }
            next_chunk = r.chunk.wrapping_add(1);
            text.push_str(r.text);
            if r.last {
                return ReadOutcome::Response(Response {
                    result: r.result,
                    text,
                    incomplete,
                });
            }
        }
    }
    ReadOutcome::Timeout { bad_version }
}

// ---- completion (target-authoritative) ------------------------------------

/// An owned copy of a completion result (the wire form borrows the frame buffer).
pub(crate) struct CompletionResult {
    pub(crate) token_start: u16,
    pub(crate) common_prefix: String,
    pub(crate) candidates: Vec<(String, CandidateKind)>,
    pub(crate) more: bool,
}

/// The result of a completion request — same tri-state rationale as [`ReadOutcome`].
pub(crate) enum CompletionOutcome {
    Completions(CompletionResult),
    Timeout { bad_version: Option<u8> },
}

impl CompletionOutcome {
    /// Test convenience mirroring the previous `Option` API.
    #[cfg(test)]
    pub(crate) fn unwrap(self) -> CompletionResult {
        match self {
            CompletionOutcome::Completions(c) => c,
            CompletionOutcome::Timeout { .. } => panic!("request_completions timed out"),
        }
    }
    /// Test convenience mirroring the previous `Option` API ("timed out?").
    #[cfg(test)]
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, CompletionOutcome::Timeout { .. })
    }
}

/// Send a `ShellComplete` and wait for the matching `ShellCompletions`. Shared by the
/// `complete` command and the interactive TAB handler. Version-rejected frames seen while
/// waiting are reported in the timeout variant (lockstep diagnosis, like [`read_response`]).
pub(crate) fn request_completions(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    line: &str,
    cursor: u16,
    req_id: u16,
    timeout: Duration,
) -> CompletionOutcome {
    let mut bad_version: Option<u8> = None;
    let mut buf = [0u8; MAX_WIRE];
    let Ok(n) = encode_frame(
        MsgType::ShellComplete,
        0,
        &ShellComplete {
            req_id,
            line,
            cursor,
        },
        &mut buf,
    ) else {
        return CompletionOutcome::Timeout { bad_version };
    };
    if sp.write_all(&buf[..n]).is_err() || sp.flush().is_err() {
        return CompletionOutcome::Timeout { bad_version };
    }

    let deadline = Instant::now() + timeout;
    let mut rbuf = [0u8; 256];
    while Instant::now() < deadline {
        let nread = match sp.read(&mut rbuf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(_) => return CompletionOutcome::Timeout { bad_version },
        };
        for &b in &rbuf[..nread] {
            let Some(inner) = dec.push(b) else { continue };
            let (mt, _, payload) = match decode_frame(inner) {
                Ok(t) => t,
                Err(Error::BadVersion { got }) => {
                    bad_version = Some(got);
                    continue;
                }
                Err(_) => continue,
            };
            if mt != MsgType::ShellCompletions {
                continue;
            }
            let Ok(c) = postcard::from_bytes::<ShellCompletions>(payload) else {
                continue;
            };
            if c.req_id != req_id {
                continue;
            }
            return CompletionOutcome::Completions(CompletionResult {
                token_start: c.token_start,
                common_prefix: c.common_prefix.to_string(),
                candidates: c
                    .candidates
                    .iter()
                    .map(|cd| (cd.text.to_string(), cd.kind))
                    .collect(),
                more: c.more,
            });
        }
    }
    CompletionOutcome::Timeout { bad_version }
}
