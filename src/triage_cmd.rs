//! `recap triage approve`: the user reviews the check scripts that the
//! triage agent wrote. The server runs a script only after an approval (see
//! `recap_server::checks`).

use anyhow::{Context, Result, bail};
use recap_server::checks;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(args: &[String]) -> Result<()> {
    let dir = match args.iter().position(|a| a == "--triage-dir") {
        Some(i) => PathBuf::from(args.get(i + 1).context("--triage-dir needs a directory")?),
        None => crate::service::installed_triage_dir()
            .context("the installed server has no triage directory; use --triage-dir")?,
    };
    let rest: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| *a != "--triage-dir" && (*i == 0 || args[i - 1] != "--triage-dir"))
        .map(|(_, a)| a)
        .collect();
    match rest.first().map(|a| a.as_str()) {
        Some("approve") => approve(&dir, &rest[1..]),
        _ => bail!("usage: recap triage approve [--triage-dir <dir>] [<script>...]"),
    }
}

/// Show each unapproved script, and ask the user for an approval. With
/// names, only those scripts.
fn approve(dir: &Path, names: &[&String]) -> Result<()> {
    let mut pending = checks::unapproved_scripts(dir);
    if !names.is_empty() {
        pending.retain(|p| names.iter().any(|n| *n == p));
    }
    if pending.is_empty() {
        println!("No check script waits for an approval in {}.", dir.display());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    for name in pending {
        let path = dir.join("checks").join(&name);
        println!("\n\x1b[1m── {name} ──\x1b[0m");
        match checks::approved_copy(dir, &name) {
            Some(_) => {
                println!("The script changed after the last approval:");
                let _ = Command::new("diff")
                    .arg("-u")
                    .arg(checks::approved_dir(dir).join(&name))
                    .arg(&path)
                    .status();
            }
            None => {
                println!("A new script:");
                print!("{}", std::fs::read_to_string(&path)?);
            }
        }
        print!("\nApprove {name}? [y/N/q] ");
        std::io::stdout().flush()?;
        let answer = lines.next().transpose()?.unwrap_or_default();
        match answer.trim() {
            "y" | "Y" | "yes" => {
                checks::approve(dir, &name)?;
                println!("Approved {name}.");
            }
            "q" | "Q" => break,
            _ => println!("Skipped {name}."),
        }
    }
    Ok(())
}
