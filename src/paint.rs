use std::process::Command;

use crate::session::SessionStatus;

struct Palette {
    /// Window background tint. None means use terminal default (max readability).
    bg: Option<&'static str>,
    /// Pane border foreground color.
    border: &'static str,
    /// Target WCAG contrast ratio of fg vs bg. None disables fg override (use
    /// terminal default fg). Encodes intent:
    ///   • low ratio  (≈2.5) → text deliberately dim, pane reads as "asleep"
    ///   • high ratio (≥7.0) → text pops, pulls attention to the prompt
    target_contrast: Option<f32>,
}

fn palette_for(status: &SessionStatus) -> Palette {
    match status {
        // Agent is busy and you aren't reading the pane.
        // Dim bg, dimmer text — the pane is soft on the eyes, easy to ignore.
        SessionStatus::Working => Palette {
            bg: Some("#0F1117"),
            border: "#2A3142",
            target_contrast: Some(2.5),
        },
        // No interaction yet. Neutral; readable.
        SessionStatus::New => Palette {
            bg: None,
            border: "#3D4759",
            target_contrast: None,
        },
        // Your turn — reading agent output, composing a reply.
        // Terminal default everywhere. Border is the only signal.
        SessionStatus::Idle => Palette {
            bg: None,
            border: "#808080",
            target_contrast: None,
        },
        // Human-in-the-loop permission prompt. Warm red bg with high-contrast
        // warm-white fg so the prompt text is unmistakably crisp.
        SessionStatus::Input => Palette {
            bg: Some("#2A1414"),
            border: "#E53E3E",
            target_contrast: Some(7.0),
        },
    }
}

/// Paint a single tmux pane to reflect agent state. The bg/fg are set as
/// pane-scope overrides of `window-style` / `window-active-style`. Pane scope
/// wins over window scope in tmux's option resolution, so painting one Claude
/// pane never bleeds into a neighbour — critical in flow mode where many
/// panes share window 0.
///
/// Note: `tmux select-pane -P` is NOT the right API for per-pane styling. In
/// tmux 3.6 (and likely earlier) `-P` silently writes to the window-level
/// option, not the pane scope. `set-option -p` is the only thing that
/// produces a true per-pane override.
///
/// Border styling is intentionally not touched here — that's static UI
/// config, owned by `flow::apply_tmux_config`.
pub fn paint_pane(pane_target: &str, status: &SessionStatus) {
    let p = palette_for(status);
    let style = match p.bg {
        Some(bg_hex) => match p.target_contrast {
            Some(ratio) => {
                let fg_hex = pick_fg_hex(bg_hex, ratio);
                Some(format!("bg={},fg={}", bg_hex, fg_hex))
            }
            None => Some(format!("bg={}", bg_hex)),
        },
        None => None,
    };

    match style {
        Some(s) => {
            set_pane_option(pane_target, "window-style", &s);
            set_pane_option(pane_target, "window-active-style", &s);
        }
        None => {
            // Drop the pane-scope override so the pane falls back to whatever
            // the window (or terminal default) provides.
            unset_pane_option(pane_target, "window-style");
            unset_pane_option(pane_target, "window-active-style");
        }
    }

    // One-time scrub: the previous paint code used `select-pane -P`, which
    // tmux silently translated into window-level writes. Those values stick
    // around and bleed onto every pane that hasn't been painted yet. Clearing
    // them on each paint guarantees the pane-scope override is what shows.
    if let Some(window) = window_from_pane(pane_target) {
        unset_window_option(&window, "window-style");
        unset_window_option(&window, "window-active-style");
    }
}

fn window_from_pane(pane_target: &str) -> Option<String> {
    let (window, _) = pane_target.rsplit_once('.')?;
    Some(window.to_string())
}

fn set_pane_option(target: &str, name: &str, value: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-p", "-t", target, name, value])
        .output();
}

fn unset_pane_option(target: &str, name: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-p", "-u", "-t", target, name])
        .output();
}

