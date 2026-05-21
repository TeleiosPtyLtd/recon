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

const ROLE_OPT: &str = "@recon-role";
const ROLE_DASHBOARD: &str = "dashboard";
const ROLE_SHELL: &str = "shell";

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
    // Session-scoped options.
    let session_opts: &[(&str, &str)] = &[
        ("display-panes-time", "4000"),
    ];
    for (k, v) in session_opts {
        let _ = Command::new("tmux")
            .args(["set-option", "-t", master, k, v])
            .output();
    }

    // Window-scoped options. These MUST be applied with `-w` against each
    // window in the session — `set-option -t <session>` without `-w` creates
    // a session-tier value that loses to any explicit `-g` window-option
    // default in the user's ~/.tmux.conf (e.g. a `set -g
    // pane-active-border-style ...` line shadows our session "default").
    //
    // We also re-apply whenever new windows are added (bg-* holding windows
    // during demote) by calling `apply_window_styles_to_all` from the orch
    // path — see `create_holding_window`.
    apply_window_styles_to_all(master);

    // Spatial pane-jump keys, mapped to mirror a numpad's bottom-2x2 cluster:
    //
    //   [Alt+4 top-left ][Alt+5 top-right]
    //   [Alt+1 btm-left ][Alt+2 btm-right]
    //   [        Alt+0 dashboard         ]
    //
    // Alt+digit (not bare digit) so the keys don't intercept typing inside
    // claude/shell panes. Scoped via if-shell to recon-flow only.
    //
    // Targets use tmux's built-in directional pane tokens — no hardcoded
    // pane indices, so the bindings survive any future layout reshuffle:
    //   {top-left}/{top-right} resolve to the top corners of the window.
    //   {bottom} resolves to the bottommost pane (the dashboard).
    //   For the grid's bottom row we'd hit the dashboard if we used
    //   {bottom-left}/{bottom-right} (the dashboard spans full width to the
    //   very bottom), so we anchor at the top corner and step `-D` one row
    //   down — that lands on the grid's bottom row, never on the dashboard.
    let cond = format!("#{{==:#{{session_name}},{master}}}");
    let spatial: &[(&str, &str)] = &[
        ("M-0", "select-pane -t {bottom}"),                            // dashboard
        ("M-4", "select-pane -t {top-left}"),                          // grid top-left
        ("M-5", "select-pane -t {top-right}"),                         // grid top-right
        ("M-1", "select-pane -t {top-left} ; select-pane -D"),         // grid bottom-left
        ("M-2", "select-pane -t {top-right} ; select-pane -D"),        // grid bottom-right
    ];
    for (key, action) in spatial {
        let _ = Command::new("tmux")
            .args(["bind-key", "-n", key, "if-shell", "-F", &cond, action])
            .output();
    }
}

/// Window-scoped style options for the master session. Set per window with
/// `-w` so explicit global window-option defaults from the user's
/// `~/.tmux.conf` don't shadow them.
const WINDOW_OPTS: &[(&str, &str)] = &[
    ("pane-border-status", "top"),
    // `@recon_dangerous` is a per-pane user option set by paint::mark_dangerous
    // when claude was launched with --dangerously-skip-permissions. Unset =
    // expands to empty; set to " ⚠ " for unsupervised panes.
    ("pane-border-format", " [#{pane_index}]#{@recon_dangerous} #{pane_title} "),
    // Dark surface as the window default. Per-pane paint::paint_pane
    // overrides win where set; unpainted panes (dashboard, orchestrator)
    // inherit this so the whole session reads as one dark theme.
    ("window-style", "bg=#0F1117,fg=#D0CCC4"),
    ("window-active-style", "bg=#0F1117,fg=#D0CCC4"),
    // Active pane gets a neon-green border so the focused slot is
    // unmistakable against the deep dark surface; inactive panes recede
    // into a dim slate so the contrast carries.
    ("pane-border-style", "fg=#1F2530"),
    ("pane-active-border-style", "fg=#39FF14,bold"),
];

fn apply_window_styles(window_target: &str) {
    for (k, v) in WINDOW_OPTS {
        let _ = Command::new("tmux")
            .args(["set-option", "-w", "-t", window_target, k, v])
            .output();
    }
}

