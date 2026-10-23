//! `slicer plan`: collect the pending changes of the repository and its
//! submodules, get the attribution of each hunk from the recap server, and
//! write the plan files.

use crate::git::{gh, git, git_opt, new_file_diff};
use crate::repo::{Repo, SubmoduleState, all_repos, on_remote, parent_of, submodule_state};
use crate::rules::{Glob, WipHit, looks_secret, markers};
use anyhow::{Context, Result};
use recap_api::{Api, Attribution, BlameRequest, HunkBlame};
use recap_unidiff::{FileDiff, Hunk, parse_unified};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// An untracked file larger than this is not in the plan. A file of this
/// size is almost always generated output or data.
const MAX_UNTRACKED_BYTES: u64 = 1 << 20;

/// The longest report that `plan` prints in full.
const MAX_PRINTED_CHARS: usize = 24_000;

/// A matched line count below this does not link two groups. One or two
/// generic lines appear in the edits of many prompts.
const MIN_LINK_LINES: usize = 3;

pub struct PlanArgs {
    pub out: PathBuf,
    pub owners: Vec<String>,
    pub fallback_branch: Option<String>,
    pub ignore: Vec<String>,
    pub wip: Vec<String>,
    /// The checkout where the agents made the edits, when it is not this one.
    pub blame_root: Option<PathBuf>,
    /// Paths relative to the root repository. Empty for all paths.
    pub scope: Vec<String>,
}

/// The file `plan.json`: what `slicer stage` needs.
#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub root: PathBuf,
    pub files: Vec<PlanFile>,
    pub groups: Vec<PlanGroup>,
    pub wip: Vec<WipHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFile {
    /// The repository, relative to the root repository. Empty for the root.
    pub repo: String,
    /// The path relative to `repo`.
    pub path: String,
    pub header: Vec<String>,
    pub hunks: Vec<PlanHunk>,
    /// True for a file that git does not track yet.
    pub untracked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanHunk {
    pub id: String,
    pub header: String,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub body: Vec<String>,
    /// The group of the winner, `None` without attribution.
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanGroup {
    pub id: String,
    pub hunks: Vec<String>,
}

impl From<&PlanHunk> for Hunk {
    fn from(h: &PlanHunk) -> Hunk {
        Hunk {
            header: h.header.clone(),
            old_start: h.old_start,
            old_lines: h.old_lines,
            new_start: h.new_start,
            new_lines: h.new_lines,
            body: h.body.clone(),
        }
    }
}

impl Plan {
    /// Read `plan.json` from the plan directory `dir`.
    pub fn read(dir: &Path) -> Result<Plan> {
        let path = dir.join("plan.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}; run `slicer plan` first", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }
}

impl PlanFile {
    /// The file part of the diff, without hunks.
    pub fn file_diff(&self) -> FileDiff {
        FileDiff {
            path: PathBuf::from(&self.path),
            header: self.header.clone(),
            hunks: Vec::new(),
        }
    }

    pub fn root_path(&self) -> String {
        if self.repo.is_empty() {
            self.path.clone()
        } else {
            format!("{}/{}", self.repo, self.path)
        }
    }
}

/// The files of one repository that the plan leaves out, by reason.
#[derive(Default)]
struct Excluded {
    ignored: Vec<String>,
    secrets: Vec<String>,
    binary: Vec<String>,
    no_hunks: Vec<String>,
    nested_repos: Vec<String>,
}

/// The `git diff` pathspecs for `repo` under `scope`. `None` when the scope
/// does not reach the repository. An empty list means the whole repository.
fn pathspecs(repo: &Repo, scope: &[String]) -> Option<Vec<String>> {
    if scope.is_empty() {
        return Some(Vec::new());
    }
    let mut specs = Vec::new();
    let mut reached = false;
    for s in scope {
        let s = s.trim_end_matches('/');
        if repo.rel.is_empty() {
            specs.push(s.to_string());
            reached = true;
        } else if s == repo.rel || repo.rel.starts_with(&format!("{s}/")) {
            return Some(Vec::new());
        } else if let Some(rest) = s.strip_prefix(&format!("{}/", repo.rel)) {
            specs.push(rest.to_string());
            reached = true;
        }
    }
    reached.then_some(specs)
}

fn with_specs<'a>(mut args: Vec<&'a str>, specs: &'a [String]) -> Vec<&'a str> {
    if !specs.is_empty() {
        args.push("--");
        args.extend(specs.iter().map(String::as_str));
    }
    args
}

/// True when the first bytes of the file hold a NUL byte, as git decides.
fn is_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 8000];
    match std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => buf[..n].contains(&0),
        Err(_) => true,
    }
}

