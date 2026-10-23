//! A parser for the unified diff format that `git diff` writes, and the
//! functions that write the parts of a diff back as a patch.
//!
//! The recap server parses a diff to attribute its hunks, and slicer parses
//! the same diff to stage the hunks. Both use this crate, so a hunk index
//! means the same hunk in both programs.

use std::path::PathBuf;

/// One file section of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The path relative to the repository root. The new path for a rename,
    /// the old path for a deleted file.
    pub path: PathBuf,
    /// The lines before the first hunk: `diff --git`, `index`, `---`, `+++`.
    pub header: Vec<String>,
    pub hunks: Vec<Hunk>,
}

/// One `@@` hunk of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@` line.
    pub header: String,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// The body lines with their ` `, `+`, `-` or `\` prefix.
    pub body: Vec<String>,
}

impl FileDiff {
    /// The file header plus these hunks, as a patch that `git apply` reads.
    /// The hunks must be in file order.
    pub fn patch<'a>(&self, hunks: impl IntoIterator<Item = &'a Hunk>) -> String {
        let mut out = String::new();
        for line in &self.header {
            out.push_str(line);
            out.push('\n');
        }
        for hunk in hunks {
            out.push_str(&hunk.header);
            out.push('\n');
            for line in &hunk.body {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

impl Hunk {
    /// The file header plus this one hunk, as a patch that `git apply` reads.
    pub fn patch(&self, file: &FileDiff) -> String {
        file.patch([self])
    }

    /// The `+` lines with their line numbers in the new file.
    pub fn added_lines(&self) -> Vec<(usize, &str)> {
        let mut out = Vec::new();
        let mut line_no = self.new_start;
        for line in &self.body {
            match line.chars().next() {
                Some('+') => {
                    out.push((line_no, &line[1..]));
                    line_no += 1;
                }
                Some(' ') => line_no += 1,
                _ => {}
            }
        }
        out
    }

    /// True when the hunk changes only whitespace: the `-` lines and the `+`
    /// lines hold the same characters after the removal of all whitespace.
    pub fn whitespace_only(&self) -> bool {
        let text = |prefix: char| -> String {
            self.body
                .iter()
                .filter(|l| l.starts_with(prefix))
                .flat_map(|l| l[1..].chars().filter(|c| !c.is_whitespace()))
                .collect()
        };
        let changed = self.body.iter().any(|l| l.starts_with('+') || l.starts_with('-'));
        changed && text('-') == text('+')
    }

    /// This hunk without the changed lines for which `drop` returns true.
    /// `drop` receives the line text without its `+` or `-` prefix. A dropped
    /// `+` line leaves the hunk, and a dropped `-` line becomes a context
    /// line, because the file then keeps that line. The `@@` line gets the
    /// new line counts. `None` when no `+` or `-` line stays: `git apply`
    /// rejects such a hunk as a corrupt patch.
    pub fn without_lines(&self, drop: impl Fn(&str) -> bool) -> Option<Hunk> {
        let mut body: Vec<String> = Vec::with_capacity(self.body.len());
        // A `\ No newline at end of file` line belongs to the line in front
        // of it, so it leaves the hunk with a dropped `+` line.
        let mut skip_marker = false;
        for line in &self.body {
            match line.chars().next() {
                Some('+') if drop(&line[1..]) => skip_marker = true,
                Some('-') if drop(&line[1..]) => {
                    body.push(format!(" {}", &line[1..]));
                    skip_marker = false;
                }
                Some('\\') if skip_marker => skip_marker = false,
                _ => {
                    body.push(line.clone());
                    skip_marker = false;
                }
            }
        }
        if !body.iter().any(|l| l.starts_with('+') || l.starts_with('-')) {
            return None;
        }
        let count = |prefixes: [char; 2]| {
            body.iter()
                .filter(|l| l.chars().next().is_some_and(|c| prefixes.contains(&c)))
                .count()
        };
        let old_lines = count([' ', '-']);
        let new_lines = count([' ', '+']);
        // Keep the text after the second `@@`: git writes the name of the
        // enclosing function there.
        let section = self
            .header
            .get(2..)
            .and_then(|rest| rest.find("@@"))
            .map(|i| &self.header[i + 4..])
            .unwrap_or("");
        Some(Hunk {
            header: format!(
                "@@ -{},{old_lines} +{},{new_lines} @@{section}",
                self.old_start, self.new_start
            ),
            old_start: self.old_start,
            old_lines,
            new_start: self.new_start,
            new_lines,
            body,
        })
    }
}

/// Parse the text of a unified diff. Binary files are skipped.
pub fn parse_unified(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut file: Option<FileDiff> = None;
    let mut hunk: Option<Hunk> = None;
    let mut binary = false;
    let mut old_path: Option<PathBuf> = None;
    let mut new_path: Option<PathBuf> = None;

    // Close the current file and push it, unless it is binary.
    fn flush(
        files: &mut Vec<FileDiff>,
        file: &mut Option<FileDiff>,
        hunk: &mut Option<Hunk>,
        binary: &mut bool,
        old_path: &mut Option<PathBuf>,
        new_path: &mut Option<PathBuf>,
    ) {
        if let Some(mut f) = file.take() {
            if let Some(h) = hunk.take() {
                f.hunks.push(h);
            }
            if let Some(p) = new_path.take().or_else(|| old_path.take()) {
                f.path = p;
            }
            if !*binary && !f.hunks.is_empty() {
                files.push(f);
            }
        }
        *binary = false;
        *old_path = None;
        *new_path = None;
    }

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(
                &mut files, &mut file, &mut hunk, &mut binary, &mut old_path, &mut new_path,
            );
            file = Some(FileDiff {
                path: git_header_path(rest),
                header: vec![line.to_string()],
                hunks: Vec::new(),
            });
            continue;
        }
        if file.is_none() {
            // A diff without `diff --git` lines starts at `---`.
            if line.starts_with("--- ") {
                file = Some(FileDiff {
                    path: PathBuf::new(),
                    header: Vec::new(),
                    hunks: Vec::new(),
                });
            } else {
                continue;
            }
        }
        let f = file.as_mut().expect("file is set");

        if line.starts_with("@@ ") {
            if let Some(h) = hunk.take() {
                f.hunks.push(h);
            }
            hunk = parse_hunk_header(line);
            continue;
        }

        match hunk.as_mut() {
            Some(h) => {
                let prefix = line.chars().next();
                match prefix {
                    Some(' ') | Some('+') | Some('-') | Some('\\') => h.body.push(line.to_string()),
                    // Some tools print an empty context line with no space.
                    None => h.body.push(" ".to_string()),
                    _ => {
                        // Not a hunk line: the file ends here.
                        f.hunks.push(hunk.take().expect("hunk is set"));
                        f.header.push(line.to_string());
                    }
                }
            }
            None => {
                if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
                    binary = true;
                }
                if let Some(p) = line.strip_prefix("--- ") {
                    old_path = header_path(p);
                }
                if let Some(p) = line.strip_prefix("+++ ") {
                    new_path = header_path(p);
                }
                f.header.push(line.to_string());
            }
        }
    }
    flush(
        &mut files, &mut file, &mut hunk, &mut binary, &mut old_path, &mut new_path,
    );
    files
}

