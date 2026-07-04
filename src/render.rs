//! Host-side rendering of the decoded frame stream for the non-TUI commands (`logs`,
//! `events`, `monitor`). Owns the streaming read loop, sequence/decode-error accounting,
//! the protocol-version mismatch banner, and the line formatting — which is deliberately
//! shared with the TUI (`log_line` / `event_fields` / `level_label`) so the two frontends
//! render a Log or Event identically.
//!
//! Output is either human text (optionally ANSI-colored) or NDJSON (`--json`): one JSON
//! object per line, so `tower logs --json | jq` works. JSON is emitted by hand (no
//! serializer dep) with minimal string escaping — the fields are small and known.

use std::io::Write;

use anyhow::Result;
use clap::ValueEnum;

use tower_protocol::msg::{Dropped, Event, Hello, Level, Log, Print};
use tower_protocol::{FrameDecoder, MsgType, decode_frame};

use crate::session::Transport;

/// Which entity stream to render.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum View {
    Logs,
    Events,
}

/// When to emit ANSI color escapes. `Auto` (the default) colors only when stdout is a
/// real terminal and `NO_COLOR` is unset — so `tower logs > file` / `| grep` stay clean.
#[derive(Clone, Copy, PartialEq, ValueEnum)]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    /// Resolve to a concrete on/off, honoring the `NO_COLOR` convention (https://no-color.org)
    /// and TTY detection for `Auto`.
    fn enabled(self) -> bool {
        use std::io::IsTerminal;
        match self {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
            }
        }
    }
}

/// Resolve the effective color choice: the deprecated `--no-colors` flag, if present, forces
/// off; otherwise `--color`'s auto/always/never decides (auto = TTY && `NO_COLOR` unset).
pub(crate) fn resolve_color(color: ColorMode, no_colors: bool) -> bool {
    if no_colors { false } else { color.enabled() }
}

/// How the streaming commands render frames: human-readable text (with or without color)
/// or one JSON object per line.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum OutputMode {
    Text { colors: bool },
    Json,
}

// ---- shared line formatting (also used by the TUI) ----

/// The uptime prefix `[   sss.mmm]` a Log line carries, from the device's microsecond clock.
fn uptime_prefix(uptime_us: u64) -> String {
    let secs = uptime_us / 1_000_000;
    let ms = (uptime_us % 1_000_000) / 1_000;
    format!("[{secs:>5}.{ms:03}]")
}

/// The fixed-width severity label for a level (padded so columns line up).
pub(crate) fn level_label(level: Level) -> &'static str {
    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN ",
        Level::Info => "INFO ",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

/// The ANSI SGR color code for a level (used by the text renderer's `paint`).
fn level_ansi(level: Level) -> u8 {
    match level {
        Level::Error => 31,
        Level::Warn => 33,
        Level::Info => 32,
        Level::Debug => 36,
        Level::Trace => 90,
    }
}

/// Format a Log's body **without** the leading wall-clock time or color, so both the CLI and
/// the TUI lay a Log out the same way: `[uptime] LEVEL module: message`. The caller prepends
/// its own timestamp and applies its own coloring.
pub(crate) fn log_line(l: &Log) -> String {
    format!(
        "{} {} {}: {}",
        uptime_prefix(l.uptime_us),
        level_label(l.level),
        l.module,
        l.message
    )
}