/// The diff text of one repository: the tracked changes and the untracked
/// files that the plan takes. Also the paths of these untracked files,
/// relative to the repository.
fn repo_diff(repo: &Repo, specs: &[String], ignore: &[Glob], excluded: &mut Excluded) -> Result<(String, Vec<String>)> {
    let tracked = git(
        &repo.abs,
        &with_specs(
            vec!["diff", "HEAD", "--no-color", "--no-ext-diff", "--no-renames", "--submodule=short"],
            specs,
        ),
    )?;
    let mut text = tracked;
    let mut taken = Vec::new();
    let untracked = git(
        &repo.abs,
        &with_specs(vec!["ls-files", "--others", "--exclude-standard", "-z"], specs),
    )?;
    for file in untracked.split('\0').filter(|f| !f.is_empty()) {
        let root_path = repo.root_path(file);
        if file.ends_with('/') {
            excluded.nested_repos.push(root_path);
        } else if ignore.iter().any(|g| g.matches(&root_path)) {
            excluded.ignored.push(root_path);
        } else if looks_secret(file) {
            excluded.secrets.push(root_path);
        } else {
            let abs = repo.abs.join(file);
            let big = std::fs::metadata(&abs).map(|m| m.len() > MAX_UNTRACKED_BYTES).unwrap_or(true);
            if big || is_binary(&abs) {
                excluded.binary.push(root_path);
            } else {
                text.push_str(&new_file_diff(&repo.abs, file)?);
                taken.push(file.to_string());
            }
        }
    }
    Ok((text, taken))
}

/// The key of a group: one prompt of one session, or the manual edits of
/// one source.
type GroupKey = (Option<String>, Option<String>, String);

fn key_of(a: &Attribution) -> GroupKey {
    (
        a.session_id.clone(),
        a.prompt_key.clone(),
        format!("{:?}", a.source),
    )
}

/// What the dossier shows about one group.
struct GroupInfo {
    id: String,
    attribution: Attribution,
    hunks: Vec<String>,
    /// The hunks whose winner explains less than half of the voting lines.
    weak: Vec<String>,
}

/// One hunk with the facts that the reports need.
struct HunkRow {
    id: String,
    file: String,
    added: usize,
    removed: usize,
    /// True when the hunk changes only whitespace.
    whitespace_only: bool,
    blame: Option<HunkBlame>,
}

