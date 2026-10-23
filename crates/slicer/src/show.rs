//! `slicer show`: print hunks of a plan by their ids.

use crate::plan::Plan;
use anyhow::{Context, Result, bail};
use recap_unidiff::Hunk;
use std::path::PathBuf;

pub struct ShowArgs {
    pub plan: PathBuf,
    /// Hunk ids (`H12`) and group ids (`G3`). A group id prints each hunk
    /// of the group.
    pub ids: Vec<String>,
}

pub fn run(args: &ShowArgs) -> Result<()> {
    let plan = Plan::read(&args.plan)?;
    let mut wanted: Vec<String> = Vec::new();
    for id in &args.ids {
        if id.starts_with('G') {
            let g = plan
                .groups
                .iter()
                .find(|g| &g.id == id)
                .with_context(|| format!("the plan has no group {id}"))?;
            wanted.extend(g.hunks.iter().cloned());
        } else if id.starts_with('H') {
            wanted.push(id.clone());
        } else {
            bail!("{id} is not a group id (G3) or a hunk id (H12)");
        }
    }
    for id in &wanted {
        let (file, h) = plan
            .files
            .iter()
            .find_map(|f| f.hunks.iter().find(|h| &h.id == id).map(|h| (f, h)))
            .with_context(|| format!("the plan has no hunk {id}"))?;
        let group = h.group.as_deref().unwrap_or("no group");
        let ws = if Hunk::from(h).whitespace_only() { ", whitespace only" } else { "" };
        println!("### {id} {} {} ({group}{ws})", file.root_path(), h.header);
        for line in &h.body {
            println!("{line}");
        }
        println!();
    }
    Ok(())
}