fn unset_window_option(target: &str, name: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-w", "-u", "-t", target, name])
        .output();
}

// ──────────────────────────────────────────────────────────────────────────
//  Color algorithm — derive a foreground that hits a target WCAG contrast
//  against the background, in the same hue family so warm bgs get warm-white
//  fg and cool bgs get cool-white fg. Low target = dim/soft; high = crisp.
// ──────────────────────────────────────────────────────────────────────────

fn pick_fg_hex(bg_hex: &str, target_ratio: f32) -> String {
    let (br, bg_, bb) = parse_hex(bg_hex);
    let (h, _, _) = rgb_to_hsl(br, bg_, bb);
    let bg_lum = relative_luminance(br, bg_, bb);

    // Low saturation so the fg reads as "white" but inherits a faint tint from
    // the bg's hue. 0.08 keeps the warmth/coolness without coloring the text.
    let s = 0.08_f32;

    // Binary search lightness for the target contrast. Both endpoints are
    // lighter than typical bgs so we converge upward.
    let (mut lo, mut hi) = (0.30_f32, 0.98_f32);
    for _ in 0..18 {
        let mid = (lo + hi) / 2.0;
        let (r, g, b) = hsl_to_rgb(h, s, mid);
        let fg_lum = relative_luminance(r, g, b);
        let ratio = contrast(bg_lum, fg_lum);
        if ratio < target_ratio {
            lo = mid; // need lighter
        } else {
            hi = mid;
        }
    }
    let l = (lo + hi) / 2.0;
    let (r, g, b) = hsl_to_rgb(h, s, l);
    format!("#{:02X}{:02X}{:02X}", r, g, b)
}

fn parse_hex(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim_start_matches('#');
    let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
    (r, g, b)
}

