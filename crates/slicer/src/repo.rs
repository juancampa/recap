//! The repository and its submodules, and the state of each submodule.

use crate::git::{git, git_opt, remote_owner};
use std::path::{Path, PathBuf};

/// One git repository: the root repository or an initialized submodule.
#[derive(Debug, Clone)]
pub struct Repo {
    /// The path relative to the root repository. Empty for the root.
    pub rel: String,
    pub abs: PathBuf,
    /// The submodule paths of this repository, relative to this repository.
    /// Only the initialized submodules.
    pub submodules: Vec<String>,
}

impl Repo {
    /// The path of `file` relative to the root repository.
    pub fn root_path(&self, file: &str) -> String {
        if self.rel.is_empty() {
            file.to_string()
        } else {
            format!("{}/{file}", self.rel)
        }
    }
}

/// The root repository and all the initialized submodules, depth first:
/// each submodule comes after its parent.
pub fn all_repos(root: &Path) -> Vec<Repo> {
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out
}

fn walk(abs: &Path, rel: &str, out: &mut Vec<Repo>) {
    let submodules = initialized_submodules(abs);
    out.push(Repo {
        rel: rel.to_string(),
        abs: abs.to_path_buf(),
        submodules: submodules.clone(),
    });
    for sub in &submodules {
        let sub_rel = if rel.is_empty() { sub.clone() } else { format!("{rel}/{sub}") };
        walk(&abs.join(sub), &sub_rel, out);
    }
}

/// The paths of the initialized submodules of the repository at `dir`.
/// `git submodule status` marks a submodule that is not initialized with `-`.
fn initialized_submodules(dir: &Path) -> Vec<String> {
    let Some(status) = git_opt(dir, &["submodule", "status"]) else {
        return Vec::new();
    };
    status
        .lines()
        .filter(|l| !l.starts_with('-'))
        .filter_map(|l| l.get(1..)?.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

/// The facts about one submodule that the plan and the finish reports need.
#[derive(Debug, Clone)]
pub struct SubmoduleState {
    pub rel: String,
    /// The branch, or `None` when HEAD is detached.
    pub branch: Option<String>,
    /// The `branch =` value of the parent `.gitmodules`.
    pub gitmodules_branch: Option<String>,
    pub origin: Option<String>,
    /// The number of `git status --porcelain` lines.
    pub dirty: usize,
    /// True when HEAD differs from the commit that the parent HEAD records.
    pub pointer_moved: bool,
    /// The commit that the parent HEAD records for this submodule.
    pub recorded: Option<String>,
    pub head: Option<String>,
    /// `None` when the branch has no upstream.
    pub unpushed: Option<usize>,
}

impl SubmoduleState {
    pub fn owner(&self) -> Option<String> {
        self.origin.as_deref().and_then(remote_owner)
    }

    /// True when the origin of this submodule belongs to one of `owners`.
    /// Without `owners`, every origin is accepted.
    pub fn owned(&self, owners: &[String]) -> bool {
        owners.is_empty() || self.owner().is_some_and(|o| owners.contains(&o))
    }

    /// The branch to create when HEAD is detached: the `.gitmodules` value,
    /// then the branch of the parent, then `fallback`.
    pub fn suggested_branch(&self, parent_branch: Option<&str>, fallback: Option<&str>) -> Option<String> {
        self.gitmodules_branch
            .clone()
            .or_else(|| parent_branch.map(str::to_string))
            .or_else(|| fallback.map(str::to_string))
    }
}

/// The state of the submodule `repo`, whose parent repository is `parent`.
pub fn submodule_state(parent: &Repo, repo: &Repo) -> SubmoduleState {
    let dir = &repo.abs;
    let sub_in_parent = repo
        .rel
        .strip_prefix(&parent.rel)
        .map(|s| s.trim_start_matches('/'))
        .unwrap_or(&repo.rel)
        .to_string();
    let branch = git_opt(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");
    let gitmodules_branch = gitmodules_name(&parent.abs, &sub_in_parent).and_then(|name| {
        git_opt(&parent.abs, &["config", "-f", ".gitmodules", "--get", &format!("submodule.{name}.branch")])
    });
    let recorded = git_opt(&parent.abs, &["ls-tree", "HEAD", "--", &sub_in_parent])
        .and_then(|l| l.split_whitespace().nth(2).map(str::to_string));
    let head = git_opt(dir, &["rev-parse", "HEAD"]);
    let dirty = git(dir, &["status", "--porcelain"])
        .map(|s| s.lines().count())
        .unwrap_or(0);
    let unpushed = git_opt(dir, &["rev-list", "--count", "@{upstream}..HEAD"]).and_then(|n| n.parse().ok());
    SubmoduleState {
        rel: repo.rel.clone(),
        branch,
        gitmodules_branch,
        origin: git_opt(dir, &["remote", "get-url", "origin"]),
        dirty,
        pointer_moved: recorded.is_some() && recorded != head,
        recorded,
        head,
        unpushed,
    }
}

/// The name of the submodule at `path` in the `.gitmodules` of `parent`.
/// The name often equals the path, but not always.
fn gitmodules_name(parent: &Path, path: &str) -> Option<String> {
    let entries = git_opt(parent, &["config", "-f", ".gitmodules", "--get-regexp", r"^submodule\..*\.path$"])?;
    entries.lines().find_map(|l| {
        let (key, value) = l.split_once(' ')?;
        (value == path).then(|| {
            key.trim_start_matches("submodule.")
                .trim_end_matches(".path")
                .to_string()
        })
    })
}

/// True when a remote-tracking branch of the repository at `dir` contains
/// `commit`. This reads the local remote refs; fetch first for a current
/// answer.
pub fn on_remote(dir: &Path, commit: &str) -> bool {
    git_opt(dir, &["branch", "-r", "--contains", commit]).is_some_and(|s| !s.trim().is_empty())
}

/// The parent of `repo` in `repos`: the repository with the longest `rel`
/// that is a prefix of the `rel` of `repo`.
pub fn parent_of<'a>(repos: &'a [Repo], repo: &Repo) -> Option<&'a Repo> {
    repos
        .iter()
        .filter(|p| {
            p.rel != repo.rel
                && (p.rel.is_empty() || repo.rel.starts_with(&format!("{}/", p.rel)))
                && p.submodules.iter().any(|s| p.root_path(s) == repo.rel)
        })
        .max_by_key(|p| p.rel.len())
}
