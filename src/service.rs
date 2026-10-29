//! `recap service install | uninstall | restart | status`: run the prod
//! server as a macOS launchd agent.
//!
//! `install` copies the current binary to `~/.local/share/recap/bin/recap`,
//! so a later `cargo build` does not change the prod server. It writes the
//! agent file `~/Library/LaunchAgents/io.membrane.recap.plist` and loads it.
//! launchd starts the server at login and starts it again when it stops.
//! To upgrade the prod server, run `recap service install` again from the
//! new build.
//!
//! `install --triage-dir <dir>` gives the server a triage directory, and
//! `install --no-triage` removes it. Without these options, `install` keeps
//! the triage directory of the agent file that exists.

use anyhow::{Context, Result, bail};
use recap_api::Api;
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The launchd label of the prod server.
const LABEL: &str = "io.membrane.recap";

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// The copy of the binary that launchd runs.
fn installed_binary() -> Result<PathBuf> {
    Ok(home()?.join(".local/share/recap/bin/recap"))
}

fn plist_path() -> Result<PathBuf> {
    Ok(home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn log_dir() -> Result<PathBuf> {
    Ok(home()?.join("Library/Logs/recap"))
}

/// The file that launchd writes the stdout and stderr of the server to.
fn log_file() -> Result<PathBuf> {
    Ok(log_dir()?.join("server.log"))
}

/// The launchd domain of the user: `gui/<uid>`.
fn domain() -> Result<String> {
    let out = Command::new("id").arg("-u").output().context("cannot run id")?;
    Ok(format!("gui/{}", String::from_utf8_lossy(&out.stdout).trim()))
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .context("cannot run launchctl")
}

/// Escape the XML special characters of `text`.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The agent file. The server gets the PATH of the shell that ran `install`,
/// because it runs `nvim` and launchd gives a process only the system
/// directories.
fn plist(binary: &str, log: &str, path: &str, triage_dir: Option<&str>) -> String {
    let triage = triage_dir
        .map(|dir| {
            format!(
                "\n        <string>--triage-dir</string>\n        <string>{}</string>",
                xml(dir)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>serve</string>
        <string>--port</string>
        <string>{port}</string>{triage}
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        binary = xml(binary),
        port = recap_client::PROD_PORT,
        path = xml(path),
        log = xml(log),
    )
}

fn unxml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The triage directory in the agent file `text`: the string after the
/// `--triage-dir` string.
fn plist_triage_dir(text: &str) -> Option<String> {
    let rest = &text[text.find("<string>--triage-dir</string>")? + 29..];
    let start = rest.find("<string>")? + 8;
    let end = start + rest[start..].find("</string>")?;
    Some(unxml(&rest[start..end]))
}

/// The triage directory of the new agent file. `--triage-dir <dir>` sets
/// it, `--no-triage` removes it. Else the directory of the agent file that
/// exists stays.
fn triage_dir_arg(args: &[String]) -> Result<Option<String>> {
    if args.iter().any(|a| a == "--no-triage") {
        return Ok(None);
    }
    if let Some(i) = args.iter().position(|a| a == "--triage-dir") {
        let dir = args.get(i + 1).context("--triage-dir needs a directory")?;
        // launchd starts the server in `/`, so a relative path is wrong.
        let dir = std::path::absolute(dir)?;
        return Ok(Some(dir.to_string_lossy().into_owned()));
    }
    Ok(fs::read_to_string(plist_path()?)
        .ok()
        .and_then(|text| plist_triage_dir(&text)))
}

fn install(args: &[String]) -> Result<()> {
    let triage_dir = triage_dir_arg(args)?;
    let binary = installed_binary()?;
    let dir = binary.parent().expect("the path has a parent");
    fs::create_dir_all(dir)?;
    // Copy to a temporary file, then rename it. The running server keeps the
    // old file open, and the rename does not change that file.
    let current = std::env::current_exe().context("cannot find the current binary")?;
    let tmp = dir.join("recap.new");
    fs::copy(&current, &tmp)
        .with_context(|| format!("cannot copy {} to {}", current.display(), tmp.display()))?;
    fs::rename(&tmp, &binary)?;

    let logs = log_dir()?;
    fs::create_dir_all(&logs)?;
    let plist_file = plist_path()?;
    fs::create_dir_all(plist_file.parent().expect("the path has a parent"))?;
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".to_string());
    fs::write(
        &plist_file,
        plist(
            &binary.to_string_lossy(),
            &log_file()?.to_string_lossy(),
            &path,
            triage_dir.as_deref(),
        ),
    )?;

    // Stop the old server first.
    let domain = domain()?;
    stop(&domain)?;
    let out = launchctl(&["bootstrap", &domain, &plist_file.to_string_lossy()])?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("installed {}", binary.display());
    println!("agent file {}", plist_file.display());
    println!("log {}", log_file()?.display());
    println!("server {}", recap_client::local_url(recap_client::PROD_PORT));
    match &triage_dir {
        Some(dir) => println!("triage {dir}"),
        None => println!("triage off"),
    }
    Ok(())
}

/// The longest time that `stop` waits for launchd to remove the agent.
const STOP_WAIT: Duration = Duration::from_secs(15);

/// Remove the agent from launchd, and wait until launchd removed it.
/// `launchctl bootout` returns before the server process stops. A
/// `bootstrap` before that fails with "Input/output error".
fn stop(domain: &str) -> Result<()> {
    let service = format!("{domain}/{LABEL}");
    // The command fails when the agent is not loaded.
    let _ = launchctl(&["bootout", &service]);
    let start = Instant::now();
    while launchctl(&["print", &service])?.status.success() {
        if start.elapsed() > STOP_WAIT {
            bail!("launchd did not remove {LABEL} after {}s", STOP_WAIT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    let domain = domain()?;
    stop(&domain)?;
    let plist_file = plist_path()?;
    if plist_file.exists() {
        fs::remove_file(&plist_file)?;
    }
    println!("removed the agent {LABEL}. The binary stays at {}", installed_binary()?.display());
    Ok(())
}

fn restart() -> Result<()> {
    let out = launchctl(&["kickstart", "-k", &format!("{}/{LABEL}", domain()?)])?;
    if !out.status.success() {
        bail!(
            "launchctl kickstart failed: {}. Run `recap service install` first.",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("restarted {LABEL}");
    Ok(())
}

fn status() -> Result<()> {
    let out = launchctl(&["print", &format!("{}/{LABEL}", domain()?)])?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with("state =") || line.starts_with("pid =") || line.starts_with("runs =")
                || line.starts_with("last exit code =")
            {
                println!("{line}");
            }
        }
    } else {
        println!("the agent {LABEL} is not loaded");
    }
    let url = recap_client::local_url(recap_client::PROD_PORT);
    match recap_client::Client::new(&url).status() {
        Ok(s) => println!(
            "{url}: version {}, {} entries, loading {}, embeddings {:?}",
            s.version, s.entries, s.loading, s.embed
        ),
        Err(e) => println!("{url}: {e}"),
    }
    Ok(())
}

/// Run `recap service <command>`.
pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(),
        Some("restart") => restart(),
        Some("status") => status(),
        _ => bail!(
            "usage: recap service install [--triage-dir <dir> | --no-triage] | uninstall | restart | status"
        ),
    }
}

/// `recap logs [-f] [-n <lines>]`: print the end of the prod server log as
/// text, with colors when stdout is a terminal. `-f` prints the new lines
/// until SIGINT. The command reads the output of `tail -F`, because the logger
/// empties the file at a size limit and `-F` reads the file again from the
/// start after that.
pub fn logs(args: &[String]) -> Result<()> {
    let follow = args.iter().any(|a| a == "-f" || a == "--follow");
    let lines = match args.iter().position(|a| a == "-n" || a == "--lines") {
        Some(i) => {
            let v = args.get(i + 1).map(String::as_str).unwrap_or("");
            v.parse::<usize>()
                .map_err(|_| anyhow::anyhow!("-n: `{v}` is not a number of lines"))?
        }
        None => 50,
    };
    let file = log_file()?;
    if !file.exists() {
        bail!("{} does not exist. Run `recap service install` first.", file.display());
    }
    let mut cmd = Command::new("tail");
    cmd.arg("-n").arg(lines.to_string());
    if follow {
        cmd.arg("-F");
    }
    let mut child = cmd
        .arg(&file)
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot run tail")?;
    let color = std::io::stdout().is_terminal();
    let mut reader = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut stdout = std::io::stdout().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = recap_server::colorize::render_line(text.trim_end_matches('\n'), color);
        // The write fails when the reader of a pipe stops, for example `head`.
        if writeln!(stdout, "{text}").and_then(|_| stdout.flush()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
    }
    let status = child.wait()?;
    // tail stops with a signal after SIGINT. That is the normal end of `-f`.
    if !status.success() && status.code().is_some() {
        bail!("tail failed: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_reads_back_the_triage_dir_that_it_wrote() {
        let with = plist("/b/recap", "/l", "/usr/bin", Some("/t/a&b"));
        assert_eq!(plist_triage_dir(&with).as_deref(), Some("/t/a&b"));
        let without = plist("/b/recap", "/l", "/usr/bin", None);
        assert_eq!(plist_triage_dir(&without), None);
    }
}
