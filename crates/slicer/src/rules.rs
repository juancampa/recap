//! The path rules (ignore globs, secret names) and the WIP markers.

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// A glob that `--ignore` takes. `*` matches inside one path part, `**`
/// matches any number of parts, and `?` matches one character. A glob
/// without a `/` matches the file name in any directory, as in `.gitignore`.
pub struct Glob {
    regex: Regex,
    name_only: bool,
}

impl Glob {
    pub fn new(glob: &str) -> Result<Glob> {
        let mut re = String::from("^");
        let mut chars = glob.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' if chars.peek() == Some(&'*') => {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                }
                '*' => re.push_str("[^/]*"),
                '?' => re.push_str("[^/]"),
                c => re.push_str(&regex::escape(&c.to_string())),
            }
        }
        re.push('$');
        Ok(Glob {
            regex: Regex::new(&re).with_context(|| format!("bad glob {glob}"))?,
            name_only: !glob.contains('/'),
        })
    }

    /// `path` is relative to the root repository.
    pub fn matches(&self, path: &str) -> bool {
        if self.name_only {
            let name = path.rsplit('/').next().unwrap_or(path);
            self.regex.is_match(name)
        } else {
            self.regex.is_match(path)
        }
    }
}

/// True when the file name looks like a file that holds a secret.
pub fn looks_secret(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || name.contains("credential")
        || name.contains("secret")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
}

/// One WIP marker rule.
pub struct Marker {
    pub name: String,
    pub regex: Regex,
}

/// The built-in markers. Each rule tests the text of one added line.
const BUILT_IN: &[(&str, &str)] = &[
    (
        "commented-out log",
        r"^\s*(//|#|--)\s*(log::|println!|eprintln!|print!|dbg!|console\.(log|debug|warn)|tracing::|(info|debug|warn|trace)!)",
    ),
    ("dbg!", r"\bdbg!\("),
    ("unfinished", r"\b(unimplemented!|todo!)\("),
    ("TODO/FIXME", r"\b(TODO|FIXME|XXX|DELETEME)\b"),
    ("home path", r"(/Users/|/home/)[A-Za-z0-9._-]+/"),
    ("forced branch", r"\bif (false|true)\b|&& false\b|\|\| true\b"),
];

pub fn markers(extra: &[String]) -> Result<Vec<Marker>> {
    let mut out = Vec::new();
    for (name, re) in BUILT_IN {
        out.push(Marker {
            name: name.to_string(),
            regex: Regex::new(re).expect("built-in regex is valid"),
        });
    }
    for re in extra {
        out.push(Marker {
            name: format!("--wip {re}"),
            regex: Regex::new(re).with_context(|| format!("bad --wip regex {re}"))?,
        });
    }
    Ok(out)
}

/// One added line that a marker matched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WipHit {
    pub hunk: String,
    /// The path relative to the root repository.
    pub file: String,
    pub line: usize,
    pub marker: String,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_gitignore() {
        let g = Glob::new("specs/**").unwrap();
        assert!(g.matches("specs/a/b.md"));
        assert!(!g.matches("gaze/specs/a.md"));
        let g = Glob::new("**/*.local.*").unwrap();
        assert!(g.matches("a.local.json"));
        assert!(g.matches("x/y/a.local.json"));
        let g = Glob::new("*.patch").unwrap();
        assert!(g.matches("deep/dir/fix.patch"));
        assert!(!g.matches("fix.patch.txt"));
    }

    #[test]
    fn markers_find_wip_lines() {
        let m = markers(&["debug_rect".to_string()]).unwrap();
        let hit = |line: &str| m.iter().find(|m| m.regex.is_match(line)).map(|m| m.name.clone());
        assert_eq!(hit("    // log::info!(\"x\");").as_deref(), Some("commented-out log"));
        assert_eq!(hit("    let p = \"/Users/juan/x\";").as_deref(), Some("home path"));
        assert_eq!(hit("    ui.debug_rect(r);").as_deref(), Some("--wip debug_rect"));
        assert_eq!(hit("    let x = 1;"), None);
    }
}
