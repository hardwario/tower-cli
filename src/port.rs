//! Serial-port selection and the console line-control pulse (NRST/BOOT0 over RTS/DTR).
//!
//! This is the host side of attaching to a TOWER device's framed console: pick the USB
//! serial port, open it at the console baud, and drive the modem lines to a known state so
//! merely opening the port can't leave the MCU held in reset. The reset pulse mirrors jolt.

use std::time::Duration;

use anyhow::{Context, Result, bail};

// ---- port selection -------------------------------------------------------

/// USB serial ports, filtered to the kinds a TOWER Core Module presents across platforms.
pub(crate) fn usb_ports() -> Vec<String> {
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

/// Resolve the port to use: the explicit `--port`, else the sole USB serial port. Ambiguity
/// (zero or several) is an error telling the user to pass `--port`.
pub(crate) fn pick_port(explicit: Option<String>) -> Result<String> {
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
// `open_with` / `reset_into_app`). We duplicate the minimal pulse here so a
// console command can reset on the *same* handle it streams from and thus
// capture boot output from the very first byte — reopening the port would drop
// the `Hello` + early logs and re-undefine the line state. RTS->NRST,
// DTR->BOOT0; (true,true) is the safe "run" baseline. If the bridge wiring,
// polarity, or timing ever changes in jolt, mirror the change here.
const RESET_PULSE: Duration = Duration::from_millis(100);
const RUN_SETTLE: Duration = Duration::from_millis(50);

/// Drive RTS/DTR to the run baseline so merely opening the port can't leave the
/// MCU held in reset by whatever level the USB bridge asserts on open. Mirrors
/// jolt's `open_with`.
pub(crate) fn set_run_baseline(sp: &mut dyn serialport::SerialPort) -> Result<()> {
    sp.write_request_to_send(true)?;
    sp.write_data_terminal_ready(true)?;
    std::thread::sleep(RUN_SETTLE);
    Ok(())
}

/// Pulse NRST to reboot into the application (BOOT0 low), returning the instant
/// reset is released so the caller can capture boot output from byte 0. Mirrors
/// jolt's `reset_into_app` minus its post-boot settle (we want the boot logs).
pub(crate) fn pulse_reset_into_app(sp: &mut dyn serialport::SerialPort) -> Result<()> {
    sp.write_request_to_send(true)?; // RTS asserted
    sp.write_data_terminal_ready(false)?; // BOOT0 low -> RESET asserted
    std::thread::sleep(RESET_PULSE);
    let _ = sp.clear(serialport::ClearBuffer::Input); // drop pre-reset bytes while held in reset
    sp.write_request_to_send(false)?; // RESET released -> boot the app
    Ok(())
}

/// Open a console port with the lines in a known state. With `reset`, reboot the
/// application first so the caller observes it coming up from the start.
pub(crate) fn open_console(port: &str, reset: bool) -> Result<Box<dyn serialport::SerialPort>> {
    let mut sp = open(port)?;
    set_run_baseline(&mut *sp)?;
    if reset {
        pulse_reset_into_app(&mut *sp)?;
        eprintln!("[tower] reset into application");
    }
    Ok(sp)
}
