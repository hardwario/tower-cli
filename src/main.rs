//! `tower` — HARDWARIO TOWER host CLI: devices, logs/events, shell/exec, the console TUI,
//! and flash/erase/reset (via the jolt engine).
//!
//! The firmware's UART is always framed (`tower-protocol`: COBS + CRC + postcard),
//! so a plain terminal shows binary — this tool decodes it. The same `FrameDecoder`
//! / `decode_frame` run on both ends, so the wire format can't drift.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Editor, Helper};
use tower_protocol::msg::{CandidateKind, ShellCommand};
use tower_protocol::{FrameDecoder, MsgType, decode_frame, encode_frame};

mod port;
mod render;
mod session;
mod tui;

use port::{devices, open_console, pick_port};
use render::{
    ColorMode, OutputMode, View, hexline, read_loop, resolve_color, warn_protocol_mismatch,
};
#[cfg(test)]
use session::await_ready;
use session::{
    CompletionOutcome, ReadOutcome, Readiness, await_ready_with, read_response, request_completions,
};

// ---- exit-code contract ---------------------------------------------------
//
// A documented, stable exit-code contract so scripts/CI can branch on *why* a
// command failed, not just that it did. `main` returns an `ExitCode` built from
// these; commands surface failures as `Err` (→ EXIT_ERROR) or return one of the
// specific codes below. `exec` additionally forwards a device-reported non-zero
// `result` verbatim (1..=123), which is why the reserved codes start at 124.
//
//   0    ok
//   1    tool error (I/O, bad file, encode/decode)
//   2    usage error (bad args — emitted by clap itself)
//   124  device command timed out (no response at all; a truncated response is 1)
//   125  protocol-version mismatch (device speaks a different tower-protocol tag)
//
// So a device `result` can't be confused with the reserved codes, `exec` clamps a
// device-reported non-zero result into 1..=123 (see `exec_cmd`); 124/125 stay reserved.
const EXIT_OK: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_DEVICE_TIMEOUT: u8 = 124;
/// A freshly-reset device announced (or its frames were tagged with) a `tower-protocol`
/// version this build doesn't speak — every frame would silently mis-decode, so `exec`
/// refuses rather than emitting junk. Distinct from a plain timeout so CI can branch on it.
const EXIT_PROTOCOL_MISMATCH: u8 = 125;

/// Default per-command response timeout (`--timeout`), in milliseconds.
///
/// Sized above the firmware's worst-case EEPROM compaction stall (~5.2 s with the whole
/// chip frozen — see tower-firmware docs/storage.md): any command that writes EEPROM
/// (`settings set`, …) can land on the append that fills the KV half and pay for the whole
/// compaction before it can respond. With the old 1.5 s default such a command *executed*
/// but the CLI reported "no response (timeout)" — a phantom failure (2026-07-05). It is an
/// *idle* timeout, so healthy commands still return in milliseconds; only a genuinely mute
/// link waits the full 7 s.
const DEFAULT_TIMEOUT_MS: u64 = 7000;

/// Animated stderr feedback for the post-reset boot wait (up to `session::HELLO_WAIT` = 8 s;
/// a fallback EEPROM compaction can legitimately hold the boot ~5 s). Silent for fast boots:
/// nothing renders before 600 ms. On a TTY it redraws in place (`\r`); piped, it prints one
/// static line so scripts/CI logs aren't spammed with animation frames.
struct BootTicker {
    shown: bool,
    is_tty: bool,
    last: Duration,
}

impl BootTicker {
    fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            shown: false,
            is_tty: std::io::stderr().is_terminal(),
            last: Duration::ZERO,
        }
    }

    fn tick(&mut self, el: Duration) {
        if el < Duration::from_millis(600) || (el - self.last) < Duration::from_millis(200) {
            return;
        }
        self.last = el;
        if self.is_tty {
            eprint!(
                "\r[tower] waiting for the device to boot… {:.1}s ",
                el.as_secs_f32()
            );
            self.shown = true;
        } else if !self.shown {
            eprintln!("[tower] waiting for the device to boot (a compaction can take ~5 s)…");
            self.shown = true;
        }
    }

    /// Close out the ticker line: report the boot time on success, or clear the line so a
    /// following warning/diagnostic starts clean. No-op if nothing was ever drawn.
    fn finish(&mut self, readiness: &Readiness) {
        if !(self.shown && self.is_tty) {
            return;
        }
        match readiness {
            Readiness::Hello(_) => {
                eprintln!(
                    "\r[tower] device booted in {:.1}s                    ",
                    self.last.as_secs_f32()
                )
            }
            // Clear the in-place line; the caller's own warning/mismatch output follows.
            _ => eprint!("\r\u{1b}[2K"),
        }
    }
}

