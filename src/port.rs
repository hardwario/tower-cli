//! Serial-port selection and the console line-control pulse (NRST/BOOT0 over RTS/DTR).
//!
//! This is the host side of attaching to a TOWER device's framed console: pick the USB
//! serial port, open it at the console baud, and drive the modem lines to a known state so
//! merely opening the port can't leave the MCU held in reset. The reset pulse mirrors jolt.

use std::time::Duration;

use anyhow::{Context, Result, bail};

// ---- port selection -------------------------------------------------------

/// USB serial ports, filtered to the kinds a TOWER Core Module presents across platforms.
/// Propagates the enumeration error (mirroring [`devices`]) rather than swallowing it into an
/// empty list, so `pick_port` can tell "no ports attached" apart from "couldn't enumerate"
/// (e.g. no udev in a container) and report the real cause.
pub(crate) fn usb_ports() -> Result<Vec<String>> {
    let ports = serialport::available_ports().context("enumerating serial ports")?;
    Ok(ports
        .into_iter()
        .filter(|p| {
            matches!(p.port_type, serialport::SerialPortType::UsbPort(_))
                || p.port_name.contains("usbserial")
                || p.port_name.contains("ttyUSB")
                || p.port_name.contains("ttyACM")
        })
        .map(|p| p.port_name)
        .collect())
}

/// Resolve the serial-port path to use: the explicit `--device`, else the sole USB serial
/// device. Ambiguity (zero or several) is an error telling the user to pass `--device`; a
/// failed enumeration surfaces its own cause (not a misleading "no device found").
pub(crate) fn pick_port(explicit: Option<String>) -> Result<String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let ports = usb_ports()?;
    match ports.len() {
        1 => Ok(ports.into_iter().next().unwrap()),
        0 => bail!("no USB serial device found; pass --device"),
        _ => bail!(
            "multiple USB serial devices; pass --device (one of: {})",
            ports.join(", ")
        ),
    }
}

/// `tower devices`: one bare port name per line (script-friendly). We deliberately don't
/// delegate to jolt's lister — this lists *all* ports, not just the TOWER-shaped ones.
pub(crate) fn devices() -> Result<()> {
    let ports = serialport::available_ports().context("listing serial ports")?;
    for p in ports {
        println!("{}", p.port_name);
    }
    Ok(())
}

/// Open the console port at 115200 baud with a short read timeout (so read loops can poll).
pub(crate) fn open(port: &str) -> Result<Box<dyn serialport::SerialPort>> {
    serialport::new(port, 115_200)
        .timeout(Duration::from_millis(200))
        .open()
        .with_context(|| format!("opening {port}"))
}

// ---- console line control (NRST/BOOT0 over RTS/DTR) -----------------------
//
// The AUTHORITATIVE copy of this sequence lives in jolt (jolt/src/port.rs:
// `open_with` / `reset_into_app`). We keep a minimal local pulse so a console
// command can reset on the *same* handle it streams from and thus capture boot
// output from the very first byte — reopening the port would drop the `Hello` +
// early logs and re-undefine the line state. RTS->NRST, DTR->BOOT0; (true,true)
// is the safe "run" baseline.
//
// The tuned *delays* are no longer copied — they're taken directly from jolt's
// public constants below, so they can't silently drift out of lockstep (a rename
// upstream becomes a compile error here). The line polarity/ordering is basic,
// stable bridge wiring.
//
// Convergence opportunity (jolt v1.3.0): `jolt::port::Port::from_handle(sp)` +
// `reset_into_app_no_settle()` implement the same run-baseline + pulse over a
// borrowed handle. We don't call it here for one reason: our console pulse clears
// the input buffer *while the chip is held in reset* (dropping pre-reset garbage so
// the boot `Hello` isn't preceded by junk), and `reset_into_app_no_settle` has no
// mid-reset clear hook — clearing after release would race the boot bytes we want.
// If jolt grows a mid-reset-clear variant, drop this fork for `from_handle`.

/// Drive RTS/DTR to the run baseline so merely opening the port can't leave the
/// MCU held in reset by whatever level the USB bridge asserts on open. Mirrors
/// jolt's `open_with` (and reuses its tuned `RUN_SETTLE`).
pub(crate) fn set_run_baseline(sp: &mut dyn serialport::SerialPort) -> Result<()> {
    sp.write_request_to_send(true)?;
    sp.write_data_terminal_ready(true)?;
    std::thread::sleep(jolt::port::RUN_SETTLE);
    Ok(())
}

/// Pulse NRST to reboot into the application (BOOT0 low) so the caller can capture
/// boot output from byte 0. Mirrors jolt's `reset_into_app` (reusing its tuned
/// `RESET_PULSE`) minus the post-boot settle (we want the boot logs), plus a
/// mid-reset input-buffer clear jolt's no-settle variant doesn't offer.
pub(crate) fn pulse_reset_into_app(sp: &mut dyn serialport::SerialPort) -> Result<()> {
    sp.write_request_to_send(true)?; // RTS asserted
    sp.write_data_terminal_ready(false)?; // BOOT0 low -> RESET asserted
    std::thread::sleep(jolt::port::RESET_PULSE);
    let _ = sp.clear(serialport::ClearBuffer::Input); // drop pre-reset bytes while held in reset
    sp.write_request_to_send(false)?; // RESET released -> boot the app
    Ok(())
}

/// Open a console port with the lines in a known state. With `reset`, reboot the
/// application first so the caller observes it coming up from the start.
///
/// On a non-reset open we clear the input buffer: a late frame buffered from a *previous*
/// run (e.g. a trailing `ShellResponse` from an earlier `tower exec`, still using the same
/// `cmd_id`) must not satisfy this run's wait. The reset path deliberately keeps the buffer
/// — `pulse_reset_into_app` already drops pre-reset bytes while the chip is held in reset,
/// and we want the boot `Hello` + early logs that arrive after release.
pub(crate) fn open_console(port: &str, reset: bool) -> Result<Box<dyn serialport::SerialPort>> {
    let mut sp = open(port)?;
    set_run_baseline(&mut *sp)?;
    if reset {
        pulse_reset_into_app(&mut *sp)?;
        eprintln!("[tower] reset into application");
    } else {
        let _ = sp.clear(serialport::ClearBuffer::Input);
    }
    Ok(sp)
}

/// Like [`open_console`] but with a short read timeout so the TUI's drain loop stays snappy
/// (it polls the keyboard between reads and can't afford to block on a quiet link). Same
/// line-state / reset / stale-input handling as [`open_console`]. Kept fallible so the
/// console's *first* open is fatal — a bad `--device` exits 1 instead of spinning in the
/// reconnect loop (the contract the streaming commands already honour).
pub(crate) fn open_console_responsive(
    port: &str,
    reset: bool,
) -> Result<Box<dyn serialport::SerialPort>> {
    let mut sp = serialport::new(port, 115_200)
        .timeout(Duration::from_millis(10))
        .open()
        .with_context(|| format!("opening {port}"))?;
    set_run_baseline(&mut *sp)?;
    if reset {
        pulse_reset_into_app(&mut *sp)?;
        eprintln!("[tower] reset into application");
    } else {
        let _ = sp.clear(serialport::ClearBuffer::Input);
    }
    Ok(sp)
}
