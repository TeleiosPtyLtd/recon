use clap::{Parser, Subcommand};

/// Monitor and manage Claude Code sessions running in tmux
#[derive(Parser)]
#[command(name = "recon", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open the visual (tamagotchi) dashboard
    View,
    /// Interactive form to create a new tmux session
    New,
    /// Create a new claude session (background by default)
    Launch {
        /// Custom session name (defaults to directory name)
        #[arg(long)]
        name: Option<String>,
        /// Working directory (defaults to current directory)
        #[arg(long)]
        cwd: Option<String>,
        /// Custom command to run instead of claude (e.g. "claude --model sonnet")
        #[arg(long)]
        command: Option<String>,
        /// Attach to the session after creating it
        #[arg(long)]
        attach: bool,
        /// Tag the session (key:value, repeatable)
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Jump directly to the next agent waiting for input
    Next,
    /// Resume a past session (interactive picker, or by ID)
    Resume {
        /// Session ID to resume directly (skips the picker)
        #[arg(long)]
        id: Option<String>,
        /// Custom tmux session name
        #[arg(long)]
        name: Option<String>,
        /// Don't attach to the session after resuming
        #[arg(long)]
        no_attach: bool,
    },
    /// Print all session state as JSON
    Json {
        /// Filter sessions by tag (key:value, repeatable, must match all)
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Save all live sessions to disk for restoring later
    Park,
    /// Restore previously parked sessions
    Unpark,
    /// Create a throwaway tmux session with one window per agent state, each
    /// painted with the corresponding palette, so you can visually inspect the
    /// styling without touching your live work. Use --cleanup to kill it.
    PaintTest {
        #[arg(long)]
        cleanup: bool,
    },
    /// Run the focus-zone orchestrator. Creates tmux session 'recon-flow' with
    /// a dashboard pane at the bottom and a top zone that auto-promotes agents
    /// needing attention (Idle/Input) and demotes them once Working for 5s+.
    /// Without a subcommand, starts (or attaches to) recon-flow.
    Flow {
        /// Max auto-promoted agents in the top zone (default 4).
        #[arg(long, default_value_t = 4)]
        slots: u32,

        #[command(subcommand)]
        action: Option<FlowAction>,
    },
    /// Synthetic test harness. Creates 'recon-flow-test' with fake Claude
    /// agents that recon discovers, plus the orchestrator. Auto-cycles state
    /// transitions when --auto is set. --cleanup tears it all down.
    FlowTest {
        #[arg(long)]
        cleanup: bool,
        /// Number of fake agents to spawn (default 6 so you can exceed the 4-slot budget).
        #[arg(long, default_value_t = 6)]
        count: u32,
        /// Auto-cycle fake agents through random state transitions on a 10s loop.
        #[arg(long)]
        auto: bool,
    },
    /// Internal: runs the orchestrator loop. Invoked by `recon flow` as a tmux
    /// pane inside the master session. Not for direct use.
    #[command(hide = true)]
    FlowOrchestrator {
        #[arg(long)]
        master: String,
        #[arg(long)]
        slots: u32,
    },
    /// Internal: runs the auto-cycler for flow-test. Invoked by `recon flow-test
    /// --auto` as a tmux pane inside the test master session.
    #[command(hide = true)]
    FlowTestCycle {
        #[arg(long)]
        dir: String,
    },
}

#[derive(Subcommand)]
pub enum FlowAction {
    /// Diagnose recon-flow: show windows, panes, orchestrator process status.
    Status,
    /// Gracefully shut down recon-flow. Break-panes every Claude pane out to
    /// its own new tmux session (named bg-<id>), then kill recon-flow.
    /// Your Claude work survives.
    Stop {
        /// Also kill all Claude processes (don't preserve them). Default is preserve.
        #[arg(long)]
        force: bool,
    },
}
