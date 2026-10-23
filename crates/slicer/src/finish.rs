//! `slicer finish`: the checks before the push, and the draft of the PR body.

use crate::git::{gh, git, git_opt};
use crate::repo::{SubmoduleState, all_repos, on_remote, parent_of, submodule_state};
use anyhow::Result;
use std::fmt::Write as _;

pub struct FinishArgs {
    pub owners: Vec<String>,
}

/// Run the checks and print the report. Returns false when a check found a
/// problem.
pub fn run(args: &FinishArgs) -> Result<bool> {
    let root = crate::git::toplevel()?;
    let repos = all_repos(&root);

    // Fetch each submodule in parallel, so the remote refs are current.
    std::thread::scope(|s| {
        for r in repos.iter().filter(|r| !r.rel.is_empty()) {
            s.spawn(|| {
                let _ = git(&r.abs, &["fetch", "-q", "origin"]);
            });
        }
    });
    let subs: Vec<SubmoduleState> = repos
        .iter()
        .filter(|r| !r.rel.is_empty())
        .filter_map(|r| parent_of(&repos, r).map(|p| submodule_state(p, r)))
        .collect();

    let mut problems: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for s in &subs {
        let dir = root.join(&s.rel);
        if s.dirty > 0 {
            notes.push(format!("{}: {} uncommitted paths stay in the working tree", s.rel, s.dirty));
        }
        let owned = s.owned(&args.owners);
        if let Some(n) = s.unpushed
            && n > 0
        {
            if owned {
                problems.push(format!("{}: {n} commits are not pushed (`git -C {} push -u origin HEAD`)", s.rel, s.rel));
            } else {
                problems.push(format!(
                    "{}: {n} local commits, but the origin owner `{}` is not ours: it needs a fork",
                    s.rel,
                    s.owner().unwrap_or_default()
                ));
            }
        }
        if s.branch.is_none() && s.pointer_moved {
            problems.push(format!("{}: HEAD is detached and moved; put it on a branch before the push", s.rel));
        }
        // CI checks out the commit that the parent HEAD records, so that
        // commit must be on the remote.
        if let Some(rec) = &s.recorded
            && !on_remote(&dir, rec)
        {
            problems.push(format!(
                "{}: the parent records {}, which no remote branch contains",
                s.rel,
                &rec[..rec.len().min(10)]
            ));
        }
    }

    let mut out = String::new();
    let branch = git_opt(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let _ = writeln!(out, "# Finish report: {} on {branch}\n", root.display());
    match git_opt(&root, &["rev-list", "--count", "@{upstream}..HEAD"]) {
        Some(n) => {
            let _ = writeln!(out, "- The root branch has {n} commits that are not pushed.");
        }
        None => {
            let _ = writeln!(out, "- The root branch has no upstream yet: `git push -u origin HEAD`.");
        }
    }
    let leftover = git(&root, &["status", "--porcelain"])?;
    if !leftover.trim().is_empty() {
        let _ = writeln!(out, "- Uncommitted paths in the root ({}):", leftover.lines().count());
        for l in leftover.lines().take(40) {
            let _ = writeln!(out, "  {l}");
        }
    }

    let _ = writeln!(out, "\n## Submodule problems ({})\n", problems.len());
    for p in &problems {
        let _ = writeln!(out, "- {p}");
    }
    for n in &notes {
        let _ = writeln!(out, "- note: {n}");
    }

    // The PR body draft: one line per commit after the default branch.
    if let Some(default) = git_opt(&root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        let subjects = git_opt(&root, &["log", "--reverse", "--format=%s", &format!("{default}..HEAD")]).unwrap_or_default();
        let _ = writeln!(out, "\n## Commits after {default}, oldest first ({})\n", subjects.lines().count());
        for s in subjects.lines() {
            let _ = writeln!(out, "- {s}");
        }
    }
    match gh(&root, &["pr", "view", "--json", "number,state,url,body"]) {
        Ok(json) => {
            let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            let _ = writeln!(out, "\n## PR #{} ({}) {}\n", v["number"], v["state"].as_str().unwrap_or("?"), v["url"].as_str().unwrap_or(""));
            let _ = writeln!(out, "The current body:\n\n{}", v["body"].as_str().unwrap_or(""));
        }
        Err(e) => {
            let _ = writeln!(out, "\n## PR\n\nNo PR for this branch ({e}).");
        }
    }
    print!("{out}");
    Ok(problems.is_empty())
}
