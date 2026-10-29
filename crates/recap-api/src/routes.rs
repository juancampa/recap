//! The route paths of the HTTP API. The server registers them and the
//! client builds its URLs from them.
//!
//! Query parameters:
//! - A [`crate::Filter`] is `project`, `file`, `agent`, `kind` (`all`,
//!   `prompts`, `changes`) and `after` (RFC 3339).
//! - A [`crate::Page`] is `since` (a store version) and `limit`.
//! - A list of entry ids is one `ids` parameter, with commas between the ids.

/// The prefix of every route of this API version.
pub const PREFIX: &str = "/v1";

/// `GET`: [`crate::Status`].
pub const STATUS: &str = "/v1/status";
/// `GET`: [`crate::ProjectsResponse`].
pub const PROJECTS: &str = "/v1/projects";
/// `GET ?project=`: [`crate::FilesResponse`].
pub const FILES: &str = "/v1/files";
/// `GET ?project=`: [`crate::AgentsResponse`].
pub const AGENTS: &str = "/v1/agents";
/// `GET` with a filter and a page: [`crate::HistoryResponse`].
pub const HISTORY: &str = "/v1/history";
/// `GET /v1/entries/{id}`: [`crate::Entry`].
pub const ENTRIES: &str = "/v1/entries";
/// `GET ?ids=`: a list of [`crate::DiffLine`]. `&format=unified` returns
/// plain text.
pub const DIFF: &str = "/v1/diff";
/// `GET ?ids=`: a map from entry id to [`crate::DiffSummary`].
pub const SUMMARIES: &str = "/v1/summaries";
/// `GET /v1/search/{kind}?q=` with a filter: [`crate::SearchResponse`].
pub const SEARCH: &str = "/v1/search";
/// `GET`: a list of [`crate::RunningSession`].
pub const RUNNING: &str = "/v1/sessions/running";
/// `POST /v1/sessions/{id}/restore`: write the files of an archived session
/// back (see [`crate::Entry::session_archived`]).
pub const SESSIONS: &str = "/v1/sessions";
/// `POST`: start a load of the sources.
pub const RELOAD: &str = "/v1/reload";
/// `POST`: start the semantic search model and indexer.
pub const EMBED_START: &str = "/v1/embed/start";
/// `POST`: ask the triage runner for a review of the agents of the last 24
/// hours.
pub const TRIAGE_REVIEW: &str = "/v1/triage/review";
/// `POST` a [`crate::BlameRequest`]: a list of [`crate::HunkBlame`].
pub const BLAME: &str = "/v1/blame";
/// `GET`: a stream of [`crate::Event`]. Server-sent events by default;
/// `?format=ndjson` sends one JSON object on each line.
pub const EVENTS: &str = "/v1/events";
