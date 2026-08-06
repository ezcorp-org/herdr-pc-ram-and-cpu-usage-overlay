# herdr-pc-ram-and-cpu-usage-overlay
<img width="393" height="243" alt="Screenshot 2026-07-11 at 2 41 03 PM" src="https://github.com/user-attachments/assets/720aace3-4aa2-4474-a8ae-3197b84f4f79" />

A [herdr](https://herdr.dev) plugin that shows **live CPU and RAM usage per
space (workspace)** — so when you're running a herd of agents, you can see at
a glance which space is eating your machine.

```
● web-app
    main
    cpu 26% · ram 8%                ← spaces card (sidebar mode, the default)

⚡ web-app
    idle · usage · cpu 26% · ram 8%   ← agents panel (mode = "agents-panel")
```

**It works the moment you install it.** No action to invoke, no config to paste
— see [Zero-setup](#zero-setup) for exactly what that involves, including the one
line it adds to herdr's own `config.toml` and how to take it back out.

With a Nerd Font installed it detects that and uses icons instead —
` 26% ·  8%`. See [Icons](#icons-and-labels).

- Per-space CPU% and RAM%, both a share of the **whole machine** (0–100%, so a
  busy space reads e.g. `cpu 4%` — not a per-core figure that can exceed 100%),
  refreshed every 5s
- **Battery**, with a charge-level gauge, on the surfaces that draw the machine
  once — never repeated per space — and **hidden entirely on a machine that has
  none**, so desktops and servers see no empty cell
- **Worktree-aware**: workspaces opened as worktree children are folded into
  their parent space's total
- All-space totals in your terminal's window title:
  `spaces · cpu 39% · ram 8% · bat 74%+`
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

## Zero-setup

Installing is the whole setup. Within a few seconds of the install — or the next
time you switch spaces — every space card is showing its CPU and RAM.

Three things have to be true for a reading to appear, and the plugin arranges all
three itself:

| | What it needs | How it gets there |
|---|---|---|
| 1 | The updater running | Wanted by default. Only an explicit `status-disable` turns it off, and that decision survives restarts. |
| 2 | `mode = "sidebar"` | The default since 1.8.0. |
| 3 | A `$usage` row in **herdr's** `config.toml` | Added once, on first run. |

Point 3 is the one worth reading about, because it edits a file you own.

**Why it is necessary.** herdr only draws a `$name` token that some row in its own
config references, and herdr's built-in rows reference none. There is no plugin
API and no manifest section for contributing one — so without this step the
daemon runs, pushes a perfectly good token, and the sidebar stays empty. That is
what "the plugin does nothing" used to mean.

**What it does.** Appends one row to `[ui.sidebar.spaces]`, wrapped in a marker:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch", "git_status"],
  # --- added by ez-corp.space-usage (removed by `status-disable`) ---
  ["$usage"],
  # --- end ez-corp.space-usage ---
]
```

**The guarantees.**

- **Once.** First run only. A later `status-enable` will not re-add a row you
  deleted.
- **Guarded.** If your config already references `$usage` anywhere in that table
  — because you set this up by hand before 1.8.0 — nothing is written at all.
- **Reversible.** `status-disable` removes the marked block. A `$usage` row *you*
  wrote carries no marker and is left alone.
- **Recoverable.** The previous config is copied to
  `config.toml.space-usage.bak` before every write, and the write itself is a
  temp-file rename, so an interrupted run cannot leave a truncated config.
- **Announced.** The enable toast says when it edited your config.

If you would rather it never touched your config, add the row yourself first —
the guard then sees it and stays out of the way.

## Usage

It is already running. To turn it off (and take the config row back out):

```sh
herdr plugin action invoke status-disable --plugin ez-corp.space-usage
```

`status-enable` brings it back; `status-toggle` flips whichever way it is.

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

The updater **survives herdr restarts**, and comes up on its own after an
install. herdr runs the manifest's `[[startup]]` hook (`--restore`) on every
server start — including a live `herdr update --handoff` — and the `[[events]]`
hook on `workspace.focused`. The second exists because `herdr plugin install`
does *not* run startup hooks: without it, a plugin installed into a herdr that is
already running would sit inert until the next restart.

Whether it runs comes down to one marker with three states: never decided (a
fresh install → run), enabled, and disabled. Only the last keeps it down, so a
deliberate `status-disable` stays disabled across restarts while a new install
starts itself. Requires herdr ≥ 0.7.5; verified against 0.7.5 and 0.8.0.

> **Upgrading from < 1.8.0?** Two defaults changed, both toward "works without
> being told": the updater now runs unless you have disabled it, and `mode`
> defaults to `sidebar` rather than `agents-panel`. If you had deliberately left
> the updater off, run `status-disable` once — the old version recorded "off" by
> deleting its marker, which now reads as a fresh install. If you preferred the
> agents panel, set `mode = "agents-panel"` in the plugin config.

## Modes

Configure in `$HERDR_PLUGIN_CONFIG_DIR/config.toml`
(herdr prints the config dir via `herdr plugin config-dir ez-corp.space-usage`):

```toml
mode = "sidebar"            # default — usage inside each spaces card
# mode = "agents-panel"     # usage on the agents-panel row instead
interval_seconds = 5        # 1..28800; statuses get a TTL of three intervals
window_title_totals = true
battery = true              # machine-wide cell on the title/report, not the rows
icons = "auto"              # auto | text | unicode | nerdfont | emoji
ram_display = "percent"     # or "gb" — always the compact absolute (513M / 1.5G)
```

- **sidebar** (default): renders usage inside each spaces card, under the branch
  name. Needs no patched build — see below.
- **agents-panel**: each space gets its own entry in the sidebar agents panel via
  a `usage` pseudo-agent on a spare shell pane, carrying the usage token.

Switching modes cleans up after the other mode automatically. The first run
writes its `$usage` row into whichever table the mode renders from
(`[ui.sidebar.spaces]` or `[ui.sidebar.agents]`), so if you change `mode` after
setup you will need the row in the other table — add it by hand, or
`status-disable` and `status-enable` to have it moved for you.

### herdr 0.7.5+ (native sidebar tokens — no patch needed)

Since herdr **0.7.5** the sidebar is drawn from configurable **token rows**, so
sidebar mode no longer needs a patched build. This plugin pushes a named
**`usage`** metadata token (`workspace.report_metadata`, replacing the old
`custom_status`), referenced as **`$usage`** in herdr's own `config.toml`. Since
1.8.0 the plugin adds that reference itself ([Zero-setup](#zero-setup)); this is
what it writes, and what to write by hand if you would rather it did not:

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

The battery is **one reading for the whole machine**, so it is drawn on the
surfaces that show the machine once and nowhere else:

| surface | battery |
|---|---|
| window title (`window_title_totals`) | yes — `spaces · cpu 39% · ram 8% · bat 74%+` |
| `--once` / dashboard report | yes, on the total line |
| `--json` payload | yes, `battery_percent` / `battery_state` |
| per-space sidebar rows | **no** — see below |

It **disappears completely on a machine that has no battery** — a desktop, a
server, or a VM shows nothing rather than a fabricated `0%`. The machine-wide
row, in the Unicode tier:

```
cpu ░26% · ram ░8% · bat ▓74%+      74% and charging
cpu ░26% · ram ░8% · bat ▒52%       52% on battery
cpu ░26% · ram ░8%                  no battery in this machine
```

A trailing `+` means charging; `=` means on power but holding (full, or capped
by a vendor charge limit). Turn the cell off with `battery = false`, which also
skips the read entirely — no sysfs walk, no `pmset` process.

**Why not on the space rows.** herdr's sidebar has no machine-wide row of its
own, so a plugin that put the battery in each space's `$usage` token would draw
the same figure once per space — three spaces, the same `bat 74%` three times —
and a number repeated beside a per-space cpu and ram reads as if the space had
its own pack. The rows stay `cpu · ram`. If you want a battery *in the sidebar*,
it belongs in herdr's own spaces header next to the cpu/ram readout, which is
herdr's to render, not a plugin's.

Two details worth knowing:

- On Linux, peripherals register as batteries too. A wireless mouse appears in
  `/sys/class/power_supply` as `type=Battery` with `scope=Device`; those are
  filtered out, so your mouse never gets reported as the machine's battery.
- Plugins run on the machine hosting the **herdr server**. Attach from a laptop
  to a remote server and the battery shown is the *server's* — usually none.
  That is consistent with cpu and ram, which are also the server's.

## Icons and labels

`icons` picks the glyph vocabulary. Run the **Preview icon tiers** action (or
`space-usage --icons`) to see all four drawn in your own terminal before
choosing — whether a Nerd Font is installed, and whether your terminal draws
emoji at one column or two, is something only you can see.

| tier | renders | needs |
|---|---|---|
| `text` | `cpu 26% · ram 8% · bat 74%` | nothing |
| `unicode` | `cpu ░26% · ram ░8% · bat ▓74%` | nothing (opt-in) |
| `nerdfont` | ` 26% ·  8% ·  74%` | a Nerd Font |
| `emoji` | `💻26% · 🧠8% · 🔋74%` | a colour emoji font |

The preview draws all three metrics so you can judge every glyph at once. A
space's row is the first two cells only — the battery rides the machine-wide
surfaces ([Battery](#battery)).

### What `auto` does

`auto` (the default) tries to detect whether this machine can draw icons:

1. If the locale is not UTF-8 → `text`. A terminal that cannot carry UTF-8
   cannot carry a Nerd Font glyph either, and this costs two `getenv`s.
2. Otherwise it asks fontconfig (`fc-list :charset=f4bc`) whether any installed
   font carries a Nerd Font glyph. Found → `nerdfont`. Not found → `text`.

The probe runs **once per process** and is cached; the updater never re-forks it.

Two honest limits:

- **It detects "a Nerd Font is installed", not "your terminal uses one".** herdr
  draws into whatever terminal emulator you launched it from, and that font
  lives in the emulator's own config, which no plugin can read. The updater
  daemon runs detached with null stdio, so it has no terminal to interrogate
  even in principle. If you have a Nerd Font on disk but your terminal is set to
  something else, `auto` will guess wrong — set `icons` explicitly.
- **fontconfig is a Linux convention.** macOS and Windows generally have no
  `fc-list`, so `auto` yields `text` there. Run the preview and pick a tier.

The fallback is always `text`, never a guess: an unreadable sidebar is strictly
worse than plain words.

`auto` never selects `unicode`. Those glyphs are *present* in the stock faces —
that was measured — but `░` and `▒` are dither patterns, and at terminal sizes
they render as an indistinct blob rather than a light shade. Present and legible
are different properties and only the first can be measured from here, so the
gauge is opt-in. If you want it: `░` below 34%, `▒` below 67%, `▓` below 90%,
`█` above.

There is no pictogram for "CPU" or "battery" that renders without installing a
font: the battery emoji `U+1F50B` is absent from both DejaVu Sans Mono and
Liberation Mono, and Nerd Font glyphs live in the Private Use Area. Every glyph
the `text` and `unicode` tiers emit was checked with `fc-list :charset=<cp>`
against both faces and is present in both; a test asserts they stay inside the
BMP, outside the Private Use Area, and on that measured list, so the "no font
install" promise cannot rot.

### One setting for both the header and these rows

On a patched build the sidebar has a **whole-machine system-usage header** as
well as these per-space rows, and the header is herdr's own — it renders from
`cpu_label` / `ram_label` in herdr's `config.toml`. Setting only the plugin's
`icons` changes the rows and leaves the header spelling `cpu` in words directly
above a row of glyphs.

Those two keys are the single point that changes both, because this plugin
honours them too — and **an explicit label replaces the tier's glyph rather than
stacking with it**, so you never get the icon drawn twice:

```toml
# in herdr's own config.toml — drives BOTH the header and these rows
[ui]
cpu_label = ""
ram_label = ""
```

```toml
# in the plugin's config.toml — battery only (title, report, JSON)
battery_label = ""
icons = "text"          # the labels are doing the naming now
```

`space-usage --icons` prints the `[ui]` block for whichever tier you are
previewing, so you can copy it straight across.

**Why battery is in the other file:** herdr has no battery of its own to label,
so `battery_label` is not a key it knows. Putting it in herdr's `[ui]` makes
every `herdr server reload-config` report
`unknown config key ui.battery_label; ignoring key`. Nothing outside this plugin
renders a battery, so there is no second surface to keep in step. `cpu_label`
and `ram_label` live in herdr's config precisely because the header does share
them.

**No header to share with?** On a stock (unpatched) build herdr draws no
system-usage header, so `cpu_label` / `ram_label` are not keys it knows either —
they still work from herdr's `[ui]`, but `herdr config check` and every reload
flag them as unknown, the same noise that moved `battery_label` out. For that
case the plugin's own `config.toml` accepts the same two keys, each overriding
the herdr-side value only when set:

```toml
cpu_label = "C"
ram_label = ""          # empty = name nothing: just the figure
```

Unlike herdr's file, where a blank label reads as unset (see below), nothing
ships these plugin keys as blank templates — an empty value here is honoured as
the deliberate "name nothing". Leave a key out to keep herdr's value; on a
patched build, remember the header follows only the herdr-side keys, so
overriding here lets the two surfaces name things differently.

**Applying a change.** The updater re-reads both files every refresh, so the
rows follow within one interval — no `status-toggle` needed. herdr's header
picks up its own config on `herdr server reload-config`. So:

```sh
herdr server reload-config     # header updates; rows follow on the next refresh
```

Watch out for empty values. herdr ships these keys commented out with **blank**
quotes and a note naming the glyph to paste:

```toml
# cpu_label = ""   #  nf-oct-cpu
```

Uncommenting that without pasting a glyph in leaves an empty label, which reads
as *unset* — you get the tier's own naming back, not a blank. That is
deliberate: honouring the blank literally would strip the naming off every row
and leave bare percentages with no clue why.

Defaults are `cpu` / `ram` / `bat` when nothing names them.

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
