//! The format version of the cache directory, the migrations between the
//! versions, and the test cache.
//!
//! The file `<cache root>/cache-version` holds one integer. If the file is
//! missing, the version is 1. At start, the server runs each migration from
//! the version in the file up to [`CACHE_VERSION`]. A change to a cache
//! format must add a migration. It must not delete the cache, because a cold
//! load and the embedding index need more than 30 minutes.
//!
//! `recap serve --test` never uses the prod cache. At each start,
//! [`prepare_test_cache`] copies the prod cache to the test cache root and
//! migrates the copy. Thus each start of a test server also tests the
//! migrations from the prod format.

use anyhow::{Context, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The cache format that this binary reads and writes.
pub const CACHE_VERSION: u32 = 2;

/// The name of the version file in the cache root.
const VERSION_FILE: &str = "cache-version";

/// The entries of the prod cache that the test cache does not copy. The
/// test server uses the model server and the model files of the prod cache.
/// `recap.log` is the log file of older versions. `embeddings.db` gets a
/// separate copy (see [`copy_cache`]).
const NOT_COPIED: &[&str] = &["llamacpp", "models", "recap.log", "embeddings.db"];

/// Migration N changes the cache from version N to version N + 1. The
/// argument is the cache root.
type Migration = fn(&Path) -> anyhow::Result<()>;

const MIGRATIONS: &[Migration] = &[relative_cache_paths];

/// Write `data` to `path` through a temporary file in the same directory.
/// A crash or a copy of the directory never sees a part of the file.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// The cache version of `root`.
pub fn version(root: &Path) -> anyhow::Result<u32> {
    match fs::read_to_string(root.join(VERSION_FILE)) {
        Ok(text) => text.trim().parse().with_context(|| {
            format!(
                "{} is not a version number",
                root.join(VERSION_FILE).display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(1),
        Err(e) => Err(e.into()),
    }
}

/// Migrate the cache at `root` to [`CACHE_VERSION`]. The function writes the
/// version file after each migration, so a failed migration runs again at
/// the next start. It fails if the cache is newer than this binary.
pub fn migrate(root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root)?;
    let mut current = version(root)?;
    if current > CACHE_VERSION {
        bail!(
            "the cache at {} has version {current}, but this binary reads version {CACHE_VERSION}. \
             Install a newer binary.",
            root.display()
        );
    }
    while current < CACHE_VERSION {
        let migration = MIGRATIONS[current as usize - 1];
        migration(root).with_context(|| {
            format!("the migration of {} from version {current}", root.display())
        })?;
        current += 1;
        write_atomic(&root.join(VERSION_FILE), format!("{current}\n").as_bytes())?;
        log::info!(
            "migrated the cache at {} to version {current}",
            root.display()
        );
    }
    Ok(())
}

/// Replace the test cache with a copy of the prod cache, migrate the copy,
/// and make it the cache root of this process.
pub fn prepare_test_cache() -> anyhow::Result<()> {
    let prod = crate::paths::prod_cache_root();
    let test = crate::paths::test_cache_root();
    let started = std::time::Instant::now();
    copy_cache(&prod, &test)?;
    log::info!(
        "copied the prod cache {} to {} in {}",
        prod.display(),
        test.display(),
        crate::logger::ms(started.elapsed())
    );
    migrate(&test)?;
    crate::paths::set_cache_root(test)
}

/// Copy the cache at `from` to `to`. The function deletes `to` first.
///
/// The snapshot directories and `sessions.json` are copied with `cp -c`,
/// which makes an APFS clone: the copy is fast and uses no extra disk space.
/// The loaders write each file atomically, so the copy holds no partial
/// file. The prod server can write to `embeddings.db` during the copy, so
/// the function copies it with `VACUUM INTO`, which reads one consistent
/// state of the database.
pub fn copy_cache(from: &Path, to: &Path) -> anyhow::Result<()> {
    if to.exists() {
        fs::remove_dir_all(to).with_context(|| format!("cannot delete {}", to.display()))?;
    }
    fs::create_dir_all(to)?;
    if !from.exists() {
        return Ok(());
    }
    for item in fs::read_dir(from)? {
        let item = item?;
        let name = item.file_name();
        if NOT_COPIED.iter().any(|n| name == *n) {
            continue;
        }
        let status = Command::new("cp")
            .arg("-cR")
            .arg(item.path())
            .arg(to.join(&name))
            .status()
            .context("cannot run cp")?;
        if !status.success() {
            bail!("cp -cR {} failed", item.path().display());
        }
    }
    let db = from.join("embeddings.db");
    if db.exists() {
        let conn =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let target = to.join("embeddings.db");
        conn.execute("VACUUM INTO ?1", [target.to_string_lossy()])
            .context("cannot copy embeddings.db")?;
    }
    Ok(())
}

/// Version 1 → 2: remove the cache root from the paths that the cache
/// stores, so a copy of the cache at another location stays valid.
///
/// - `embeddings.db`: the keys of the vectors (see [`crate::store::entry_key`]).
/// - `cursor/sessions.json`: the `snapshot_path` and `base_path` of each
///   cached entry.
///
/// A version 1 cache always holds the paths of the prod cache, also in a
/// copy of it, so the function removes the prod root.
fn relative_cache_paths(root: &Path) -> anyhow::Result<()> {
    let prefix = format!("{}/", crate::paths::prod_cache_root().display());

    let db = root.join("embeddings.db");
    if db.exists() {
        let conn = rusqlite::Connection::open(&db)?;
        let has_table: bool = conn.query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'embedding'",
            [],
            |r| r.get(0),
        )?;
        if has_table {
            conn.execute(
                "UPDATE OR REPLACE embedding SET key = replace(key, ?1, '') WHERE instr(key, ?1) > 0",
                [&prefix],
            )?;
        }
    }

    let sessions = root.join("cursor").join("sessions.json");
    if sessions.exists() {
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&sessions)?)
            .with_context(|| format!("cannot read {}", sessions.display()))?;
        let strip = |v: &mut serde_json::Value| {
            if let Some(s) = v.as_str()
                && let Some(rest) = s.strip_prefix(&prefix)
            {
                *v = serde_json::Value::String(rest.to_string());
            }
        };
        if let Some(composers) = value.get_mut("composers").and_then(|c| c.as_object_mut()) {
            for composer in composers.values_mut() {
                for list in ["prompts", "edits"] {
                    let Some(entries) = composer.get_mut(list).and_then(|l| l.as_array_mut())
                    else {
                        continue;
                    };
                    for entry in entries {
                        for field in ["snapshot_path", "base_path"] {
                            if let Some(v) = entry.get_mut(field) {
                                strip(v);
                            }
                        }
                    }
                }
            }
        }
        value["version"] = serde_json::Value::from(2);
        write_atomic(&sessions, &serde_json::to_vec(&value)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("recap-cache-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A version 1 cache with one vector key and one Cursor entry that hold
    /// absolute paths into the prod cache.
    fn version_1_fixture(root: &Path) {
        let prod = crate::paths::prod_cache_root();
        let snapshot = prod.join("cursor").join("aa");
        let base = prod.join("cursor").join("bb");
        fs::create_dir_all(root.join("cursor")).unwrap();
        let sessions = serde_json::json!({
            "version": 1,
            "composers": {
                "c1": {
                    "bubble_count": 2, "created": null, "updated": null, "name": null,
                    "files": [],
                    "prompts": [{
                        "ts_ms": 1, "file_path": "cursor:c1:b1", "snapshot_path": "",
                        "base_path": null, "project_roots": [], "prompt": ["hi", null],
                        "session_id": "c1", "session_cwd": null
                    }],
                    "edits": [{
                        "ts_ms": 2, "file_path": "/repo/a.rs", "snapshot_path": snapshot,
                        "base_path": base, "project_roots": ["/repo"], "prompt": null,
                        "session_id": "c1", "session_cwd": null
                    }]
                }
            }
        });
        fs::write(root.join("cursor/sessions.json"), sessions.to_string()).unwrap();

        let conn = rusqlite::Connection::open(root.join("embeddings.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE embedding (key TEXT PRIMARY KEY, kind INTEGER NOT NULL, vec BLOB NOT NULL);",
        )
        .unwrap();
        let key = format!("{}::{}", base.display(), snapshot.display());
        conn.execute("INSERT INTO embedding VALUES (?1, 1, x'00')", [&key])
            .unwrap();
        conn.execute("INSERT INTO embedding VALUES ('claude:s:u', 0, x'00')", [])
            .unwrap();
    }

    /// The migrations change a version 1 cache to the current version. The
    /// cached paths and the vector keys are then relative to the cache root.
    #[test]
    fn the_migrations_make_the_cached_paths_relative() {
        let root = temp_dir("migrate");
        version_1_fixture(&root);

        migrate(&root).unwrap();

        assert_eq!(version(&root).unwrap(), CACHE_VERSION);
        let conn = rusqlite::Connection::open(root.join("embeddings.db")).unwrap();
        let mut keys: Vec<String> = conn
            .prepare("SELECT key FROM embedding ORDER BY key")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["claude:s:u".to_string(), "cursor/bb::cursor/aa".to_string()]
        );

        let sessions: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("cursor/sessions.json")).unwrap()).unwrap();
        assert_eq!(sessions["version"], 2);
        let edit = &sessions["composers"]["c1"]["edits"][0];
        assert_eq!(edit["snapshot_path"], "cursor/aa");
        assert_eq!(edit["base_path"], "cursor/bb");
        assert_eq!(
            sessions["composers"]["c1"]["prompts"][0]["snapshot_path"],
            ""
        );

        // A second start finds the current version and changes nothing.
        migrate(&root).unwrap();
        assert_eq!(version(&root).unwrap(), CACHE_VERSION);
        let _ = fs::remove_dir_all(&root);
    }

    /// The server refuses a cache that a newer binary wrote.
    #[test]
    fn a_newer_cache_stops_the_migration() {
        let root = temp_dir("newer");
        fs::write(root.join(VERSION_FILE), format!("{}\n", CACHE_VERSION + 1)).unwrap();
        assert!(migrate(&root).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    /// The copy holds the snapshots and the vectors, but not the model files
    /// or the log. It replaces an old copy.
    #[test]
    fn the_copy_skips_the_model_files_and_replaces_the_old_copy() {
        let from = temp_dir("copy-from");
        let to = temp_dir("copy-to");
        version_1_fixture(&from);
        fs::create_dir_all(from.join("claude/s1")).unwrap();
        fs::write(from.join("claude/s1/u1"), "text").unwrap();
        fs::create_dir_all(from.join("models")).unwrap();
        fs::write(from.join("recap.log"), "log").unwrap();
        fs::write(to.join("old"), "old").unwrap();

        copy_cache(&from, &to).unwrap();

        assert_eq!(fs::read_to_string(to.join("claude/s1/u1")).unwrap(), "text");
        assert!(to.join("cursor/sessions.json").exists());
        assert!(to.join("embeddings.db").exists());
        assert!(!to.join("models").exists());
        assert!(!to.join("recap.log").exists());
        assert!(!to.join("old").exists());
        let _ = fs::remove_dir_all(&from);
        let _ = fs::remove_dir_all(&to);
    }
}
