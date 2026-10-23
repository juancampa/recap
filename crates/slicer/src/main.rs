//! slicer: plan and stage a series of commits from the pending changes of a
//! git repository and its submodules.
//!
//! `slicer plan` writes a short report of the changes, with the hunks in
//! groups: one group for each prompt of an agent session, from the
//! attribution of the recap server. `slicer stage` stages groups and hunks by
//! their ids. `slicer show` prints hunks by their ids. `slicer finish` checks the submodules before the push and
//! prints the commit list for the PR body.

mod finish;
mod git;
mod plan;
mod repo;
mod rules;
mod show;
mod stage;

use anyhow::{Result, bail};
use std::path::PathBuf;

const USAGE: &str = "\
usage:
  slicer plan [--out <dir>] [--owner <github owner>]... [--fallback-branch <branch>]
              [--ignore <glob>]... [--wip <regex>]... [--blame-root <path>] [-- <path>...]
  slicer show [--plan <dir>] <G3|H12>...
  slicer stage [--plan <dir>] <G3|H12>... [--exclude-hunk <H12>]... [--drop-line <regex>]...
  slicer finish [--owner <github owner>]...

An id list can also use commas: `G3,G5 H40`. The default plan directory is
`slice` in the git directory of the outermost repository (`.git/slice`).

plan    Print a report of the pending changes and write <dir>/plan.json,
        <dir>/dossier.md and <dir>/groups/G<n>.md. The paths after `--` limit
        the plan to these paths, relative to the root repository.
        --out             the plan directory
        --owner           a GitHub owner whose submodule remotes slicer may push to
        --fallback-branch the branch for a detached submodule without another choice
        --ignore          an untracked path to leave out (`*`, `**`, `?`; a glob
                          without `/` matches the file name)
        --wip             an extra WIP marker, a regex for an added line
        --blame-root      the checkout where the agents made the edits, when the
                          diff comes from another checkout of the same repository
show    Print the diff of each hunk, and of each hunk of a group.
stage   Stage the hunks of groups and single hunks in the index of their
        repositories.
        --exclude-hunk    a hunk of a named group that stays out of the index
        --drop-line       a regex: the changed lines that match stay out of the
                          index, and the rest of the hunk goes in
finish  Fetch the submodules, report unpushed commits and pointers that no remote
        branch contains, and print the commit list and the PR body. The exit
        code is 1 when it found a problem.

The recap server address comes from RECAP_URL (default http://127.0.0.1:7717).
";

/// The command-line flags: each flag takes one value, and `--` ends them.
struct Flags {
    values: Vec<(String, String)>,
    positional: Vec<String>,
    after_dashes: Vec<String>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Flags> {
        let mut values = Vec::new();
        let mut positional = Vec::new();
        let mut after_dashes = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--" {
                after_dashes.extend(it.by_ref().cloned());
            } else if a == "-h" || a == "--help" {
                print!("{USAGE}");
                std::process::exit(0);
            } else if let Some(name) = a.strip_prefix("--") {
                let Some(v) = it.next() else {
                    bail!("--{name} needs a value\n\n{USAGE}");
                };
                values.push((name.to_string(), v.clone()));
            } else {
                positional.push(a.clone());
            }
        }
        Ok(Flags { values, positional, after_dashes })
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.values.iter().filter(|(n, _)| n == name).map(|(_, v)| v.clone()).collect()
    }

    fn one(&self, name: &str) -> Option<String> {
        self.all(name).pop()
    }

    /// The ids of the positional arguments, or of all the values of the flag
    /// `name`. Each value can hold several ids, separated by commas.
    fn ids(&self, name: Option<&str>) -> Vec<String> {
        let values = match name {
            Some(n) => self.all(n),
            None => self.positional.clone(),
        };
        values
            .iter()
            .flat_map(|v| v.split([',', ' ']))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The plan directory: the value of `--plan` or `--out`, or the default.
    fn plan_dir(&self, flag: &str) -> Result<PathBuf> {
        match self.one(flag) {
            Some(dir) => Ok(PathBuf::from(dir)),
            None => git::default_plan_dir(),
        }
    }

    /// Fail on a flag that the command does not know, so a typo does not
    /// pass silently.
    fn only(&self, known: &[&str]) -> Result<()> {
        for (n, _) in &self.values {
            if !known.contains(&n.as_str()) {
                bail!("unknown flag --{n}\n\n{USAGE}");
            }
        }
        Ok(())
    }
}

fn main() {
    // Print the error without a backtrace: the agent that reads the output
    // needs the message, not the frames.
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        print!("{USAGE}");
        std::process::exit(2);
    };
    let flags = Flags::parse(&args[1..])?;
    match command.as_str() {
        "plan" => {
            flags.only(&["out", "owner", "fallback-branch", "ignore", "wip", "blame-root"])?;
            plan::run(&plan::PlanArgs {
                out: flags.plan_dir("out")?,
                owners: flags.all("owner"),
                fallback_branch: flags.one("fallback-branch"),
                ignore: flags.all("ignore"),
                wip: flags.all("wip"),
                blame_root: flags.one("blame-root").map(PathBuf::from),
                scope: flags.after_dashes.clone(),
            })
        }
        "show" => {
            flags.only(&["plan"])?;
            show::run(&show::ShowArgs {
                plan: flags.plan_dir("plan")?,
                ids: flags.ids(None),
            })
        }
        "stage" => {
            flags.only(&["plan", "exclude-hunk", "drop-line"])?;
            stage::run(&stage::StageArgs {
                plan: flags.plan_dir("plan")?,
                ids: flags.ids(None),
                exclude_hunks: flags.ids(Some("exclude-hunk")),
                drop_lines: flags.all("drop-line"),
            })
        }
        "finish" => {
            flags.only(&["owner"])?;
            let ok = finish::run(&finish::FinishArgs { owners: flags.all("owner") })?;
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command {other}\n\n{USAGE}"),
    }
}