#[derive(Parser)]
#[command(name = "tower", version, about = "HARDWARIO TOWER console host")]
struct Cli {
    /// Serial device (auto-detected when exactly one USB serial device is present).
    // The field stays `port` since it holds a serial-port path; the user-facing flag is `--device`.
    #[arg(short = 'd', long = "device", value_name = "DEVICE", global = true)]
    port: Option<String>,
    /// Don't auto-reconnect on the streaming commands (`logs`/`events`):
    /// exit when the link drops instead of retrying. (The first open is always fatal.)
    #[arg(long, global = true)]
    no_reconnect: bool,
    /// No subcommand opens the console TUI (the documented bare-`tower` UX).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List connected serial devices.
    Devices,
    /// Stream device logs (and `print!` output) to stdout.
    Logs {
        /// When to colorize output.
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,
        /// Disable ANSI colors (deprecated alias for `--color never`).
        #[arg(long, hide = true)]
        no_colors: bool,
        /// Emit one JSON object per line (NDJSON) instead of formatted text.
        #[arg(long)]
        json: bool,
        /// Send this text to the device once on connect (RX probe / quick poke).
        #[arg(long)]
        send: Option<String>,
        /// Reboot the application on connect (NRST pulse) so you see it come up from the start.
        #[arg(long)]
        reset: bool,
        /// With --reset and --send: extra ms to settle after the boot Hello (or fallback wait if none).
        #[arg(long, value_name = "MS", requires = "reset")]
        delay: Option<u64>,
    },
    /// Stream device events (structured key=value) to stdout.
    Events {
        /// When to colorize output.
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,
        /// Disable ANSI colors (deprecated alias for `--color never`).
        #[arg(long, hide = true)]
        no_colors: bool,
        /// Emit one JSON object per line (NDJSON) instead of formatted text.
        #[arg(long)]
        json: bool,
        /// Reboot the application on connect (NRST pulse) so you see it come up from the start.
        #[arg(long)]
        reset: bool,
    },
    /// Open an interactive shell (commands start with `/`).
    Shell {
        /// Reboot the application before the shell opens, waiting for it to come up.
        #[arg(long)]
        reset: bool,
        /// With --reset: extra ms to settle after the boot Hello (or fallback wait if none).
        #[arg(long, value_name = "MS", requires = "reset")]
        delay: Option<u64>,
        /// Per-command idle response timeout in ms (reset each time a matching chunk arrives).
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_TIMEOUT_MS)]
        timeout: u64,
    },
    /// Run one shell command and print its response, then exit (for scripts / CI).
    Exec {
        /// The command line, e.g. "/system/resource print".
        line: String,
        /// Reboot the application first, waiting for it to come up before sending (clean CI state).
        #[arg(long)]
        reset: bool,
        /// With --reset: extra ms to settle after the boot Hello (or fallback wait if none).
        #[arg(long, value_name = "MS", requires = "reset")]
        delay: Option<u64>,
        /// Idle response timeout in ms (reset each time a matching chunk arrives).
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_TIMEOUT_MS)]
        timeout: u64,
    },
    /// Open the full-screen TUI console (logs + events + shell).
    Console {
        /// Reboot the application on connect (NRST pulse) so you see it come up from the start.
        #[arg(long)]
        reset: bool,
    },
    /// Ask the target to complete a partial command line (target-authoritative).
    Complete {
        /// The partial line (cursor is taken at its end).
        line: String,
    },
    /// Transport debugging: dump frames (or, with --hex, every raw byte).
    Monitor {
        /// Dump raw received bytes as hex instead of decoded frames.
        #[arg(long)]
        hex: bool,
        /// Reboot the application on connect (NRST pulse) so you capture its startup bytes.
        #[arg(long)]
        reset: bool,
    },
    /// Flash a raw firmware `.bin` over the STM32 UART bootloader (via jolt).
    Flash {
        /// Path to the raw firmware `.bin`.
        file: PathBuf,
        /// Skip erasing before writing.
        #[arg(long)]
        no_erase: bool,
        /// Skip read-back verification.
        #[arg(long)]
        no_verify: bool,
        /// Do not reset/jump into the application after flashing.
        #[arg(long)]
        no_run: bool,
        /// Use the bootloader Go command instead of a hardware reset to start the app.
        #[arg(long)]
        go: bool,
        /// Print bootloader connect diagnostics.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Erase the entire device flash over the STM32 UART bootloader (via jolt).
    Erase {
        /// Print bootloader connect diagnostics.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Reset the device into the application (default) or the system bootloader.
    Reset {
        /// Reset into the system bootloader instead of the application.
        #[arg(long)]
        bootloader: bool,
    },
}

fn main() -> ExitCode {
    // Rust's runtime installs SIG_IGN for SIGPIPE, so writing to a closed downstream pipe
    // (`tower logs | head`) surfaces as an EPIPE that print!/println! unwrap into a panic
    // (exit 101, "failed printing to stdout: Broken pipe"). Restore the default disposition
    // so we die quietly on the signal like every other Unix filter. No-op on non-unix.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("[tower] error: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Dispatch a parsed command. Returns the process exit code (see the exit-code contract
/// above); an `Err` here becomes `EXIT_ERROR`. A bare `tower` (no subcommand) opens the TUI.
fn run(cli: Cli) -> Result<u8> {
    let reconnect = !cli.no_reconnect;
    match cli.cmd.unwrap_or(Cmd::Console { reset: false }) {
        Cmd::Devices => devices().map(|()| EXIT_OK),
        Cmd::Logs {
            color,
            no_colors,
            json,
            send,
            reset,
            delay,
        } => stream(
            cli.port,
            output_mode(color, no_colors, json),
            View::Logs,
            send,
            reset,
            delay,
            reconnect,
        )
        .map(|()| EXIT_OK),
        Cmd::Events {
            color,
            no_colors,
            json,
            reset,
        } => stream(
            cli.port,
            output_mode(color, no_colors, json),
            View::Events,
            None,
            reset,
            None,
            reconnect,
        )
        .map(|()| EXIT_OK),
        Cmd::Shell {
            reset,
            delay,
            timeout,
        } => shell(cli.port, reset, delay, Duration::from_millis(timeout)),
        Cmd::Exec {
            line,
            reset,
            delay,
            timeout,
        } => exec_cmd(cli.port, line, reset, delay, Duration::from_millis(timeout)),
        Cmd::Console { reset } => tui::run(pick_port(cli.port)?, reset).map(|()| EXIT_OK),
        Cmd::Complete { line } => complete_cmd(cli.port, line),
        Cmd::Monitor { hex, reset } => monitor(cli.port, hex, reset).map(|()| EXIT_OK),
        Cmd::Flash {
            file,
            no_erase,
            no_verify,
            no_run,
            go,
            verbose,
        } => {
            flash_cmd(cli.port, file, !no_erase, !no_verify, !no_run, go, verbose).map(|()| EXIT_OK)
        }
        Cmd::Erase { verbose } => erase_cmd(cli.port, verbose).map(|()| EXIT_OK),
        Cmd::Reset { bootloader } => reset_cmd(cli.port, bootloader).map(|()| EXIT_OK),
    }
}

/// Build the streaming output mode: `--json` wins (structured NDJSON); otherwise text with
/// color resolved from `--color`/`--no-colors`.
fn output_mode(color: ColorMode, no_colors: bool, json: bool) -> OutputMode {
    if json {
        OutputMode::Json
    } else {
        OutputMode::Text {
            colors: resolve_color(color, no_colors),
        }
    }
}

fn stream(
    port: Option<String>,
    mode: OutputMode,
    view: View,
    send: Option<String>,
    reset: bool,
    delay: Option<u64>,
    reconnect: bool,
) -> Result<()> {
    let port = pick_port(port)?;
    // The FIRST open is fatal: a nonexistent --device must exit 1, not retry forever
    // (that used to spin silently). Enter the reconnect loop only after one success.
    // --reset fires once, on that initial attach — not on every auto-reconnect, or a
    // flaky link would turn into a reboot loop.
    let mut sp = open_console(&port, reset)?;
    let mut first = true;
    loop {
        eprintln!("[tower] connected {port}");
        // `--send` fires ONCE, on the initial attach (as documented) — not on every
        // auto-reconnect. Re-sending on each reconnect would double-poke the device and, worse,
        // skip the readiness wait (which only runs on `first`), racing the boot.
        if first && let Some(s) = &send {
            // On a reset attach, wait for the device to come up before poking it — and
            // surface a version mismatch immediately (the stream continues; render's
            // decode-failure path keeps counting, but the banner names the cause up front).
            if reset {
                let mut dec = FrameDecoder::new();
                let mut ticker = BootTicker::new();
                let readiness = await_ready_with(&mut *sp, &mut dec, delay, |el| ticker.tick(el));
                ticker.finish(&readiness);
                match readiness {
                    Readiness::BadVersion(got) => warn_protocol_mismatch(got),
                    Readiness::Hello(v) if v != tower_protocol::PROTOCOL_VERSION => {
                        warn_protocol_mismatch(v)
                    }
                    Readiness::Hello(_) | Readiness::Timeout => {}
                }
            }
            let _ = sp.write_all(s.as_bytes());
            let _ = sp.flush();
            eprintln!("[tower] sent {} byte(s)", s.len());
        }
        if let Err(e) = read_loop(&mut *sp, mode, view) {
            eprintln!("[tower] {port} lost: {e}");
        }
        first = false;
        if !reconnect {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(800));
        eprintln!("[tower] reconnecting…");
        // Reopen without re-resetting; tolerate a transient failure and keep retrying.
        match open_console(&port, false) {
            Ok(reopened) => sp = reopened,
            Err(e) => {
                eprintln!("[tower] {e}");
                continue;
            }
        }
    }
}

// ---- interactive shell ----------------------------------------------------

/// Shared serial connection — the TAB completer and the command loop both use it.
struct Conn {
    sp: Box<dyn serialport::SerialPort>,
    dec: FrameDecoder,
    req_id: u16,
}

/// rustyline helper: TAB completion delegates entirely to the target.
struct ShellHelper {
    conn: Rc<RefCell<Conn>>,
}

impl Completer for ShellHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let mut conn = self.conn.borrow_mut();
        conn.req_id = conn.req_id.wrapping_add(1);
        let req_id = conn.req_id;
        let Conn { sp, dec, .. } = &mut *conn;
        match request_completions(
            &mut **sp,
            dec,
            line,
            pos as u16,
            req_id,
            Duration::from_millis(800),
        ) {
            CompletionOutcome::Completions(r) => {
                let pairs = r
                    .candidates
                    .into_iter()
                    .map(|(text, kind)| {
                        let sep = match kind {
                            CandidateKind::Menu => "/",
                            CandidateKind::Command => " ",
                            CandidateKind::Arg => "=",
                            CandidateKind::Value => "",
                        };
                        Pair {
                            display: text.clone(),
                            replacement: format!("{text}{sep}"),
                        }
                    })
                    .collect();
                Ok((r.token_start as usize, pairs))
            }
            // Timeout (mismatch or not): offer no candidates. The per-command path
            // reports a lockstep mismatch loudly; TAB shouldn't spam mid-edit.
            CompletionOutcome::Timeout { .. } => Ok((pos, Vec::new())),
        }
    }
}