pub fn run(args: &PlanArgs) -> Result<()> {
    let root = crate::git::toplevel()?;
    let ignore: Vec<Glob> = args.ignore.iter().map(|g| Glob::new(g)).collect::<Result<_>>()?;
    let markers = markers(&args.wip)?;
    let client = recap_client::Client::from_env();

    let repos = all_repos(&root);
    let mut excluded = Excluded::default();
    let mut files: Vec<PlanFile> = Vec::new();
    let mut rows: Vec<HunkRow> = Vec::new();
    let mut blame_error: Option<String> = None;
    let mut next_hunk = 1;

    for repo in &repos {
        let Some(specs) = pathspecs(repo, &args.scope) else {
            continue;
        };
        let (text, untracked) = repo_diff(repo, &specs, &ignore, &mut excluded)?;
        if text.trim().is_empty() {
            continue;
        }
        let parsed: Vec<FileDiff> = parse_unified(&text)
            .into_iter()
            .filter(|f| !repo.submodules.iter().any(|s| Path::new(s) == f.path))
            .collect();
        let changed = git(
            &repo.abs,
            &with_specs(vec!["diff", "HEAD", "--name-only", "--no-renames"], &specs),
        )?;
        for path in changed.lines() {
            let in_parsed = parsed.iter().any(|f| f.path == Path::new(path));
            let is_sub = repo.submodules.iter().any(|s| s == path);
            if !in_parsed && !is_sub {
                excluded.no_hunks.push(repo.root_path(path));
            }
        }

        let blame_repo_root = match &args.blame_root {
            Some(b) if repo.rel.is_empty() => b.clone(),
            Some(b) => b.join(&repo.rel),
            None => repo.abs.clone(),
        };
        let blames: HashMap<(String, usize), HunkBlame> = match client.blame(&BlameRequest {
            diff: text.clone(),
            repo_root: blame_repo_root,
        }) {
            Ok(list) => list.into_iter().map(|b| ((b.file.clone(), b.hunk_index), b)).collect(),
            Err(e) => {
                blame_error.get_or_insert_with(|| e.to_string());
                HashMap::new()
            }
        };

        for f in parsed {
            let path = f.path.display().to_string();
            let mut plan_file = PlanFile {
                repo: repo.rel.clone(),
                path: path.clone(),
                header: f.header.clone(),
                hunks: Vec::new(),
                untracked: untracked.contains(&path),
            };
            for (index, h) in f.hunks.iter().enumerate() {
                let id = format!("H{next_hunk}");
                next_hunk += 1;
                let added = h.body.iter().filter(|l| l.starts_with('+')).count();
                let removed = h.body.iter().filter(|l| l.starts_with('-')).count();
                rows.push(HunkRow {
                    id: id.clone(),
                    file: plan_file.root_path(),
                    added,
                    removed,
                    whitespace_only: h.whitespace_only(),
                    blame: blames.get(&(path.clone(), index)).cloned(),
                });
                plan_file.hunks.push(PlanHunk {
                    id,
                    header: h.header.clone(),
                    old_start: h.old_start,
                    old_lines: h.old_lines,
                    new_start: h.new_start,
                    new_lines: h.new_lines,
                    body: h.body.clone(),
                    group: None,
                });
            }
            files.push(plan_file);
        }
    }

    // Groups: one for each winner key, ordered by the prompt time.
    let mut by_key: HashMap<GroupKey, usize> = HashMap::new();
    let mut groups: Vec<GroupInfo> = Vec::new();
    for row in &rows {
        let Some(w) = row.blame.as_ref().and_then(|b| b.winner.as_ref()) else {
            continue;
        };
        let k = key_of(w);
        let gi = *by_key.entry(k).or_insert_with(|| {
            groups.push(GroupInfo {
                id: String::new(),
                attribution: w.clone(),
                hunks: Vec::new(),
                weak: Vec::new(),
            });
            groups.len() - 1
        });
        groups[gi].hunks.push(row.id.clone());
        let b = row.blame.as_ref().expect("a winner implies a blame");
        if w.matched_lines * 2 < b.voting_lines {
            groups[gi].weak.push(row.id.clone());
        }
    }
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by(|&a, &b| {
        let t = |g: &GroupInfo| g.attribution.prompt_time.clone().unwrap_or_else(|| g.attribution.edit_time.clone());
        t(&groups[a]).cmp(&t(&groups[b]))
    });
    let mut renumber = vec![0; groups.len()];
    for (new, &old) in order.iter().enumerate() {
        renumber[old] = new;
    }
    for (old, g) in groups.iter_mut().enumerate() {
        g.id = format!("G{}", renumber[old] + 1);
    }
    for k in by_key.values_mut() {
        *k = renumber[*k];
    }
    groups.sort_by_key(|g| g.id[1..].parse::<usize>().unwrap_or(0));

    let group_of_hunk: HashMap<&str, &str> = groups
        .iter()
        .flat_map(|g| g.hunks.iter().map(move |h| (h.as_str(), g.id.as_str())))
        .collect();
    for f in &mut files {
        for h in &mut f.hunks {
            h.group = group_of_hunk.get(h.id.as_str()).map(|g| g.to_string());
        }
    }

    // WIP markers on the added lines.
    let mut wip: Vec<WipHit> = Vec::new();
    for f in &files {
        for h in &f.hunks {
            for (line, text) in Hunk::from(h).added_lines() {
                if let Some(m) = markers.iter().find(|m| m.regex.is_match(text)) {
                    wip.push(WipHit {
                        hunk: h.id.clone(),
                        file: f.root_path(),
                        line,
                        marker: m.name.clone(),
                        text: text.trim().chars().take(160).collect(),
                    });
                }
            }
        }
    }

    let subs: Vec<SubmoduleState> = repos
        .iter()
        .filter(|r| !r.rel.is_empty() && pathspecs(r, &args.scope).is_some())
        .filter_map(|r| parent_of(&repos, r).map(|p| submodule_state(p, r)))
        .collect();

    // The files of an earlier plan, such as a group number that no longer
    // exists, must not stay next to this plan.
    for old in ["groups", "staged"] {
        let dir = args.out.join(old);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
        }
    }
    std::fs::create_dir_all(args.out.join("groups"))
        .with_context(|| format!("cannot create {}", args.out.display()))?;
    let plan = Plan {
        root: root.clone(),
        files,
        groups: groups
            .iter()
            .map(|g| PlanGroup {
                id: g.id.clone(),
                hunks: g.hunks.clone(),
            })
            .collect(),
        wip,
    };
    std::fs::write(args.out.join("plan.json"), serde_json::to_string(&plan)?)?;
    write_group_files(&args.out, &plan, &groups, &rows)?;

    let dossier = dossier(&root, args, &plan, &groups, &rows, &by_key, &subs, &excluded, blame_error.as_deref());
    let path = args.out.join("dossier.md");
    std::fs::write(&path, &dossier)?;
    // A long tool output reaches the agent only in part, so a long report
    // goes to the file, and stdout says where it is.
    match dossier.find("\n## Groups").filter(|_| dossier.len() > MAX_PRINTED_CHARS) {
        Some(cut) => {
            print!("{}", &dossier[..cut]);
            println!(
                "\n## Groups\n\nThe report is long ({} characters). Read the groups in {} with the Read tool: they start at the line `## Groups`.",
                dossier.len(),
                path.display()
            );
        }
        None => print!("{dossier}"),
    }
    Ok(())
}

