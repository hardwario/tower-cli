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
use tower_protocol::{FrameDecoder, MAX_WIRE, MsgType, decode_frame, encode_frame};

/// The byte transport the session runs over: just `Read + Write`. A real
/// `Box<dyn serialport::SerialPort>` already satisfies this (so production callers are
/// unchanged), and a test can supply an in-memory duplex mock. A drained mock returns
/// `ErrorKind::TimedOut`, exactly as a serial port does on its read timeout, so the loops
/// exercise their real timeout paths.
pub(crate) trait Transport: Read + Write {}
impl<T: Read + Write + ?Sized> Transport for T {}

/// How long to wait for the boot `Hello` before falling back to `--delay`.
const HELLO_WAIT: Duration = Duration::from_millis(1500);
/// Fallback settle when `--reset` is used on a send path but no `Hello` arrives
/// and no explicit `--delay` was given.
const DEFAULT_SETTLE: Duration = Duration::from_millis(250);

/// Block until the device announces itself with a `Hello` frame (so its shell is
/// up before we send), or `timeout`. Bytes seen meanwhile feed `dec`. Returns the
/// announced `protocol_version` if `Hello` arrived (`None` on timeout). Only
/// meaningful right after a reset.
pub(crate) fn wait_for_hello(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    timeout: Duration,
) -> Option<u8> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        let n = match sp.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(_) => return None,
        };
        for &b in &buf[..n] {
            if let Some(inner) = dec.push(b)
                && let Ok((MsgType::Hello, _, payload)) = decode_frame(inner)
                && let Ok(h) = postcard::from_bytes::<Hello>(payload)
            {
                return Some(h.protocol_version);
            }
        }
    }
    None
}

/// Get a freshly reset device ready to accept a command: wait for the boot
/// `Hello` (self-calibrating to real boot time), then honor an explicit `--delay`
/// as extra settle. If no `Hello` arrives, fall back to `--delay` (or a default)
/// so we don't send into a link that isn't up yet. Returns the announced protocol
/// version if a `Hello` arrived, so callers can enforce the lockstep rule.
pub(crate) fn await_ready(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    delay: Option<u64>,
) -> Option<u8> {
    let version = wait_for_hello(sp, dec, HELLO_WAIT);
    if version.is_some() {
        if let Some(ms) = delay {
            std::thread::sleep(Duration::from_millis(ms));
        }
    } else {
        std::thread::sleep(delay.map_or(DEFAULT_SETTLE, Duration::from_millis));
    }
    version
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

/// Read frames until the `ShellResponse` for `cmd_id` completes (`last`), or the idle
/// `timeout` elapses. Non-matching frames (logs/events, other `cmd_id`s) are ignored.
///
/// `timeout` is an *idle* deadline: it is reset every time a matching chunk arrives, so a
/// long command that keeps producing output isn't cut off — only a genuinely silent link
/// times out. The `chunk` index is tracked per response: a gap means a middle chunk was
/// CRC-dropped (the frame decoder silently discards corrupt frames), which would otherwise
/// yield silently-truncated output with result 0 — so we flag it `incomplete` instead.
///
/// Returns `None` only on a hard read error or if no matching chunk ever arrives before the
/// idle timeout. A response that starts but never reaches `last` before the idle timeout also
/// returns `None` (treated as a timeout by callers).
pub(crate) fn read_response(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    cmd_id: u16,
    timeout: Duration,
) -> Option<Response> {
    let mut deadline = Instant::now() + timeout;
    let mut text = String::new();
    let mut next_chunk: u16 = 0; // expected `chunk` index of the next matching frame
    let mut incomplete = false;
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        let nread = match sp.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(_) => return None,
        };
        for &b in &buf[..nread] {
            if let Some(inner) = dec.push(b)
                && let Ok((MsgType::ShellResponse, _, payload)) = decode_frame(inner)
                && let Ok(r) = postcard::from_bytes::<ShellResponse>(payload)
                && r.cmd_id == cmd_id
            {
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
                    return Some(Response {
                        result: r.result,
                        text,
                        incomplete,
                    });
                }
            }
        }
    }
    None
}

// ---- completion (target-authoritative) ------------------------------------

/// An owned copy of a completion result (the wire form borrows the frame buffer).
pub(crate) struct CompletionResult {
    pub(crate) token_start: u16,
    pub(crate) common_prefix: String,
    pub(crate) candidates: Vec<(String, CandidateKind)>,
    pub(crate) more: bool,
}

/// Send a `ShellComplete` and wait for the matching `ShellCompletions`. Shared by the
/// `complete` command and the interactive TAB handler.
pub(crate) fn request_completions(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    line: &str,
    cursor: u16,
    req_id: u16,
    timeout: Duration,
) -> Option<CompletionResult> {
    let mut buf = [0u8; MAX_WIRE];
    let n = encode_frame(
        MsgType::ShellComplete,
        0,
        &ShellComplete {
            req_id,
            line,
            cursor,
        },
        &mut buf,
    )
    .ok()?;
    sp.write_all(&buf[..n]).ok()?;
    sp.flush().ok()?;

    let deadline = Instant::now() + timeout;
    let mut rbuf = [0u8; 256];
    while Instant::now() < deadline {
        let nread = match sp.read(&mut rbuf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(_) => return None,
        };
        for &b in &rbuf[..nread] {
            if let Some(inner) = dec.push(b)
                && let Ok((MsgType::ShellCompletions, _, payload)) = decode_frame(inner)
                && let Ok(c) = postcard::from_bytes::<ShellCompletions>(payload)
                && c.req_id == req_id
            {
                return Some(CompletionResult {
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
    }
    None
}
