# herdr-pc-ram-and-cpu-usage-overlay
<img width="393" height="243" alt="Screenshot 2026-07-11 at 2 41 03 PM" src="https://github.com/user-attachments/assets/720aace3-4aa2-4474-a8ae-3197b84f4f79" />

A [herdr](https://herdr.dev) plugin that shows **live CPU and RAM usage per
space (workspace)** — so when you're running a herd of agents, you can see at
a glance which space is eating your machine.

```
● web-app
    main
    cpu 26% · ram 8%          ← spaces card (sidebar mode, patched herdr)

⚡ web-app
    idle · usage · cpu 26% · ram 8%   ← agents panel (default mode, stock herdr)
```

- Per-space CPU% and RAM%, both a share of the **whole machine** (0–100%, so a
  busy space reads e.g. `cpu 4%` — not a per-core figure that can exceed 100%),
  refreshed every 5s
- **Worktree-aware**: workspaces opened as worktree children are folded into
  their parent space's total
- All-space totals in your terminal's window title: `spaces · cpu 39% · ram 8%`
- A live dashboard pane and one-shot report/JSON actions
- A small static Rust binary (~2–5 MB resident) that talks to herdr over its
  unix socket (a named pipe on Windows) — no per-sample subprocess spawns, no
  Node runtime
- Runs on **Linux and Windows** (herdr Windows beta)

## Install

```sh
herdr plugin install ezcorp-org/herdr-pc-ram-and-cpu-usage-overlay
```

Requirements: Linux or Windows, and the **Rust toolchain** (`cargo`) on the box
hosting the herdr server — herdr compiles the plugin at install time via
`cargo build --release`. Plugins run on the machine hosting the herdr server, so
remote setups need these on the server box only. `node` is no longer required.

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

## Labels

The `cpu`/`ram` tokens are read from herdr's own `config.toml` `[ui]`
(`cpu_label` / `ram_label`, default `cpu`/`ram`) — set them to nerd-font icons to
taste. On a patched build this also matches the sidebar's system-usage header,
which reads the same two keys. Restart the updater to pick up a change.

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
  gets no row of its own; its usage is shown on one of its agent rows instead.
  Open any plain shell pane in the space to get the dedicated row back.
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
</content>
</invoke>
