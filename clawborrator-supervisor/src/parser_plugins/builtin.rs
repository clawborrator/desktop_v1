// Built-in plugins for the six known CC startup prompts.
//
// Naming convention: each plugin's `name()` is a short kebab-case
// identifier used in tracing + the watcher's fire-once set.
//
// Sentinels: each plugin picks a substring that uniquely identifies
// its prompt (i.e. won't false-match against any other CC screen).
// Where the cursor needs to land on a specific option, the plugin
// also checks `ScreenView::highlighted_option()` so we don't fire
// while CC is mid-render with an indeterminate cursor.
//
// Byte sequences:
//   - Plain Enter:     b"\r"
//   - Arrow-down + Enter: b"\x1b[B\r"
//     (ESC [ B is the standard xterm "Cursor Down" sequence)
//
// All sentinels are matched against the raw `screen.contents()`
// from vt100 — no ANSI residue, just plain text from rendered cells.

use super::{Action, ParserPlugin, ScreenView};

pub fn default_plugins() -> Vec<Box<dyn ParserPlugin>> {
    vec![
        Box::new(NoResume),
        Box::new(NoContinue),
        Box::new(ResumePicker),
        Box::new(TrustFolder),
        Box::new(DevChannels),
        Box::new(McpServer),
        Box::new(BypassPermissions),
    ]
}

// === Sentinel-only restart plugins ============================

/// `--resume` with no resumable conversation → respawn without
/// `--resume`. CC prints the sentinel then waits on Ctrl+C.
pub struct NoResume;
impl ParserPlugin for NoResume {
    fn name(&self) -> &'static str { "no-resume" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if screen.contains("No conversations found to resume") {
            Some(Action::RestartWithoutFlag("--resume".to_string()))
        } else { None }
    }
}

/// `--continue` with no convo to continue → respawn without
/// `--continue`. CC prints the sentinel then exits.
pub struct NoContinue;
impl ParserPlugin for NoContinue {
    fn name(&self) -> &'static str { "no-continue" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if screen.contains("No conversation found to continue") {
            Some(Action::RestartWithoutFlag("--continue".to_string()))
        } else { None }
    }
}

/// `--resume` with at least one resumable conversation → CC shows
/// the "Resume session" picker with the most-recent session
/// highlighted. We auto-pick it by pressing Enter. Sentinel pair
/// ("Resume session" + the picker footer) keeps this from
/// false-matching anywhere else.
pub struct ResumePicker;
impl ParserPlugin for ResumePicker {
    fn name(&self) -> &'static str { "resume-picker" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("Resume session") { return None; }
        if !screen.contains("Ctrl+A to show all projects") { return None; }
        // Cursor marker `> ` indicates the highlighted session;
        // require it before firing so we don't poke an
        // empty/loading picker.
        let has_highlight = screen.lines.iter().any(|l| l.trim_start().starts_with("> "));
        if !has_highlight { return None; }
        Some(Action::WriteBytes(b"\r".to_vec()))
    }
}

// === Enter-on-cursor-1 plugins ================================

/// "Quick safety check: Is this a project you created…" trust-folder
/// prompt. Default highlight is option 1 ("Yes, I trust this
/// folder"); we just need to send Enter.
pub struct TrustFolder;
impl ParserPlugin for TrustFolder {
    fn name(&self) -> &'static str { "trust-folder" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("Quick safety check: Is this a project") { return None; }
        if !screen.contains("Yes, I trust this folder") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

/// `--dangerously-load-development-channels` warning. Default
/// highlight is option 1 ("I am using this for local development").
pub struct DevChannels;
impl ParserPlugin for DevChannels {
    fn name(&self) -> &'static str { "dev-channels" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("WARNING: Loading development channels") { return None; }
        if !screen.contains("I am using this for local development") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

/// "New MCP server found in .mcp.json" — three-option prompt; we
/// pick option 1 ("Use this and all future MCP servers in this
/// project") because the .mcp.json was placed by the daemon itself
/// and is implicitly trusted for the lifetime of the project.
pub struct McpServer;
impl ParserPlugin for McpServer {
    fn name(&self) -> &'static str { "mcp-server" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("New MCP server found in .mcp.json") { return None; }
        if !screen.contains("Use this and all future MCP servers") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        if opt == 1 { Some(Action::WriteBytes(b"\r".to_vec())) } else { None }
    }
}

// === Arrow-down-then-Enter plugin =============================

/// `--dangerously-skip-permissions` warning. CC defaults the highlight
/// to option 1 ("No, exit") to make the dangerous path explicit. We
/// want option 2 ("Yes, I accept"), so send arrow-down + Enter.
pub struct BypassPermissions;
impl ParserPlugin for BypassPermissions {
    fn name(&self) -> &'static str { "bypass-permissions" }
    fn inspect(&self, screen: &ScreenView) -> Option<Action> {
        if !screen.contains("WARNING: Claude Code running in Bypass Permissions mode") { return None; }
        if !screen.contains("Yes, I accept") { return None; }
        let (_, opt) = screen.highlighted_option()?;
        // Cursor defaults to option 1; we want option 2 → ↓ + Enter.
        if opt == 1 {
            Some(Action::WriteBytes(b"\x1b[B\r".to_vec()))
        } else { None }
    }
}