fn short(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}…", flat.chars().take(n).collect::<String>())
    }
}

fn time_of(a: &Attribution) -> String {
    let t = a.prompt_time.as_deref().unwrap_or(&a.edit_time);
    t.get(..16).unwrap_or(t).replace('T', " ")
}

fn source_name(a: &Attribution) -> String {
    let source = format!("{:?}", a.source);
    match (&a.session_id, &a.session_name) {
        (Some(id), Some(name)) => format!("{source} session {} \"{name}\"", &id[..id.len().min(8)]),
        (Some(id), None) => format!("{source} session {}", &id[..id.len().min(8)]),
        (None, _) => format!("{source}, manual edit"),
    }
}

fn write_hunk(out: &mut String, plan: &Plan, id: &str, row: &HunkRow) {
    let (file, hunk) = plan
        .files
        .iter()
        .find_map(|f| f.hunks.iter().find(|h| h.id == id).map(|h| (f, h)))
        .expect("each row has a hunk");
    let vote = row
        .blame
        .as_ref()
        .map(|b| match &b.winner {
            Some(w) => format!(" (matched {}/{})", w.matched_lines, b.voting_lines),
            None => format!(" (no winner, {} voting lines)", b.voting_lines),
        })
        .unwrap_or_default();
    if row.whitespace_only {
        let _ = writeln!(
            out,
            "### {id} {} {}{vote} — whitespace only; `slicer show {id}` prints it\n",
            file.root_path(),
            hunk.header
        );
        return;
    }
    let _ = writeln!(out, "### {id} {} {}{vote}\n```diff", file.root_path(), hunk.header);
    for line in &hunk.body {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "```\n");
}

