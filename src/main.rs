//! `tower` — HARDWARIO TOWER host CLI: devices, logs/events, shell/exec, the console TUI,
//! flash/erase/reset (via the jolt engine), and `fota serve`.
//!
//! The firmware's UART is always framed (`tower-protocol`: COBS + CRC + postcard),
//! so a plain terminal shows binary — this tool decodes it. The same `FrameDecoder`
//! / `decode_frame` run on both ends, so the wire format can't drift.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
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
use tower_protocol::msg::{
    CandidateKind, Dropped, Event, Hello, Level, Log, Print, ShellCommand, ShellComplete,
    ShellCompletions, ShellResponse,
};
use tower_protocol::fota::{FOTA_MANIFEST_OFFSET, SIGNED_LEN};
use tower_protocol::{FrameDecoder, MAX_WIRE, MsgType, decode_frame, encode_frame, encode_frame_raw};

mod tui;

/// Which entity stream to render.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Logs,
    Events,
}

#[derive(Parser)]
#[command(name = "tower", version, about = "HARDWARIO TOWER console host")]
struct Cli {
    /// Serial port (auto-detected when exactly one USB serial device is present).
    #[arg(short, long, global = true)]
    port: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List available serial ports.
    Devices,
    /// Stream device logs (and `print!` output) to stdout.
    Logs {
        /// Disable ANSI colors.
        #[arg(long)]
        no_colors: bool,
        /// Send this text to the device once on connect (RX probe / quick poke).
        #[arg(long)]
        send: Option<String>,
    },
    /// Stream device events (structured key=value) to stdout.
    Events {
        /// Disable ANSI colors.
        #[arg(long)]
        no_colors: bool,
    },
    /// Open an interactive shell (commands start with `/`).
    Shell,
    /// Run one shell command and print its response, then exit (for scripts / CI).
    Exec {
        /// The command line, e.g. "/system/resource print".
        line: String,
    },
    /// Open the full-screen TUI console (logs + events + shell).
    Console,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Devices => devices(),
        Cmd::Logs { no_colors, send } => stream(cli.port, !no_colors, View::Logs, send),
        Cmd::Events { no_colors } => stream(cli.port, !no_colors, View::Events, None),
        Cmd::Shell => shell(cli.port),
        Cmd::Exec { line } => exec_cmd(cli.port, line),
        Cmd::Console => tui::run(pick_port(cli.port)?),
        Cmd::Complete { line } => complete_cmd(cli.port, line),
        Cmd::Monitor { hex } => monitor(cli.port, hex),
        Cmd::Flash {
            file,
            no_erase,
            no_verify,
            no_run,
            go,
            verbose,
        } => flash_cmd(cli.port, file, !no_erase, !no_verify, !no_run, go, verbose),
        Cmd::Erase { verbose } => erase_cmd(cli.port, verbose),
        Cmd::Reset { bootloader } => reset_cmd(cli.port, bootloader),
        Cmd::Fota { cmd } => match cmd {
            FotaCmd::Serve { image, manifest } => fota_serve(cli.port, image, manifest),
        },
    }
}

// ---- port selection -------------------------------------------------------

fn usb_ports() -> Vec<String> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            matches!(p.port_type, serialport::SerialPortType::UsbPort(_))
                || p.port_name.contains("usbserial")
                || p.port_name.contains("ttyUSB")
                || p.port_name.contains("ttyACM")
        })
        .map(|p| p.port_name)
        .collect()
}

fn pick_port(explicit: Option<String>) -> Result<String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let ports = usb_ports();
    match ports.len() {
        1 => Ok(ports.into_iter().next().unwrap()),
        0 => bail!("no USB serial port found; pass --port"),
        _ => bail!(
            "multiple USB serial ports; pass --port (one of: {})",
            ports.join(", ")
        ),
    }
}

fn devices() -> Result<()> {
    // tower-cli's own serial enumeration — one bare port name per line, nothing
    // else (script-friendly). We deliberately don't delegate to jolt's lister.
    let ports = serialport::available_ports().context("listing serial ports")?;
    for p in ports {
        println!("{}", p.port_name);
    }
    Ok(())
}

// ---- logs (with reconnect) ------------------------------------------------

fn open(port: &str) -> Result<Box<dyn serialport::SerialPort>> {
    serialport::new(port, 115_200)
        .timeout(Duration::from_millis(200))
        .open()
        .with_context(|| format!("opening {port}"))
}

// ---- FOTA host-proxy serve ------------------------------------------------