/// The new path of a `diff --git a/<old> b/<new>` line.
fn git_header_path(rest: &str) -> PathBuf {
    match rest.find(" b/") {
        Some(i) => PathBuf::from(&rest[i + 3..]),
        None => PathBuf::from(rest),
    }
}

/// The path of a `--- a/<path>` or `+++ b/<path>` line. `None` for `/dev/null`.
fn header_path(p: &str) -> Option<PathBuf> {
    let p = p.split('\t').next().unwrap_or(p);
    if p == "/dev/null" {
        return None;
    }
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    Some(PathBuf::from(p))
}

/// Parse `@@ -a[,b] +c[,d] @@ ...`. A missing count means 1.
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let mut parts = line.split(' ');
    parts.next()?; // "@@"
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_lines) = parse_range(old)?;
    let (new_start, new_lines) = parse_range(new)?;
    Some(Hunk {
        header: line.to_string(),
        old_start,
        old_lines,
        new_start,
        new_lines,
        body: Vec::new(),
    })
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FILES: &str = "\
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index 1111111..2222222 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!(\"hello\");
     let x = 1;
 }
@@ -10 +11 @@
-old line
+new line
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3333333..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-first
-second
diff --git a/img.png b/img.png
index 4444444..5555555 100644
Binary files a/img.png and b/img.png differ
";

    #[test]
    fn parses_rename_delete_and_skips_binary() {
        let files = parse_unified(TWO_FILES);
        assert_eq!(files.len(), 2);

        let f = &files[0];
        assert_eq!(f.path, PathBuf::from("src/new.rs"));
        assert_eq!(f.header.len(), 7);
        assert_eq!(f.hunks.len(), 2);
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (1, 3, 1, 4));
        assert_eq!(h.body.len(), 4);
        let h = &f.hunks[1];
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (10, 1, 11, 1));
        assert_eq!(h.body, vec!["-old line", "+new line", "\\ No newline at end of file"]);

        let g = &files[1];
        assert_eq!(g.path, PathBuf::from("gone.txt"));
        assert_eq!(g.hunks.len(), 1);
        assert_eq!(g.hunks[0].body, vec!["-first", "-second"]);
    }

    #[test]
    fn patch_holds_the_header_and_one_hunk() {
        let files = parse_unified(TWO_FILES);
        let patch = files[0].hunks[1].patch(&files[0]);
        let expected = "\
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index 1111111..2222222 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -10 +11 @@
-old line
+new line
\\ No newline at end of file
";
        assert_eq!(patch, expected);
    }

    #[test]
    fn a_patch_with_two_hunks_has_one_header() {
        let files = parse_unified(TWO_FILES);
        let patch = files[0].patch(&files[0].hunks);
        assert_eq!(patch.matches("diff --git").count(), 1);
        assert_eq!(patch.matches("\n@@ ").count(), 2);
    }

    #[test]
    fn added_lines_have_new_file_line_numbers() {
        let files = parse_unified(TWO_FILES);
        assert_eq!(files[0].hunks[0].added_lines(), vec![(2, "    println!(\"hello\");")]);
        assert_eq!(files[0].hunks[1].added_lines(), vec![(11, "new line")]);
    }

    #[test]
    fn without_lines_drops_an_added_line_and_keeps_a_removed_line() {
        let hunk = parse_hunk_header("@@ -5,4 +5,4 @@ fn f() {").map(|mut h| {
            h.body = [" a", "-old", "+new", "+debug();", " b"].map(String::from).to_vec();
            h
        });
        let hunk = hunk.unwrap();
        let only_new = hunk.without_lines(|l| l == "debug();").unwrap();
        assert_eq!(only_new.header, "@@ -5,3 +5,3 @@ fn f() {");
        assert_eq!(only_new.body, [" a", "-old", "+new", " b"]);

        // Dropping the `-` line keeps that line in the file as context.
        let keep_old = hunk.without_lines(|l| l == "old").unwrap();
        assert_eq!(keep_old.header, "@@ -5,3 +5,5 @@ fn f() {");
        assert_eq!(keep_old.body[1], " old");
    }

    #[test]
    fn whitespace_only_ignores_a_new_line_break_but_not_a_new_token() {
        let hunk = |body: &[&str]| Hunk {
            header: "@@ -1 +1 @@".to_string(),
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            body: body.iter().map(|s| s.to_string()).collect(),
        };
        assert!(hunk(&[" a", "-f(x, y);", "+f(", "+    x,", "+    y);"]).whitespace_only());
        assert!(!hunk(&["-f(x, y);", "+f(x, y, z);"]).whitespace_only());
        assert!(!hunk(&[" only context"]).whitespace_only());
    }

    #[test]
    fn without_lines_returns_none_when_no_change_stays() {
        let files = parse_unified(TWO_FILES);
        let h = &files[0].hunks[1];
        assert!(h.without_lines(|l| l == "old line" || l == "new line").is_none());
        // The marker leaves with the dropped `+` line that it belongs to.
        let only_removal = h.without_lines(|l| l == "new line").unwrap();
        assert_eq!(only_removal.body, vec!["-old line"]);
        assert_eq!(only_removal.header, "@@ -10,1 +11,0 @@");
    }
}
