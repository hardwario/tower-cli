# tower-cli — working notes for Claude

The `tower` host CLI: decodes the framed serial link from the TOWER firmware (logs / events /
shell / TUI), flashes/erases/resets over the UART bootloader (via the `jolt` engine), and serves
FOTA images (`tower fota serve`).

## Shared wire protocol (`tower-protocol`) — keep it in lockstep

The wire format lives in a **separate repo**, github.com/hardwario/tower-protocol, pinned here by
git tag in `Cargo.toml`. The firmware (`tower-firmware`) pins the **same tag** — the two MUST move
together, because postcard isn't self-describing (mismatched versions silently mis-decode).

**To bump tower-protocol** (after a change is released there — see that repo's `CLAUDE.md`):

```sh
# set tag = "vX.Y.Z" in Cargo.toml, then:
mv .cargo/config.toml .cargo/config.toml.bak   # the paths override shadows the git source
cargo update -p tower-protocol                  # re-resolve the lock to the new tag
mv .cargo/config.toml.bak .cargo/config.toml
cargo build
```

…and bump **tower-firmware** to the same tag in the same change-set.

## Local dev override

`.cargo/config.toml` is **gitignored** and holds a `paths` override to side-by-side checkouts
(`../../tower-protocol`, `../jolt`) so local edits are picked up without re-tagging. A standalone
clone has no such file and builds from the pinned tags. The override does **not** modify
`Cargo.lock`, and (as above) must be moved aside when you actually want to re-resolve the lock.