/// Serve a signed firmware image to a FOTA gateway over the framed console link: read the
/// image + manifest once, then answer each `FotaReq{offset,len}` frame with a `FotaData`
/// frame (the manifest for the sentinel offset, image bytes otherwise). The gateway relays
/// the bytes to the node over the radio; the node's bootloader verifies signature + hash
/// before swapping. Reconnects if the gateway resets. Runs until interrupted.
fn fota_serve(port: Option<String>, image_path: PathBuf, manifest_path: PathBuf) -> Result<()> {
    let image = std::fs::read(&image_path)
        .with_context(|| format!("reading image {}", image_path.display()))?;
    let manifest = std::fs::read(&manifest_path)
        .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
    if manifest.len() != SIGNED_LEN {
        bail!(
            "manifest must be {SIGNED_LEN} bytes (a `fota-sign` .fmanifest), got {}",
            manifest.len()
        );
    }
    let port = pick_port(port)?;
    eprintln!(
        "[tower] fota serve: image {} B + manifest {} B; answering FotaReq on {port}",
        image.len(),
        manifest.len()
    );
    loop {
        match open(&port) {
            Ok(mut sp) => {
                eprintln!("[tower] connected {port}");
                if let Err(e) = fota_serve_loop(&mut *sp, &image, &manifest) {
                    eprintln!("[tower] {port} lost: {e}");
                }
            }
            Err(e) => eprintln!("[tower] {e}"),
        }
        std::thread::sleep(Duration::from_millis(800));
        eprintln!("[tower] reconnecting…");
    }
}

fn fota_serve_loop(
    sp: &mut dyn serialport::SerialPort,
    image: &[u8],
    manifest: &[u8],
) -> Result<()> {
    let mut dec = FrameDecoder::new();
    let mut rbuf = [0u8; 512];
    let mut seq: u16 = 0;
    let mut served_to = 0usize; // high-water of image bytes served, for the progress line
    loop {
        let n = match sp.read(&mut rbuf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        for &b in &rbuf[..n] {
            let Some(inner) = dec.push(b) else { continue };
            let Ok((MsgType::FotaReq, _seq, p)) = decode_frame(inner) else { continue };
            if p.len() < 6 {
                continue;
            }
            let offset = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            let len = u16::from_le_bytes([p[4], p[5]]) as usize;

            // FotaData payload: offset (echoed) ‖ bytes.
            let mut payload = Vec::with_capacity(4 + len);
            payload.extend_from_slice(&offset.to_le_bytes());
            if offset == FOTA_MANIFEST_OFFSET {
                payload.extend_from_slice(manifest);
                eprintln!("[tower] -> manifest ({} B)", manifest.len());
            } else {
                let start = (offset as usize).min(image.len());
                let end = (start + len).min(image.len());
                payload.extend_from_slice(&image[start..end]);
                served_to = served_to.max(end);
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

fn stream(port: Option<String>, colors: bool, view: View, send: Option<String>) -> Result<()> {
    let port = pick_port(port)?;
    loop {
        match open(&port) {
            Ok(mut sp) => {
                eprintln!("[tower] connected {port}");
                if let Some(s) = &send {
                    let _ = sp.write_all(s.as_bytes());
                    let _ = sp.flush();
                    eprintln!("[tower] sent {} byte(s)", s.len());
                }
                if let Err(e) = read_loop(&mut *sp, colors, view) {
                    eprintln!("[tower] {port} lost: {e}");
                }
            }
            Err(e) => eprintln!("[tower] {e}"),
        }
        std::thread::sleep(Duration::from_millis(800));
        eprintln!("[tower] reconnecting…");
    }
}

fn read_loop(sp: &mut dyn serialport::SerialPort, colors: bool, view: View) -> Result<()> {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 512];
    let mut last_seq: Option<u16> = None;
    loop {
        let n = match sp.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        for &b in &buf[..n] {
            if let Some(inner) = dec.push(b) {
                render(inner, colors, view, &mut last_seq);
            }
        }
    }
}

fn render(inner: &[u8], colors: bool, view: View, last_seq: &mut Option<u16>) {
    let (mt, seq, payload) = match decode_frame(inner) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[tower] dropped a corrupt frame: {e:?}");
            return;
        }
    };
    if let Some(prev) = *last_seq {
        let expected = prev.wrapping_add(1);
        if seq != expected {
            eprintln!("[tower] seq gap: expected {expected}, got {seq}");
        }
    }
    *last_seq = Some(seq);

    match mt {
        MsgType::Hello => {
            if let Ok(h) = postcard::from_bytes::<Hello>(payload) {
                eprintln!(
                    "[tower] hello: firmware {:?}, protocol v{}",
                    h.firmware_version, h.protocol_version
                );
            }
        }
        MsgType::Log if view == View::Logs => {
            if let Ok(l) = postcard::from_bytes::<Log>(payload) {
                print_log(&l, colors);
            }
        }
        MsgType::Print if view == View::Logs => {
            if let Ok(p) = postcard::from_bytes::<Print>(payload) {
                print!("{}", p.text);
                let _ = std::io::stdout().flush();
            }
        }
        MsgType::Dropped if view == View::Logs => {
            if let Ok(d) = postcard::from_bytes::<Dropped>(payload) {
                eprintln!(
                    "{} {} log frame(s) dropped (device queue full)",
                    paint("⚠", 33, colors),
                    d.count
                );
            }
        }
        MsgType::Event if view == View::Events => {
            if let Ok(e) = postcard::from_bytes::<Event>(payload) {
                print_event(&e, colors);
            }
        }
        _ => {} // frames not relevant to this view (or later-phase types)
    }
}

fn print_log(l: &Log, colors: bool) {
    let now = chrono::Local::now().format("%H:%M:%S%.3f");
    let secs = l.uptime_us / 1_000_000;
    let ms = (l.uptime_us % 1_000_000) / 1_000;
    let (label, code) = match l.level {
        Level::Error => ("ERROR", 31),
        Level::Warn => ("WARN ", 33),
        Level::Info => ("INFO ", 32),
        Level::Debug => ("DEBUG", 36),
        Level::Trace => ("TRACE", 90),
    };
    println!(
        "{now} [{secs:>5}.{ms:03}] {} {}: {}",
        paint(label, code, colors),
        l.module,
        l.message
    );
}

fn print_event(e: &Event, colors: bool) {
    let now = chrono::Local::now().format("%H:%M:%S%.3f");
    let fields: Vec<String> = e.fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!(
        "{now} {} {}  {}",
        paint("EVENT", 35, colors),
        e.name,
        fields.join(" ")
    );
}

fn paint(s: &str, code: u8, colors: bool) -> String {
    if colors {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
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

fn shell(port: Option<String>) -> Result<()> {
    let port = pick_port(port)?;
    let sp = open(&port)?;
    eprintln!("[tower] shell on {port} — TAB completes; commands start with '/'; 'exit' to quit");

    let conn = Rc::new(RefCell::new(Conn {
        sp,
        dec: FrameDecoder::new(),
        req_id: 0,
    }));
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
                match read_response(&mut **sp, dec, cmd_id, Duration::from_millis(1500)) {
                    Some((result, text)) => {
                        print!("{text}");
                        if !text.is_empty() && !text.ends_with('\n') {
                            println!();
                        }
                        if result != 0 {
                            eprintln!("[result {result}]");
                        }
                    }
                    None => eprintln!("[tower] no response (timeout)"),
                }
                cmd_id = cmd_id.wrapping_add(1);
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("[tower] {e}");
                break;
            }
        }
    }
    Ok(())
}

/// Run a single shell command non-interactively: send it, print the (reassembled)
/// response, and exit non-zero if the device reports a non-zero result or times out.
fn exec_cmd(port: Option<String>, line: String) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = open(&port)?;
    let mut dec = FrameDecoder::new();
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
    match read_response(&mut *sp, &mut dec, 1, Duration::from_millis(1500)) {
        Some((result, text)) => {
            print!("{text}");
            if !text.is_empty() && !text.ends_with('\n') {
                println!();
            }
            if result != 0 {
                eprintln!("[result {result}]");
                std::process::exit(i32::from(result));
            }
            Ok(())
        }
        None => bail!("no response (timeout)"),
    }
}

