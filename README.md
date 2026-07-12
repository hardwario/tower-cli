# tower-cli

Host CLI/TUI for the **HARDWARIO TOWER** console link. The `tower` binary connects to a
TOWER device over the USB serial port and renders the **framed** host↔target protocol
(COBS + CRC + postcard) emitted by the [tower firmware](https://github.com/hardwario/tower-firmware)
— logs, events, and an interactive shell. (A plain serial monitor shows that link as raw
frame bytes; `tower` decodes it.)

The wire format is shared with the firmware via the `tower-protocol` crate (pinned to a
firmware tag), so the two cannot drift.

## Install

Grab the archive for your platform from the [latest release](https://github.com/hardwario/tower-cli/releases/latest),
extract it, and put `tower` on your `PATH`:

```sh
# macOS / Linux
tar -xzf tower-vX.Y.Z-<target>.tar.gz
sudo mv tower-vX.Y.Z-<target>/tower /usr/local/bin/

# verify the download against its checksum
shasum -a 256 -c tower-vX.Y.Z-<target>.tar.gz.sha256
```

On Windows, extract the `.zip` and put `tower.exe` on your `PATH`.

| Platform | Target |
|----------|--------|
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| Linux ARMv7 (Raspberry Pi) | `armv7-unknown-linux-gnueabihf` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

## Usage

```sh
tower --help          # all commands
tower --version       # print the tower-cli version
tower                 # open the TUI console on the detected device (bare, no subcommand)
```

The streaming commands render the framed link to stdout:

```sh
tower logs                        # decoded logs + print! output
tower logs --color never          # force plain output (--color auto|always|never)
tower events                      # structured key=value events
tower exec "/system/resource print"   # run one shell command, print its reply, exit
tower exec "/slow/op" --timeout 5000   # widen the per-command idle timeout (ms)
tower logs --no-reconnect         # exit when the link drops instead of retrying
```

The interactive commands:

```sh
tower                             # bare: open the TUI console (same as `tower console`)
tower console                     # full-screen TUI: logs + events + shell in one view
tower console --reset             # NRST-pulse the device on connect to see it boot
tower shell                       # line-based interactive shell (commands start with /)
tower shell --reset --timeout 3000
tower complete "/sys"             # ask the target to complete a partial command line
```

And for transport debugging:

```sh
tower monitor                     # dump decoded frames as they arrive
tower monitor --hex               # dump every raw received byte as hex
```

Colors default to **auto**: on when stdout is a terminal and `NO_COLOR` is unset,
off when piped or redirected. The first device open is always fatal (a bad `--device`
exits non-zero rather than retrying); reconnection only kicks in after a successful
attach, and `--no-reconnect` disables it entirely.

The device is selected with `-d`/`--device` (auto-detected when exactly one USB serial
device is present). List what's connected with `tower devices`.

### Gateway & MQTT bridge

With a Radio Dongle running the `radio_dongle_gateway` firmware, `tower gateway` bridges
its radio network to MQTT — a full-screen TUI by default, or headless with `--service`. It
hosts an embedded MQTT broker unless pointed at an external one:

```sh
tower gateway                       # TUI + embedded broker on 127.0.0.1:1883
tower gateway --service             # headless: MQTT is the only interface, logs to stderr
tower gateway --broker 0.0.0.0:1883 # embedded broker, bound to a given address
tower gateway --mqtt host:1883      # use an existing broker instead of hosting one
                                    #   (--mqtt-user / TOWER_MQTT_PASSWORD for auth)
tower gateway --prefix tower/       # MQTT topic prefix (default tower/)
tower gateway --reset               # NRST-pulse the dongle on attach (clears its RAM queue)
```

The MQTT topic tree (under `--prefix`) is the gateway's public API — the full table lives
in [`src/gateway/topics.rs`](src/gateway/topics.rs).

The **client** commands drive a running gateway over MQTT (they share `--mqtt`/`--prefix`;
`<node>` is a `0xHHHHHHHH` address or a friendly name):

```sh
tower nodes list                                   # the gateway's registered nodes
tower nodes show <node> [--keys]                   # one node (--keys reveals its AES key)
tower nodes add --ota [--window 60] [--name N]     # pair over the air (node held in join mode)
tower nodes add --port <SERIAL> [--name N]         # pair a node over its USB cable
tower nodes rename <node> <name>
tower nodes remove <node>
tower nodes shell <node> "<line>" [--timeout MS] [--no-wait]   # remote shell, queued until wake
tower nodes pending <node>                         # queued, not-yet-delivered commands
tower nodes dequeue <node> <ref>                   # drop a queued command by ref
tower net status                                   # gateway online/offline + stats summary
```

Role is enforced: `tower gateway` at a node, or `nodes add --port` at a gateway, exits `126`.

### Exit codes

`tower` follows a small, stable exit-code contract so scripts and CI can branch on
the cause of a failure:

| Code | Meaning |
|------|---------|
| `0`  | success |
| `1`  | tool error (I/O, bad file, encode/decode, truncated/incomplete device response) |
| `2`  | usage error (bad arguments — emitted by clap) |
| `124`| device command timed out (no response at all — also: no gateway reply to an MQTT RPC) |
| `125`| protocol-version mismatch (device speaks a different `tower-protocol` tag — rebuild/re-pin) |
| `126`| wrong firmware role (`tower gateway`/`nodes add --port` pointed at the wrong kind of device) |
| `1..=123` | `exec` only: the device-reported shell result (clamped into this range) |

A response that arrives but is missing chunks exits `1`, not `124` — the device *did*
answer, the output just can't be trusted. `125` is deliberately distinct so CI can
detect the lockstep failure mode (stale `tower` binary vs. freshly flashed firmware)
instead of misreading it as a timeout.

## Flashing firmware

`tower` can program the device's STM32L0 over its UART bootloader (toggling
NRST/BOOT0 through the USB-UART bridge), so a separate flashing tool isn't
needed. The bootloader engine is the [`jolt`](https://github.com/hardwario/jolt)
crate, integrated as a library:

```sh
tower flash firmware.bin     # erase, write, verify, then reset into the app
tower flash firmware.bin --no-verify --no-run
tower erase                  # erase the whole flash, reset into the app
tower reset                  # reset into the application
tower reset --bootloader     # reset into the system bootloader
```

Firmware must be a raw `.bin` (convert `.elf`/`.hex` with
`arm-none-eabi-objcopy -O binary in.elf out.bin`). The device is auto-detected when
exactly one USB serial device is present; otherwise pass `--device`.

## Build from source

Requires a Rust toolchain. On Linux the `serialport` dependency needs libudev:

```sh
sudo apt-get install -y libudev-dev pkg-config   # Debian/Ubuntu
cargo build --release                            # binary at target/release/tower
```

## License

MIT © 2026 HARDWARIO a.s.
