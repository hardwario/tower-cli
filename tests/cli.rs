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
        .stdout(predicate::str::contains("fota"))
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

