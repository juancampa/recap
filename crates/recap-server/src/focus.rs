//! Show the terminal window of a Claude Code session to the user.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The path of the `claude` binary. A process that Ghostty or launchd
/// starts does not have `~/.local/bin` on its PATH, so the function prefers
/// the usual install location.
pub fn claude_bin() -> String {
    if let Ok(home) = std::env::var("HOME") {
        let p = format!("{home}/.local/bin/claude");
        if Path::new(&p).exists() {
            return p;
        }
    }
    "claude".to_string()
}

/// The path of the `findterm` helper (`~/bin/findterm`), which focuses the
/// terminal window of a process.
fn findterm_bin() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let p = PathBuf::from(home).join("bin/findterm");
    p.exists().then_some(p)
}

/// Focus the terminal window of the running session with the process id
/// `pid`. Without a `pid`, the session does not run: open a new Ghostty
/// window that resumes the session `session_id` in `cwd`.
pub fn focus_or_resume(session_id: &str, pid: Option<i32>, cwd: Option<&Path>) {
    match pid {
        Some(pid) => {
            if let Some(findterm) = findterm_bin() {
                let _ = Command::new(findterm)
                    .arg("--pid")
                    .arg(pid.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        }
        None => open_resume_window(session_id, cwd),
    }
}

/// Open a new Ghostty window that runs `claude --resume <id>` in `cwd`.
///
/// The function uses the AppleScript interface of Ghostty and not
/// `open -na Ghostty.app --args -e …`. macOS ignores `--args` for an app
/// that runs already, so `open` needs `-n` (a second instance of the app).
/// The second instance restores the saved windows of the user as more tabs,
/// and shows an "Allow Ghostty to execute" prompt for each command.
/// `new window with configuration` opens one window in the instance that
/// runs, uses the working directory, and shows no prompt. If Ghostty does
/// not run, the `tell` block starts it.
pub fn open_resume_window(session_id: &str, cwd: Option<&Path>) {
    // Ghostty runs `command` with `/bin/sh -c`.
    let command = format!("{} --resume {}", shell_quote(&claude_bin()), shell_quote(session_id));
    let workdir = cwd.map(|d| d.to_string_lossy().into_owned());

    let mut script = String::from("tell application \"Ghostty\"\n");
    script.push_str("  set cfg to new surface configuration\n");
    script.push_str(&format!("  set command of cfg to {}\n", applescript_quote(&command)));
    if let Some(dir) = &workdir {
        script.push_str(&format!(
            "  set initial working directory of cfg to {}\n",
            applescript_quote(dir)
        ));
    }
    script.push_str("  new window with configuration cfg\n");
    script.push_str("end tell\n");

    let _ = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// `s` in single quotes for `/bin/sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as an AppleScript string.
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
