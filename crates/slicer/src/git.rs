//! Small wrappers around the `git` and `gh` commands.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Run `git` in `dir` and return its stdout. A non-zero exit is an error
/// that holds the stderr of git.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("cannot run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim_end()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `git` in `dir` and return its trimmed stdout, or `None` when git
/// fails. For a question whose failure is an answer, such as "is there an
/// upstream branch".
pub fn git_opt(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).ok().map(|s| s.trim_end().to_string())
}

/// The diff of a new, untracked file: `git diff --no-index` exits with 1
/// when the files differ, which is the normal case here.
pub fn new_file_diff(dir: &Path, file: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null", file])
        .stdin(Stdio::null())
        .output()
        .context("cannot run git")?;
    match out.status.code() {
        Some(0) | Some(1) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => bail!(
            "git diff --no-index failed for {file}: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        ),
    }
}

/// Run `git apply` in `dir` with `patch` on stdin.
pub fn apply(dir: &Path, args: &[&str], patch: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("apply")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run git apply")?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(patch.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim_end());
    }
    Ok(())
}

/// The root of the repository that holds the current directory.
pub fn toplevel() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let root = git(&cwd, &["rev-parse", "--show-toplevel"])
        .context("slicer must run inside a git repository")?;
    Ok(PathBuf::from(root.trim_end()))
}

/// The default plan directory: `slice` in the git directory of the outermost
/// repository. A command that runs in a submodule finds the same plan. A
/// process in a sandbox can also write there, unlike a temporary directory
/// that differs between processes.
pub fn default_plan_dir() -> Result<PathBuf> {
    let mut dir = toplevel()?;
    while let Some(parent) = git_opt(&dir, &["rev-parse", "--show-superproject-working-tree"]).filter(|s| !s.is_empty()) {
        dir = PathBuf::from(parent);
    }
    let git_dir = git(&dir, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(git_dir.trim_end()).join("slice"))
}

/// Run `gh` in `dir` and return its stdout, or the error text.
pub fn gh(dir: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let out = Command::new("gh")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run gh: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim_end().to_string())
    }
}

/// The owner part of a GitHub remote URL: `membrane-io` for
/// `git@github.com:membrane-io/mnode.git` or
/// `https://github.com/membrane-io/mnode`.
pub fn remote_owner(url: &str) -> Option<String> {
    let rest = url
        .split_once("github.com:")
        .or_else(|| url.split_once("github.com/"))
        .map(|(_, rest)| rest)?;
    let owner = rest.split('/').next()?;
    (!owner.is_empty()).then(|| owner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_owner_reads_ssh_and_https_urls() {
        assert_eq!(remote_owner("git@github.com:membrane-io/mnode.git").as_deref(), Some("membrane-io"));
        assert_eq!(remote_owner("https://github.com/juancampa/egui").as_deref(), Some("juancampa"));
        assert_eq!(remote_owner("/local/path"), None);
    }
}
