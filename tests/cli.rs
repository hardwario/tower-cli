//! End-to-end CLI tests: drive the compiled `tower` binary with no hardware attached, so
//! they exercise argument parsing, the `--help`/usage surface, and the exit-code contract
//! (see `main.rs`: 0 ok, 1 tool error, 2 usage). Anything that would open a serial port is
//! avoided — `devices` merely enumerates ports (fine with none), and the bad-argument cases
//! fail in clap before any I/O.

use assert_cmd::Command;
use predicates::prelude::*;

fn tower() -> Command {
    Command::cargo_bin("tower").expect("binary `tower` builds")
}

#[test]
fn help_lists_all_subcommands() {
    tower()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("HARDWARIO TOWER console host"))
        .stdout(predicate::str::contains("logs"))
        .stdout(predicate::str::contains("events"))
        .stdout(predicate::str::contains("shell"))
        .stdout(predicate::str::contains("exec"))
        .stdout(predicate::str::contains("console"))
        // The bare-`tower` UX: the command is optional (no subcommand → TUI).
        .stdout(predicate::str::contains("[COMMAND]"));
}

#[test]
fn version_prints_a_version() {
    tower()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn delay_without_reset_is_a_usage_error() {
    // `--delay` `requires = "reset"`, so this must be a clap usage error (exit 2), not a run.
    tower()
        .args(["logs", "--delay", "100"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--reset"));
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    tower().arg("definitely-not-a-command").assert().code(2);
}

#[test]
fn color_rejects_invalid_value() {
    // `--color` is an enum {auto,always,never}; anything else is a usage error.
    tower()
        .args(["logs", "--color", "rainbow"])
        .assert()
        .code(2);
}

#[test]
fn devices_runs_with_no_hardware() {
    // Enumerating ports must succeed (exit 0) even when none are present — it's the one
    // command that touches the serial subsystem but never opens a port.
    tower().arg("devices").assert().success();
}

#[test]
fn no_colors_alias_is_accepted_but_hidden() {
    // The deprecated `--no-colors` alias still parses (help hides it). Pair it with an
    // impossible device so we fail fast on the (now-fatal) open rather than hanging on a real
    // device — the point is only that the *flag* parses, i.e. we don't get a usage error (2).
    tower()
        .args([
            "logs",
            "--no-colors",
            "--device",
            "/dev/tower-cli-test-nonexistent",
        ])
        .assert()
        .code(1);
}

#[test]
fn nonexistent_device_first_open_is_fatal() {
    // A bad `--device` must exit 1 (tool error), not spin forever in the reconnect loop.
    tower()
        .args(["logs", "--device", "/dev/tower-cli-test-nonexistent"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error"));
}

#[test]
fn console_nonexistent_device_first_open_is_fatal() {
    // The console TUI honours the same "first open is fatal" contract: a bad `--device` must
    // fail before ratatui takes the terminal (so the error prints normally and it exits 1),
    // rather than opening an empty four-pane UI that reconnects forever.
    tower()
        .args(["console", "--device", "/dev/tower-cli-test-nonexistent"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error"));
}

// ---- gateway / nodes / net surfaces (no hardware, no broker) ---------------------

#[test]
fn gateway_help_lists_modes() {
    Command::cargo_bin("tower")
        .unwrap()
        .args(["gateway", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--service"))
        .stdout(predicates::str::contains("--mqtt"))
        .stdout(predicates::str::contains("--prefix"));
}

#[test]
fn nodes_help_lists_subcommands() {
    Command::cargo_bin("tower")
        .unwrap()
        .args(["nodes", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("list"))
        .stdout(predicates::str::contains("add"))
        .stdout(predicates::str::contains("shell"))
        .stdout(predicates::str::contains("dequeue"));
}

#[test]
fn gateway_broker_conflicts_with_mqtt() {
    // Usage errors are clap's: exit 2.
    Command::cargo_bin("tower")
        .unwrap()
        .args([
            "gateway",
            "--broker",
            "127.0.0.1:1883",
            "--mqtt",
            "host:1883",
        ])
        .assert()
        .code(2);
}

#[test]
fn nodes_list_fails_fast_without_broker() {
    // Port 1 is never a broker: a clean tool error (1), not a hang.
    Command::cargo_bin("tower")
        .unwrap()
        .args(["nodes", "list", "--mqtt", "127.0.0.1:1", "--timeout", "500"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("error"));
}

#[test]
fn net_status_fails_fast_without_broker() {
    Command::cargo_bin("tower")
        .unwrap()
        .args(["net", "status", "--mqtt", "127.0.0.1:1", "--timeout", "500"])
        .assert()
        .code(1);
}