fn write_group_files(dir: &Path, plan: &Plan, groups: &[GroupInfo], rows: &[HunkRow]) -> Result<()> {
    let row_of: HashMap<&str, &HunkRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
    for g in groups {
        let a = &g.attribution;
        let mut out = String::new();
        let _ = writeln!(out, "# {} — {} — {}\n", g.id, source_name(a), time_of(a));
        let _ = writeln!(out, "## Prompt\n\n{}\n", a.prompt.as_deref().unwrap_or("(none)"));
        let _ = writeln!(out, "## Response\n\n{}\n", a.response.as_deref().unwrap_or("(none)"));
        let _ = writeln!(out, "## Hunks\n");
        for id in &g.hunks {
            write_hunk(&mut out, plan, id, row_of[id.as_str()]);
        }
        std::fs::write(dir.join("groups").join(format!("{}.md", g.id)), out)?;
    }
    let mut out = String::from("# Hunks without attribution\n\n");
    for r in rows.iter().filter(|r| r.blame.as_ref().and_then(|b| b.winner.as_ref()).is_none()) {
        write_hunk(&mut out, plan, &r.id, r);
    }
    std::fs::write(dir.join("groups").join("unattributed.md"), out)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dossier(
    root: &Path,
    args: &PlanArgs,
    plan: &Plan,
    groups: &[GroupInfo],
    rows: &[HunkRow],
    by_key: &HashMap<GroupKey, usize>,
    subs: &[SubmoduleState],
    excluded: &Excluded,
    blame_error: Option<&str>,
) -> String {
    let mut out = String::new();
    let branch = git_opt(root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let _ = writeln!(out, "# Slice plan: {} on {branch}\n", root.display());

    // ── Repository ──
    let _ = writeln!(out, "## Repository\n");
    let default = git_opt(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
    if let Some(d) = &default
        && d.trim_start_matches("origin/") == branch
    {
        let _ = writeln!(out, "**STOP: the current branch is the default branch {d}.**\n");
    }
    match git_opt(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        Some(up) => {
            let counts = git_opt(root, &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"]).unwrap_or_default();
            let mut c = counts.split_whitespace();
            let behind = c.next().unwrap_or("?");
            let ahead = c.next().unwrap_or("?");
            let _ = writeln!(out, "- Upstream: {up} (this branch is {ahead} ahead, {behind} behind)");
        }
        None => {
            let _ = writeln!(out, "- Upstream: none");
        }
    }
    if let Some(d) = &default {
        let n = git_opt(root, &["rev-list", "--count", &format!("{d}..HEAD")]).unwrap_or_default();
        let _ = writeln!(out, "- Default branch: {d}; {n} commits on this branch after it");
    }
    match gh(root, &["pr", "view", "--json", "number,state,title,url"]) {
        Ok(json) => {
            let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            let _ = writeln!(
                out,
                "- PR: #{} {} \"{}\" {}",
                v["number"], v["state"].as_str().unwrap_or("?"), v["title"].as_str().unwrap_or(""), v["url"].as_str().unwrap_or("")
            );
            if v["state"] == "MERGED" {
                let _ = writeln!(out, "  **The PR of this branch is merged. The new commits must go to a new branch.**");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "- PR: none ({})", short(&e, 80));
        }
    }
    if let Some(log) = git_opt(root, &["log", "--format=%s", "-10"]) {
        let _ = writeln!(out, "- The last 10 commit subjects:");
        for l in log.lines() {
            let _ = writeln!(out, "  - {l}");
        }
    }
    if let Some(e) = blame_error {
        let _ = writeln!(out, "\n**The recap server did not answer, so no hunk has an attribution: {}**", short(e, 200));
    }

    // ── Submodules ──
    let parent_branch = Some(branch.as_str()).filter(|b| *b != "HEAD");
    let interesting: Vec<&SubmoduleState> = subs
        .iter()
        .filter(|s| s.dirty > 0 || s.pointer_moved || s.unpushed.unwrap_or(0) > 0)
        .collect();
    let _ = writeln!(out, "\n## Submodules\n");
    if interesting.is_empty() {
        let _ = writeln!(out, "No submodule has changes.");
    } else {
        let _ = writeln!(out, "Commit inside each submodule first, deepest first. Then stage the pointer in the parent with `git add <path>`.\n");
        let _ = writeln!(out, "| submodule | branch | dirty files | pointer moved | unpushed | notes |\n|---|---|---|---|---|---|");
        for s in &interesting {
            let mut notes: Vec<String> = Vec::new();
            if s.branch.is_none() {
                match s.suggested_branch(parent_branch, args.fallback_branch.as_deref()) {
                    Some(b) => notes.push(format!("detached: check out or create `{b}`")),
                    None => notes.push("detached".to_string()),
                }
            }
            if !s.owned(&args.owners) {
                notes.push(format!("origin owner `{}` is not ours: do not push", s.owner().unwrap_or_else(|| "?".into())));
            }
            if s.pointer_moved
                && let Some(h) = &s.head
                && !on_remote(&root.join(&s.rel), h)
            {
                notes.push("HEAD is on no remote branch".to_string());
            }
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                s.rel,
                s.branch.as_deref().unwrap_or("(detached)"),
                s.dirty,
                if s.pointer_moved { "yes" } else { "no" },
                s.unpushed.map(|n| n.to_string()).unwrap_or_else(|| "no upstream".into()),
                notes.join("; ")
            );
        }
        let clean = subs.len() - interesting.len();
        if clean > 0 {
            let _ = writeln!(out, "\n{clean} other submodules have no changes.");
        }
    }

    // ── Left out ──
    let _ = writeln!(out, "\n## Not in the plan\n");
    let list = |out: &mut String, title: &str, items: &[String]| {
        if !items.is_empty() {
            let shown: Vec<&str> = items.iter().take(15).map(String::as_str).collect();
            let more = if items.len() > 15 { format!(" and {} more", items.len() - 15) } else { String::new() };
            let _ = writeln!(out, "- {title} ({}): {}{more}", items.len(), shown.join(", "));
        }
    };
    list(&mut out, "Untracked, matched by --ignore", &excluded.ignored);
    list(&mut out, "Untracked, possible secrets", &excluded.secrets);
    list(&mut out, "Untracked binary or large files (stage with `git add` if they belong)", &excluded.binary);
    list(&mut out, "Untracked nested repositories", &excluded.nested_repos);
    list(&mut out, "Changed files with no text hunks (binary or mode change; stage with `git add`)", &excluded.no_hunks);
    let tracked_secrets: Vec<String> = plan
        .files
        .iter()
        .filter(|f| looks_secret(&f.path))
        .map(|f| format!("{} ({})", f.root_path(), f.hunks.iter().map(|h| h.id.as_str()).collect::<Vec<_>>().join(" ")))
        .collect();
    list(&mut out, "Tracked files that may hold secrets: read before you commit", &tracked_secrets);

    // ── WIP ──
    let _ = writeln!(out, "\n## WIP markers ({})\n", plan.wip.len());
    let mut per_file: BTreeMap<&str, Vec<&WipHit>> = BTreeMap::new();
    for w in &plan.wip {
        per_file.entry(w.file.as_str()).or_default().push(w);
    }
    for (file, hits) in per_file {
        for w in hits.iter().take(MAX_WIP_PER_FILE) {
            let _ = writeln!(out, "- {} {file}:{} [{}] `{}`", w.hunk, w.line, w.marker, short(&w.text, 100));
        }
        if hits.len() > MAX_WIP_PER_FILE {
            let _ = writeln!(out, "- … {} more in {file}", hits.len() - MAX_WIP_PER_FILE);
        }
    }

    // ── Untracked ──
    let untracked: Vec<&PlanFile> = plan.files.iter().filter(|f| f.untracked).collect();
    if !untracked.is_empty() {
        let _ = writeln!(out, "\n## Untracked files in the plan ({})\n", untracked.len());
        for f in untracked {
            let lines: usize = f.hunks.iter().map(|h| h.new_lines).sum();
            let ids: Vec<&str> = f.hunks.iter().map(|h| h.id.as_str()).collect();
            let junk = junk_hint(&f.path).map(|h| format!(" — looks like junk: {h}")).unwrap_or_default();
            let _ = writeln!(out, "- {} ({lines} lines, {}){junk}", f.root_path(), id_ranges(&ids));
        }
    }

    // ── Groups ──
    let unattributed: Vec<&HunkRow> = rows
        .iter()
        .filter(|r| r.blame.as_ref().and_then(|b| b.winner.as_ref()).is_none())
        .collect();
    let _ = writeln!(
        out,
        "\n## Groups ({} groups, {} hunks, {} hunks without attribution)\n",
        groups.len(),
        rows.len(),
        unattributed.len()
    );
    let _ = writeln!(
        out,
        "Each group G<n> is one prompt of one session. The sessions are in the order of their first prompt. \
         P is the prompt and R is the response, both shortened. `{}/groups/G<n>.md` holds the full prompt, the full response and the hunks. \
         `slicer show <ids>` prints single hunks.\n",
        args.out.display()
    );
    let row_of: HashMap<&str, &HunkRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
    let group_ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
    // The sessions, in the order of their first group.
    let mut sessions: Vec<(String, Vec<&GroupInfo>)> = Vec::new();
    for g in groups {
        let name = source_name(&g.attribution);
        match sessions.iter_mut().find(|(n, _)| *n == name) {
            Some((_, gs)) => gs.push(g),
            None => sessions.push((name, vec![g])),
        }
    }
    for (name, gs) in sessions {
        let _ = writeln!(out, "### {name}");
        for g in gs {
            let a = &g.attribution;
            let (plus, minus) = g
                .hunks
                .iter()
                .fold((0, 0), |(p, m), h| (p + row_of[h.as_str()].added, m + row_of[h.as_str()].removed));
            let ws: Vec<&str> = g.hunks.iter().filter(|h| row_of[h.as_str()].whitespace_only).map(String::as_str).collect();
            let ws = if ws.is_empty() { String::new() } else { format!(" — whitespace only: {}", id_ranges(&ws)) };
            let weak = if g.weak.is_empty() { String::new() } else { format!(" — weak: {}", g.weak.join(" ")) };
            let _ = writeln!(out, "- **{}** {} — {} hunks, +{plus} −{minus}{weak}{ws}", g.id, time_of(a), g.hunks.len());
            if let Some(p) = &a.prompt {
                let _ = writeln!(out, "  P: {}", prompt_text(p));
            }
            if let Some(r) = &a.response {
                let _ = writeln!(out, "  R: {}", short(r, 220));
            }
            let mut per_file: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for h in &g.hunks {
                per_file.entry(row_of[h.as_str()].file.as_str()).or_default().push(h);
            }
            let files: Vec<String> = per_file.iter().map(|(f, hs)| format!("{f} {}", id_ranges(hs))).collect();
            let _ = writeln!(out, "  Files: {}", files.join("; "));
            // Links: hunks of this group that lines of another group also match.
            let mut links: Vec<String> = Vec::new();
            for h in &g.hunks {
                let Some(b) = &row_of[h.as_str()].blame else { continue };
                for o in &b.others {
                    if o.matched_lines >= MIN_LINK_LINES
                        && let Some(&gi) = by_key.get(&key_of(o))
                    {
                        links.push(format!("{h} also matches {} ({} lines)", group_ids[gi], o.matched_lines));
                    }
                }
            }
            if !links.is_empty() {
                let _ = writeln!(out, "  Links: {}", links.join("; "));
            }
        }
        let _ = writeln!(out);
    }

    if !unattributed.is_empty() {
        let _ = writeln!(
            out,
            "### Without attribution — {} hunks (`{}/groups/unattributed.md`)",
            unattributed.len(),
            args.out.display()
        );
        let mut per_file: BTreeMap<&str, Vec<&HunkRow>> = BTreeMap::new();
        for r in &unattributed {
            per_file.entry(r.file.as_str()).or_default().push(r);
        }
        for (file, rs) in per_file {
            let ids: Vec<&str> = rs.iter().map(|r| r.id.as_str()).collect();
            let plus: usize = rs.iter().map(|r| r.added).sum();
            let minus: usize = rs.iter().map(|r| r.removed).sum();
            let mut maybe: Vec<String> = Vec::new();
            for r in &rs {
                if let Some(b) = &r.blame
                    && let Some(o) = b.others.first()
                    && o.matched_lines >= MIN_LINK_LINES
                    && let Some(&gi) = by_key.get(&key_of(o))
                {
                    maybe.push(format!("{} maybe {} ({}/{} lines)", r.id, group_ids[gi], o.matched_lines, b.voting_lines));
                }
            }
            let maybe = if maybe.is_empty() { String::new() } else { format!(" — {}", maybe.join(", ")) };
            let ws: Vec<&str> = rs.iter().filter(|r| r.whitespace_only).map(|r| r.id.as_str()).collect();
            let ws = if ws.is_empty() {
                String::new()
            } else if ws.len() == rs.len() {
                " — all whitespace only".to_string()
            } else {
                format!(" — whitespace only: {}", id_ranges(&ws))
            };
            let _ = writeln!(out, "- {file}: {} (+{plus} −{minus}){maybe}{ws}", id_ranges(&ids));
        }
    }
    out
}

/// The WIP lines that the report shows for one file. The plan holds all.
const MAX_WIP_PER_FILE: usize = 3;

/// The prompt for the report. A skill load and a context summary are
/// recorded as prompts, but their text does not say what the user asked.
fn prompt_text(p: &str) -> String {
    if p.starts_with("Base directory for this skill:") {
        "(the text of a skill; read R)".to_string()
    } else if p.starts_with("This session is being continued from a previous conversation") {
        "(a summary of an earlier context; read R)".to_string()
    } else {
        short(p, 180)
    }
}

/// `H3-H5 H9` for `H3 H4 H5 H9`.
fn id_ranges(ids: &[&str]) -> String {
    let nums: Vec<usize> = ids.iter().filter_map(|i| i.get(1..)?.parse().ok()).collect();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < nums.len() {
        let mut j = i;
        while j + 1 < nums.len() && nums[j + 1] == nums[j] + 1 {
            j += 1;
        }
        if j > i {
            parts.push(format!("H{}-H{}", nums[i], nums[j]));
        } else {
            parts.push(format!("H{}", nums[i]));
        }
        i = j + 1;
    }
    parts.join(" ")
}

/// A reason to think that an untracked file is not part of the work.
fn junk_hint(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    [
        (".orig", "a merge leftover"),
        (".rej", "a patch leftover"),
        (".snap.new", "a snapshot that a test wrote and nobody accepted"),
        (".bak", "a backup"),
        (".log", "a log"),
        ("~", "an editor backup"),
    ]
    .into_iter()
    .find(|(suffix, _)| name.ends_with(suffix))
    .map(|(_, why)| why)
    .or_else(|| {
        ["deleteme", "scratch", "tmp", "dryrun"]
            .into_iter()
            .any(|w| name.contains(w))
            .then_some("a scratch name")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(rel: &str) -> Repo {
        Repo {
            rel: rel.to_string(),
            abs: PathBuf::from("/r").join(rel),
            submodules: Vec::new(),
        }
    }

    #[test]
    fn id_ranges_join_consecutive_ids() {
        assert_eq!(id_ranges(&["H3", "H4", "H5", "H9", "H11", "H12"]), "H3-H5 H9 H11-H12");
        assert_eq!(id_ranges(&[]), "");
    }

    #[test]
    fn pathspecs_map_the_scope_into_each_repository() {
        let scope = vec!["gaze/".to_string(), "ide".to_string()];
        assert_eq!(pathspecs(&repo(""), &scope), Some(vec!["gaze".to_string(), "ide".to_string()]));
        assert_eq!(pathspecs(&repo("gaze/egui"), &scope), Some(vec![]));
        assert_eq!(pathspecs(&repo("api"), &scope), None);
        let deep = vec!["gaze/egui/crates".to_string()];
        assert_eq!(pathspecs(&repo("gaze/egui"), &deep), Some(vec!["crates".to_string()]));
        assert_eq!(pathspecs(&repo("gaze/dioxus"), &deep), None);
        assert_eq!(pathspecs(&repo("x"), &[]), Some(vec![]));
    }
}