fn apply_window_styles_to_all(master: &str) {
    let out = Command::new("tmux")
        .args(["list-windows", "-t", master, "-F", "#{window_index}"])
        .output();
    let Ok(out) = out else { return };
    if !out.status.success() { return }
    let s = String::from_utf8_lossy(&out.stdout);
    for idx in s.lines().filter(|l| !l.trim().is_empty()) {
        apply_window_styles(&format!("{master}:{idx}"));
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
                || is_shellish_cmd(cmd);
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

/// Self-tag dashboard and shell roles on legacy sessions that predate
/// `@recon-role`. Idempotent: exits early when both roles are present, so
/// safe to call on every heal/orchestrator-start.
fn self_tag_roles(master: &str) {
    let need_shell = panes_with_role(master, "shell").is_empty();
    let need_dashboard = panes_with_role(master, "dashboard").is_empty();
    if !need_shell && !need_dashboard { return }

    let out = Command::new("tmux")
        .args([
            "list-panes", "-t", &format!("{}:0", master),
            "-F", "#{pane_index}\t#{pane_title}\t#{pane_start_command}",
        ])
        .output();
    let Ok(out) = out else { return };
    if !out.status.success() { return }
    let s = String::from_utf8_lossy(&out.stdout);

    let mut tagged_shell = !need_shell;
    let mut tagged_dashboard = !need_dashboard;

    for line in s.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 3 { continue }
        let idx = parts[0];
        let title = parts[1];
        let start_cmd = parts[2];
        let target = format!("{}:0.{}", master, idx);

        // Dashboard: title still matches, or start command contains the
        // unique tail of dashboard_respawn_cmd.
        if !tagged_dashboard
            && (title == DASHBOARD_TITLE || start_cmd.contains("sleep 0.4"))
        {
            set_pane_role(&target, "dashboard");
            eprintln!("[self-tag] dashboard @ {target}");
            tagged_dashboard = true;
            continue;
        }

        // Shell: title still matches, or start command contains the unique
        // banner substring of shell_respawn_cmd.
        if !tagged_shell
            && (title == SHELL_TITLE || start_cmd.contains("spawn an agent"))
        {
            set_pane_role(&target, "shell");
            eprintln!("[self-tag] shell @ {target}");
            tagged_shell = true;
            continue;
        }
    }
}

/// Repair partial state: ensure orchestrator window exists with a live
/// process, and the dashboard pane is present in window 0.
fn heal_master(master: &str, slots: u32) {
    let recon_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "recon".to_string());

    // Tag pre-existing dashboard / shell panes from legacy sessions that
    // were created before `@recon-role` markers existed. Idempotent.
    self_tag_roles(master);

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
                set_pane_role(&format!("{master}:0.{idx}"), "dashboard");
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

    // Heal legacy sessions on upgrade: without this, an in-place binary
    // upgrade leaves the shell pane invisible to the orchestrator and the
    // bottom-right cell would be treated as a free slot.
    self_tag_roles(master);

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

    // Tag the initial pane as the dashboard. The role marker is position-
    // bound and survives even if the dashboard process respawns or gets
    // replaced — title is cosmetic, role is identity.
    set_pane_title(&format!("{master}:0.0"), DASHBOARD_TITLE);
    set_pane_role(&format!("{master}:0.0"), "dashboard");

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

    /// Working claude in the focus zone, not the currently active pane —
    /// the one the orchestrator is allowed to swap out under pressure. Shared
    /// by every demote candidate filter so the rules stay in lockstep.
    fn is_demotable_working_in_zone(&self, s: &Session, active_pane: Option<&str>) -> bool {
        let Some(t) = s.pane_target.as_deref() else { return false };
        is_in_focus_zone(&self.master, t)
            && s.status == SessionStatus::Working
            && active_pane != Some(t)
    }

    /// Move a Claude pane out of the focus zone into a fresh `bg-<id>`
    /// holding window. Shared by the pressure-driven and strict-cap demote
    /// passes; differs only in the log tag.
    fn demote_one(&mut self, s: &Session, target: &str, tag: &str) -> bool {
        // Guard against stale `sessions` data inside a single tick. Multiple
        // demote paths (pressure → strict-cap → shell-spawn) all read the
        // same start-of-tick snapshot. If an earlier path already swapped
        // this Claude into its bg-* window, `target` no longer holds claude
        // — it holds the placeholder that came back via swap-pane. Demoting
        // again would re-enter `create_holding_window` for the same name and
        // kill the still-live Claude inside it.
        let cmd = pane_command(target).unwrap_or_default();
        if !looks_like_claude_cmd(&cmd) {
            eprintln!(
                "[{tag}] skip {} ({}) — {} runs `{}`, not claude (stale session data)",
                short_id(&s.session_id),
                s.tmux_session.as_deref().unwrap_or("?"),
                target,
                cmd,
            );
            return false;
        }
        let holding_name = format!("bg-{}", short_id(&s.session_id));
        let Some(holding_pane) = create_holding_window(&self.master, &holding_name) else { return false };
        if !swap_panes(target, &holding_pane) { return false }
        self.demote_at.remove(&s.session_id);
        println!(
            "[{tag}] {} ({}) → {}",
            short_id(&s.session_id),
            s.tmux_session.as_deref().unwrap_or("?"),
            holding_name,
        );
        true
    }

    fn tick(&mut self, sessions: &[Session]) {
        // Single `list-panes` covers every per-tick query (active pane,
        // dashboard, shell-role, full pane list, per-pane current command) —
        // saves ~4 fork+exec/tick versus the older split queries.
        let snap = WindowZeroSnapshot::capture(&self.master);
        let active_pane = snap.active.clone();
        let dashboard_pane = snap.dashboard.clone();

        // If the user ran `claude` inside the shell pane the role marker is
        // stale; strip it so the orchestrator treats that cell normally and
        // is free to spawn a fresh shell into a different empty slot.
        let mut shell_panes: std::collections::HashSet<String> = std::collections::HashSet::new();
        for t in &snap.shell {
            let cmd = snap.commands.get(t).map(String::as_str).unwrap_or("");
            if is_shellish_cmd(cmd) {
                shell_panes.insert(t.clone());
            } else {
                unset_shell_role(t);
            }
        }
        let mut layout_dirty = false;

        let mut occupied: u32 = 0;
        let mut needs_attention_out_of_zone: u32 = 0;
        for s in sessions {
            let Some(t) = s.pane_target.as_deref() else { continue };
            if is_in_focus_zone(&self.master, t) {
                if shell_panes.contains(t) { continue }
                occupied += 1;
            } else if matches!(s.status, SessionStatus::Idle | SessionStatus::Input) {
                needs_attention_out_of_zone += 1;
            }
        }

        // Pressure-driven demote: only when an out-of-zone session needs in
        // and no placeholder slot can receive it. Respects DEMOTE_GRACE so a
        // freshly-Working claude isn't yanked immediately.
        let free_slot_count = self.slots.saturating_sub(occupied);
        let mut demote_budget = needs_attention_out_of_zone.saturating_sub(free_slot_count);

        for s in sessions {
            let Some(target) = s.pane_target.as_deref() else { continue };
            let in_zone = is_in_focus_zone(&self.master, target);

            if shell_panes.contains(target) {
                self.demote_at.remove(&s.session_id);
                continue;
            }

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
            // Keep at least one visible so the user always sees work above the
            // (thin) dashboard bar.
            if Instant::now() < at || active_pane.as_deref() == Some(target) || occupied <= 1 {
                continue;
            }

            if self.demote_one(s, target, "demote") {
                occupied = occupied.saturating_sub(1);
                demote_budget = demote_budget.saturating_sub(1);
                layout_dirty = true;
            }
        }

        // Strict cap: catch extras that crept in (manual splits, legacy
        // layouts, shell-role stripped because user ran claude in shell).
        // No grace period — these cells were already over budget. Active
        // pane stays; Idle/Input never get demoted.
        if occupied > self.slots {
            let over = (occupied - self.slots) as usize;
            let mut candidates: Vec<&Session> = sessions
                .iter()
                .filter(|s| {
                    self.is_demotable_working_in_zone(s, active_pane.as_deref())
                        && s.pane_target.as_deref().map_or(false, |t| !shell_panes.contains(t))
                })
                .collect();
            candidates.sort_by_key(|s| s.last_activity.clone().unwrap_or_default());
            for s in candidates.into_iter().take(over) {
                let Some(target) = s.pane_target.as_deref() else { continue };
                if self.demote_one(s, target, "demote-cap") {
                    occupied = occupied.saturating_sub(1);
                    layout_dirty = true;
                }
            }
        }

        // claude_panes tracks which focus-zone positions currently hold a
        // claude. Starts from start-of-tick `sessions`; promote/demote within
        // this same tick mutate it so the shell-guarantee block below sees
        // up-to-date occupancy. Without that update, shell-guarantee can
        // `respawn-pane -k` a slot a same-tick promote just landed a claude
        // in — killing the claude.
        let mut claude_panes: std::collections::HashSet<String> = sessions.iter()
            .filter_map(|s| s.pane_target.clone())
            .filter(|t| is_in_focus_zone(&self.master, t))
            .collect();
        let mut free_slots: Vec<String> = snap.targets.iter()
            .filter(|p| {
                !claude_panes.contains(*p)
                    && !shell_panes.contains(*p)
                    && dashboard_pane.as_deref() != Some(p.as_str())
            })
            .cloned()
            .collect();

        // Shell-yield safety: we may evict the shell to admit an attention-
        // needing claude, but only if the shell-guarantee block (below) can
        // restore it this same tick — and that block only demotes non-active
        // Working claudes. If no such claude exists, yielding the shell
        // strands the user with no CLI surface to spawn new agents,
        // deadlocking the workspace once every visible cell is Idle/Input.
        let can_restore_shell = sessions
            .iter()
            .any(|s| self.is_demotable_working_in_zone(s, active_pane.as_deref()));

        for s in sessions {
            let Some(target) = s.pane_target.as_deref() else { continue };
            if is_in_focus_zone(&self.master, target) { continue }
            if !matches!(s.status, SessionStatus::Idle | SessionStatus::Input) { continue }
            if occupied >= self.slots { break }

            let Some(slot) = take_slot_or_yield_shell(
                &mut free_slots,
                &mut shell_panes,
                &active_pane,
                can_restore_shell,
            ) else {
                break;
            };

            if swap_panes(target, &slot) {
                // The placeholder that came out of `slot` now sits at the
                // source location. Don't kill it — kill-pane chains under
                // the 200ms tick correlate with window 0 losing cells.
                // `recon flow stop` tidies orphan windows.
                occupied += 1;
                layout_dirty = true;
                // Keep claude_panes in sync within the tick: `slot` (focus
                // zone) now holds the just-promoted claude. The shell-
                // guarantee block below filters placeholder candidates with
                // `!claude_panes.contains(p)`; without this insert it would
                // treat `slot` as a placeholder and `respawn-pane -k` the
                // claude we just put there.
                claude_panes.insert(slot.clone());
                println!(
                    "[promote] {} ({}) → {}",
                    short_id(&s.session_id),
                    s.tmux_session.as_deref().unwrap_or("?"),
                    slot,
                );
            }
        }

        // Shell guarantee: there should always be exactly one shell pane in
        // the zone. If none exists, prefer a free placeholder cell; if every
        // cell is a claude, demote the oldest non-active Working claude into
        // the queue (bg-* holding window) to make room. No grace period —
        // a missing shell breaks the "always one terminal to spawn agents"
        // contract the focus zone is built around. Runs before relayout so a
        // demote here gets picked up by the same tick's layout pass.
        if shell_panes.is_empty() {
            let mut target: Option<String> = snap.targets.iter().find(|p| {
                !claude_panes.contains(*p)
                    && dashboard_pane.as_deref() != Some(p.as_str())
                    && active_pane.as_deref() != Some(p.as_str())
            }).cloned();

            if target.is_none() {
                let mut candidates: Vec<&Session> = sessions
                    .iter()
                    .filter(|s| self.is_demotable_working_in_zone(s, active_pane.as_deref()))
                    .collect();
                candidates.sort_by_key(|s| s.last_activity.clone().unwrap_or_default());
                if let Some(s) = candidates.first().copied() {
                    if let Some(t) = s.pane_target.as_deref() {
                        if self.demote_one(s, t, "demote-for-shell") {
                            target = Some(t.to_string());
                            layout_dirty = true;
                        }
                    }
                }
            }

            if let Some(t) = target {
                if convert_placeholder_to_shell(&t) {
                    println!("[shell-spawn] {} ← shell", t);
                }
            }
        }

        if layout_dirty {
            relayout(&self.master, &dashboard_pane);
        } else {
            // External events (pane killed, claude exited, manual layout
            // changes) can leave the dashboard oversized; snap it back.
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

/// Pop a placeholder slot. If none, demote the shell pane (if any, and not
/// active) to free its cell — needs-attention claudes outrank shell. Returns
/// None when even shell-yield can't supply a cell, signalling the caller to
/// stop trying to promote this tick.
///
/// `can_restore_shell` guards against deadlock: the shell-restore block
/// downstream of this only demotes non-active Working claudes, so yielding
/// the shell when none exist leaves the user with no CLI surface and every
/// visible cell stuck on an Idle/Input claude — no way to spawn a new chat
/// to break the jam. In that case we refuse to yield and let the attention-
/// needing claude stay outside the zone (the user can still attach via the
/// dashboard).
fn take_slot_or_yield_shell(
    free_slots: &mut Vec<String>,
    shell_panes: &mut std::collections::HashSet<String>,
    active_pane: &Option<String>,
    can_restore_shell: bool,
) -> Option<String> {
    if let Some(s) = free_slots.pop() { return Some(s) }
    if !can_restore_shell {
        return None;
    }
    let shell = shell_panes
        .iter()
        .find(|t| active_pane.as_deref() != Some(t.as_str()))
        .cloned()?;
    if !convert_shell_to_placeholder(&shell) { return None }
    shell_panes.remove(&shell);
    println!("[shell-yield] {} → placeholder (claude wants cell)", shell);
    Some(shell)
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

fn window_exists(master: &str, name: &str) -> bool {
    let out = Command::new("tmux")
        .args(["list-windows", "-t", master, "-F", "#{window_name}"])
        .output();
    let Ok(out) = out else { return false };
    if !out.status.success() { return false; }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().any(|l| l == name)
}

/// Query `pane_current_command` for a single target. Used by `demote_one` to
/// re-verify a cached `pane_target` still actually holds claude before doing
/// anything destructive.
fn pane_command(target: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_current_command}"])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve any pane target spec (e.g. `session:window.index`) to a stable
/// `%pane_id`. Used when reusing an existing holding window so the subsequent
/// swap-pane keeps working even if window/pane indices reshuffle.
fn pane_id_for(target: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_id}"])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() { None } else { Some(id) }
}

/// Match the same commands the session-discovery layer treats as a live
/// claude pane. Mirrors `is_claude` in `flow::stop` and the detector in
/// `session::*` — keep these in sync.
fn looks_like_claude_cmd(cmd: &str) -> bool {
    cmd == "claude"
        || cmd == "claude.exe"
        || cmd == "node"
        || cmd.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
}

fn is_in_focus_zone(master: &str, pane_target: &str) -> bool {
    let prefix = format!("{}:0.", master);
    pane_target.starts_with(&prefix)
}

/// Everything the orchestrator needs to know about window 0 in one `list-panes`
/// call: every pane's target, which is active, which is the dashboard, and
/// which carry `@recon-role=shell`. Replaces the previous mix of separate
/// `display-message` + `panes_with_role` + `all_panes_in_window_0` calls.
struct WindowZeroSnapshot {
    targets: Vec<String>,
    active: Option<String>,
    dashboard: Option<String>,
    shell: Vec<String>,
    /// pane_target → pane_current_command. Used to detect when an `@recon-role=shell`
    /// pane has been taken over by claude (user ran `claude` inside it).
    commands: HashMap<String, String>,
}

impl WindowZeroSnapshot {
    fn capture(master: &str) -> Self {
        let mut snap = Self {
            targets: Vec::new(),
            active: None,
            dashboard: None,
            shell: Vec::new(),
            commands: HashMap::new(),
        };
        let format = format!("#{{pane_index}}\t#{{?pane_active,1,0}}\t#{{{ROLE_OPT}}}\t#{{pane_current_command}}");
        let out = Command::new("tmux")
            .args(["list-panes", "-t", &format!("{}:0", master), "-F", &format])
            .output();
        let Ok(out) = out else { return snap };
        if !out.status.success() { return snap }
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.is_empty() { continue }
            let idx = parts[0];
            if idx.trim().is_empty() { continue }
            let is_active = parts.get(1).copied() == Some("1");
            let role = parts.get(2).copied().unwrap_or("");
            let cmd = parts.get(3).copied().unwrap_or("").to_string();
            let target = format!("{}:0.{}", master, idx);
            if is_active { snap.active = Some(target.clone()); }
            if role == ROLE_DASHBOARD {
                snap.dashboard = Some(target.clone());
            } else if role == ROLE_SHELL {
                snap.shell.push(target.clone());
            }
            snap.commands.insert(target.clone(), cmd);
            snap.targets.push(target);
        }
        snap
    }
}

fn find_dashboard_pane(master: &str) -> Option<String> {
    panes_with_role(master, "dashboard")
        .into_iter()
        .next()
        .or_else(|| find_panes_by_title(master, DASHBOARD_TITLE).into_iter().next())
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
/// swap-pane, and are overwritten by any program that emits an OSC 2
/// title-set escape — Claude's TUI does this, which is why titles are NOT
/// reliable for identifying recon-owned roles (see `set_pane_role`).
fn set_pane_title(target: &str, title: &str) {
    let _ = Command::new("tmux")
        .args(["select-pane", "-t", target, "-T", title])
        .output();
}

/// Tag a tmux pane with a recon role via the `@recon-role` user option.
/// Unlike `pane_title`, user options are not rewritten by the running
/// program — so the marker survives Claude's TUI taking over the pane.
fn set_pane_role(target: &str, role: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-p", "-t", target, ROLE_OPT, role])
        .output();
}

/// List every pane in master:0 whose `@recon-role` user-option matches.
fn panes_with_role(master: &str, role: &str) -> Vec<String> {
    let out = Command::new("tmux")
        .args([
            "list-panes", "-t", &format!("{}:0", master),
            "-F", "#{pane_index}\t#{@recon-role}",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() { return Vec::new(); }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let idx = parts.next()?;
            let r = parts.next().unwrap_or("");
            (r == role).then(|| format!("{}:0.{}", master, idx))
        })
        .collect()
}

/// Placeholder slot command: an interactive shell with a one-line banner so
/// the user can `cd` somewhere and run `claude` in-pane. Placeholders carry
/// no role marker — the orchestrator identifies free slots by exclusion
/// (any in-zone pane that is neither dashboard, shell, nor a Claude).
const PLACEHOLDER_CMD: &str =
    "clear; printf '\\033[2m(empty slot - cd then claude)\\033[0m\\n\\n'; exec ${SHELL:-/bin/sh}";

/// Pane title applied to every placeholder pane. Travels with the pane across
/// swap-pane (titles are pane-identity-bound, not position-bound). Used to
/// distinguish "this is one of our empty slots" from "this is a user-spawned
/// shell or a leftover after claude exited" — the latter we leave alone.
const PLACEHOLDER_TITLE: &str = "recon-slot";

/// Pane title for the permanent shell pane (bottom-right of the grid). Unlike
/// placeholder slots, this pane is NEVER swapped by the orchestrator — it's
/// the always-available terminal where you cd somewhere and run `claude` to
/// spawn a new agent. Sticky title means even if you `exec claude` here, the
/// orchestrator still leaves the pane alone (it's "your" pane, not its).
const SHELL_TITLE: &str = "recon-shell";

/// Shell command for the permanent shell pane. Wrapped in a respawn loop so
/// that exiting the inner shell (or exec-ing claude and then letting claude
/// exit) brings the shell right back — "always one free terminal" really
/// means always. Note: inner shell is NOT exec-ed (unlike PLACEHOLDER_CMD),
/// otherwise the outer while-loop would be replaced and the respawn lost.
fn shell_respawn_cmd() -> String {
    "while true; do clear; printf '\\033[2m(shell \\xe2\\x80\\x94 cd then \\x60claude\\x60 to spawn an agent)\\033[0m\\n\\n'; ${SHELL:-/bin/sh}; done".to_string()
}

/// Build the persistent 2x2 grid above the dashboard. Called once at master
/// creation. All four cells start as orchestrator-managed placeholder slots;
/// the orchestrator's first tick converts a free cell into the shell pane.
/// Claudes are slid in and out of slots via `swap-pane`, which preserves the
/// grid layout — so we never call `select-layout` and never rebuild this
/// geometry.
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
    // p3 / p4: derive an explicit line count from p1's current height so
    // the row split is symmetric. `-l 50%` (and `-p 50`) left the bottom row
    // a line or two shorter than the top once the dashboard + border were
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

/// Replace the process inside an existing pane with the shell-respawn loop
/// and tag it as the shell pane. Used by the orchestrator to opportunistically
/// fill a free placeholder slot with the always-available terminal.
///
/// Defensive: re-query `pane_current_command` before respawning. `-k` kills
/// whatever runs in the pane, so if a same-tick promote (or a TOCTOU racy
/// user action) parked a claude at `target` between snapshot and now, the
/// respawn would silently kill the claude. Refuse instead.
fn convert_placeholder_to_shell(target: &str) -> bool {
    let cmd = pane_command(target).unwrap_or_default();
    if looks_like_claude_cmd(&cmd) {
        eprintln!(
            "[convert_placeholder_to_shell] refusing — {} holds claude (`{}`), not a placeholder",
            target, cmd,
        );
        return false;
    }
    let cmd = shell_respawn_cmd();
    let ok = Command::new("tmux")
        .args(["respawn-pane", "-k", "-t", target, "sh", "-c", &cmd])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok { return false; }
    set_pane_title(target, SHELL_TITLE);
    set_pane_role(target, "shell");
    true
}

/// Inverse of `convert_placeholder_to_shell`. Loses shell scrollback —
/// acceptable since shell is ephemeral / re-creatable from any slot.
///
/// Defensive: same TOCTOU guard as `convert_placeholder_to_shell`. The shell
/// role tag is set at snapshot time, but a user can `exec claude` inside the
/// shell between snapshot and yield — `-k` would then kill that claude.
fn convert_shell_to_placeholder(target: &str) -> bool {
    let cmd = pane_command(target).unwrap_or_default();
    if looks_like_claude_cmd(&cmd) {
        eprintln!(
            "[convert_shell_to_placeholder] refusing — {} holds claude (`{}`), not a shell",
            target, cmd,
        );
        return false;
    }
    let ok = Command::new("tmux")
        .args(["respawn-pane", "-k", "-t", target, "sh", "-c", PLACEHOLDER_CMD])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok { return false; }
    set_pane_title(target, PLACEHOLDER_TITLE);
    unset_shell_role(target);
    true
}

fn is_shellish_cmd(cmd: &str) -> bool {
    matches!(cmd, "bash" | "zsh" | "sh" | "fish" | "dash" | "ksh" | "tcsh")
}

fn unset_shell_role(target: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-p", "-u", "-t", target, ROLE_OPT])
        .output();
}

/// Create (or reuse) a background window in `master` with a single placeholder
/// pane. Returns the pane id to swap a claude into.
///
/// When `bg-<id>` already exists we deliberately do NOT kill it: a previous
/// promote-back cycle leaves a placeholder behind in that window, and an
/// in-flight demote of the same session id leaves the real Claude there.
/// Killing blind to which one it is would terminate live work. Instead we
/// inspect the pane: placeholder/shell-ish → reuse; anything else → refuse.
fn create_holding_window(master: &str, name: &str) -> Option<String> {
    if window_exists(master, name) {
        let target = format!("{master}:{name}.0");
        let cmd = pane_command(&target).unwrap_or_default();
        if looks_like_claude_cmd(&cmd) {
            eprintln!(
                "[create_holding_window] {}:{} already holds claude (`{}`) — refusing to overwrite",
                master, name, cmd,
            );
            return None;
        }
        // Resolve to a stable pane id so subsequent swap-pane survives any
        // window/pane index churn.
        let pane_id = pane_id_for(&target)?;
        set_pane_title(&pane_id, PLACEHOLDER_TITLE);
        apply_window_styles(&format!("{master}:{name}"));
        return Some(pane_id);
    }

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
    // New window — apply our window-scoped styles so the border colors and
    // dark surface carry over here too.
    apply_window_styles(&format!("{master}:{name}"));
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
