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
  unix socket — no per-sample subprocess spawns, no Node runtime

## Install

```sh
herdr plugin install ezcorp-org/herdr-pc-ram-and-cpu-usage-overlay
```

Requirements: Linux (reads `/proc`) and the **Rust toolchain** (`cargo`) on the
box hosting the herdr server — herdr compiles the plugin at install time via
`cargo build --release`. Plugins run on the machine hosting the herdr server, so
remote setups need these on the server box only. `node` is no longer required.

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

Statuses carry a TTL and self-clear if the updater dies; disabling clears
everything immediately.

## Modes

Configure in `$HERDR_PLUGIN_CONFIG_DIR/config.toml`
(herdr prints the config dir via `herdr plugin config-dir ez-corp.space-usage`):

```toml
mode = "agents-panel"       # default — works on stock herdr
# mode = "sidebar"          # for herdr builds with the sidebar patch (below)
interval_seconds = 5
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

The binary opens one persistent connection to the herdr unix socket and speaks
its newline-delimited JSON-RPC. Per refresh: `session.snapshot` returns every
workspace and pane in a single call → `pane.process_info` yields each pane's
`shell_pid` → the process walks that PID's `/proc` subtree, summing CPU
(utime+stime jiffie deltas over a sample window) and RSS. Branch comes from the
pane cwd's git checkout, and worktree families from `worktree.list`. Clock ticks
(`_SC_CLK_TCK`) and page size (`_SC_PAGESIZE`) are probed via `sysconf`.

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