impl Hinter for ShellHelper {
    type Hint = String;
}
impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

fn shell(port: Option<String>, reset: bool, delay: Option<u64>, timeout: Duration) -> Result<u8> {
    let port = pick_port(port)?;
    let mut sp = open_console(&port, reset)?;
    eprintln!("[tower] shell on {port} — TAB completes; commands start with '/'; 'exit' to quit");

    let mut dec = FrameDecoder::new();
    if reset {
        // Don't drop into the prompt until the freshly reset device can answer — and refuse
        // to open it at all on a protocol mismatch (same rule as `exec`): every command
        // would otherwise just time out mute, which is exactly how the stale-binary
        // lockstep incident presented.
        let mut ticker = BootTicker::new();
        let readiness = await_ready_with(&mut *sp, &mut dec, delay, |el| ticker.tick(el));
        ticker.finish(&readiness);
        let mismatch = match readiness {
            Readiness::BadVersion(got) => Some(got),
            Readiness::Hello(v) if v != tower_protocol::PROTOCOL_VERSION => Some(v),
            Readiness::Hello(_) | Readiness::Timeout => None,
        };
        if let Some(v) = mismatch {
            warn_protocol_mismatch(v);
            eprintln!(
                "[tower] protocol version mismatch (device v{v}, tower v{}) — refusing to open the shell; rebuild/repin against the same tower-protocol tag",
                tower_protocol::PROTOCOL_VERSION
            );
            return Ok(EXIT_PROTOCOL_MISMATCH);
        }
    }
    let conn = Rc::new(RefCell::new(Conn { sp, dec, req_id: 0 }));
    let mut rl: Editor<ShellHelper, rustyline::history::DefaultHistory> = Editor::new()?;
    rl.set_helper(Some(ShellHelper { conn: conn.clone() }));

    let mut cmd_id: u16 = 1;
    let mut seq: u16 = 0;
    // The full remediation banner once per session; later mismatches get the short line.
    let mut warned_mismatch = false;
    loop {
        match rl.readline("> ") {
            Ok(input) => {
                let line = input.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "exit" || line == "quit" {
                    break;
                }
                let _ = rl.add_history_entry(line);

                let mut c = conn.borrow_mut();
                let Conn { sp, dec, .. } = &mut *c;
                let mut buf = [0u8; tower_protocol::MAX_WIRE];
                // A line too long to fit one frame fails to encode. That's a *per-line* error,
                // not a session error: print a hint and return to the prompt instead of
                // propagating `?` (which used to tear the whole interactive shell down).
                let n = match encode_frame(
                    MsgType::ShellCommand,
                    seq,
                    &ShellCommand { cmd_id, line },
                    &mut buf,
                ) {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!(
                            "[tower] line too long (max ~{} bytes) — not sent",
                            tower_protocol::MAX_FRAME - 12
                        );
                        continue;
                    }
                };
                seq = seq.wrapping_add(1);
                sp.write_all(&buf[..n])?;
                sp.flush()?;
                match read_response(&mut **sp, dec, cmd_id, timeout) {
                    ReadOutcome::Response(r) => {
                        print!("{}", r.text);
                        if !r.text.is_empty() && !r.text.ends_with('\n') {
                            println!();
                        }
                        if r.result != 0 {
                            eprintln!("[result {}]", r.result);
                        }
                    }
                    // The device answered — with frames from a different tower-protocol
                    // tag. Without this, the interactive shell was the one consumer that
                    // stayed mute on the mismatch the ecosystem got burned by.
                    ReadOutcome::Timeout {
                        bad_version: Some(v),
                    } => {
                        if !warned_mismatch {
                            warn_protocol_mismatch(v);
                            warned_mismatch = true;
                        }
                        eprintln!(
                            "[tower] no response — device speaks protocol v{v}, tower speaks v{} (lockstep mismatch)",
                            tower_protocol::PROTOCOL_VERSION
                        );
                    }
                    ReadOutcome::Timeout { bad_version: None } => {
                        eprintln!("[tower] no response (timeout)")
                    }
                }
                cmd_id = cmd_id.wrapping_add(1);
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            // A real readline failure (e.g. a broken terminal) is an error, not a clean
            // exit — propagate it so the shell exits non-zero rather than swallowing it.
            Err(e) => return Err(anyhow::Error::new(e).context("readline")),
        }
    }
    Ok(EXIT_OK)
}

