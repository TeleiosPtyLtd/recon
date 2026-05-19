//! Synthetic test harness for `recon flow`.
//!
//! Spawns N fake "Claude" agents — bash panes each with a `sleep infinity`
//! child and a fabricated `~/.claude/sessions/{child_pid}.json`. Recon
//! discovers them as regular Claude panes (its bash branch finds the child
//! via `pgrep -P` and matches a session file by child PID).
//!
//! State is driven by per-agent files in /tmp/recon-flow-test/. Each agent's
//! script polls its state file and updates the last line of its pane to one
//! of the strings recon's status-bar detection recognises.
//!
//! `--auto` spawns a cycler pane that randomly changes states every ~10s.
//! `--cleanup` tears the whole thing down.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::flow;

const TEST_MASTER: &str = "recon-flow-test";
const FAKE_PREFIX: &str = "claude-fake-";
const TEST_DIR: &str = "/tmp/recon-flow-test";
const FAKE_MARKER: &str = "\"_recon_fake\":true";
const FAKE_PROJECT_DIR: &str = "-tmp-recon-flow-test";

pub fn run(count: u32, auto: bool) {
    if let Err(e) = fs::create_dir_all(TEST_DIR) {
        eprintln!("Failed to create {TEST_DIR}: {e}");
        std::process::exit(1);
    }

    // Ensure no leftover from a previous run.
    cleanup_silent();
    // Re-create work dir (cleanup deleted it).
    let _ = fs::create_dir_all(TEST_DIR);

    let script_path = match write_agent_script() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to write agent script: {e}");
            std::process::exit(1);
        }
    };

    let mut spawned = 0u32;
    for i in 0..count {
        let name = format!("{:02}", i);
        match spawn_fake_agent(&name, &script_path) {
            Ok(()) => spawned += 1,
            Err(e) => eprintln!("Failed to spawn fake agent {name}: {e}"),
        }
    }

    eprintln!("recon flow-test: {spawned} fake agents spawned.");
    eprintln!("  control files at: {TEST_DIR}/agent-NN.state");
    eprintln!("  manually drive a state with e.g.:");
    eprintln!("    echo input > {TEST_DIR}/agent-00.state");
    if auto {
        eprintln!("  --auto: cycler pane will run inside the test session");
    }
    eprintln!("  stop & cleanup: kill the recon-flow-test tmux session, then `recon flow-test --cleanup`");

    // Give the fake agents a moment to write their session.json files before
    // the orchestrator first ticks.
    thread::sleep(Duration::from_secs(1));

    // Hand off to flow::setup_and_attach with the cycler as an extra window.
    let extra: Vec<(&str, Vec<String>)> = if auto {
        let recon_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| "recon".to_string());
        vec![("cycle", vec![
            recon_path,
            "flow-test-cycle".to_string(),
            "--dir".to_string(), TEST_DIR.to_string(),
        ])]
    } else {
        vec![]
    };
    flow::setup_and_attach(TEST_MASTER, 4, &extra);
}

pub fn cleanup() {
    cleanup_silent();
    println!("Cleaned up flow-test: killed sessions, removed fake session files, deleted {TEST_DIR}/.");
}

/// Hidden subcommand: the auto-cycler. Periodically writes random state values
/// to files in `dir`. Runs forever; tmux kills it when the master session ends.
pub fn run_cycler(dir: &str) {
    println!("flow-test cycler: watching {dir}/agent-*.state");
    println!("(this pane stays in 'cycle' window — switch back to window 0 for the focus zone)");
    println!();

    let states = ["working", "idle", "input", "idle", "working", "working"];
    let mut seed: u64 = 0xC0FFEE_u64.wrapping_add(std::process::id() as u64);

    loop {
        thread::sleep(Duration::from_secs(10));

        // Enumerate state files on each tick — picks up agents added/removed mid-run.
        let agents: Vec<PathBuf> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("agent-") && n.ends_with(".state"))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();

        if agents.is_empty() { continue; }

        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let agent_idx = ((seed >> 33) as usize) % agents.len();
        let state_idx = (((seed.wrapping_mul(31)) >> 33) as usize) % states.len();
        let next_state = states[state_idx];
        let target = &agents[agent_idx];

        if fs::write(target, format!("{}\n", next_state)).is_ok() {
            println!("→ {}: {}", target.file_name().and_then(|n| n.to_str()).unwrap_or("?"), next_state);
        }
    }
}

