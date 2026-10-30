# Continue on the Mac: Herdr desktop CPU and RAM overlay

Finish installing and checking the desktop mod on the user's Mac. The implementation is ready on a Linux host. The Mac still reports:

```json
{"error":"no mod folder \"space-usage\""}
```

The next step is to install the mod on the Mac, then open it and check its live readings. Continue with local Mac tools. A pairing connection is unnecessary when your agent already runs on the Mac.

## Target and access

- Run on the Mac that runs the user's Herdr desktop app.
- Requested terminal: **This computer → homebrew**. A local shell is also suitable for the installation.
- Desktop mod destination: `$HERDR_APP_HOME/mods/space-usage`, normally `~/.config/herdr-app/mods/space-usage`.
- The user has authorized the installation and opening the overlay. Check the chosen terminal before sending commands.

The previous agent installed the mod on its Linux host only. No files were installed on the Mac. Use local Mac tools and confirm the app can find the mod before reporting success. If terminal control is needed, use the Mac's own Orca runtime.

## 1. Check the Mac environment

```sh
uname -s
uname -m
command -v herdr-app
herdr-app status
```

Proceed when the shell reports `Darwin` and the CLI reaches the intended desktop app. Read the Mac's `~/.config/herdr-app/AGENTS.md` and `herdr-app --skill` for the installed version's API and layout rules. Account for `HERDR_APP_HOME` if set.

## 2. Clone the feature branch and install on the Mac

The desktop implementation and this handoff are on `feat/herdr-desktop-support`:

```sh
git clone --branch feat/herdr-desktop-support https://github.com/ezcorp-org/herdr-pc-ram-and-cpu-usage-overlay.git ~/work/herdr-pc-ram-and-cpu-usage-overlay
cd ~/work/herdr-pc-ram-and-cpu-usage-overlay
cargo build --release
./target/release/space-usage --install-desktop
herdr-app do mod.open '{"name":"space-usage"}'
```

Check for Rust before using the build path. If that checkout already exists, preserve its changes and fetch the feature branch there.

### Optional Mac binaries

Apple Silicon and Intel binaries were cross-compiled on the original Linux host. They are outside Git, in its `target/space-usage-macos.tar.gz`. The source-based install above is self-contained and is the preferred route after cloning this branch.

The binaries were compiled with Rust 1.92.0, cargo-zigbuild 0.23.4, and Zig 0.15.2. Both are valid Mach-O executables. The Apple Silicon binary has an ad hoc code signature. Neither binary has been run on a Mac yet. The cross builds emitted an SDK discovery warning because `xcrun` was absent on Linux.

If the Mac rejects a binary, build from the feature branch as shown in section 2. Keep the native error output for diagnosis.

The feature branch starts from `main` commit `9872a499181b5a52472da3f74b9c567d3898069f`.

## Implementation map

| File | Responsibility |
| --- | --- |
| `src/desktop.rs` | Install the mod and sample desktop process roots through `--desktop-json JSON`. |
| `desktop/main.js` | Native GPUI view; refresh readings, show errors, and select a space on click. |
| `desktop/model.js` | Map local terminals to spaces; include sub-agents through parent links; exclude remote PIDs. |
| `desktop/mod.json` | Mod placement and overlay size: 760 × 260 pixels. |
| `desktop/install-macos.sh` | Install the correct binary from the Mac bundle and open the overlay. |
| `src/config.rs` | Shared config loading through `load_config_at`. |
| `src/render.rs` | Shared numeric readings through `json_spaces`; preserve terminal JSON key order. |
| `src/main.rs` | Add `--install-desktop [app-home]` and `--desktop-json JSON`. |
| `tests/desktop.rs` | Check installation, upgrades, config preservation, and real process sampling without a terminal-app server. |
| `desktop/model.test.mjs` | Check local/remote separation, sub-agents, PID deduplication, cycles, and failed responses. |
| `.github/workflows/ci.yml` | Add Node 22 and desktop model tests to existing platform checks. |
| `README.md` | Desktop install, config, overlay, and personal-layout instructions. |

The mod reuses the existing Rust process sampler and metric formatters. Each refresh starts the copied sampler, measures CPU over 500 ms, and reads RAM from the process trees. Config lives beside that copied binary at `mods/space-usage/config.toml`. Defaults: text labels and a five-second refresh interval.

The implemented desktop scope is **local spaces**. Remote spaces show `Remote · not sampled`. Desktop worktree spaces remain separate. Battery and disk appear with totals. This scope is explicit; remote sampling has not been implemented.

## 3. Verify in the actual Mac app

1. Confirm the mod folder contains `main.js`, `model.js`, `mod.json`, `config.toml`, and the copied `space-usage` binary.
2. Run `herdr-app check` against that mod folder. Require a passing result.
3. Open the overlay. Wait through at least two refresh cycles. Confirm local space names, branch text, CPU, and RAM appear. Check `herdr-app status` for mod errors.
4. Use a short, controlled workload in a local space to check that CPU changes and RAM is measured. Stop only the workload you started.
5. Use `herdr-app shot /tmp/space-usage-mac.png` and inspect the image. Check text clipping, long space names, spacing, and the current theme.
6. Check a row's space-selection action. Preserve the user's mod config through any reinstall.

Herdr's built-in layouts do not accept automatic mod docking. The overlay works in those layouts. If the user wants a permanent bottom panel, make a personal layout:

```sh
herdr-app do layout.clone '{"name":"main","as":"with-space-usage"}'
```

Choose a fresh clone name if that name exists. A restart was needed in the Linux live check before the new docked view rendered. Coordinate a desktop-app restart if needed; keep the terminal core running.

## Checks already passed on Linux

- 277 Rust unit tests, one installation/sampling integration test, and four desktop model tests.
- Formatting, Clippy with warnings denied, release build, and `git diff --check`.
- Herdr desktop 0.1.9 native mod check.
- Live overlay and docked panel under Xvfb with an isolated app home; no mod errors.
- Installed sampler measured a controlled process at 3.1% CPU and 19.8 MB RAM.
- Mac bundle contents, both binary architectures, shell syntax, and the installer's refusal to run on Linux.

For source changes, run:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
node --test desktop/model.test.mjs
cargo build --release
```

The Linux PATH mixed rustc 1.98.1 with Clippy 1.92.0. Its lint check used a matching 1.92.0 toolchain and a separate target directory. Diagnose toolchain mismatches before changing code to address compiler errors.

## Completion and working rules

Finish when the mod is installed in the Mac app's actual home, the native check passes, live CPU/RAM readings work, the UI has been inspected, and the user's requested overlay is open. Report the exact Mac install path and any remaining limit.

Use small changes and shared code. Reproduce Mac failures through the real app before fixing them. Preserve user settings and use personal copies for layout changes. Keep unrelated terminals and the shared core running. Follow the local repository's instructions. End the final response with `Your Actions:` and the user's remaining actions, or `no actions`.