/// Format an Event's `key=value` fields into one space-joined string (shared CLI/TUI).
pub(crate) fn event_fields(e: &Event) -> String {
    e.fields
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wrap `s` in an ANSI SGR color if `colors`, else return it plain.
fn paint(s: &str, code: u8, colors: bool) -> String {
    if colors {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

// ---- JSON (NDJSON) ----

/// Minimal JSON string escaping for the small, known fields we emit.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- streaming read loop ----

/// Per-stream render state: sequence tracking, plus mismatch/decode-error accounting so a
/// protocol drift is surfaced loudly rather than as garbled output.
#[derive(Default)]
pub(crate) struct RenderState {
    pub(crate) last_seq: Option<u16>,
    /// Frames the codec refused to decode (`BadVersion`/`BadType`/`BadCrc`/…). A burst of
    /// these on an otherwise-live link is the classic symptom of a protocol tag mismatch.
    pub(crate) decode_failures: u64,
    /// Whether we've already shouted about a protocol-version mismatch (warn once per stream).
    warned_mismatch: bool,
}

/// Emit the loud, persistent protocol-mismatch banner (stderr). `device` is the version the
/// firmware announced in its `Hello`; we compare against the version this binary was built
/// against. A mismatch means postcard will silently mis-decode — see the lockstep rule.
pub(crate) fn warn_protocol_mismatch(device: u8) {
    eprintln!("[tower] ============================================================");
    eprintln!(
        "[tower] PROTOCOL VERSION MISMATCH: device speaks v{device}, this `tower` was built for v{}.",
        tower_protocol::PROTOCOL_VERSION
    );
    eprintln!("[tower] postcard is NOT self-describing — frames will silently mis-decode. Rebuild");
    eprintln!(
        "[tower] `tower` and the firmware against the SAME tower-protocol tag (the lockstep rule)."
    );
    eprintln!("[tower] ============================================================");
}

/// Stream the framed link to stdout until the port errors, rendering `view` in `mode`.
pub(crate) fn read_loop(
    sp: &mut (impl Transport + ?Sized),
    mode: OutputMode,
    view: View,
) -> Result<()> {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 512];
    let mut st = RenderState::default();
    loop {
        let n = match sp.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        for &b in &buf[..n] {
            if let Some(inner) = dec.push(b) {
                render(inner, mode, view, &mut st);
            }
        }
    }
}

/// Decode one frame and render it per `view`/`mode`, updating sequence + error accounting.
/// (Kept `pub(crate)` so unit tests can drive it directly with synthesized frames.)
pub(crate) fn render(inner: &[u8], mode: OutputMode, view: View, st: &mut RenderState) {
    let (mt, seq, payload) = match decode_frame(inner) {
        Ok(t) => t,
        Err(e) => {
            st.decode_failures += 1;
            // A version-tagged codec error is the smoking gun for a tag mismatch; surface the
            // running count so a wall of them reads as one diagnosable cause, not noise. In
            // JSON mode make it a first-class NDJSON record (like device-side `Dropped`) so
            // `| jq` consumers see it on stdout, not just humans on stderr.
            match mode {
                OutputMode::Json => println!(
                    "{{\"type\":\"decode_error\",\"error\":{},\"count\":{}}}",
                    json_str(&format!("{e:?}")),
                    st.decode_failures
                ),
                OutputMode::Text { .. } => eprintln!(
                    "[tower] dropped a corrupt frame: {e:?} ({} undecodable frame(s) — version mismatch?)",
                    st.decode_failures
                ),
            }
            return;
        }
    };
    // A `Hello` marks a new device session: the firmware resets its per-session `seq`
    // (the dynamic console re-emits `Hello` on every USB plug-in), so re-baseline our
    // tracking on it rather than reporting a spurious gap across the reconnect.
    if matches!(mt, MsgType::Hello) {
        st.last_seq = None;
    }
    if let Some(prev) = st.last_seq {
        let expected = prev.wrapping_add(1);
        if seq != expected {
            match mode {
                OutputMode::Json => {
                    println!("{{\"type\":\"seq_gap\",\"expected\":{expected},\"got\":{seq}}}")
                }
                OutputMode::Text { .. } => {
                    eprintln!("[tower] seq gap: expected {expected}, got {seq}")
                }
            }
        }
    }
    st.last_seq = Some(seq);

    match mt {
        MsgType::Hello => {
            if let Ok(h) = postcard::from_bytes::<Hello>(payload) {
                eprintln!(
                    "[tower] hello: firmware {:?}, protocol v{}",
                    h.firmware_version, h.protocol_version
                );
                if h.protocol_version != tower_protocol::PROTOCOL_VERSION && !st.warned_mismatch {
                    warn_protocol_mismatch(h.protocol_version);
                    st.warned_mismatch = true;
                }
            }
        }
        MsgType::Log if view == View::Logs => {
            if let Ok(l) = postcard::from_bytes::<Log>(payload) {
                emit_log(&l, mode);
            }
        }
        MsgType::Print if view == View::Logs => {
            if let Ok(p) = postcard::from_bytes::<Print>(payload) {
                emit_print(&p, mode);
            }
        }
        MsgType::Dropped if view == View::Logs => {
            if let Ok(d) = postcard::from_bytes::<Dropped>(payload) {
                emit_dropped(&d, mode);
            }
        }
        MsgType::Event if view == View::Events => {
            if let Ok(e) = postcard::from_bytes::<Event>(payload) {
                emit_event(&e, mode);
            }
        }
        _ => {} // frames not relevant to this view (or later-phase types)
    }
}

fn emit_log(l: &Log, mode: OutputMode) {
    match mode {
        OutputMode::Text { colors } => {
            // Re-color just the level label inside the shared layout.
            let body = log_line(l);
            let label = level_label(l.level);
            let colored = body.replacen(label, &paint(label, level_ansi(l.level), colors), 1);
            println!("{} {colored}", now());
        }
        OutputMode::Json => {
            println!(
                "{{\"type\":\"log\",\"uptime_us\":{},\"level\":{},\"module\":{},\"message\":{}}}",
                l.uptime_us,
                json_str(level_label(l.level).trim()),
                json_str(l.module),
                json_str(l.message)
            );
        }
    }
}

fn emit_print(p: &Print, mode: OutputMode) {
    match mode {
        OutputMode::Text { .. } => {
            print!("{}", p.text);
            let _ = std::io::stdout().flush();
        }
        OutputMode::Json => println!("{{\"type\":\"print\",\"text\":{}}}", json_str(p.text)),
    }
}

fn emit_dropped(d: &Dropped, mode: OutputMode) {
    match mode {
        OutputMode::Text { colors } => eprintln!(
            "{} {} log frame(s) dropped (device queue full)",
            paint("⚠", 33, colors),
            d.count
        ),
        OutputMode::Json => println!("{{\"type\":\"dropped\",\"count\":{}}}", d.count),
    }
}

fn emit_event(e: &Event, mode: OutputMode) {
    match mode {
        OutputMode::Text { colors } => println!(
            "{} {} {}  {}",
            now(),
            paint("EVENT", 35, colors),
            e.name,
            event_fields(e)
        ),
        OutputMode::Json => {
            let mut fields = String::from("{");
            for (i, (k, v)) in e.fields.iter().enumerate() {
                if i > 0 {
                    fields.push(',');
                }
                fields.push_str(&format!("{}:{}", json_str(k), json_str(v)));
            }
            fields.push('}');
            println!(
                "{{\"type\":\"event\",\"name\":{},\"fields\":{fields}}}",
                json_str(e.name)
            );
        }
    }
}

// ---- monitor (transport debugging) ----

/// Hex-dump a byte slice as contiguous lowercase pairs (no separators).
pub(crate) fn hexline(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_protocol::encode_frame;
    use tower_protocol::msg::Hello;

    const TEXT: OutputMode = OutputMode::Text { colors: false };

    fn frame<T: serde::Serialize>(mt: MsgType, seq: u16, payload: &T) -> Vec<u8> {
        let mut buf = [0u8; tower_protocol::MAX_WIRE];
        let n = encode_frame(mt, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn hello_frame(protocol_version: u8) -> Vec<u8> {
        frame(
            MsgType::Hello,
            0,
            &Hello {
                protocol_version,
                firmware_version: "test",
            },
        )
    }

    fn log_frame(seq: u16, msg: &str) -> Vec<u8> {
        frame(
            MsgType::Log,
            seq,
            &Log {
                level: Level::Info,
                uptime_us: 0,
                module: "t",
                message: msg,
            },
        )
    }

    /// Feed whole wire frames through a fresh decoder into `render`, accumulating state.
    fn feed_render(st: &mut RenderState, frames: &[u8]) {
        let mut d = FrameDecoder::new();
        for &b in frames {
            if let Some(inner) = d.push(b) {
                render(inner, TEXT, View::Logs, st);
            }
        }
    }

    #[test]
    fn render_rebaselines_seq_on_hello() {
        // A Hello resets seq tracking, so a fresh session starting at seq 0 after a prior
        // stream at seq 100 is NOT reported as a gap.
        let mut st = RenderState::default();
        feed_render(&mut st, &log_frame(100, "a"));
        assert_eq!(st.last_seq, Some(100));
        feed_render(&mut st, &hello_frame(tower_protocol::PROTOCOL_VERSION));
        // Hello re-baselined: last_seq is the Hello's own seq (0), no spurious gap.
        assert_eq!(st.last_seq, Some(0));
        assert_eq!(st.decode_failures, 0);
    }

    #[test]
    fn render_counts_decode_failures() {
        // A frame that COBS-decodes but fails CRC must be counted as a decode failure (the
        // symptom we surface as "N undecodable frames — version mismatch?"). Flip the CRC
        // byte just before the trailing 0x00: it stays a data byte in the COBS stream, so
        // COBS still decodes, but the CRC check then fails. XOR 0x80 avoids making a 0x00.
        let mut st = RenderState::default();
        let mut bytes = log_frame(0, "ok");
        let last = bytes.len() - 2;
        bytes[last] ^= 0x80;
        feed_render(&mut st, &bytes);
        assert_eq!(st.decode_failures, 1);
    }

    /// Like [`feed_render`] but in an explicit [`OutputMode`], for the JSON-diagnostics paths.
    fn feed_render_mode(st: &mut RenderState, mode: OutputMode, frames: &[u8]) {
        let mut d = FrameDecoder::new();
        for &b in frames {
            if let Some(inner) = d.push(b) {
                render(inner, mode, View::Logs, st);
            }
        }
    }

    #[test]
    fn json_mode_emits_and_counts_decode_error() {
        // In JSON mode a corrupt frame becomes a `decode_error` NDJSON record on stdout (like
        // the device-side `dropped` record) rather than only a human stderr line — here we pin
        // that it still runs the JSON branch and keeps the running count.
        let mut st = RenderState::default();
        let mut bytes = log_frame(0, "ok");
        let last = bytes.len() - 2;
        bytes[last] ^= 0x80; // break the CRC (stays COBS-decodable)
        feed_render_mode(&mut st, OutputMode::Json, &bytes);
        assert_eq!(st.decode_failures, 1);
    }

    #[test]
    fn json_mode_detects_seq_gap() {
        // A seq discontinuity in JSON mode emits a `seq_gap` NDJSON record on stdout; assert the
        // gap is detected (state advances to the latest seq) without a Hello re-baseline.
        let mut st = RenderState::default();
        feed_render_mode(&mut st, OutputMode::Json, &log_frame(1, "a"));
        feed_render_mode(&mut st, OutputMode::Json, &log_frame(5, "b")); // gap 2..=4
        assert_eq!(st.last_seq, Some(5));
        assert_eq!(st.decode_failures, 0);
    }

    #[test]
    fn color_mode_never_and_always() {
        assert!(!ColorMode::Never.enabled());
        assert!(ColorMode::Always.enabled());
    }

    #[test]
    fn resolve_color_no_colors_forces_off() {
        // The deprecated --no-colors flag always wins, even over --color always.
        assert!(!resolve_color(ColorMode::Always, true));
        assert!(resolve_color(ColorMode::Always, false));
    }

    #[test]
    fn json_escapes_control_and_quotes() {
        assert_eq!(json_str("a\"b\\c\n"), r#""a\"b\\c\n""#);
    }

    #[test]
    fn log_line_layout_is_stable() {
        let l = Log {
            level: Level::Info,
            uptime_us: 1_234_567,
            module: "radio",
            message: "up",
        };
        assert_eq!(log_line(&l), "[    1.234] INFO  radio: up");
    }

    #[test]
    fn hexline_is_contiguous_lowercase() {
        assert_eq!(hexline(&[0x00, 0xAB, 0x0f]), "00ab0f");
    }
}
