# herdr-pc-ram-and-cpu-usage-overlay
<img width="393" height="243" alt="Screenshot 2026-07-11 at 2 41 03 PM" src="https://github.com/user-attachments/assets/720aace3-4aa2-4474-a8ae-3197b84f4f79" />

A [herdr](https://herdr.dev) plugin that shows **live CPU and RAM usage per
space (workspace)** — so when you're running a herd of agents, you can see at
a glance which space is eating your machine.

```
● web-app
    main
    cpu ░26% · ram ░8% · bat ▓74%+   ← spaces card (sidebar mode)

⚡ web-app
    idle · usage · cpu ░26% · ram ░8%   ← agents panel (default mode)
```

- Per-space CPU% and RAM%, both a share of the **whole machine** (0–100%, so a
  busy space reads e.g. `cpu 4%` — not a per-core figure that can exceed 100%),
  refreshed every 5s
- **Battery** next to them, with a charge-level gauge — and **hidden entirely on
  a machine that has none**, so desktops and servers see no empty cell
- **Worktree-aware**: workspaces opened as worktree children are folded into
  their parent space's total
- All-space totals in your terminal's window title: `spaces · cpu 39% · ram 8%`
- A live dashboard pane and one-shot report/JSON actions
- A small static Rust binary (~2–5 MB resident) that talks to herdr over its
  unix socket (a named pipe on Windows) — no per-sample subprocess spawns, no
  Node runtime
- Runs on **Linux, macOS, and Windows** (herdr Windows beta)

## Install

```sh
herdr plugin install ezcorp-org/herdr-pc-ram-and-cpu-usage-overlay
```

Requirements: Linux, macOS, or Windows, and the **Rust toolchain** (`cargo`) on
the box hosting the herdr server — herdr compiles the plugin at install time via
`cargo build --release`. Plugins run on the machine hosting the herdr server, so
remote setups need these on the server box only. `node` is no longer required.

Each platform reads processes its own way, chosen at compile time:

| Platform | Process sampling | Battery |
|---|---|---|
| Linux | `/proc` | `/sys/class/power_supply` |
| macOS | `libproc` (`proc_listallpids`, `proc_pidinfo`) | `pmset -g batt` |
| Windows | Toolhelp32 + `GetProcessTimes` | `GetSystemPowerStatus` |

On Windows the sampling uses the Win32 process APIs instead of `/proc`, and the
herdr socket is reached through its named pipe; both are handled automatically.
If your Rust toolchain targets `x86_64-pc-windows-gnu`, the MinGW binutils
(`dlltool`, `as`) must be on `PATH` for the build (rustup's self-contained set
is not sufficient); the MSVC target needs the Visual C++ build tools plus the
Windows SDK, as usual.

## Usage

Toggle the background updater (statuses appear in the sidebar within ~5s):

```sh
herdr plugin action invoke status-toggle --plugin ez-corp.space-usage
```

Other entrypoints:

```sh
herdr plugin pane open --plugin ez-corp.space-usage --entrypoint dashboard  # live dashboard
herdr plugin action invoke report --plugin ez-corp.space-usage             # one-shot snapshot
herdr plugin action invoke icons --plugin ez-corp.space-usage              # preview icon tiers
./target/release/space-usage --json                                        # machine-readable
```

Status **text** carries a TTL and self-clears if the updater dies. In
agents-panel mode the `usage` pseudo-agent row itself has no TTL (herdr's
`pane.report_agent` takes none), so a hard-killed updater leaves an empty row
behind until the next `status-enable`/`status-disable`. Disabling clears
everything immediately either way.

The updater **survives herdr restarts**. Enabling it records that you want it;
herdr then runs the manifest's `[[startup]]` hook (`--restore`) on every server
start — including a live `herdr update --handoff` — which brings the daemon back
if it isn't already running. Disabling clears that record, so a deliberate
`status-disable` stays disabled across restarts. A fresh install starts with no
record, so nothing runs until you enable it once. Requires herdr ≥ 0.7.5;
verified against 0.7.5 and 0.8.0.

> **Upgrading from < 1.2.0?** The record is only written when you enable the
> updater, so an updater enabled under an older version isn't yet marked as
> wanted. Run `status-enable` (or `status-toggle` twice) once after upgrading —
> from then on it is permanent.

## Modes

Configure in `$HERDR_PLUGIN_CONFIG_DIR/config.toml`
(herdr prints the config dir via `herdr plugin config-dir ez-corp.space-usage`):

```toml
mode = "agents-panel"       # default — works on stock herdr
# mode = "sidebar"          # for herdr builds with the sidebar patch (below)
interval_seconds = 5        # 1..28800; statuses get a TTL of three intervals
window_title_totals = true
battery = true              # show the battery cell when the machine has one
icons = "auto"              # auto | text | unicode | nerdfont | emoji
```

- **agents-panel** (default): each space gets its own entry in the sidebar agents
  panel via a `usage` pseudo-agent on a spare shell pane, carrying the usage token.
- **sidebar**: renders usage inside each spaces card, under the branch name.

Switching modes cleans up after the other mode automatically.

### herdr 0.7.5+ (native sidebar tokens — no patch needed)

Since herdr **0.7.5** the sidebar is drawn from configurable **token rows**, so
sidebar mode no longer needs a patched build. This plugin pushes a named
**`usage`** metadata token (`pane.report_metadata`, replacing the old
`custom_status`), and you reference it as **`$usage`** in herdr's own
`config.toml`:

```toml
[ui.sidebar.spaces]          # sidebar mode → usage under each space
rows = [
  ["state_icon", "workspace"],
  ["branch", "git_status"],
  ["$usage"],
]

[ui.sidebar.agents]          # agents-panel mode → usage on the agent row
rows = [["state_icon", "workspace", "tab"], ["agent", "$usage"]]
```

Built-in `branch` / `git_status` (ahead/behind) tokens are native, so the old
space-usage-line and git-dirty herdr patches are retired. Requires herdr ≥ 0.7.5
(the `tokens` metadata API); older builds need plugin v1.0.x.

## Battery

The battery cell sits next to cpu and ram, and **disappears completely on a
machine that has no battery** — a desktop, a server, or a VM shows nothing
rather than a fabricated `0%`.

```
cpu ░26% · ram ░8% · bat ▓74%+      74% and charging
cpu ░26% · ram ░8% · bat ▒52%       52% on battery
cpu ░26% · ram ░8%                  no battery in this machine
```

A trailing `+` means charging; `=` means on power but holding (full, or capped
by a vendor charge limit). Turn the cell off with `battery = false`, which also
skips the read entirely — no sysfs walk, no `pmset` process.

Two details worth knowing:

- On Linux, peripherals register as batteries too. A wireless mouse appears in
  `/sys/class/power_supply` as `type=Battery` with `scope=Device`; those are
  filtered out, so your mouse never gets reported as the machine's battery.
- Plugins run on the machine hosting the **herdr server**. Attach from a laptop
  to a remote server and the battery shown is the *server's* — usually none.
  That is consistent with cpu and ram, which are also the server's.

Because herdr's sidebar has no machine-wide row, one battery reading is drawn
once per space card. With three spaces open you will see it three times. The
full-width `--once` report avoids this by putting it on the total line only.

## Icons and labels

`icons` picks the glyph vocabulary. Run the **Preview icon tiers** action (or
`space-usage --icons`) to see all four drawn in your own terminal before
choosing — whether a Nerd Font is installed, and whether your terminal draws
emoji at one column or two, is something only you can see.

| tier | renders | needs |
|---|---|---|
| `text` | `cpu 26% · ram 8% · bat 74%` | nothing |
| `unicode` | `cpu ░26% · ram ░8% · bat ▓74%` | nothing |
| `nerdfont` | ` 26% ·  8% ·  74%` | a Nerd Font |
| `emoji` | `💻26% · 🧠8% · 🔋74%` | a colour emoji font |

`auto` (the default) picks `unicode` on a UTF-8 locale and `text` otherwise.

The `unicode` tier is a **level gauge**, not a pictogram: `░` below 34%, `▒`
below 67%, `▓` below 90%, `█` above. That choice is deliberate. There is no
pictogram for "CPU" or "battery" that renders without installing a font — the
battery emoji `U+1F50B` is absent from both DejaVu Sans Mono and Liberation Mono
(the default Linux mono faces), and Nerd Font glyphs live in the Private Use
Area. Every glyph the safe tiers emit was checked with `fc-list :charset=<cp>`
against both faces and is present in both; a test asserts they stay inside the
BMP, outside the Private Use Area, and on that measured list, so the "no font
install" promise cannot rot.

The words themselves come from herdr's own `config.toml` `[ui]` — `cpu_label`,
`ram_label`, and `battery_label` (default `cpu`/`ram`/`bat`). An explicit label
always wins over the tier's own wording, so you can still set them to whatever
you like. Restart the updater to pick up a change.

## How it works

The binary opens one persistent connection to the herdr unix socket (on Windows
the same JSON-RPC rides the named pipe `\\.\pipe\<HERDR_SOCKET_PATH>`) and
speaks its newline-delimited JSON-RPC. Per refresh: `session.snapshot` returns
every workspace and pane in a single call → `pane.process_info` yields each
pane's `shell_pid` → the process walks that PID's process subtree, summing CPU
and RSS over a sample window. Branch comes from the pane cwd's git checkout, and
worktree families from `worktree.list`.

Per platform: Linux reads `/proc/<pid>/stat` (utime+stime jiffie deltas,
`sysconf` clock ticks/page size) and `statm` RSS; Windows snapshots the process
table via Toolhelp32, reads cumulative CPU from `GetProcessTimes` (100 ns
FILETIME ticks), working sets via `K32GetProcessMemoryInfo`, and the machine
total via `GlobalMemoryStatusEx`. Processes the plugin cannot open still keep
their pid→ppid edge so subtree topology stays correct.

Workspaces and panes deliberately come from the **one** `session.snapshot` call
rather than `workspace.list` plus a `pane.list` per workspace: the two can be
read torn apart, and a workspace that closes in between makes the follow-up
`pane.list` fail with `workspace_not_found` — which used to abort the whole
sample.

Branch and worktree grouping are read from the pane `cwd` and `worktree.list`
rather than from the `worktree` block on `workspace.list`. That block can stay
`null` indefinitely for a workspace that really is a repo, so relying on it
would blank the branch out. For the same reason the branch uses `cwd` and not
`foreground_cwd` — the latter is the field that became non-blocking in 0.8.0
(#1838, #2206) and transiently reports `/`.

### Living alongside other plugins

In **agents-panel** mode the usage row needs a pane to live on, and it will
never take one that belongs to another plugin. A pane with no agent but with
metadata tokens that aren't ours (herdr-sidebar's explorer/git heartbeats, for
example) is treated as owned and skipped — otherwise the panel grows a second
"agent" row and the usage text ends up pinned inside someone else's pane.

Two consequences worth knowing:

- If **every** agent-less pane in a space belongs to another plugin, that space
  gets no row of its own; its usage is shown on one of its agent rows instead
  (which is what `[ui.sidebar.agents]`'s `$usage` renders anyway). If it has no
  agent panes either, the space shows nothing until one appears. Opening any
  plain shell pane in the space restores the dedicated row.
- Usage numbers are never affected. Measurement walks every pane in the space
  regardless of who owns it, so plugin panes still count toward its CPU and RAM.

This is a heuristic, and it has to be: herdr reports a pane's tokens as one flat
map merged across all plugins, with no record of who set what. A plugin that
puts tokens on ordinary shell panes will shrink the pool of panes this one can
use, and a plugin that names a token `usage` will look like us.

**sidebar** mode is unaffected — it reports at the workspace level and never
claims a pane at all.

## Development

```sh
git clone <this repo>
cd herdr-pc-ram-and-cpu-usage-overlay
cargo build --release
herdr plugin link .
```

`herdr plugin link` references the directory in place and does **not** run the
build step, so run `cargo build --release` first — the linked commands invoke
`./target/release/space-usage`. (`herdr plugin install` builds automatically.)

## License

MIT — see [LICENSE](LICENSE).
