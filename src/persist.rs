//! On-disk persistence for UI state that should survive restarts: the
//! selected project / file (stored by path, not index, since the projects
//! list re-sorts as async load batches arrive) and the display toggles.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    /// Project root of the selected project, or `None` for "All projects".
    pub selected_project: Option<PathBuf>,
    /// Path of the selected file, or `None` for "All files".
    pub selected_file: Option<PathBuf>,
    pub absolute_time: bool,
    pub auto_expand: bool,
    /// Max number of visual lines a prompt entry may occupy in the History
    /// list (the `>`/`<` toggle). Defaults to 5.
    pub prompt_lines: usize,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            selected_project: None,
            selected_file: None,
            absolute_time: false,
            auto_expand: false,
            prompt_lines: 5,
        }
    }
}

fn state_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("recap").join("state.json"))
}

impl PersistedState {
    /// Read persisted state. Any failure (missing file, parse error) yields
    /// defaults so a corrupt file never blocks startup.
    pub fn load() -> Self {
        let Some(path) = state_path() else {
            return Self::default();
        };
        let Ok(data) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    /// Best-effort write; errors are ignored since failing to persist UI
    /// state should never abort a clean exit.
    pub fn save(&self) {
        let Some(path) = state_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }
}
