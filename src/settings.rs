//! User settings for the History panel, and the state of the settings menu.
//!
//! The settings control which columns the History panel draws and whether the
//! panel prints a date row before the first entry of each day. The app saves
//! them with the rest of the UI state (see [`crate::persist`]).

use serde::{Deserialize, Serialize};

/// One column of a History row. The panel draws the columns in this order,
/// and it skips a column that the user turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Time,
    Change,
    Source,
    Title,
    Project,
}

/// The columns the History panel draws, plus the date-row option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub show_time: bool,
    pub show_change: bool,
    pub show_source: bool,
    /// The title of the agent session: the Cursor composer name, or the
    /// Claude Code `aiTitle`.
    pub show_title: bool,
    /// The project chain, for example `mnode → gaze`.
    pub show_project: bool,
    /// Pad the project column to one width for every row, so the text after
    /// it starts at the same column on every row. Off: the column is only as
    /// wide as the chain of that row.
    pub align_project: bool,
    /// Draw the pad of the project column as a dotted rule, which leads the
    /// eye from the project chain to the text. Off: the pad is blank.
    pub project_fill: bool,
    /// Print one date row before the first entry of each day. The row names
    /// the day both as a relative time and as a date, so the user can hide
    /// the time column and still know the day of each entry.
    pub date_separator: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_time: true,
            show_change: true,
            show_source: true,
            show_title: true,
            show_project: true,
            align_project: false,
            project_fill: true,
            date_separator: false,
        }
    }
}

impl Settings {
    /// True if the panel draws `column`.
    pub fn shows(&self, column: Column) -> bool {
        match column {
            Column::Time => self.show_time,
            Column::Change => self.show_change,
            Column::Source => self.show_source,
            Column::Title => self.show_title,
            Column::Project => self.show_project,
        }
    }

    /// A compact form of the settings, for the row-height cache key. The
    /// column set changes the width that a prompt wraps to, and the date row
    /// changes the height of a row. `project_fill` is not part of the key,
    /// because it changes the pad character and not the width.
    pub fn cache_bits(&self) -> u8 {
        (self.show_time as u8)
            | (self.show_change as u8) << 1
            | (self.show_source as u8) << 2
            | (self.show_title as u8) << 3
            | (self.show_project as u8) << 4
            | (self.align_project as u8) << 5
            | (self.date_separator as u8) << 6
    }
}

/// One row of the settings menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsItem {
    Column(Column),
    AlignProject,
    ProjectFill,
    DateSeparator,
}

/// The rows of the settings menu, in the order the menu lists them.
pub const SETTINGS_ITEMS: [SettingsItem; 8] = [
    SettingsItem::Column(Column::Time),
    SettingsItem::Column(Column::Change),
    SettingsItem::Column(Column::Source),
    SettingsItem::Column(Column::Title),
    SettingsItem::Column(Column::Project),
    SettingsItem::AlignProject,
    SettingsItem::ProjectFill,
    SettingsItem::DateSeparator,
];

impl SettingsItem {
    pub fn label(self) -> &'static str {
        match self {
            SettingsItem::Column(Column::Time) => "Time column",
            SettingsItem::Column(Column::Change) => "Change column",
            SettingsItem::Column(Column::Source) => "Source column",
            SettingsItem::Column(Column::Title) => "Session title column",
            SettingsItem::Column(Column::Project) => "Project column",
            SettingsItem::AlignProject => "Align project column",
            SettingsItem::ProjectFill => "Project column rule",
            SettingsItem::DateSeparator => "Date row for each day",
        }
    }

    /// A short description of what the row controls.
    pub fn hint(self) -> &'static str {
        match self {
            SettingsItem::Column(Column::Time) => "relative or absolute time of the entry",
            SettingsItem::Column(Column::Change) => "added and removed line counts",
            SettingsItem::Column(Column::Source) => "cursor / claude / nvim",
            SettingsItem::Column(Column::Title) => "name of the agent session",
            SettingsItem::Column(Column::Project) => "project chain, e.g. mnode → gaze",
            SettingsItem::AlignProject => "one width for every row",
            SettingsItem::ProjectFill => "draw the pad as ┄ instead of blanks",
            SettingsItem::DateSeparator => "e.g. 4 days ago - 2026-09-16",
        }
    }
}

/// The state of the settings menu overlay.
#[derive(Debug, Default)]
pub struct SettingsMenu {
    pub open: bool,
    pub selected: usize,
}

impl SettingsMenu {
    pub fn down(&mut self) {
        if self.selected + 1 < SETTINGS_ITEMS.len() {
            self.selected += 1;
        }
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn item(&self) -> SettingsItem {
        SETTINGS_ITEMS[self.selected.min(SETTINGS_ITEMS.len() - 1)]
    }
}

impl Settings {
    /// Turn one setting on or off.
    pub fn toggle(&mut self, item: SettingsItem) {
        match item {
            SettingsItem::Column(Column::Time) => self.show_time = !self.show_time,
            SettingsItem::Column(Column::Change) => self.show_change = !self.show_change,
            SettingsItem::Column(Column::Source) => self.show_source = !self.show_source,
            SettingsItem::Column(Column::Title) => self.show_title = !self.show_title,
            SettingsItem::Column(Column::Project) => self.show_project = !self.show_project,
            SettingsItem::AlignProject => self.align_project = !self.align_project,
            SettingsItem::ProjectFill => self.project_fill = !self.project_fill,
            SettingsItem::DateSeparator => self.date_separator = !self.date_separator,
        }
    }

    /// The current value of one setting.
    pub fn value(&self, item: SettingsItem) -> bool {
        match item {
            SettingsItem::Column(c) => self.shows(c),
            SettingsItem::AlignProject => self.align_project,
            SettingsItem::ProjectFill => self.project_fill,
            SettingsItem::DateSeparator => self.date_separator,
        }
    }
}