/// Read frames until the `ShellResponse` for `cmd_id` completes (`last`), or timeout.
/// Non-matching frames (logs/events) are ignored.
fn read_response(
    sp: &mut dyn serialport::SerialPort,
    dec: &mut FrameDecoder,
    cmd_id: u16,
    timeout: Duration,
) -> Option<(u8, String)> {
    let deadline = Instant::now() + timeout;
    let mut text = String::new();
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
                text.push_str(r.text);
                if r.last {
                    return Some((r.result, text));
                }
            }
        }
    }
    None
}

// ---- completion (target-authoritative) ------------------------------------

/// An owned copy of a completion result (the wire form borrows the frame buffer).
struct CompletionResult {
    token_start: u16,
    common_prefix: String,
    candidates: Vec<(String, CandidateKind)>,
    more: bool,
}

/// Send a `ShellComplete` and wait for the matching `ShellCompletions`. Shared by the
/// `complete` command and (later) the interactive TAB handler.
fn request_completions(
    sp: &mut dyn serialport::SerialPort,
    dec: &mut FrameDecoder,
    line: &str,
    cursor: u16,
    req_id: u16,
    timeout: Duration,
) -> Option<CompletionResult> {
    let mut buf = [0u8; tower_protocol::MAX_WIRE];
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

fn complete_cmd(port: Option<String>, line: String) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = open(&port)?;
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

fn monitor(port: Option<String>, hex: bool) -> Result<()> {
    let port = pick_port(port)?;
    let mut sp = open(&port)?;
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

fn hexline(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
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
    let opts = jolt::flash::FlashOptions {
        erase,
        verify,
        run,
        go,
        verbose,
    };
    jolt::flash::flash(&mut sp, &fw, &opts).context("flashing firmware")
}

fn erase_cmd(port: Option<String>, verbose: bool) -> Result<()> {
    let port = pick_port(port)?;
    eprintln!("[tower] erasing {port}");
    let mut sp = jolt::port::Port::open(&port).with_context(|| format!("opening {port}"))?;
    let pages = jolt::flash::erase(&mut sp, verbose).context("erasing flash")?;
    eprintln!("[tower] erased {pages} page(s), reset into application");
    Ok(())
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