fn relative_luminance(r: u8, g: u8, b: u8) -> f32 {
    let f = |c: u8| -> f32 {
        let s = (c as f32) / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

fn contrast(a: f32, b: f32) -> f32 {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == rf {
        ((gf - bf) / d) + if gf < bf { 6.0 } else { 0.0 }
    } else if max == gf {
        ((bf - rf) / d) + 2.0
    } else {
        ((rf - gf) / d) + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s.abs() < 1e-6 {
        let v = (l * 255.0).round().clamp(0.0, 255.0) as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue2rgb = |p: f32, q: f32, mut t: f32| -> f32 {
        if t < 0.0 { t += 1.0; }
        if t > 1.0 { t -= 1.0; }
        if t < 1.0 / 6.0 { return p + (q - p) * 6.0 * t; }
        if t < 1.0 / 2.0 { return q; }
        if t < 2.0 / 3.0 { return p + (q - p) * (2.0 / 3.0 - t) * 6.0; }
        p
    };
    let r = hue2rgb(p, q, h + 1.0 / 3.0);
    let g = hue2rgb(p, q, h);
    let b = hue2rgb(p, q, h - 1.0 / 3.0);
    (
        (r * 255.0).round().clamp(0.0, 255.0) as u8,
        (g * 255.0).round().clamp(0.0, 255.0) as u8,
        (b * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

// ──────────────────────────────────────────────────────────────────────────
//  Test harness — isolated tmux session for visual palette inspection.
// ──────────────────────────────────────────────────────────────────────────

const TEST_SESSION: &str = "recon-paint-test";

/// Create a throwaway tmux session with four windows, one per agent state,
/// each painted with the corresponding palette. Touches no other sessions.
pub fn test_setup() {
    test_cleanup_silent();

    let states = [
        ("new",     SessionStatus::New),
        ("working", SessionStatus::Working),
        ("idle",    SessionStatus::Idle),
        ("input",   SessionStatus::Input),
    ];

    for (i, (name, status)) in states.iter().enumerate() {
        let body = sample_body(name);
        if i == 0 {
            let _ = Command::new("tmux")
                .args([
                    "new-session", "-d", "-s", TEST_SESSION, "-n", name,
                    "sh", "-c", &body,
                ])
                .output();
        } else {
            let _ = Command::new("tmux")
                .args([
                    "new-window", "-t", TEST_SESSION, "-n", name,
                    "sh", "-c", &body,
                ])
                .output();
        }
        let pane = format!("{}:{}.0", TEST_SESSION, i);
        paint_pane(&pane, status);
    }

    println!("Painted test session '{TEST_SESSION}' with 4 windows: new, working, idle, input.");
    println!();
    println!("Computed palette:");
    for (name, status) in &states {
        let p = palette_for(status);
        let bg_disp = p.bg.unwrap_or("(terminal default)");
        let fg_disp = match (p.bg, p.target_contrast) {
            (Some(bg), Some(ratio)) => {
                let fg = pick_fg_hex(bg, ratio);
                let (br, bgc, bb) = parse_hex(bg);
                let (fr, fgc, fbc) = parse_hex(&fg);
                let ratio_actual = contrast(
                    relative_luminance(br, bgc, bb),
                    relative_luminance(fr, fgc, fbc),
                );
                format!("{fg}  (target {ratio:.1}:1, actual {ratio_actual:.2}:1)")
            }
            _ => "(terminal default)".to_string(),
        };
        println!("  {:>7}  bg {:<24}  fg {}", name, bg_disp, fg_disp);
        println!("  {:>7}  border {}", "", p.border);
    }
    println!();
    println!("To inspect (from inside tmux):");
    println!("  tmux switch-client -t {TEST_SESSION}");
    println!("Or from outside tmux:");
    println!("  tmux attach -t {TEST_SESSION}");
    println!();
    println!("Cycle windows with: prefix+0 / prefix+1 / prefix+2 / prefix+3");
    println!();
    println!("When done, clean up with:");
    println!("  recon paint-test --cleanup");
}

/// Kill the throwaway test session.
pub fn test_cleanup() {
    let killed = test_cleanup_silent();
    if killed {
        println!("Killed test session '{TEST_SESSION}'.");
    } else {
        println!("No test session '{TEST_SESSION}' to clean up.");
    }
}

fn test_cleanup_silent() -> bool {
    Command::new("tmux")
        .args(["kill-session", "-t", TEST_SESSION])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn sample_body(state: &str) -> String {
    let banner = format!(
        "═══ STATE: {} ═══\n\n\
         Sample agent output (default text — should follow window-style fg):\n\
         > Reading src/session.rs (1343 lines)\n\
         > Found build_live_session_map at line 211\n\
         > Modifying parse_jsonl to skip already-seen offsets\n\n\
         Sample prompt area:\n\
         > How should I handle the case where the JSONL file is rotated?\n\n\
         (window blocks here — recon paint-test --cleanup to kill)\n",
        state.to_uppercase()
    );
    format!("clear; printf %s {}; tail -f /dev/null", shell_quote(&banner))
}

fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', r"'\''");
    format!("'{}'", escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_fg_hits_target() {
        let bg = "#0F1117";
        let fg = pick_fg_hex(bg, 2.5);
        let (br, bg_, bb) = parse_hex(bg);
        let (fr, fg_, fb) = parse_hex(&fg);
        let ratio = contrast(
            relative_luminance(br, bg_, bb),
            relative_luminance(fr, fg_, fb),
        );
        assert!(ratio >= 2.45 && ratio <= 2.6, "Working contrast={ratio}");
    }

    #[test]
    fn input_fg_hits_target() {
        let bg = "#2A1414";
        let fg = pick_fg_hex(bg, 7.0);
        let (br, bg_, bb) = parse_hex(bg);
        let (fr, fg_, fb) = parse_hex(&fg);
        let ratio = contrast(
            relative_luminance(br, bg_, bb),
            relative_luminance(fr, fg_, fb),
        );
        assert!(ratio >= 6.95 && ratio <= 7.15, "Input contrast={ratio}");
    }
}
