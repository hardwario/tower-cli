//! `tower` — HARDWARIO TOWER host CLI: devices, logs/events, shell/exec, the console TUI,
//! flash/erase/reset (via the jolt engine), and `fota serve`.
//!
//! The firmware's UART is always framed (`tower-protocol`: COBS + CRC + postcard),
//! so a plain terminal shows binary — this tool decodes it. The same `FrameDecoder`
//! / `decode_frame` run on both ends, so the wire format can't drift.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

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
use session::{
    await_ready, fota_serve_loop, read_response, request_completions, validate_manifest,
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
//   1    tool error (I/O, bad file, encode/decode, protocol mismatch)
//   2    usage error (bad args — emitted by clap itself)
//   124  device command timed out (no/incomplete response)
//
// So a device `result` can't be confused with the reserved 124, `exec` clamps a
// device-reported non-zero result into 1..=123 (see `exec_cmd`).
const EXIT_OK: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_DEVICE_TIMEOUT: u8 = 124;

/// Default per-command response timeout (`--timeout`), in milliseconds. Long enough
/// for a slow device print, short enough that a wedged link fails a script promptly.
const DEFAULT_TIMEOUT_MS: u64 = 1500;

#[derive(Parser)]
#[command(name = "tower", version, about = "HARDWARIO TOWER console host")]
struct Cli {
    /// Serial device (auto-detected when exactly one USB serial device is present).
    // The field stays `port` since it holds a serial-port path; the user-facing flag is `--device`.
    #[arg(short = 'd', long = "device", value_name = "DEVICE", global = true)]
    port: Option<String>,
    /// Don't auto-reconnect on the streaming commands (`logs`/`events`/`fota serve`):
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
    /// Firmware-over-the-air (FOTA) host-side helpers.
    Fota {
        #[command(subcommand)]
        cmd: FotaCmd,
    },
}

#[derive(Subcommand)]
enum FotaCmd {
    /// Host-proxy image source: serve a signed firmware image to a FOTA gateway on demand.
    ///
    /// The gateway (which holds no image of its own) sends `FotaReq{offset,len}` frames over
    /// the console link; this answers each with the requested image bytes (or the signed
    /// manifest for the sentinel offset). The node pulls it over the radio, and the
    /// bootloader verifies the Ed25519 signature + SHA-256 before swapping. See docs/fota.md.
    Serve {
        /// The raw firmware image (e.g. `target/fota-ota-v2.bin`).
        #[arg(long)]
        image: PathBuf,
        /// The signed manifest for that image (`fota-sign sign ...`, 116 bytes).
        #[arg(long)]
        manifest: PathBuf,
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
        } => shell(cli.port, reset, delay, Duration::from_millis(timeout)).map(|()| EXIT_OK),
        Cmd::Exec {
            line,
            reset,
            delay,
            timeout,
        } => exec_cmd(cli.port, line, reset, delay, Duration::from_millis(timeout)),
        Cmd::Console { reset } => tui::run(pick_port(cli.port)?, reset).map(|()| EXIT_OK),
        Cmd::Complete { line } => complete_cmd(cli.port, line).map(|()| EXIT_OK),
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
        Cmd::Fota { cmd } => match cmd {
            FotaCmd::Serve { image, manifest } => {
                fota_serve(cli.port, image, manifest, reconnect).map(|()| EXIT_OK)
            }
        },
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

// ---- FOTA host-proxy serve ------------------------------------------------

/// Serve a signed firmware image to a FOTA gateway over the framed console link: read the
/// image + manifest once, then answer each `FotaReq{offset,len}` frame with a `FotaData`
/// frame (the manifest for the sentinel offset, image bytes otherwise). The gateway relays
/// the bytes to the node over the radio; the node's bootloader verifies signature + hash
/// before swapping. Reconnects if the gateway resets (unless `reconnect` is false). Runs
/// until interrupted.
fn fota_serve(
    port: Option<String>,
    image_path: PathBuf,
    manifest_path: PathBuf,
    reconnect: bool,
) -> Result<()> {
    let image = std::fs::read(&image_path)
        .with_context(|| format!("reading image {}", image_path.display()))?;
    let manifest = std::fs::read(&manifest_path)
        .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
    validate_manifest(&image, &manifest)?;
    let port = pick_port(port)?;
    eprintln!(
        "[tower] fota serve: image {} B + manifest {} B (validated); answering FotaReq on {port}",
        image.len(),
        manifest.len()
    );
    // The FIRST open is fatal: a bad --device or a busy device should exit 1, not spin forever.
    // Enter the reconnect loop only after one success (and only if reconnection is enabled).
    let mut sp = open_console(&port, false)?;
    loop {
        eprintln!("[tower] connected {port}");
        if let Err(e) = fota_serve_loop(&mut *sp, &image, &manifest) {
            eprintln!("[tower] {port} lost: {e}");
        }
        if !reconnect {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(800));
        eprintln!("[tower] reconnecting…");
        // Re-establish the run baseline on every reopen (like every other command) so the
        // bridge can't leave the gateway held in reset. Tolerate a failed reopen and retry.
        match open_console(&port, false) {
            Ok(reopened) => sp = reopened,
            Err(e) => {
                eprintln!("[tower] {e}");
                continue;
            }
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
        if let Some(s) = &send {
            // On a reset attach, wait for the device to come up before poking it.
            if reset && first {
                let mut dec = FrameDecoder::new();
                await_ready(&mut *sp, &mut dec, delay);
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
            Some(r) => {
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
            None => Ok((pos, Vec::new())),
        }
    }
}

impl Hinter for ShellHelper {
    type Hint = String;
}
impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

fn shell(port: Option<String>, reset: bool, delay: Option<u64>, timeout: Duration) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = open_console(&port, reset)?;
    eprintln!("[tower] shell on {port} — TAB completes; commands start with '/'; 'exit' to quit");

    let mut dec = FrameDecoder::new();
    if reset {
        // Don't drop into the prompt until the freshly reset device can answer.
        await_ready(&mut *sp, &mut dec, delay);
    }
    let conn = Rc::new(RefCell::new(Conn { sp, dec, req_id: 0 }));
    let mut rl: Editor<ShellHelper, rustyline::history::DefaultHistory> = Editor::new()?;
    rl.set_helper(Some(ShellHelper { conn: conn.clone() }));

    let mut cmd_id: u16 = 1;
    let mut seq: u16 = 0;
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
                let n = encode_frame(
                    MsgType::ShellCommand,
                    seq,
                    &ShellCommand { cmd_id, line },
                    &mut buf,
                )
                .map_err(|e| anyhow::anyhow!("encode: {e:?}"))?;
                seq = seq.wrapping_add(1);
                sp.write_all(&buf[..n])?;
                sp.flush()?;
                match read_response(&mut **sp, dec, cmd_id, timeout) {
                    Some(r) => {
                        print!("{}", r.text);
                        if !r.text.is_empty() && !r.text.ends_with('\n') {
                            println!();
                        }
                        if r.result != 0 {
                            eprintln!("[result {}]", r.result);
                        }
                    }
                    None => eprintln!("[tower] no response (timeout)"),
                }
                cmd_id = cmd_id.wrapping_add(1);
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            // A real readline failure (e.g. a broken terminal) is an error, not a clean
            // exit — propagate it so the shell exits non-zero rather than swallowing it.
            Err(e) => return Err(anyhow::Error::new(e).context("readline")),
        }
    }
    Ok(())
}

/// Run a single shell command non-interactively: send it, print the (reassembled)
/// response, and return an exit code (see the exit-code contract): a device-reported
/// non-zero `result` (clamped into 1..=123 so it can't collide with our reserved 124),
/// `EXIT_ERROR` on a truncated (chunk-dropped) response, `EXIT_DEVICE_TIMEOUT` on no/
/// incomplete reply, else `EXIT_OK`.
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
        // every subsequent frame silently mis-decodes — better a clear error than junk.
        if let Some(v) = await_ready(&mut *sp, &mut dec, delay)
            && v != tower_protocol::PROTOCOL_VERSION
        {
            warn_protocol_mismatch(v);
            bail!(
                "protocol version mismatch (device v{v}, tower v{}) — refusing to exec",
                tower_protocol::PROTOCOL_VERSION
            );
        }
    }
    let mut buf = [0u8; tower_protocol::MAX_WIRE];
    let n = encode_frame(
        MsgType::ShellCommand,
        0,
        &ShellCommand {
            cmd_id: 1,
            line: &line,
        },
        &mut buf,
    )
    .map_err(|e| anyhow::anyhow!("encode: {e:?}"))?;
    sp.write_all(&buf[..n])?;
    sp.flush()?;
    match read_response(&mut *sp, &mut dec, 1, timeout) {
        Some(r) => {
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
        None => {
            eprintln!("[tower] no response (timeout)");
            Ok(EXIT_DEVICE_TIMEOUT)
        }
    }
}

fn complete_cmd(port: Option<String>, line: String) -> Result<()> {
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
        Some(r) => {
            println!(
                "token_start={} common_prefix={:?}{}",
                r.token_start,
                r.common_prefix,
                if r.more { " (more…)" } else { "" }
            );
            for (text, kind) in &r.candidates {
                println!("  {kind:?}  {text}");
            }
        }
        None => eprintln!("[tower] no completions (timeout)"),
    }
    Ok(())
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
    jolt::flash::flash(&mut sp, &fw, &opts, &mut report).context("flashing firmware")
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

/// A jolt flash/erase progress sink. jolt's library no longer prints (it emits `Progress`
/// events); we render them to stderr — every event under `--verbose`, else just the chip-id
/// milestone so a normal flash still shows the target was identified.
fn flash_progress(verbose: bool) -> impl FnMut(jolt::flash::Progress) {
    move |p| {
        if verbose {
            eprintln!("[tower] {p:?}");
        } else if let jolt::flash::Progress::ChipIdentified { id } = p {
            eprintln!("[tower] chip 0x{id:03x}");
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
    use crate::session::{fota_data_payload, image_digest, validate_manifest, wait_for_hello};
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::time::Duration;
    // (CandidateKind, ShellCommand, MsgType, decode_frame, encode_frame, FrameDecoder come
    // via `super::*` from the command layer's imports; only add what's test-only here.)
    use tower_protocol::fota::{FOTA_MANIFEST_OFFSET, Manifest, SIGNED_LEN};
    use tower_protocol::msg::{
        Candidate, Dropped, Event, Hello, Level, Log, ShellCompletions, ShellResponse,
    };
    use tower_protocol::{MAX_WIRE, encode_frame_raw};

    /// In-memory duplex transport. `to_read` feeds the code-under-test (as if from the
    /// device); `written` captures everything the code writes (host→device).
    struct MockPort {
        to_read: VecDeque<u8>,
        written: Vec<u8>,
        /// Cap each `read` to this many bytes, to exercise chunk reassembly across reads
        /// (a real UART delivers bytes in arbitrary-sized reads). 0 = no cap.
        read_chunk: usize,
        /// When true, a drained read returns `Ok(0)` (EOF) instead of `TimedOut` — used to
        /// let the `fota_serve_loop` (which treats `Ok(0)` as EOF) terminate in a test.
        eof_when_drained: bool,
    }

    impl MockPort {
        fn new(to_read: Vec<u8>) -> Self {
            MockPort {
                to_read: to_read.into(),
                written: Vec::new(),
                read_chunk: 0,
                eof_when_drained: false,
            }
        }
        /// A mock that hands out at most `n` bytes per `read` call.
        fn with_read_chunk(to_read: Vec<u8>, n: usize) -> Self {
            let mut m = MockPort::new(to_read);
            m.read_chunk = n;
            m
        }
        /// A mock that returns EOF (`Ok(0)`) once drained.
        fn with_eof(to_read: Vec<u8>) -> Self {
            let mut m = MockPort::new(to_read);
            m.eof_when_drained = true;
            m
        }
    }

    impl Read for MockPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.to_read.is_empty() {
                return if self.eof_when_drained {
                    Ok(0) // EOF
                } else {
                    // Drained: behave like a serial port past its read timeout.
                    Err(io::Error::from(io::ErrorKind::TimedOut))
                };
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

    // ---- Hello handshake ----

    #[test]
    fn wait_for_hello_returns_protocol_version() {
        let mut sp = MockPort::new(hello(1));
        let mut dec = FrameDecoder::new();
        assert_eq!(wait_for_hello(&mut sp, &mut dec, SHORT), Some(1));
    }

    #[test]
    fn wait_for_hello_reports_mismatched_version() {
        let mut sp = MockPort::new(hello(99));
        let mut dec = FrameDecoder::new();
        // It surfaces the *device's* version; the caller compares against PROTOCOL_VERSION.
        assert_eq!(wait_for_hello(&mut sp, &mut dec, SHORT), Some(99));
        assert_ne!(99, tower_protocol::PROTOCOL_VERSION);
    }

    #[test]
    fn wait_for_hello_times_out_without_hello() {
        let mut sp = MockPort::new(log_frame(0, "not a hello"));
        let mut dec = FrameDecoder::new();
        assert_eq!(wait_for_hello(&mut sp, &mut dec, SHORT), None);
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

    // ---- fota_data_payload: offset math (sentinel / mid-image / past-EOF) ----

    #[test]
    fn fota_payload_manifest_for_sentinel_offset() {
        let image = vec![0xAAu8; 100];
        let manifest = vec![0x55u8; SIGNED_LEN];
        let p = fota_data_payload(FOTA_MANIFEST_OFFSET, 64, &image, &manifest);
        assert_eq!(&p[..4], &FOTA_MANIFEST_OFFSET.to_le_bytes());
        assert_eq!(&p[4..], &manifest[..]);
    }

    #[test]
    fn fota_payload_mid_image_slice() {
        let image: Vec<u8> = (0..100).collect();
        let manifest = vec![0u8; SIGNED_LEN];
        let p = fota_data_payload(10, 8, &image, &manifest);
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 10);
        assert_eq!(&p[4..], &image[10..18]);
    }

    #[test]
    fn fota_payload_clamps_partial_tail() {
        let image: Vec<u8> = (0..10).collect();
        let manifest = vec![0u8; SIGNED_LEN];
        // Ask for 8 bytes at offset 6 — only 4 exist; the tail is clamped.
        let p = fota_data_payload(6, 8, &image, &manifest);
        assert_eq!(&p[4..], &image[6..10]);
    }

    #[test]
    fn fota_payload_past_eof_is_offset_only() {
        let image = vec![0u8; 10];
        let manifest = vec![0u8; SIGNED_LEN];
        let p = fota_data_payload(1000, 8, &image, &manifest);
        assert_eq!(p.len(), 4); // just the echoed offset, no bytes
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 1000);
    }

    /// Encode a raw `FotaReq{offset,len}` wire frame.
    fn fota_req(offset: u32, len: u16) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&offset.to_le_bytes());
        p.extend_from_slice(&len.to_le_bytes());
        let mut wire = [0u8; MAX_WIRE];
        let n = encode_frame_raw(MsgType::FotaReq, 0, &p, &mut wire).unwrap();
        wire[..n].to_vec()
    }

    /// Decode the FotaData frames the serve loop wrote back, returning each `(offset, bytes)`.
    fn decode_fota_data(written: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut dec = FrameDecoder::new();
        let mut out = Vec::new();
        for &b in written {
            if let Some(inner) = dec.push(b)
                && let Ok((MsgType::FotaData, _seq, p)) = decode_frame(inner)
            {
                let off = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                out.push((off, p[4..].to_vec()));
            }
        }
        out
    }

    #[test]
    fn fota_serve_loop_answers_request_then_ends_on_eof() {
        // A FotaReq for the manifest and one for a mid-image slice; the loop answers both and
        // returns Ok once the (EOF) mock drains.
        let image: Vec<u8> = (0..64).collect();
        let manifest = vec![0x11u8; SIGNED_LEN];
        let mut wire = fota_req(FOTA_MANIFEST_OFFSET, SIGNED_LEN as u16);
        wire.extend(fota_req(8, 16));
        let mut sp = MockPort::with_eof(wire);
        fota_serve_loop(&mut sp, &image, &manifest).unwrap();

        let answers = decode_fota_data(&sp.written);
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].0, FOTA_MANIFEST_OFFSET);
        assert_eq!(answers[0].1, manifest);
        assert_eq!(answers[1].0, 8);
        assert_eq!(answers[1].1, &image[8..24]);
    }

    #[test]
    fn fota_serve_loop_clamps_past_eof_request() {
        // A request beyond the image end is answered with the echoed offset and zero bytes.
        let image = vec![0xABu8; 10];
        let manifest = vec![0u8; SIGNED_LEN];
        let mut sp = MockPort::with_eof(fota_req(1000, 32));
        fota_serve_loop(&mut sp, &image, &manifest).unwrap();
        let answers = decode_fota_data(&sp.written);
        assert_eq!(answers, vec![(1000u32, Vec::new())]);
    }

    #[test]
    fn fota_serve_loop_ignores_truncated_request() {
        // A FotaReq with a <6-byte payload must be dropped by the guard → no FotaData written.
        let image = vec![0u8; 4];
        let manifest = vec![0u8; SIGNED_LEN];
        let short = [1u8, 2, 3]; // only 3 bytes
        let mut wire = [0u8; MAX_WIRE];
        let n = encode_frame_raw(MsgType::FotaReq, 0, &short, &mut wire).unwrap();
        let mut sp = MockPort::with_eof(wire[..n].to_vec());
        fota_serve_loop(&mut sp, &image, &manifest).unwrap();
        assert!(sp.written.is_empty());
    }

    // ---- validate_manifest / image_digest ----

    fn signed_manifest_for(image: &[u8], override_size: Option<u32>) -> Vec<u8> {
        let m = Manifest {
            flags: 0,
            hw_id: 0,
            version: 1,
            size: override_size.unwrap_or(image.len() as u32),
            sha256: image_digest(image),
        };
        let sig = [0u8; tower_protocol::fota::SIG_LEN];
        m.encode_signed(&sig).to_vec()
    }

    #[test]
    fn validate_manifest_accepts_matching_pair() {
        let image = vec![1u8, 2, 3, 4, 5];
        let manifest = signed_manifest_for(&image, None);
        assert!(validate_manifest(&image, &manifest).is_ok());
    }

    #[test]
    fn validate_manifest_rejects_wrong_length() {
        let image = vec![0u8; 8];
        let mut manifest = signed_manifest_for(&image, None);
        manifest.pop(); // wrong byte length
        assert!(validate_manifest(&image, &manifest).is_err());
    }

    #[test]
    fn validate_manifest_rejects_size_mismatch() {
        let image = vec![0u8; 8];
        // size field claims 9 but the image is 8.
        let manifest = signed_manifest_for(&image, Some(9));
        assert!(validate_manifest(&image, &manifest).is_err());
    }

    #[test]
    fn validate_manifest_rejects_hash_mismatch() {
        let image = vec![0u8; 8];
        let other = vec![0xFFu8; 8]; // same length, different content → different digest
        let manifest = signed_manifest_for(&other, Some(8));
        assert!(validate_manifest(&image, &manifest).is_err());
    }

    #[test]
    fn image_digest_matches_fota_sign_scheme() {
        // SHA-512 truncated to 256 bits (see fota-sign::image_digest). Pin a known value so a
        // future refactor can't silently switch hash schemes and break FOTA validation.
        use sha2::{Digest, Sha512};
        let image = b"hardwario tower";
        let full: [u8; 64] = Sha512::digest(image).into();
        assert_eq!(image_digest(image), full[..32]);
    }
}
