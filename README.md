# recon

A tmux-native cockpit for running many [Claude Code](https://claude.ai/claude-code) agents in parallel.

**flow** — one tmux session, a 2×2 grid of live agent panes on top, a dashboard underneath. Agents that need your attention are auto-promoted into the grid; Working agents quietly demote to background windows once they're heads-down. You stay in one window and the cockpit reshapes itself around what needs you.

![recon demo](assets/demo.gif)

## The cockpit

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

- **Sticky 2×2 focus zone.** Up to 3 Claude panes share the top zone alongside a shell; an agent stays put while Working — only displaced when something else needs attention and the grid is full.
- **Auto-promote / auto-demote.** Idle or Input agents living in background `bg-<id>` windows are promoted into a free slot. Working agents that have been heads-down for 30s+ are demoted out to make room.
- **Dashboard pinned at the bottom**, 9 rows tall, running in a respawn loop. Quit it with `q` and it restarts; the orchestrator never loses sight of the grid.
- **Dark theme + neon-green active border** so the focused slot is unmistakable across a wide monitor.
- **⚠ dangerous marker** in the pane title for agents launched with `--dangerously-skip-permissions`.
- **Graceful shutdown.** `recon flow stop` break-panes every Claude out to its own `bg-<id>` tmux session so your work survives — pass `--force` to kill them.

## How it works

Each tick (200ms), the orchestrator reads tmux pane state and recon's session table, then applies one rule:

1. Make sure the 2×2 zone has the configured number of slots — fill any vacancy with a placeholder pane.
2. Promote out-of-zone Idle/Input sessions into placeholder slots via tmux `swap-pane` (preserves layout exactly).
3. If no placeholder is free and an attention-needing session is waiting, demote a non-active Working session (30s+ heads-down) into a `bg-<id>` holding window, then promote into the freed slot.

The grid's pane positions never change — claudes slide in and out underneath. The orchestrator itself lives in a hidden `orch` window; the dashboard pane and grid live in window 0.

## Setup

Requires **tmux** and **[Claude Code](https://claude.ai/claude-code)**.

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

## Driving it

**Spatial pane keys** (active only inside `recon-flow`, so they don't intercept typing elsewhere):

```
[Alt+4 top-left ][Alt+5 top-right]
[Alt+1 btm-left ][Alt+2 btm-right]
[        Alt+0 dashboard         ]
```

The bindings use tmux directional pane tokens (`{top-left}`, `{bottom}`), so they survive any future layout reshuffle.

**Spawning a new agent inside flow:** focus the shell pane (Alt+anything that lands on it), then run `claude` directly. It immediately joins the rotation — the orchestrator picks it up on the next tick and slots it into the grid when it needs attention.

**The dashboard pane** is a live recon table — navigate with `j`/`k`, search with `/`, switch into a session with `Enter`, kill with `x`. When you switch into an agent, the spatial Alt+digit keys still bring you straight back to the dashboard.

## Known Limitations

- **`/clear` resets session tracking** — Claude Code's `/clear` command creates a new JSONL file without updating the session-to-process mapping. After `/clear`, recon may show stale data (old tokens, old timestamps) until the session is restarted. Workaround: kill the session in flow and start a new one.

## Contribution Policy

This project is not accepting code contributions (Pull Requests) at this time.

Due to the sensitive nature of reconnaissance and session tracking, I prefer to maintain full control over the codebase to ensure security and auditability.

Ideas and feedback are welcome! Please open an [Issue](https://github.com/gavraz/recon/issues) if you have a feature request or have found a bug. If I like an idea, I will implement it myself.

## License

MIT
