//! `slicer stage`: stage the hunks of groups and single hunks of a plan in
//! the index of their repositories.

use crate::git::{apply, git};
use crate::plan::Plan;
use anyhow::{Context, Result, bail};
use recap_unidiff::Hunk;
use regex::Regex;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

pub struct StageArgs {
    pub plan: PathBuf,
    /// Group ids (`G3`) and hunk ids (`H12`).
    pub ids: Vec<String>,
    /// Hunk ids to leave out of the groups.
    pub exclude_hunks: Vec<String>,
    /// The `+` and `-` lines that match one of these stay out of the index.
    pub drop_lines: Vec<String>,
}

pub fn run(args: &StageArgs) -> Result<()> {
    let plan = Plan::read(&args.plan)?;
    let drops: Vec<Regex> = args
        .drop_lines
        .iter()
        .map(|r| Regex::new(r).with_context(|| format!("bad --drop-line regex {r}")))
        .collect::<Result<_>>()?;

    // The hunk ids to stage.
    let mut wanted: HashSet<String> = HashSet::new();
    for id in &args.ids {
        if id.starts_with('G') {
            let g = plan
                .groups
                .iter()
                .find(|g| &g.id == id)
                .with_context(|| format!("the plan has no group {id}"))?;
            wanted.extend(g.hunks.iter().cloned());
        } else if id.starts_with('H') {
            if !plan.files.iter().any(|f| f.hunks.iter().any(|h| &h.id == id)) {
                bail!("the plan has no hunk {id}");
            }
            wanted.insert(id.clone());
        } else {
            bail!("{id} is not a group id (G3) or a hunk id (H12)");
        }
    }
    for id in &args.exclude_hunks {
        wanted.remove(id);
    }
    if wanted.is_empty() {
        bail!("no hunk to stage");
    }

    // One patch per repository, with the files and hunks in plan order.
    let mut patches: BTreeMap<&str, String> = BTreeMap::new();
    // Each staged hunk alone, to name the hunk that git refuses.
    let mut singles: BTreeMap<&str, Vec<(&str, String)>> = BTreeMap::new();
    let mut staged_ids: Vec<&str> = Vec::new();
    let mut dropped_lines = 0;
    let mut emptied: Vec<&str> = Vec::new();
    for f in &plan.files {
        // The staged hunks of this file, with their ids.
        let mut hunks: Vec<(&str, Hunk)> = Vec::new();
        for h in f.hunks.iter().filter(|h| wanted.contains(&h.id)) {
            let hunk = Hunk::from(h);
            if drops.is_empty() {
                hunks.push((&h.id, hunk));
                continue;
            }
            let before = changed_lines(&hunk);
            match hunk.without_lines(|text| drops.iter().any(|r| r.is_match(text))) {
                Some(kept) => {
                    dropped_lines += before - changed_lines(&kept);
                    hunks.push((&h.id, kept));
                }
                None => {
                    dropped_lines += before;
                    emptied.push(&h.id);
                }
            }
        }
        let file_diff = f.file_diff();
        for (id, h) in &hunks {
            staged_ids.push(id);
            singles.entry(f.repo.as_str()).or_default().push((id, h.patch(&file_diff)));
        }
        if !hunks.is_empty() {
            patches
                .entry(f.repo.as_str())
                .or_default()
                .push_str(&file_diff.patch(hunks.iter().map(|(_, h)| h)));
        }
    }

    let staged_dir = args.plan.join("staged");
    std::fs::create_dir_all(&staged_dir)?;
    for (repo, patch) in &patches {
        let dir = plan.root.join(repo);
        let name = if repo.is_empty() { "root".to_string() } else { repo.replace('/', "__") };
        let patch_path = staged_dir.join(format!("{name}.patch"));
        std::fs::write(&patch_path, patch)?;
        if let Err(e) = apply(&dir, &["--cached", "--check"], patch) {
            let mut why = String::new();
            for (id, single) in singles.get(repo).into_iter().flatten() {
                if apply(&dir, &["--cached", "--check"], single).is_ok() {
                    continue;
                }
                if apply(&dir, &["--cached", "--check", "--reverse"], single).is_ok() {
                    why.push_str(&format!("\n- {id} is in the index already (staged or committed)"));
                } else {
                    why.push_str(&format!("\n- {id} does not apply alone: it may depend on a hunk that is not staged"));
                }
            }
            bail!(
                "git apply --cached refused the patch for {} (saved at {}). slicer staged nothing.\n{e}{why}",
                if repo.is_empty() { "." } else { repo },
                patch_path.display()
            );
        }
    }
    // Apply only after each repository accepted its patch, so a refusal
    // never leaves a part of the hunks in the index.
    for (repo, patch) in &patches {
        apply(&plan.root.join(repo), &["--cached"], patch)?;
    }

    println!("Staged {} hunks: {}", staged_ids.len(), staged_ids.join(" "));
    if dropped_lines > 0 {
        println!("Left {dropped_lines} changed lines out of the index (--drop-line).");
    }
    if !emptied.is_empty() {
        println!("Left out these hunks, because no change stayed after --drop-line: {}", emptied.join(" "));
    }
    if patches.len() > 1 {
        println!("The hunks are in {} repositories. Commit each repository separately, the deepest submodule first.", patches.len());
    }
    for repo in patches.keys() {
        let dir = plan.root.join(repo);
        let stat = git(&dir, &["diff", "--cached", "--stat"])?;
        println!("\n== git diff --cached --stat in {}\n{}", if repo.is_empty() { "." } else { repo }, stat.trim_end());
    }

    // The WIP markers of the plan whose lines are now in the index.
    let staged: HashSet<&str> = staged_ids.iter().copied().collect();
    let hits: Vec<_> = plan
        .wip
        .iter()
        .filter(|w| staged.contains(w.hunk.as_str()))
        .filter(|w| !drops.iter().any(|r| r.is_match(&w.text)))
        .collect();
    if !hits.is_empty() {
        println!("\nWIP markers in the staged hunks:");
        for w in hits {
            println!("- {} {}:{} [{}] `{}`", w.hunk, w.file, w.line, w.marker, w.text);
        }
    }
    Ok(())
}

fn changed_lines(h: &Hunk) -> usize {
    h.body.iter().filter(|l| l.starts_with('+') || l.starts_with('-')).count()
}
