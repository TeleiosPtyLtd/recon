//! Focus-zone orchestrator. Creates tmux session 'recon-flow' with a dashboard
//! pane at the bottom and a 2x2 focus grid above. The grid is sticky: a Claude
//! pane stays in its slot while Working — it's only displaced when another
//! session needs attention and the grid is full. If a slot ever empties
//! (Claude died, etc.), a placeholder is spawned to preserve the quadrant
//! shape.
//!
//! Policy per tick:
//! - Ensure the zone has `slots` panes (placeholders fill any vacancy).
//! - Promote out-of-zone Idle/Input sessions into placeholder slots.
//! - If no placeholder is free, demote a non-active Working session and
//!   promote the new attention-needing one into the freed slot.

use std::collections::HashMap;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::session::{self, Session, SessionStatus};

pub const DEFAULT_MASTER: &str = "recon-flow";
const DASHBOARD_HEIGHT: u32 = 9;
const DASHBOARD_TITLE: &str = "recon-dashboard";
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const DEMOTE_GRACE: Duration = Duration::from_secs(30);

/// Shell command that runs the dashboard in a respawn loop. If recon exits
/// (q, normal exit, crash), the loop restarts it after a short pause.
fn dashboard_respawn_cmd(recon_path: &str) -> String {
    format!("while true; do '{recon_path}'; sleep 0.4; done")
}

/// Top-level `recon flow` entry point.
/// Ensures the master session exists (with dashboard + orchestrator pane),
/// then drops the user into it.
pub fn run(slots: u32) {
    setup_and_attach(DEFAULT_MASTER, slots, &[]);
}

/// Setup with extra helper windows (used by flow-test for the cycler).
pub fn setup_and_attach(master: &str, slots: u32, extra_windows: &[(&str, Vec<String>)]) {
    if !session_exists(master) {
        if !create_master(master, slots, extra_windows) {
            eprintln!("Failed to create '{master}' tmux session.");
            std::process::exit(1);
        }
    } else {
        // Session exists; heal partial state if needed.
        heal_master(master, slots);
    }
    apply_tmux_config(master);
    attach_to(master);
}

/// Idempotent: per-session UI (always-visible pane index labels, longer
/// display-panes pop-up) and server-global Alt+0..9 → jump-to-pane bindings
/// scoped to this session via if-shell so other tmux sessions are untouched.
fn apply_tmux_config(master: &str) {
    let opts: &[(&str, &str)] = &[
        ("pane-border-status", "top"),
        ("pane-border-format", " [#{pane_index}] #{pane_title} "),
        ("display-panes-time", "4000"),
    ];
    for (k, v) in opts {
        let _ = Command::new("tmux")
            .args(["set-option", "-t", master, k, v])
            .output();
    }

    let cond = format!("#{{==:#{{session_name}},{master}}}");
    for n in 0..=9 {
        let key = format!("M-{n}");
        let action = format!("select-pane -t :0.{n}");
        let _ = Command::new("tmux")
            .args(["bind-key", "-n", &key, "if-shell", "-F", &cond, &action])
            .output();
    }
}

