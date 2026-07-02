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
tower exec "/system/resource"     # run one shell command, print its reply, exit
tower exec "/slow/op" --timeout 5000   # widen the per-command idle timeout (ms)
tower logs --no-reconnect         # exit when the link drops instead of retrying
```

Colors default to **auto**: on when stdout is a terminal and `NO_COLOR` is unset,
off when piped or redirected. The first device open is always fatal (a bad `--device`
exits non-zero rather than retrying); reconnection only kicks in after a successful
attach, and `--no-reconnect` disables it entirely.

The device is selected with `-d`/`--device` (auto-detected when exactly one USB serial
device is present). List what's connected with `tower devices`.

### Exit codes

`tower` follows a small, stable exit-code contract so scripts and CI can branch on
the cause of a failure:

| Code | Meaning |
|------|---------|
| `0`  | success |
| `1`  | tool error (I/O, bad file, encode/decode, protocol-version mismatch) |
| `2`  | usage error (bad arguments — emitted by clap) |
| `124`| device command timed out (no or incomplete response) |
| `1..=123` | `exec` only: the device-reported shell result (clamped into this range) |

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
