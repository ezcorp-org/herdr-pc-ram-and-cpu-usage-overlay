# Desktop handoff

Status: working. The user confirmed the fix on 2026-10-30.
Version: `1.13.0`. Building either installation requires Rust 1.88 or later.
The existing terminal-plugin install command, config paths, and JSON format
are preserved. Desktop support uses a separate app-home mod folder.

Use the [README desktop guide](README.md#herdr-desktop-app) for installation,
the [bottom-right status bar](README.md#bottom-right-status-bar),
[remote devices](README.md#remote-devices), and
[settings](README.md#desktop-settings). Keep installation instructions there.

## Current setup

| Device | Installed sampler | Executable lookup |
| --- | --- | --- |
| Mac, Apple Silicon | `/Users/obvi/.config/herdr-app/mods/space-usage/space-usage` | The mod uses this path directly. |
| `nixos-amd`, Linux x86_64 | `/home/dev/.config/herdr-app/mods/space-usage/space-usage` | `/home/dev/.local/bin/space-usage` links to the installed sampler. |

- Herdr desktop 0.1.9 uses the personal layout `with-space-usage`.
- Its 28 px status row has the standard status block and a 400 px usage block
  on the right, with `props: {compact: true}`. A layout preview is saved beside it.
- Both devices use `icons = "nerdfont"` and `interval_seconds = 5`.
- The user's original local space was restored and the test dialog closed.
  Test workloads and their terminals were removed. The app and core were not restarted.

## Behavior

The footer follows the active space's device and shows whole-device CPU/RAM,
battery, and disk. Clicking it opens the per-space process list. Each device
has separate readings, errors, refresh timing, and an in-flight guard.
Remote PIDs are sampled only on their owning device. An unavailable device
shows an error instead of another device's readings.

The two fixed causes were a footer that always showed local process totals,
and a sampler with no whole-device CPU/RAM fields. The desktop sampler now
uses `sysinfo` with only its `system` feature and shares the existing 500 ms
measurement window and metric formatters. The terminal plugin's JSON contract
and process sampler remain unchanged.

## Source map

| Files | Role |
| --- | --- |
| `src/desktop.rs` | Install the mod, preserve config, and return system and per-space readings. |
| `desktop/main.js` | Full list, compact footer, and independent device refreshes. |
| `desktop/model.js` | Device ownership, process roots, parent links, and response validation. |
| `src/render.rs`, `src/config.rs` | Shared formatters and config loading. |
| `tests/desktop.rs` | Installation, upgrades, config preservation, and real sampling. |
| `desktop/main.test.mjs`, `desktop/model.test.mjs` | Device switching, stale replies, failures, PID separation, and rendering. |
| `.github/workflows/ci.yml` | Native platform checks and `node --test`. |

## Verification

- 282 Mac and 277 Linux Rust unit tests, the installation/sampling test,
  and 15 JavaScript tests passed.
- Formatting, Clippy with warnings denied, both native release builds,
  Windows cross-target compilation, and native mod checks passed.
- Controlled 64 MB workloads ran on both devices. CPU readings agreed with
  OS counters within 0.1 percentage point; RAM readings matched the OS counters.
- Local and remote footers were inspected for device names, icons, and clipping.
  App status reported no mod errors. The user then confirmed it works.

For code changes, run:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
node --test
cargo build --release --locked
```

The Mac build used temporary Rust 1.92.0 under `/tmp/space-usage-cargo` and
`/tmp/space-usage-rustup`, with no shell profile changes. Set `CARGO_HOME`,
`RUSTUP_HOME`, and PATH to those locations if reusing that toolchain; otherwise
use a normal Rust installation. These temporary directories may be removed by the OS.

## Limits and next work

No required product work remains. New remote devices need a matching sampler
on PATH. Windows was compile-checked; its desktop UI was not tested live.
See the README for the CPU/RAM definitions and per-space percentage rounding.

A saved Herdr image can be an old frame while the window is hidden. Check
that the frame is current before drawing conclusions. Preserve user config,
use personal layout copies, and keep unrelated terminals and the core running.