/// Diagnose what's running for recon-flow. Read-only.
pub fn status() {
    let master = DEFAULT_MASTER;
    if !session_exists(master) {
        println!("recon-flow: not running.");
        println!();
        println!("Start with: recon flow");
        return;
    }

    println!("recon-flow: running.");
    println!();
    println!("Windows:");
    let windows_out = Command::new("tmux")
        .args(["list-windows", "-t", master, "-F", "  #I  #W  (#{window_panes} panes)"])
        .output()
        .ok();
    if let Some(out) = windows_out {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    println!();
    println!("Claude panes in flow window (window 0):");
    let panes_out = Command::new("tmux")
        .args([
            "list-panes", "-t", &format!("{master}:0"),
            "-F", "  #{pane_index}  cmd=#{pane_current_command}  pid=#{pane_pid}",
        ])
        .output()
        .ok();
    if let Some(out) = panes_out {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim().is_empty() {
            println!("  (window 0 not present — partial state, run `recon flow` to heal)");
        } else {
            print!("{}", s);
        }
    }
    println!();

    let orch_running = find_orchestrator_pid(master).is_some();
    println!("Orchestrator: {}", if orch_running { "running" } else { "NOT RUNNING (will be respawned on next `recon flow`)" });

    println!();
    println!("Attach:   tmux attach -t {master}");
    println!("Stop:     recon flow stop          (preserves Claude panes in new sessions)");
    println!("Stop:     recon flow stop --force  (kills everything in recon-flow)");
}

/// Graceful shutdown. Move every Claude-ish pane in recon-flow out to its own
/// new tmux session, then kill recon-flow. Claude processes survive.
pub fn stop(force: bool) {
    let master = DEFAULT_MASTER;
    if !session_exists(master) {
        println!("recon-flow is not running. Nothing to stop.");
        return;
    }

    if force {
        let _ = Command::new("tmux").args(["kill-session", "-t", master]).output();
        println!("recon-flow killed (--force). All panes terminated.");
        return;
    }

    // Find all Claude-ish panes inside recon-flow (any window). Rescue each.
    let panes_out = Command::new("tmux")
        .args([
            "list-panes", "-s", "-t", master,
            "-F", "#{session_name}:#{window_index}.#{pane_index} #{pane_current_command} #{pane_pid}",
        ])
        .output();

    let mut rescued = 0;
    if let Ok(out) = panes_out {
        let s = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<String> = s.lines().map(String::from).collect();
        for line in &lines {
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() < 3 { continue; }
            let target = parts[0];
            let cmd = parts[1];
            let pid = parts[2];
            let is_claude = cmd == "claude" || cmd == "claude.exe" || cmd == "node"
                || cmd.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
                || cmd == "bash" || cmd == "sh" || cmd == "zsh";
            if !is_claude { continue; }
            // Don't rescue the orch/cycle helper panes — they're recon, not bash.
            if cmd == "recon" { continue; }

            let new_name = format!("rescued-{pid}");
            if move_pane_to_new_session(target, &new_name) {
                rescued += 1;
                println!("  rescued {target} → session '{new_name}'");
            } else {
                eprintln!("  failed to rescue {target}");
            }
        }
    }

    // Kill the master after all rescues are done.
    let killed = Command::new("tmux")
        .args(["kill-session", "-t", master])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if killed {
        println!();
        println!("recon-flow stopped. {rescued} Claude pane(s) preserved as new tmux sessions.");
        if rescued > 0 {
            println!("Find them with: tmux list-sessions | grep rescued-");
        }
    } else {
        eprintln!("Failed to kill recon-flow.");
    }
}

/// Move a pane to a brand-new tmux session, preserving the pane's process.
/// Strategy: create empty target session (with placeholder shell), join source
/// pane into it, kill the placeholder.
fn move_pane_to_new_session(src_pane: &str, new_name: &str) -> bool {
    // Pick a unique session name if `new_name` is taken.
    let mut name = new_name.to_string();
    let mut suffix = 2;
    while session_exists(&name) {
        name = format!("{new_name}-{suffix}");
        suffix += 1;
    }

    // 1. Create empty target.
    let created = Command::new("tmux")
        .args(["new-session", "-d", "-s", &name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !created { return false; }

    // 2. Join the source pane into the target's window 0 (creates pane 1).
    let joined = Command::new("tmux")
        .args(["join-pane", "-d", "-s", src_pane, "-t", &format!("{name}:0")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !joined {
        let _ = Command::new("tmux").args(["kill-session", "-t", &name]).output();
        return false;
    }

    // 3. Kill the original placeholder shell (pane 0). Our rescued pane becomes pane 0.
    let _ = Command::new("tmux")
        .args(["kill-pane", "-t", &format!("{name}:0.0")])
        .status();

    true
}

/// Repair partial state: ensure orchestrator window exists with a live
/// process, and the dashboard pane is present in window 0.
fn heal_master(master: &str, slots: u32) {
    let recon_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "recon".to_string());

    // ── orchestrator window ───────────────────────────────────────────────
    let windows = Command::new("tmux")
        .args(["list-windows", "-t", master, "-F", "#{window_name}"])
        .output()
        .ok();
    let has_orch = windows
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l == "orch"))
        .unwrap_or(false);

    if !has_orch {
        let _ = Command::new("tmux")
            .args([
                "new-window", "-d", "-t", master,
                "-n", "orch",
                &recon_path, "flow-orchestrator",
                "--master", master,
                "--slots", &slots.to_string(),
            ])
            .status();
        eprintln!("[heal] respawned orchestrator window in '{master}'");
    } else if find_orchestrator_pid(master).is_none() {
        let _ = Command::new("tmux")
            .args([
                "respawn-pane", "-k", "-t", &format!("{master}:orch"),
                &recon_path, "flow-orchestrator",
                "--master", master,
                "--slots", &slots.to_string(),
            ])
            .status();
        eprintln!("[heal] respawned dead orchestrator in '{master}:orch'");
    }

    // ── dashboard pane ────────────────────────────────────────────────────
    if find_dashboard_pane(master).is_none() {
        // Add a dashboard pane to the BOTTOM of window 0.
        let cmd = dashboard_respawn_cmd(&recon_path);
        let split = Command::new("tmux")
            .args([
                "split-window", "-d", "-v",
                "-t", &format!("{master}:0"),
                "-l", &DASHBOARD_HEIGHT.to_string(),
                "sh", "-c", &cmd,
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if split {
            // The new pane is the last one in window 0. Find its index and tag it.
            if let Some(idx) = newest_pane_in_window(master, "0") {
                set_pane_title(&format!("{master}:0.{idx}"), DASHBOARD_TITLE);
                eprintln!("[heal] re-added dashboard pane at {master}:0.{idx}");
            }
        }
    }
}

fn newest_pane_in_window(master: &str, window: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args([
            "list-panes", "-t", &format!("{master}:{window}"),
            "-F", "#{pane_index}",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .last()
        .map(|s| s.to_string())
}

fn find_orchestrator_pid(master: &str) -> Option<i32> {
    let out = Command::new("tmux")
        .args([
            "list-panes", "-t", &format!("{master}:orch"),
            "-F", "#{pane_pid} #{pane_current_command}",
        ])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        let mut parts = line.split_whitespace();
        let pid = parts.next()?.parse::<i32>().ok()?;
        let cmd = parts.next().unwrap_or("");
        if cmd == "recon" {
            return Some(pid);
        }
    }
    None
}

/// Hidden subcommand entry: actually runs the orchestrator loop. Invoked as a
/// tmux pane inside the master session.
pub fn run_orchestrator(master: &str, slots: u32) {
    println!("orchestrator: master={master} slots={slots} (sticky grid, demote-on-pressure)");
    println!("(this is the orchestrator pane — switch back to window 0 for the focus zone)");
    println!();

    let mut orch = Orchestrator::new(slots, master.to_string());
    let mut prev_sessions: HashMap<String, Session> = HashMap::new();

    loop {
        if !session_exists(master) {
            // Master session gone; our own pane is presumably about to die too.
            return;
        }

        let sessions: Vec<Session> = session::discover_sessions(&prev_sessions)
            .into_iter()
            .filter(|s| s.tmux_session.is_some())
            .collect();

        orch.tick(&sessions);

        prev_sessions = sessions
            .iter()
            .map(|s| (s.session_id.clone(), s.clone()))
            .collect();

        thread::sleep(POLL_INTERVAL);
    }
}

fn create_master(master: &str, slots: u32, extra_windows: &[(&str, Vec<String>)]) -> bool {
    let recon_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "recon".to_string());

    // Window 0: "flow" — the user-facing window. Initial pane runs the
    // dashboard in a respawn loop so quitting recon (q/Esc) doesn't leave a
    // dead pane. The pane is tagged with a unique title so we can find it
    // again regardless of which process is foregrounded inside the loop.
    let dashboard_cmd = dashboard_respawn_cmd(&recon_path);
    let ok = Command::new("tmux")
        .args([
            "new-session", "-d", "-s", master,
            "-n", "flow",
            "sh", "-c", &dashboard_cmd,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok { return false; }

    // Tag the initial pane as the dashboard.
    set_pane_title(&format!("{master}:0.0"), DASHBOARD_TITLE);

    // Apply tmux config now so `pane-border-status top` is in effect during
    // the splits below — otherwise tmux's later reflow on title-bar enable
    // gives uneven cell heights. Idempotent: setup_and_attach calls it again.
    apply_tmux_config(master);

    // Build the persistent 2x2 placeholder grid above the dashboard. The
    // grid never changes shape — claudes are slid in and out via swap-pane,
    // which preserves layout exactly.
    build_focus_grid(master, &format!("{master}:0.0"));

    // Window 1: "orch" — orchestrator loop. Hidden by default (user lands on window 0).
    let _ = Command::new("tmux")
        .args([
            "new-window", "-d", "-t", master,
            "-n", "orch",
            &recon_path, "flow-orchestrator",
            "--master", master,
            "--slots", &slots.to_string(),
        ])
        .status();

    // Any extra helper windows (e.g. flow-test cycler).
    for (name, argv) in extra_windows {
        let mut args = vec![
            "new-window".to_string(), "-d".to_string(), "-t".to_string(), master.to_string(),
            "-n".to_string(), name.to_string(),
        ];
        args.extend(argv.iter().cloned());
        let _ = Command::new("tmux").args(&args).status();
    }

    // Make sure window 0 is selected so attach lands the user on the focus zone.
    let _ = Command::new("tmux")
        .args(["select-window", "-t", &format!("{}:0", master)])
        .status();

    true
}

fn attach_to(master: &str) -> ! {
    if std::env::var("TMUX").is_ok() {
        let _ = Command::new("tmux")
            .args(["switch-client", "-t", master])
            .status();
        std::process::exit(0);
    } else {
        use std::os::unix::process::CommandExt;
        let err = Command::new("tmux")
            .args(["attach-session", "-t", master])
            .exec();
        eprintln!("Failed to attach to '{master}': {err}");
        std::process::exit(1);
    }
}

// ──────────────────────────────────────────────────────────────────────────

struct Orchestrator {
    slots: u32,
    master: String,
    demote_at: HashMap<String, Instant>,
    prev_status: HashMap<String, SessionStatus>,
}

impl Orchestrator {
    fn new(slots: u32, master: String) -> Self {
        Self { slots, master, demote_at: HashMap::new(), prev_status: HashMap::new() }
    }

    fn tick(&mut self, sessions: &[Session]) {
        let active_pane = active_pane_target(&self.master);
        let dashboard_pane = find_dashboard_pane(&self.master);
        let mut layout_dirty = false;

        // Single pass over `sessions` to compute both counters used below.
        let mut occupied: u32 = 0;
        let mut needs_attention_out_of_zone: u32 = 0;
        for s in sessions {
            let Some(t) = s.pane_target.as_deref() else { continue };
            if is_in_focus_zone(&self.master, t) {
                occupied += 1;
            } else if matches!(s.status, SessionStatus::Idle | SessionStatus::Input) {
                needs_attention_out_of_zone += 1;
            }
        }

        // Pressure: how many demotions we actually need this tick. Demotion
        // is only justified when there's an out-of-zone session wanting in
        // and no placeholder slot to receive it. With ≤ slots Claudes total,
        // every Claude stays visible.
        let free_slots = self.slots.saturating_sub(occupied);
        let mut demote_budget = needs_attention_out_of_zone.saturating_sub(free_slots);

        // Demotions and timer management first — they free up slots, but only
        // up to `demote_budget`. We still respect DEMOTE_GRACE so a Claude
        // that just started Working isn't yanked immediately.
        for s in sessions {
            let Some(target) = s.pane_target.as_deref() else { continue };
            let in_zone = is_in_focus_zone(&self.master, target);

            if !in_zone || s.status != SessionStatus::Working {
                self.demote_at.remove(&s.session_id);
                continue;
            }

            let prev = self.prev_status.get(&s.session_id).cloned();
            if !matches!(prev, Some(SessionStatus::Working)) {
                self.demote_at.insert(s.session_id.clone(), Instant::now() + DEMOTE_GRACE);
            }

            if demote_budget == 0 { continue; }
            let Some(at) = self.demote_at.get(&s.session_id).copied() else { continue };
            // Never demote the last Claude pane out of the focus zone —
            // keep at least one visible so the user always sees work
            // happening above the (thin) dashboard bar.
            if Instant::now() < at || active_pane.as_deref() == Some(target) || occupied <= 1 {
                continue;
            }

            let holding_name = format!("bg-{}", short_id(&s.session_id));
            let Some(holding_pane) = create_holding_window(&self.master, &holding_name) else { continue };
            if !swap_panes(target, &holding_pane) { continue }

            self.demote_at.remove(&s.session_id);
            occupied = occupied.saturating_sub(1);
            demote_budget = demote_budget.saturating_sub(1);
            layout_dirty = true;
            println!(
                "[demote] {} ({}) → {}",
                short_id(&s.session_id),
                s.tmux_session.as_deref().unwrap_or("?"),
                holding_name,
            );
        }

        // Promotions. Slide eligible Claudes into placeholder slots via
        // swap-pane — preserves the 2x2 grid exactly. Layout doesn't change;
        // only contents do. Slots are pre-collected once: pane indices are
        // stable across swap-pane, so a single list-panes serves the whole
        // loop instead of N.
        let mut free_slots: Vec<String> = find_panes_by_title(&self.master, PLACEHOLDER_TITLE);
        for s in sessions {
            let Some(target) = s.pane_target.as_deref() else { continue };
            if is_in_focus_zone(&self.master, target) { continue }
            if !matches!(s.status, SessionStatus::Idle | SessionStatus::Input) { continue }
            if occupied >= self.slots { break }

            // No empty slot. Probably means slots == filled, or grid is
            // missing — heal_master will catch the latter on next call.
            let Some(slot) = free_slots.pop() else { break };

            if swap_panes(target, &slot) {
                // The claude is now in `slot`. The source location now holds
                // the placeholder that came out of `slot`. We deliberately
                // do NOT kill that pane — observation showed that kill-pane
                // chains under the 200ms tick correlated with window 0's
                // grid losing cells. Orphan placeholder windows are tidied
                // up by `recon flow stop`.
                occupied += 1;
                layout_dirty = true;
                println!(
                    "[promote] {} ({}) → {}",
                    short_id(&s.session_id),
                    s.tmux_session.as_deref().unwrap_or("?"),
                    slot,
                );
            }
        }

        if layout_dirty {
            relayout(&self.master, &dashboard_pane);
        } else {
            // Pin the dashboard every tick even when no promote/demote ran.
            // External events (pane killed, claude exited, manual layout
            // changes) can leave the dashboard oversized; this snaps it back.
            pin_dashboard_height(&self.master, &dashboard_pane);
        }

        // Per-tick grid enforcement. Without this the 2x2 drifts over time:
        // claudes that die inside a slot leave their parent shell (zsh/bash)
        // running, and terminal resizes or swap chains can leave a column's
        // top and bottom cells with different heights. Cheap to re-run.
        heal_focus_grid(&self.master);

        // Update prev_status. Drop entries for sessions that have disappeared.
        let live: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.prev_status.retain(|k, _| live.contains(k.as_str()));
        for s in sessions {
            self.prev_status.insert(s.session_id.clone(), s.status.clone());
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
//  tmux operations
// ──────────────────────────────────────────────────────────────────────────

fn session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_in_focus_zone(master: &str, pane_target: &str) -> bool {
    let prefix = format!("{}:0.", master);
    pane_target.starts_with(&prefix)
}

fn active_pane_target(master: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args([
            "display-message", "-t", &format!("{}:0", master),
            "-p", "#{session_name}:#{window_index}.#{pane_index}",
        ])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn find_dashboard_pane(master: &str) -> Option<String> {
    find_panes_by_title(master, DASHBOARD_TITLE).into_iter().next()
}

/// List every pane in master:0 whose pane_title matches. Used to locate the
/// dashboard (exactly one match) and placeholder slots (zero or more).
fn find_panes_by_title(master: &str, title: &str) -> Vec<String> {
    let out = Command::new("tmux")
        .args([
            "list-panes", "-t", &format!("{}:0", master),
            "-F", "#{pane_index}\t#{pane_title}",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() { return Vec::new(); }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let idx = parts.next()?;
            let t = parts.next().unwrap_or("");
            (t == title).then(|| format!("{}:0.{}", master, idx))
        })
        .collect()
}

/// Tag a tmux pane with a title (visible in the pane-border-status line and
/// queryable via `#{pane_title}`). Titles travel with the pane across
/// swap-pane, so they're a reliable way to mark recon-owned panes.
fn set_pane_title(target: &str, title: &str) {
    let _ = Command::new("tmux")
        .args(["select-pane", "-t", target, "-T", title])
        .output();
}

/// Placeholder slot command: an interactive shell with a one-line banner so
/// the user can `cd` somewhere and run `claude` in-pane. Identified by pane
/// title (PLACEHOLDER_TITLE), not by command — the running shell looks like
/// any other zsh/bash.
const PLACEHOLDER_CMD: &str =
    "clear; printf '\\033[2m(empty slot - cd then claude)\\033[0m\\n\\n'; exec ${SHELL:-/bin/sh}";

/// Pane title applied to every placeholder pane. Travels with the pane across
/// swap-pane (titles are pane-identity-bound, not position-bound). Used to
/// distinguish "this is one of our empty slots" from "this is a user-spawned
/// shell or a leftover after claude exited" — the latter we leave alone.
const PLACEHOLDER_TITLE: &str = "recon-slot";

/// Build the persistent 2x2 placeholder grid above the dashboard. Called once
/// at master creation. Claudes are slid in and out of these slots via
/// `swap-pane`, which preserves the grid layout — so we never call
/// `select-layout` and never rebuild this geometry.
///
/// Layout produced (over a `DASHBOARD_HEIGHT`-pinned dashboard at the bottom):
///
///   ┌──────────┬──────────┐
///   │   p1     │   p2     │
///   ├──────────┼──────────┤
///   │   p3     │   p4     │
///   ├──────────┴──────────┤
///   │      dashboard       │
///   └──────────────────────┘
fn build_focus_grid(_master: &str, dashboard_target: &str) {
    // p1: above dashboard, full width of top area.
    let Some(p1) = split_placeholder(&["-b", "-v", "-t", dashboard_target]) else {
        eprintln!("[grid] failed to spawn p1");
        return;
    };
    // Pin dashboard height now that there's another pane to take up the slack.
    let _ = Command::new("tmux")
        .args(["resize-pane", "-t", dashboard_target, "-y", &DASHBOARD_HEIGHT.to_string()])
        .output();

    // p2: to the right of p1 (top row gets two columns) — explicit 50% so
    // the column widths are balanced regardless of tmux defaults.
    let Some(p2) = split_placeholder(&["-h", "-l", "50%", "-t", &p1]) else {
        eprintln!("[grid] failed to spawn p2");
        return;
    };
    // p3 / p4: derive an explicit line count from p1's current height so the
    // row split is symmetric. `-l 50%` (and `-p 50`) left the bottom row a
    // line or two shorter than the top once the dashboard + border were
    // accounted for.
    let row_h = pane_height(&p1).map(|h| (h.saturating_sub(1) / 2).max(1));
    let p3 = match row_h {
        Some(h) => split_placeholder(&["-v", "-l", &h.to_string(), "-t", &p1]),
        None => split_placeholder(&["-v", "-l", "50%", "-t", &p1]),
    };
    let p4 = match row_h {
        Some(h) => split_placeholder(&["-v", "-l", &h.to_string(), "-t", &p2]),
        None => split_placeholder(&["-v", "-l", "50%", "-t", &p2]),
    };

    // Belt-and-suspenders: if either column still ended up uneven, equalise.
    if let Some(p3) = p3.as_deref() {
        equalize_column(&p1, p3);
    }
    if let Some(p4) = p4.as_deref() {
        equalize_column(&p2, p4);
    }
}

fn pane_height(pane_id: &str) -> Option<u32> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "-t", pane_id, "#{pane_height}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Resize so the top and bottom panes in a column differ by at most 1 line.
fn equalize_column(top: &str, bottom: &str) {
    let (Some(top_h), Some(bot_h)) = (pane_height(top), pane_height(bottom)) else { return };
    let diff = top_h.max(bot_h) - top_h.min(bot_h);
    if diff <= 1 {
        return;
    }
    let target = (top_h + bot_h) / 2;
    let _ = Command::new("tmux")
        .args(["resize-pane", "-t", bottom, "-y", &target.to_string()])
        .output();
}

/// Re-equalise column heights every tick. Previously this also auto-respawned
/// any non-dashboard / non-claude / non-`tail` pane as a placeholder — which
/// quietly clobbered user-spawned shells whenever they manually split a slot.
/// Now placeholders are identified by `pane_title == PLACEHOLDER_TITLE`, and
/// anything else (user shells, leftover shells after claude exited) is left
/// alone. The user can re-`exec claude` in a leftover themselves.
fn heal_focus_grid(master: &str) {
    let out = match Command::new("tmux")
        .args([
            "list-panes", "-t", &format!("{}:0", master),
            "-F", "#{pane_id}\t#{pane_title}\t#{pane_left}\t#{pane_top}",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };

    struct P {
        id: String,
        title: String,
        left: u32,
        top: u32,
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let panes: Vec<P> = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 4 { return None; }
            Some(P {
                id: parts[0].to_string(),
                title: parts[1].to_string(),
                left: parts[2].parse().ok()?,
                top: parts[3].parse().ok()?,
            })
        })
        .collect();

    // Re-equalise column heights. Group non-dashboard panes by pane_left;
    // within each column with exactly two cells, level them.
    let mut by_col: HashMap<u32, Vec<&P>> = HashMap::new();
    for p in &panes {
        if p.title == DASHBOARD_TITLE { continue; }
        by_col.entry(p.left).or_default().push(p);
    }
    for col in by_col.values_mut() {
        if col.len() != 2 { continue; }
        col.sort_by_key(|p| p.top);
        equalize_column(&col[0].id, &col[1].id);
    }
}

/// Run `split-window` with placeholder boilerplate and return the new pane's
/// pane_id (e.g. `%23`). `-P -F #{pane_id}` makes tmux print the new pane's
/// id on stdout, which is more reliable than scanning list-panes after.
fn split_placeholder(args_before_cmd: &[&str]) -> Option<String> {
    let mut args: Vec<&str> = vec!["split-window", "-d"];
    args.extend_from_slice(args_before_cmd);
    args.extend_from_slice(&["-P", "-F", "#{pane_id}", "sh", "-c", PLACEHOLDER_CMD]);

    let out = Command::new("tmux").args(&args).output().ok()?;
    if !out.status.success() { return None; }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() { return None; }
    set_pane_title(&id, PLACEHOLDER_TITLE);
    Some(id)
}

/// Create a new background window in `master` with a single placeholder pane.
/// Returns the new pane's id. Used during demote: swap the active claude into
/// this pane, leaving the original slot occupied by the placeholder that came
/// out of the swap.
fn create_holding_window(master: &str, name: &str) -> Option<String> {
    // If a window with this name already exists (e.g. same claude was
    // demoted before), drop the stale one — it only contains a placeholder
    // and re-using it would muddle ownership.
    let _ = Command::new("tmux")
        .args(["kill-window", "-t", &format!("{master}:{name}")])
        .output();

    let out = Command::new("tmux")
        .args([
            "new-window", "-d", "-t", master,
            "-n", name,
            "-P", "-F", "#{pane_id}",
            "sh", "-c", PLACEHOLDER_CMD,
        ])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() { return None; }
    set_pane_title(&id, PLACEHOLDER_TITLE);
    Some(id)
}

/// Swap the contents of two panes. Layout positions stay fixed — only the
/// processes/contents exchange places. `-d` keeps the user's active pane
/// selection where it was (no focus jump).
///
/// After the swap we clear pane-scope `window-style` / `window-active-style`
/// at both positions. tmux's per-pane options appear to be position-bound,
/// not pane-identity-bound: the style stays attached to "recon-flow:0.N"
/// even though the pane sitting at index N is now a different process. Left
/// alone, that bleeds the demoted claude's status colour onto whatever
/// placeholder swaps into the slot. Paint re-applies for live claudes on
/// the next dashboard tick (≤200ms), so the gap is invisible in practice.
fn swap_panes(src: &str, dst: &str) -> bool {
    let ok = Command::new("tmux")
        .args(["swap-pane", "-d", "-s", src, "-t", dst])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok { return false; }
    for target in [src, dst] {
        let _ = Command::new("tmux")
            .args(["set-option", "-p", "-u", "-t", target, "window-style"])
            .output();
        let _ = Command::new("tmux")
            .args(["set-option", "-p", "-u", "-t", target, "window-active-style"])
            .output();
    }
    true
}

/// Kill a pane; if it's the only pane in its window/session, that goes too.
/// Not currently wired up (see promote loop for context) — kept as
/// vocabulary for if/when we re-introduce post-swap cleanup.
#[allow(dead_code)]
fn kill_pane(pane_target: &str) {
    let _ = Command::new("tmux")
        .args(["kill-pane", "-t", pane_target])
        .output();
}

/// After a promote/demote, pin the dashboard to its fixed height. We do NOT
/// call `select-layout tiled` — tiled produces a square-ish grid that puts a
/// claude pane next to the dashboard at half width. The horizontal join
/// strategy in the promote loop already keeps claudes side-by-side above the
/// dashboard, so the natural layout is correct.
fn relayout(_master: &str, dashboard: &Option<String>) {
    if let Some(dash) = dashboard {
        let _ = Command::new("tmux")
            .args(["resize-pane", "-t", dash, "-y", &DASHBOARD_HEIGHT.to_string()])
            .output();
    }
}

/// Snap the dashboard pane back to its configured height. No-op when the
/// dashboard is the only pane in window 0 (tmux can't shrink a sole pane).
fn pin_dashboard_height(master: &str, dashboard: &Option<String>) {
    let Some(dash) = dashboard else { return };
    let out = Command::new("tmux")
        .args(["list-panes", "-t", &format!("{}:0", master), "-F", "#{pane_index}\t#{pane_height}"])
        .output()
        .ok();
    let Some(out) = out else { return };
    if !out.status.success() { return; }
    let s = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 { return; }

    // Find dashboard's current height; only resize if it's wrong.
    let dash_idx_suffix = dash.rsplit('.').next().unwrap_or("");
    for line in &lines {
        let mut parts = line.splitn(2, '\t');
        let idx = parts.next().unwrap_or("");
        let h: u32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
        if idx == dash_idx_suffix {
            if h != DASHBOARD_HEIGHT {
                let _ = Command::new("tmux")
                    .args(["resize-pane", "-t", dash, "-y", &DASHBOARD_HEIGHT.to_string()])
                    .output();
            }
            return;
        }
    }
}

fn short_id(session_id: &str) -> &str {
    let end = 8.min(session_id.len());
    &session_id[..end]
}