/// A per-invocation `cmd_id` for the one-shot `exec`: the low 15 bits of the PID, never 0.
/// A previous `tower exec` (a *different* process) used a different PID, so its late/queued
/// `ShellResponse` — which would otherwise carry the same hardcoded `cmd_id` and satisfy this
/// run's wait — no longer matches. 15 bits keeps clear of the `0` sentinel other paths use.
fn exec_cmd_id() -> u16 {
    let id = (std::process::id() & 0x7FFF) as u16;
    if id == 0 { 1 } else { id }
}

/// Run a single shell command non-interactively: send it, print the (reassembled)
/// response, and return an exit code (see the exit-code contract): a device-reported
/// non-zero `result` (clamped into 1..=123 so it can't collide with our reserved 124/125),
/// `EXIT_PROTOCOL_MISMATCH` on a version mismatch, `EXIT_ERROR` on a truncated (chunk-dropped)
/// response, `EXIT_DEVICE_TIMEOUT` on no/incomplete reply, else `EXIT_OK`.
fn exec_cmd(
    port: Option<String>,
    line: String,
    reset: bool,
    delay: Option<u64>,
    timeout: Duration,
) -> Result<u8> {
    let port = pick_port(port)?;
    let mut sp = open_console(&port, reset)?;
    let mut dec = FrameDecoder::new();
    if reset {
        // Wait for the reset device to boot before issuing the command, so the
        // response we capture is from a known-clean state (the CI use case).
        // Fail fast on a protocol-version mismatch: `exec` feeds CI, and a mismatch means
        // every subsequent frame silently mis-decodes — better a clear error than junk. A
        // *real* mismatch is caught at the frame header (`Readiness::BadVersion`) before any
        // `Hello` payload parses; the payload-version check below is a secondary guard.
        let mut ticker = BootTicker::new();
        let readiness = await_ready_with(&mut *sp, &mut dec, delay, |el| ticker.tick(el));
        ticker.finish(&readiness);
        let mismatch = match readiness {
            Readiness::BadVersion(got) => Some(got),
            Readiness::Hello(v) if v != tower_protocol::PROTOCOL_VERSION => Some(v),
            Readiness::Hello(_) | Readiness::Timeout => None,
        };
        if let Some(v) = mismatch {
            warn_protocol_mismatch(v);
            eprintln!(
                "[tower] protocol version mismatch (device v{v}, tower v{}) — refusing to exec; rebuild/repin against the same tower-protocol tag",
                tower_protocol::PROTOCOL_VERSION
            );
            return Ok(EXIT_PROTOCOL_MISMATCH);
        }
    }
    let cmd_id = exec_cmd_id();
    let mut buf = [0u8; tower_protocol::MAX_WIRE];
    let n = encode_frame(
        MsgType::ShellCommand,
        0,
        &ShellCommand {
            cmd_id,
            line: &line,
        },
        &mut buf,
    )
    .map_err(|e| anyhow::anyhow!("encode: {e:?}"))?;
    sp.write_all(&buf[..n])?;
    sp.flush()?;
    match read_response(&mut *sp, &mut dec, cmd_id, timeout) {
        ReadOutcome::Response(r) => {
            print!("{}", r.text);
            if !r.text.is_empty() && !r.text.ends_with('\n') {
                println!();
            }
            if r.incomplete {
                // Output was silently truncated by a dropped chunk — fail even if the
                // device's `last` chunk said result 0 (the reported result is unreliable).
                eprintln!("[tower] response incomplete");
                return Ok(EXIT_ERROR);
            }
            if r.result != 0 {
                eprintln!("[result {}]", r.result);
                // Device results share the byte with our exit code; keep them in 1..=123 so
                // they never masquerade as the reserved timeout (124) code.
                return Ok(r.result.clamp(1, 123));
            }
            Ok(EXIT_OK)
        }
        // Frames arrived but every one was version-rejected: that's the lockstep failure
        // (125), not a mute device (124) — even without --reset, where the Hello-based
        // check above never ran.
        ReadOutcome::Timeout {
            bad_version: Some(v),
        } => {
            warn_protocol_mismatch(v);
            eprintln!(
                "[tower] no response — device speaks protocol v{v}, tower speaks v{} (lockstep mismatch)",
                tower_protocol::PROTOCOL_VERSION
            );
            Ok(EXIT_PROTOCOL_MISMATCH)
        }
        ReadOutcome::Timeout { bad_version: None } => {
            eprintln!("[tower] no response (timeout)");
            Ok(EXIT_DEVICE_TIMEOUT)
        }
    }
}

