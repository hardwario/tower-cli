//! The framed console **session** over a byte transport: the boot-`Hello` handshake, the
//! `ShellResponse` reassembler, the `ShellCompletions` request/response, and the FOTA
//! host-proxy serve loop. Everything here is generic over [`Transport`] (`Read + Write`), so
//! it runs unchanged over a real `Box<dyn serialport::SerialPort>` in production and over an
//! in-memory mock in tests — which is what makes the session logic testable without hardware.
//!
//! What is deliberately *not* here: opening/enumerating ports and the reset pulse (that's
//! `port`), rendering the stream (that's `render`), and the rustyline/clap glue for the
//! interactive shell and FOTA command wiring (that stays in the command layer in `main`).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use tower_protocol::fota::{FOTA_MANIFEST_OFFSET, Manifest, SHA256_LEN, SIGNED_LEN};
use tower_protocol::msg::{CandidateKind, Hello, ShellComplete, ShellCompletions, ShellResponse};
use tower_protocol::{
    FrameDecoder, MAX_WIRE, MsgType, decode_frame, encode_frame, encode_frame_raw,
};

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

// ---- FOTA host-proxy serve ------------------------------------------------

/// The FOTA image digest carried in `Manifest::sha256`: **SHA-512 truncated to 256 bits**.
/// This MUST match `firmware/tools/fota-sign::image_digest` (and the device bootloader),
/// which reuse salty's SHA-512 rather than carrying a second hash engine — so we validate
/// against the same value the signer committed to.
pub(crate) fn image_digest(image: &[u8]) -> [u8; SHA256_LEN] {
    use sha2::{Digest, Sha512};
    let full: [u8; 64] = Sha512::digest(image).into();
    let mut out = [0u8; SHA256_LEN];
    out.copy_from_slice(&full[..SHA256_LEN]);
    out
}

/// Validate that the signed manifest actually describes the image we're about to serve, so a
/// stale/mismatched `--manifest` fails here at startup rather than as a silent verify failure
/// on the node hours into an OTA. Checks the byte length, that the manifest header parses, that
/// `size` equals the image length, and that `sha256` equals our recomputed digest. We do NOT
/// re-verify the Ed25519 signature (that's the node bootloader's job, against the vendor key);
/// we only confirm the image/manifest pairing is self-consistent.
pub(crate) fn validate_manifest(image: &[u8], manifest: &[u8]) -> Result<()> {
    if manifest.len() != SIGNED_LEN {
        bail!(
            "manifest must be {SIGNED_LEN} bytes (a `fota-sign` .fmanifest), got {}",
            manifest.len()
        );
    }
    let m = Manifest::decode(manifest)
        .context("manifest header invalid (bad magic/format — not a `fota-sign` .fmanifest?)")?;
    if m.size as usize != image.len() {
        bail!(
            "manifest/image mismatch: manifest size {} B but image is {} B (wrong --manifest for this --image?)",
            m.size,
            image.len()
        );
    }
    let digest = image_digest(image);
    if m.sha256 != digest {
        bail!(
            "manifest/image mismatch: sha256 differs from the image digest (stale manifest for this image?)"
        );
    }
    Ok(())
}

/// Build the `FotaData` payload answering a `FotaReq{offset,len}`: the echoed `offset`
/// followed by the requested bytes — the whole signed manifest for the sentinel offset,
/// otherwise the image slice `[offset, offset+len)` **clamped to the image bounds** (a
/// request past EOF yields just the echoed offset with no bytes; a partial tail is
/// truncated to what exists). Pure, so it can be unit-tested against the offset math.
pub(crate) fn fota_data_payload(offset: u32, len: usize, image: &[u8], manifest: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + len);
    payload.extend_from_slice(&offset.to_le_bytes());
    if offset == FOTA_MANIFEST_OFFSET {
        payload.extend_from_slice(manifest);
    } else {
        let start = (offset as usize).min(image.len());
        let end = start.saturating_add(len).min(image.len());
        payload.extend_from_slice(&image[start..end]);
    }
    payload
}

/// Answer `FotaReq` frames with `FotaData` until the transport errors or reaches EOF.
pub(crate) fn fota_serve_loop(
    sp: &mut (impl Transport + ?Sized),
    image: &[u8],
    manifest: &[u8],
) -> Result<()> {
    let mut dec = FrameDecoder::new();
    let mut rbuf = [0u8; 512];
    let mut seq: u16 = 0;
    let mut served_to = 0usize; // high-water of image bytes served, for the progress line
    loop {
        let n = match sp.read(&mut rbuf) {
            // A real serial port returns `TimedOut` when idle, never `Ok(0)` on a live port;
            // `Ok(0)` therefore means EOF (a drained test transport), so end the loop cleanly.
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        for &b in &rbuf[..n] {
            let Some(inner) = dec.push(b) else { continue };
            let Ok((MsgType::FotaReq, _seq, p)) = decode_frame(inner) else {
                continue;
            };
            if p.len() < 6 {
                continue;
            }
            let offset = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            let len = u16::from_le_bytes([p[4], p[5]]) as usize;

            let payload = fota_data_payload(offset, len, image, manifest);
            if offset == FOTA_MANIFEST_OFFSET {
                eprintln!("[tower] -> manifest ({} B)", manifest.len());
            } else {
                served_to = served_to.max((offset as usize).saturating_add(len).min(image.len()));
                eprint!("\r[tower] serving {served_to}/{} B", image.len());
                let _ = std::io::stderr().flush();
            }

            let mut frame = [0u8; MAX_WIRE];
            match encode_frame_raw(MsgType::FotaData, seq, &payload, &mut frame) {
                Ok(fn_len) => {
                    sp.write_all(&frame[..fn_len])?;
                    sp.flush()?;
                    seq = seq.wrapping_add(1);
                }
                Err(e) => eprintln!("\n[tower] encode FotaData failed: {e:?}"),
            }
        }
    }
}