fn cleanup_silent() {
    let _ = Command::new("tmux").args(["kill-session", "-t", TEST_MASTER]).output();

    if let Ok(out) = Command::new("tmux").args(["list-sessions", "-F", "#S"]).output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.starts_with(FAKE_PREFIX) {
                let _ = Command::new("tmux").args(["kill-session", "-t", line]).output();
            }
        }
    }

    if let Some(home) = dirs::home_dir() {
        let sess_dir = home.join(".claude/sessions");
        if let Ok(entries) = fs::read_dir(&sess_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if content.contains(FAKE_MARKER) {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }
        let proj_dir = home.join(".claude/projects").join(FAKE_PROJECT_DIR);
        let _ = fs::remove_dir_all(&proj_dir);
    }

    let _ = fs::remove_dir_all(TEST_DIR);
}

fn write_agent_script() -> std::io::Result<PathBuf> {
    let path = PathBuf::from(TEST_DIR).join("agent.sh");
    fs::write(&path, AGENT_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms)?;
    }
    Ok(path)
}

fn spawn_fake_agent(name: &str, script_path: &PathBuf) -> Result<(), String> {
    let state_file = PathBuf::from(TEST_DIR).join(format!("agent-{name}.state"));
    fs::write(&state_file, "idle\n").map_err(|e| e.to_string())?;

    let session_name = format!("{FAKE_PREFIX}{name}");
    let script_str = script_path.to_string_lossy().into_owned();
    let state_str = state_file.to_string_lossy().into_owned();

    let status = Command::new("tmux")
        .args([
            "new-session", "-d",
            "-s", &session_name,
            "-c", TEST_DIR,
            "bash", &script_str, name, &state_str,
        ])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("tmux new-session failed for {session_name}"));
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
//  Fake agent script.
// ──────────────────────────────────────────────────────────────────────────

const AGENT_SCRIPT: &str = r#"#!/bin/bash
# Fake claude agent for recon flow-test.
NAME="$1"
STATE_FILE="$2"

# Spawn a fake "Claude" child process whose PID we register with recon.
sleep infinity &
CHILD_PID=$!
disown $CHILD_PID 2>/dev/null || true

RAND=$(od -An -N4 -tx4 /dev/urandom 2>/dev/null | tr -d ' \n' || echo "$$_$RANDOM")
SESSION_ID="fake-${NAME}-${RAND}"
SESSIONS_DIR="$HOME/.claude/sessions"
PROJECT_DIR="$HOME/.claude/projects/-tmp-recon-flow-test"
SESSION_JSON="$SESSIONS_DIR/${CHILD_PID}.json"
JSONL_PATH="$PROJECT_DIR/${SESSION_ID}.jsonl"
mkdir -p "$SESSIONS_DIR" "$PROJECT_DIR"

NOW_MS=$(($(date +%s) * 1000))
cat > "$SESSION_JSON" <<EOF
{"pid": $CHILD_PID, "sessionId": "$SESSION_ID", "startedAt": $NOW_MS, "_recon_fake": true}
EOF

NOW_ISO=$(date -u +%Y-%m-%dT%H:%M:%SZ)
CWD=$(pwd)
cat > "$JSONL_PATH" <<EOF
{"type":"assistant","timestamp":"$NOW_ISO","cwd":"$CWD","sessionId":"$SESSION_ID","message":{"model":"claude-opus-4-7","usage":{"input_tokens":1000,"output_tokens":500,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
EOF

cleanup() {
    kill "$CHILD_PID" 2>/dev/null || true
    rm -f "$SESSION_JSON" "$JSONL_PATH"
}
trap cleanup EXIT INT TERM

while true; do
    state=$(cat "$STATE_FILE" 2>/dev/null || echo "idle")
    state=$(echo "$state" | tr -d '[:space:]')

    clear
    printf '\033[1m═══ FAKE CLAUDE: %s ═══\033[0m\n' "$NAME"
    printf 'session: %s\n' "$SESSION_ID"
    printf 'state:   %s\n\n' "$state"
    printf 'Synthetic agent for recon flow-test. Edit\n'
    printf '  %s\n' "$STATE_FILE"
    printf 'with one of: working / idle / input\n\n'
    printf 'Sample output:\n'
    printf '  > Reading src/session.rs (1343 lines)\n'
    printf '  > Found build_live_session_map at line 211\n'
    printf '  > Patching parse_jsonl to skip seen offsets\n'
    for _ in $(seq 1 6); do echo; done
    case "$state" in
        working) echo "esc to interrupt" ;;
        input)   echo "Esc to cancel" ;;
        *)       echo "> ready for next prompt" ;;
    esac
    sleep 0.5
done
"#;