fn complete_cmd(port: Option<String>, line: String) -> Result<u8> {
    let port = pick_port(port)?;
    // No --reset here (completion is a momentary query), but still establish the
    // run baseline so we don't query a device the bridge left held in reset.
    let mut sp = open_console(&port, false)?;
    let mut dec = FrameDecoder::new();
    let cursor = line.len() as u16;
    match request_completions(
        &mut *sp,
        &mut dec,
        &line,
        cursor,
        1,
        Duration::from_millis(1500),
    ) {
        CompletionOutcome::Completions(r) => {
            println!(
                "token_start={} common_prefix={:?}{}",
                r.token_start,
                r.common_prefix,
                if r.more { " (more…)" } else { "" }
            );
            for (text, kind) in &r.candidates {
                println!("  {kind:?}  {text}");
            }
            Ok(EXIT_OK)
        }
        // Same 125 rule as exec: version-rejected frames mean lockstep, not a dead link.
        CompletionOutcome::Timeout {
            bad_version: Some(v),
        } => {
            warn_protocol_mismatch(v);
            eprintln!(
                "[tower] no completions — device speaks protocol v{v}, tower speaks v{} (lockstep mismatch)",
                tower_protocol::PROTOCOL_VERSION
            );
            Ok(EXIT_PROTOCOL_MISMATCH)
        }
        CompletionOutcome::Timeout { bad_version: None } => {
            eprintln!("[tower] no completions (timeout)");
            Ok(EXIT_OK)
        }
    }
}

// ---- monitor (transport debugging) ----------------------------------------

fn monitor(port: Option<String>, hex: bool, reset: bool) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = open_console(&port, reset)?;
    eprintln!(
        "[tower] monitoring {port} ({})",
        if hex { "raw hex" } else { "frames" }
    );
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 512];
    loop {
        let n = match sp.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        if hex {
            for &b in &buf[..n] {
                print!("{b:02x} ");
            }
            let _ = std::io::stdout().flush();
            continue;
        }
        for &b in &buf[..n] {
            if let Some(inner) = dec.push(b) {
                match decode_frame(inner) {
                    Ok((mt, seq, payload)) => println!(
                        "frame seq={seq:<5} type={mt:?} payload={}B  {}",
                        payload.len(),
                        hexline(payload)
                    ),
                    Err(e) => println!("bad frame ({e:?}): {}", hexline(inner)),
                }
            }
        }
    }
}

// ---- firmware: flash / erase / reset (STM32 UART bootloader, via jolt) -----
//
// The console protocol above runs over the firmware's framed UART link; these
// commands instead drive the STM32 system bootloader (toggling NRST/BOOT0 over
// the bridge's RTS/DTR). The whole bootloader engine is the `jolt` crate — we
// only pick the port (reusing the same auto-detect as the other commands) and
// hand off to it.

