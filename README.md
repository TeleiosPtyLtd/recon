# recon

A tmux-native cockpit for running many [Claude Code](https://claude.ai/claude-code) agents in parallel.

The headline mode is **flow** — one tmux session, a 2×2 grid of live agent panes on top, a dashboard underneath. Agents that need your attention are auto-promoted into the grid; Working agents quietly demote to background windows once they're heads-down. You stay in one window and the cockpit reshapes itself around what needs you.

![recon demo](assets/demo.gif)

## Flow

```
┌─ tmux session: recon-flow ──────────────────────────────────────────────┐
│  [1] api-refactor      ⚠            │  [2] debug-pipeline               │
│  ● Input — waiting on you           │  ● Working — streaming response    │
│                                     │                                    │
│  ─────────────────────────────────────────────────────────────────────  │
│  [3] write-tests                    │  [4] recon-shell                  │
│  ● Idle — done, waiting             │  $ _                              │
│                                     │                                    │
├─────────────────────────────────────────────────────────────────────────┤
│  [0] recon-dashboard                                                    │
│   #  Session          Status   Model      Context   Last Active         │
│   1  api-refactor     ● Input  Opus 4.6   45k/1M    2m ago              │
│   2  debug-pipeline   ● Work   Sonnet 4.6 12k/200k  < 1m                │
│   3  write-tests      ● Idle   Haiku 4.5  8k/200k   < 1m                │
│   …                                                                     │
└─────────────────────────────────────────────────────────────────────────┘
```

**What it does**

- **Sticky 2×2 focus zone.** Up to 3 Claude panes share the top zone alongside a shell; an agent stays put while Working — only displaced when something else needs attention and the grid is full.
- **Auto-promote / auto-demote.** Idle or Input agents living in background `bg-<id>` windows are promoted into a free slot. Working agents that have been heads-down for 30s+ are demoted out to make room.
- **Dashboard at the bottom.** Pinned 9 rows tall, runs the standard recon table in a respawn loop. Quit it with `q` and it restarts; the orchestrator never loses sight of the grid.
- **Dark theme + neon-green active border** so the focused slot is unmistakable across a wide monitor.
- **⚠ dangerous marker** in the pane title for agents launched with `--dangerously-skip-permissions`.
- **Graceful shutdown.** `recon flow stop` break-panes every Claude out to its own `bg-<id>` tmux session so your work survives — pass `--force` to kill them.

**Setup**

```bash
cargo install --path .   # puts `recon` on PATH
recon flow               # creates tmux session 'recon-flow' and attaches
```

That's it — `recon flow` is idempotent. First run creates the session, splits the focus grid, spawns the orchestrator in a hidden window, and attaches. Re-running heals any missing pieces (e.g. if you killed the orchestrator pane) and re-attaches.

```bash
recon flow --slots 3     # cap Claude panes in the grid (default 3, leaves 1 cell for shell)
recon flow status        # diagnose: windows, panes, orchestrator status
recon flow stop          # graceful shutdown — preserves Claude work as bg-* sessions
recon flow stop --force  # kill all Claude processes too
```

**Spatial pane keys** (active only inside `recon-flow`, so they don't intercept typing elsewhere):

```
[Alt+4 top-left ][Alt+5 top-right]
[Alt+1 btm-left ][Alt+2 btm-right]
[        Alt+0 dashboard         ]
```

The bindings use tmux directional pane tokens (`{top-left}`, `{bottom}`), so they survive any future layout reshuffle.

**Spawning a new agent inside flow:** focus the shell pane (Alt+anything that lands on it), run `claude` (or `recon launch --attach`). It immediately joins the rotation — the orchestrator picks it up on the next tick.

## Other Views

### Tamagotchi View (`recon view` or press `v`)

A visual dashboard where each agent is a pixel-art creature living in a room. Designed for a side monitor — glance over and instantly see who's working, sleeping, or needs attention.

Creatures are rendered as colored pixel art using half-block characters. Working and Input creatures animate; Idle and New stay still.

| State | Creature | Color |
|-------|----------|-------|
| **Working** | Happy blob with sparkles and feet | Green |
| **Input** | Angry blob with furrowed brows | Orange (pulsing) |
| **Idle** | Sleeping blob with Zzz | Blue-grey |
| **New** | Egg with spots | Cream |

- **Rooms** group agents by git repository — worktrees of the same repo share a room, while monorepo sub-projects get their own (e.g. `myapp` vs `myapp › tools/cli`) (2×2 grid, paginated)
- **Zoom** into a room with `1`-`4`, page with `j`/`k`
- **Context bar** per agent with green/yellow/red coloring

### Table View (default)

```
┌─ recon — Claude Code Sessions ──────────────────────────────────────────────────────────────────────────┐
│  #  Session          Git(Project::Branch)   Directory          Status  Model       Context  Last Active │
│  1  api-refactor     myapp::feat/auth       ~/repos/myapp      ● Input Opus 4.6    45k/1M   2m ago      │
│  2  debug-pipeline   infra::main            ~/repos/infra      ● Work  Sonnet 4.6  12k/200k < 1m        │
│  3  write-tests      myapp::feat/auth       ~/repos/myapp      ● Work  Haiku 4.5   8k/200k  < 1m        │
│  4  code-review      webapp::pr-452         ~/repos/webapp     ● Idle  Sonnet 4.6  90k/200k 5m ago      │
│  5  scratch          recon::main            ~/repos/recon      ● Idle  Opus 4.6    3k/1M    10m ago     │
│  6  new-session      dotfiles::main         ~/repos/dotfiles   ● New   —           —        —           │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
j/k navigate  Enter switch  / search  v view  q quit
```

- **Input** rows are highlighted — these sessions are blocked waiting for your approval
- **Working** sessions are actively streaming or running tools
- **Idle** sessions are done and waiting for your next prompt
- **New** sessions haven't had any interaction yet

## How it works

recon is built around **tmux**. Each Claude Code instance runs in its own tmux session.

```
┌─────────────────────────────────────────────────────────┐
│                      tmux server                        │
│                                                         │
│  ┌───────────────┐  ┌───────────────┐  ┌──────────────┐ │
│  │ session:      │  │ session:      │  │ session:     │ │
│  │ api-refactor  │  │ debug-pipe    │  │ scratch      │ │
│  │               │  │               │  │              │ │
│  │  ┌──────────┐ │  │  ┌──────────┐ │  │  ┌────────┐  │ │
│  │  │  claude  │ │  │  │  claude  │ │  │  │ claude │  │ │
│  │  └──────────┘ │  │  └──────────┘ │  │  └────────┘  │ │
│  └───────┬───────┘  └───────┬───────┘  └───────┬──────┘ │
│          │                  │                  │        │
└──────────┼──────────────────┼──────────────────┼────────┘
           │                  │                  │
           ▼                  ▼                  ▼
     ┌──────────────────────────────────────────────┐
     │                 recon (TUI)                   │
     │                                               │
     │  reads:                                       │
     │   • tmux list-panes → PID, session name       │
     │   • ~/.claude/sessions/{PID}.json             │
     │   • ~/.claude/projects/…/*.jsonl              │
     │   • tmux capture-pane → status bar text       │
     └──────────────────────────────────────────────┘
```

**Status detection** inspects the Claude Code TUI status bar at the bottom of each tmux pane:

| Status bar text | State |
|---|---|
| `esc to interrupt` | **Working** — streaming response or running a tool |
| `Esc to cancel` | **Input** — permission prompt, waiting for you |
| anything else | **Idle** — waiting for your next prompt |
| *(0 tokens)* | **New** — no interaction yet |

**Session matching** uses `~/.claude/sessions/{PID}.json` files that Claude Code writes, linking each process to its session ID. No `ps` parsing or CWD-based heuristics.

## Install

```bash
cargo install --path .
```

Requires tmux and [Claude Code](https://claude.ai/claude-code).

## Usage

```bash
recon flow                                   # Focus-zone cockpit (the headline mode)
recon flow status                            # Diagnose recon-flow
recon flow stop                              # Graceful shutdown (preserves Claude work)
recon                                        # Table dashboard
recon view                                   # Tamagotchi visual dashboard
recon json                                   # JSON output (for scripting)
recon launch                                 # Create a new claude session (background)
recon launch --name foo --cwd ~/repos/myapp  # Custom name and directory
recon launch --command "claude --model sonnet" --attach  # Custom command, attach to session
recon launch --tag env:staging --tag role:reviewer       # Tag a session (key:value metadata)
recon json --tag role:reviewer               # Filter JSON output by tag (must match all)
recon new                                    # Interactive new session form
recon resume                                 # Interactive resume picker
recon resume --id <session-id>               # Resume a specific session
recon resume --id <session-id> --name foo    # Resume with a custom tmux session name
recon next                                   # Jump to the next agent waiting for input
recon park                                   # Save all live sessions to disk
recon unpark                                 # Restore previously parked sessions
```

### Keybindings — Table View

| Key | Action |
|---|---|
| `j` / `k` | Navigate sessions |
| `Enter` | Switch to selected tmux session |
| `/` | Search / filter sessions by name |
| `i` / `Tab` | Jump to next agent waiting for input |
| `x` | Kill selected session |
| `v` | Switch to Tamagotchi view |
| `q` / `Esc` | Quit (Esc clears filter first) |

### Keybindings — Tamagotchi View

| Key | Action |
|---|---|
| `1`-`4` | Zoom into room |
| `/` | Search / filter sessions by name |
| `j` / `k` | Previous / next page |
| `h` / `l` | Select agent (when zoomed) |
| `Enter` | Switch to selected agent (when zoomed) |
| `x` | Kill selected agent (when zoomed) |
| `n` | New session in room (when zoomed) |
| `Esc` | Zoom out (or quit) |
| `v` | Switch to table view |
| `q` | Quit |

## tmux config

The included `tmux.conf` provides keybindings to open recon as a popup overlay:

```bash
# Add to your ~/.tmux.conf
bind g display-popup -E -w 80% -h 60% "recon"        # prefix + g → dashboard
bind n display-popup -E -w 80% -h 60% "recon new"    # prefix + n → new session
bind r display-popup -E -w 80% -h 60% "recon resume" # prefix + r → resume picker
bind i run-shell "recon next"                         # prefix + i → jump to next input agent
bind X confirm-before -p "Kill session #S? (y/n)" kill-session
```

This lets you pop open the dashboard from any tmux session, pick a session with `Enter`, and jump straight to it.

## Known Limitations

- **`/clear` resets session tracking** — Claude Code's `/clear` command creates a new JSONL file without updating the session-to-process mapping. After `/clear`, recon may show stale data (old tokens, old timestamps) until the session is restarted. Workaround: kill the session in recon and create a new one.

## Contribution Policy

This project is not accepting code contributions (Pull Requests) at this time.

Due to the sensitive nature of reconnaissance and session tracking, I prefer to maintain full control over the codebase to ensure security and auditability.

Ideas and feedback are welcome! Please open an [Issue](https://github.com/gavraz/recon/issues) if you have a feature request or have found a bug. If I like an idea, I will implement it myself.

## License

MIT