fn flash_cmd(
    port: Option<String>,
    file: PathBuf,
    erase: bool,
    verify: bool,
    run: bool,
    go: bool,
    verbose: bool,
) -> Result<()> {
    let port = pick_port(port)?;
    let fw = jolt::firmware::load(&file)?;
    if fw.len() as u32 > jolt::target::MAX_FLASH_SIZE {
        bail!(
            "firmware is {} bytes, exceeding the {} KiB maximum for any STM32L0 device",
            fw.len(),
            jolt::target::MAX_FLASH_SIZE / 1024
        );
    }
    eprintln!(
        "[tower] flashing {} ({} bytes) to {port}",
        file.display(),
        fw.len()
    );
    let mut sp = jolt::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    // jolt's FlashOptions is #[non_exhaustive] (a new option won't break us): start from Default
    // and override. `verbose` moved out of the options into the progress sink below.
    let mut opts = jolt::flash::FlashOptions::default();
    opts.erase = erase;
    opts.verify = verify;
    opts.run = run;
    opts.go = go;
    let mut report = flash_progress(verbose);
    let start = Instant::now();
    jolt::flash::flash(&mut sp, &fw, &opts, &mut report).context("flashing firmware")?;
    // A ~45 s flash prints only live progress above; without a terminal line the caller
    // can't tell success from a hang, so always confirm completion.
    eprintln!(
        "[tower] done: {} bytes in {:.1}s",
        fw.len(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn erase_cmd(port: Option<String>, verbose: bool) -> Result<()> {
    let port = pick_port(port)?;
    eprintln!("[tower] erasing {port}");
    let mut sp = jolt::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    let mut report = flash_progress(verbose);
    let pages = jolt::flash::erase(&mut sp, &mut report).context("erasing flash")?;
    eprintln!("[tower] erased {pages} page(s), reset into application");
    Ok(())
}

/// A jolt flash/erase progress sink. jolt's library is UI-free (it emits `Progress` events);
/// we render them with `indicatif` progress bars — the same crate the standalone `jolt` CLI
/// uses — so a ~45 s flash shows live erase/write/verify progress instead of looking hung.
/// `indicatif` auto-detects the terminal, so a redirected stderr (a log, a pipe) stays clean.
/// `--verbose` dumps the raw events instead.
fn flash_progress(verbose: bool) -> impl FnMut(jolt::flash::Progress) {
    use indicatif::{ProgressBar, ProgressStyle};
    use jolt::flash::Progress;

    fn pages_bar(total: usize, msg: &'static str) -> ProgressBar {
        let bar = ProgressBar::new(total as u64);
        bar.set_style(
            ProgressStyle::with_template(
                "[tower] {msg:>9} [{bar:28.cyan/blue}] {pos:>4}/{len} pages",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        bar.set_message(msg);
        bar
    }
    fn bytes_bar(total: u64, msg: &'static str) -> ProgressBar {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template(
                "[tower] {msg:>9} [{bar:28.cyan/blue}] {bytes:>8}/{total_bytes:<8} {percent:>3}%",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        bar.set_message(msg);
        bar
    }

    // The single bar for the active phase; swapped as erase → write → verify advance.
    let mut bar: Option<ProgressBar> = None;
    move |p| {
        if verbose {
            eprintln!("[tower] {p:?}");
            return;
        }
        match p {
            Progress::ChipIdentified { id } => eprintln!("[tower] chip 0x{id:03x}"),
            Progress::Erase {
                pages_done,
                pages_total,
            } => {
                let b = bar.get_or_insert_with(|| pages_bar(pages_total, "erasing"));
                b.set_length(pages_total as u64);
                b.set_position(pages_done as u64);
                if pages_done == pages_total
                    && let Some(b) = bar.take()
                {
                    b.finish_with_message("erased");
                }
            }
            Progress::Write {
                bytes_done,
                bytes_total,
            } => {
                let b = bar.get_or_insert_with(|| bytes_bar(bytes_total as u64, "writing"));
                b.set_position(bytes_done as u64);
                if bytes_done == bytes_total
                    && let Some(b) = bar.take()
                {
                    b.finish_with_message("written");
                }
            }
            Progress::Verify {
                bytes_done,
                bytes_total,
            } => {
                let b = bar.get_or_insert_with(|| bytes_bar(bytes_total as u64, "verifying"));
                b.set_position(bytes_done as u64);
                if bytes_done == bytes_total
                    && let Some(b) = bar.take()
                {
                    b.finish_with_message("verified");
                }
            }
            // Finish any lingering bar (e.g. verify disabled) so it doesn't outlive the flash.
            Progress::Starting => {
                if let Some(b) = bar.take() {
                    b.finish_and_clear();
                }
            }
            // Connecting / ConnectError (and any future variant): quiet unless --verbose.
            _ => {}
        }
    }
}

fn reset_cmd(port: Option<String>, bootloader: bool) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = jolt::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    if bootloader {
        sp.reset_into_bootloader()
            .context("resetting into bootloader")?;
        eprintln!("[tower] {port} reset into bootloader");
    } else {
        sp.reset_into_app().context("resetting into application")?;
        eprintln!("[tower] {port} reset into application");
    }
    Ok(())
}

// ===========================================================================
// Tests
//
// The frame-session functions are generic over `Transport` (Read + Write), so we drive
// them against an in-memory duplex mock instead of hardware: the "device" side is a queue
// of bytes the code-under-test reads, and everything the code writes lands in a second
// queue we can inspect. The mock returns `ErrorKind::TimedOut` when its read queue is
// drained, exactly as a serial port does on its read timeout — so the read loops exercise
// their real timeout paths.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    // Session functions live in `crate::session`; pull in the ones exercised here that aren't
    // already re-exported into `main` (which imports only what the command layer calls).
    use crate::session::wait_for_hello;
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::time::Duration;
    // (CandidateKind, ShellCommand, MsgType, decode_frame, encode_frame, FrameDecoder come
    // via `super::*` from the command layer's imports; only add what's test-only here.)
    use tower_protocol::MAX_WIRE;
    use tower_protocol::msg::{
        Candidate, Dropped, Event, Hello, Level, Log, ShellCompletions, ShellResponse,
    };

    /// In-memory duplex transport. `to_read` feeds the code-under-test (as if from the
    /// device); `written` captures everything the code writes (host→device).
    struct MockPort {
        to_read: VecDeque<u8>,
        written: Vec<u8>,
        /// Cap each `read` to this many bytes, to exercise chunk reassembly across reads
        /// (a real UART delivers bytes in arbitrary-sized reads). 0 = no cap.
        read_chunk: usize,
    }

    impl MockPort {
        fn new(to_read: Vec<u8>) -> Self {
            MockPort {
                to_read: to_read.into(),
                written: Vec::new(),
                read_chunk: 0,
            }
        }
        /// A mock that hands out at most `n` bytes per `read` call.
        fn with_read_chunk(to_read: Vec<u8>, n: usize) -> Self {
            let mut m = MockPort::new(to_read);
            m.read_chunk = n;
            m
        }
    }

    impl Read for MockPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.to_read.is_empty() {
                // Drained: behave like a serial port past its read timeout.
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            let mut cap = buf.len().min(self.to_read.len());
            if self.read_chunk != 0 {
                cap = cap.min(self.read_chunk);
            }
            for slot in buf.iter_mut().take(cap) {
                *slot = self.to_read.pop_front().unwrap();
            }
            Ok(cap)
        }
    }

    impl Write for MockPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ---- frame construction helpers ----

    fn frame<T: serde::Serialize>(mt: MsgType, seq: u16, payload: &T) -> Vec<u8> {
        let mut buf = [0u8; MAX_WIRE];
        let n = encode_frame(mt, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn shell_resp(cmd_id: u16, result: u8, chunk: u16, last: bool, text: &str) -> Vec<u8> {
        frame(
            MsgType::ShellResponse,
            0,
            &ShellResponse {
                cmd_id,
                result,
                chunk,
                last,
                text,
            },
        )
    }

    fn hello(protocol_version: u8) -> Vec<u8> {
        frame(
            MsgType::Hello,
            0,
            &Hello {
                protocol_version,
                firmware_name: "app",
                firmware_version: "test",
                session_id: 1,
            },
        )
    }

    /// Canonical COBS encode of `inner`, plus the trailing `0x00` delimiter — the same wire
    /// shape `encode_frame` produces. Used to forge a frame the host's own encoder can't (a
    /// mismatched-version header always stamps the *current* version otherwise).
    fn cobs_frame(inner: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8]; // placeholder for the first code byte
        let mut code_pos = 0usize;
        let mut code: u8 = 1;
        for &b in inner {
            if b == 0 {
                out[code_pos] = code;
                code_pos = out.len();
                out.push(0);
                code = 1;
            } else {
                out.push(b);
                code += 1;
                if code == 0xFF {
                    out[code_pos] = code;
                    code_pos = out.len();
                    out.push(0);
                    code = 1;
                }
            }
        }
        out[code_pos] = code;
        out.push(0); // frame delimiter
        out
    }

    /// A full wire frame whose *header* advertises protocol version `ver` (top 3 bits of
    /// `ver_type`) — what a firmware built against a different tower-protocol tag actually puts
    /// on the wire. `decode_frame` rejects it at the version check (before CRC), so the session
    /// sees `Error::BadVersion` and never parses the Hello payload. `ver` must be 0..=7.
    fn hello_wire_bad_version(ver: u8) -> Vec<u8> {
        let mut inner = vec![(ver << 5) | (MsgType::Hello as u8 & 0x1F)];
        inner.extend_from_slice(&0u16.to_le_bytes()); // seq 0
        let mut pbuf = [0u8; 64];
        let pn = postcard::to_slice(
            &Hello {
                protocol_version: ver,
                firmware_name: "app",
                firmware_version: "mismatch",
                session_id: 1,
            },
            &mut pbuf,
        )
        .unwrap()
        .len();
        inner.extend_from_slice(&pbuf[..pn]);
        let crc = tower_protocol::crc::crc32_ieee(&inner);
        inner.extend_from_slice(&crc.to_le_bytes());
        cobs_frame(&inner)
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

    const SHORT: Duration = Duration::from_millis(200);

    // ---- read_response: reassembly, cmd_id filtering, chunk gaps ----

    #[test]
    fn read_response_reassembles_chunks() {
        let mut bytes = shell_resp(7, 0, 0, false, "hello ");
        bytes.extend(shell_resp(7, 0, 1, false, "wor"));
        bytes.extend(shell_resp(7, 0, 2, true, "ld\n"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 7, SHORT).unwrap();
        assert_eq!(r.text, "hello world\n");
        assert_eq!(r.result, 0);
        assert!(!r.incomplete);
    }

    #[test]
    fn read_response_survives_byte_at_a_time_reads() {
        // The same three chunks, but delivered one byte per read() — reassembly must not
        // depend on frame boundaries aligning with read boundaries.
        let mut bytes = shell_resp(1, 0, 0, false, "ab");
        bytes.extend(shell_resp(1, 0, 1, true, "cd"));
        let mut sp = MockPort::with_read_chunk(bytes, 1);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 1, SHORT).unwrap();
        assert_eq!(r.text, "abcd");
    }

    #[test]
    fn read_response_ignores_interleaved_logs() {
        // A Log frame lands between response chunks; it must not corrupt reassembly.
        let mut bytes = shell_resp(3, 0, 0, false, "x");
        bytes.extend(log_frame(9, "noise"));
        bytes.extend(shell_resp(3, 0, 1, true, "y"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 3, SHORT).unwrap();
        assert_eq!(r.text, "xy");
    }

    #[test]
    fn read_response_ignores_wrong_cmd_id() {
        // A complete response for cmd_id 99 must not satisfy a wait for cmd_id 1 (C18).
        let mut bytes = shell_resp(99, 0, 0, true, "stale");
        bytes.extend(shell_resp(1, 0, 0, true, "mine"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 1, SHORT).unwrap();
        assert_eq!(r.text, "mine");
    }

    #[test]
    fn read_response_flags_dropped_chunk() {
        // Chunk 1 is missing (as if CRC-dropped): 0 then 2 → incomplete (C19).
        let mut bytes = shell_resp(5, 0, 0, false, "a");
        bytes.extend(shell_resp(5, 0, 2, true, "c"));
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = read_response(&mut sp, &mut dec, 5, SHORT).unwrap();
        assert!(
            r.incomplete,
            "a chunk-index gap must mark the response incomplete"
        );
        assert_eq!(r.text, "ac"); // what did arrive is still returned
    }

    #[test]
    fn read_response_times_out_when_silent() {
        let mut sp = MockPort::new(Vec::new());
        let mut dec = FrameDecoder::new();
        assert!(read_response(&mut sp, &mut dec, 1, SHORT).is_none());
    }

    #[test]
    fn read_response_drops_corrupt_crc_frame() {
        // Flip a byte inside the COBS-encoded frame so the CRC fails; the decoder drops it
        // and the waiter times out rather than returning garbage.
        let mut bytes = shell_resp(1, 0, 0, true, "hello");
        bytes[2] ^= 0xFF; // corrupt a payload byte (not the trailing 0x00 delimiter)
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        assert!(read_response(&mut sp, &mut dec, 1, SHORT).is_none());
    }

    #[test]
    fn read_response_timeout_carries_the_mismatched_version() {
        // The device answers — but every frame is tagged with a different protocol version.
        // The timeout must carry that version so exec/shell report lockstep (125), not a
        // mute device (124): the exact stale-binary incident this ecosystem had.
        let mut sp = MockPort::new(hello_wire_bad_version(1));
        let mut dec = FrameDecoder::new();
        match read_response(&mut sp, &mut dec, 1, SHORT) {
            ReadOutcome::Timeout { bad_version } => assert_eq!(bad_version, Some(1)),
            ReadOutcome::Response(_) => panic!("a version-rejected frame must not decode"),
        }
    }

    #[test]
    fn request_completions_timeout_carries_the_mismatched_version() {
        let mut sp = MockPort::new(hello_wire_bad_version(1));
        let mut dec = FrameDecoder::new();
        match request_completions(&mut sp, &mut dec, "/s", 2, 1, SHORT) {
            CompletionOutcome::Timeout { bad_version } => assert_eq!(bad_version, Some(1)),
            CompletionOutcome::Completions(_) => {
                panic!("a version-rejected frame must not decode")
            }
        }
    }

    // ---- Hello handshake ----

    #[test]
    fn wait_for_hello_returns_protocol_version() {
        // The matched-peer case: `hello()` uses the host's own encoder, so the header carries
        // the current version and the Hello decodes cleanly.
        let mut sp = MockPort::new(hello(tower_protocol::PROTOCOL_VERSION));
        let mut dec = FrameDecoder::new();
        assert_eq!(
            wait_for_hello(&mut sp, &mut dec, SHORT),
            Readiness::Hello(tower_protocol::PROTOCOL_VERSION)
        );
    }

    #[test]
    fn wait_for_hello_reports_mismatched_version() {
        // A REAL mismatched peer tags the frame *header* with its own version, so `decode_frame`
        // rejects it before the Hello payload parses. Forge that on the wire — the host's own
        // encoder always stamps the current version, so the old `hello(99)` test (payload-only
        // mismatch, header still valid) decoded fine and never modelled a real mismatch.
        // A device still speaking the previous protocol (v1) is the realistic mismatch.
        assert_ne!(1, tower_protocol::PROTOCOL_VERSION);
        let mut sp = MockPort::new(hello_wire_bad_version(1));
        let mut dec = FrameDecoder::new();
        assert_eq!(
            wait_for_hello(&mut sp, &mut dec, SHORT),
            Readiness::BadVersion(1)
        );
    }

    #[test]
    fn await_ready_reports_bad_version() {
        // The readiness path (used by `exec`) propagates the mismatch so `exec` can bail fast
        // with a distinct exit code instead of a generic timeout.
        let mut sp = MockPort::new(hello_wire_bad_version(1));
        let mut dec = FrameDecoder::new();
        assert_eq!(
            await_ready(&mut sp, &mut dec, None),
            Readiness::BadVersion(1)
        );
    }

    #[test]
    fn wait_for_hello_times_out_without_hello() {
        let mut sp = MockPort::new(log_frame(0, "not a hello"));
        let mut dec = FrameDecoder::new();
        assert_eq!(wait_for_hello(&mut sp, &mut dec, SHORT), Readiness::Timeout);
    }

    #[test]
    fn exec_cmd_id_is_nonzero_and_15_bit() {
        let id = exec_cmd_id();
        assert_ne!(id, 0, "cmd_id 0 is a reserved sentinel");
        assert!(id <= 0x7FFF, "cmd_id must fit the low 15 bits");
        // Deterministic within a process (same PID) so the send and the wait agree on it.
        assert_eq!(id, exec_cmd_id());
    }

    #[test]
    fn overlong_shell_line_fails_encode_but_short_line_fits() {
        // The boundary the interactive shell + TUI now tolerate per-line (print a hint, keep the
        // prompt) instead of propagating `?` and killing the session: a line that overflows one
        // frame fails to encode; a normal line encodes fine.
        let mut buf = [0u8; MAX_WIRE];
        let long = "x".repeat(300);
        assert!(
            encode_frame(
                MsgType::ShellCommand,
                0,
                &ShellCommand {
                    cmd_id: 1,
                    line: &long,
                },
                &mut buf,
            )
            .is_err()
        );
        assert!(
            encode_frame(
                MsgType::ShellCommand,
                0,
                &ShellCommand {
                    cmd_id: 1,
                    line: "ok",
                },
                &mut buf,
            )
            .is_ok()
        );
    }

    // ---- render: seq-gap detection + Hello re-baseline ----

    // (render()/RenderState + ColorMode behavior are unit-tested in `render.rs`, next to
    // their now-private helpers. The golden-bytes decode below pins the shared wire contract.)

    // ---- golden-bytes decode: pins the wire contract ----

    #[test]
    fn golden_frames_roundtrip_byte_at_a_time() {
        // Synthesize one of each rendered type, concatenate, and feed byte-by-byte through a
        // single decoder — asserting each decodes to the expected (type, seq) with a valid
        // payload. This pins the encode/decode contract shared with the firmware.
        let mut wire = Vec::new();
        wire.extend(hello(tower_protocol::PROTOCOL_VERSION)); // seq 0
        wire.extend(log_frame(1, "hi"));
        wire.extend(frame(
            MsgType::Event,
            2,
            &Event {
                name: "boot",
                fields: Default::default(),
            },
        ));
        wire.extend(frame(MsgType::Dropped, 3, &Dropped { count: 4 }));
        wire.extend(shell_resp(1, 0, 0, true, "ok"));

        let expected = [
            (MsgType::Hello, 0u16),
            (MsgType::Log, 1),
            (MsgType::Event, 2),
            (MsgType::Dropped, 3),
            (MsgType::ShellResponse, 0),
        ];
        let mut dec = FrameDecoder::new();
        let mut seen = Vec::new();
        for &b in &wire {
            if let Some(inner) = dec.push(b) {
                let (mt, seq, _payload) = decode_frame(inner).expect("valid frame");
                seen.push((mt, seq));
            }
        }
        assert_eq!(seen, expected);
    }

    // ---- request_completions ----

    #[test]
    fn request_completions_matches_req_id() {
        let mut cands: heapless::Vec<Candidate, 16> = heapless::Vec::new();
        cands
            .push(Candidate {
                text: "system",
                kind: CandidateKind::Menu,
            })
            .unwrap();
        let bytes = frame(
            MsgType::ShellCompletions,
            0,
            &ShellCompletions {
                req_id: 42,
                token_start: 1,
                common_prefix: "sys",
                candidates: cands,
                more: false,
            },
        );
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        let r = request_completions(&mut sp, &mut dec, "/s", 2, 42, SHORT).unwrap();
        assert_eq!(r.token_start, 1);
        assert_eq!(r.common_prefix, "sys");
        assert_eq!(r.candidates.len(), 1);
        // And a ShellComplete was actually written to the wire.
        assert!(!sp.written.is_empty());
    }

    #[test]
    fn request_completions_ignores_wrong_req_id() {
        let cands: heapless::Vec<Candidate, 16> = heapless::Vec::new();
        let bytes = frame(
            MsgType::ShellCompletions,
            0,
            &ShellCompletions {
                req_id: 1,
                token_start: 0,
                common_prefix: "",
                candidates: cands,
                more: false,
            },
        );
        let mut sp = MockPort::new(bytes);
        let mut dec = FrameDecoder::new();
        // We asked for req_id 2, device answered 1 → no match, times out.
        assert!(request_completions(&mut sp, &mut dec, "", 0, 2, SHORT).is_none());
    }
}
