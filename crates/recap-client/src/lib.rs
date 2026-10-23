//! A blocking Rust client for the recap HTTP API.
//!
//! [`Client`] implements [`recap_api::Api`], so a program can use the same
//! code with a remote server or with an in-process server. [`Client::events`]
//! reads the live event stream, and [`watch`] runs that stream on a thread
//! and connects again after an error.
//!
//! ```no_run
//! use recap_api::Api;
//! let client = recap_client::Client::from_env();
//! for project in client.projects().unwrap().projects {
//!     println!("{} {}", project.count, project.name);
//! }
//! ```

use recap_api::{
    AgentsResponse, Api, ApiError, ApiResult, BlameRequest, DiffLine, DiffSummary, Entry, EntryId,
    Event, FilesResponse, Filter, HistoryResponse, HunkBlame, Page, ProjectsResponse,
    RunningSession, SearchKind, SearchResponse, Status, routes,
};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// The port of the server that launchd runs.
pub const PROD_PORT: u16 = 7717;
/// The port of `recap serve --test`.
pub const TEST_PORT: u16 = 7718;

/// The environment variable that holds the server URL.
pub const URL_VAR: &str = "RECAP_URL";

/// The URL of the server on `port` of this machine.
pub fn local_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// The time that the client waits to connect. The server runs on the same
/// machine, so a longer wait only delays the error.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest time that a request can take. A semantic search waits for
/// the model server to start, which can take up to two minutes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// The longest time between two lines of the event stream. The server sends
/// an empty line every 15 seconds, so a longer silence means that the
/// connection stopped.
const EVENT_READ_TIMEOUT: Duration = Duration::from_secs(45);

/// The wait before [`watch`] connects again after an error.
const RETRY: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Client {
    base: String,
    agent: ureq::Agent,
}

fn error(e: ureq::Error) -> ApiError {
    match e {
        // The server sends the message of an error as plain text.
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            ApiError(format!("HTTP {code}: {}", body.trim()))
        }
        ureq::Error::Transport(t) => ApiError(t.to_string()),
    }
}

impl Client {
    /// A client of the server at `base`, for example `http://127.0.0.1:7717`.
    pub fn new(base: impl Into<String>) -> Client {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build();
        Client {
            base: base.into().trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// A client of the server that `RECAP_URL` names, or of the prod server
    /// when the variable is not set.
    pub fn from_env() -> Client {
        match std::env::var(URL_VAR) {
            Ok(url) if !url.is_empty() => Client::new(url),
            _ => Client::new(local_url(PROD_PORT)),
        }
    }

    /// The server URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn get(&self, path: &str) -> ureq::Request {
        self.agent.get(&format!("{}{path}", self.base))
    }

    fn post(&self, path: &str) -> ureq::Request {
        self.agent.post(&format!("{}{path}", self.base))
    }

    fn fetch<T: DeserializeOwned>(&self, request: ureq::Request) -> ApiResult<T> {
        let response = request.call().map_err(error)?;
        response
            .into_json()
            .map_err(|e| ApiError(format!("cannot read the response: {e}")))
    }

    fn with_pairs(mut request: ureq::Request, pairs: &[(&str, String)]) -> ureq::Request {
        for (key, value) in pairs {
            request = request.query(key, value);
        }
        request
    }

    fn with_project(request: ureq::Request, project: Option<&Path>) -> ureq::Request {
        match project {
            Some(p) => request.query("project", &p.to_string_lossy()),
            None => request,
        }
    }

    fn with_since(request: ureq::Request, since: Option<u64>) -> ureq::Request {
        match since {
            Some(v) => request.query("since", &v.to_string()),
            None => request,
        }
    }

    /// Open the event stream. The iterator ends when the server closes the
    /// connection, and gives an error when the connection fails.
    pub fn events(&self) -> ApiResult<Events> {
        // The stream has no end, so it needs its own agent without the
        // request timeout, and a read timeout instead.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(EVENT_READ_TIMEOUT)
            .build();
        let response = agent
            .get(&format!("{}{}", self.base, routes::EVENTS))
            .query("format", "ndjson")
            .call()
            .map_err(error)?;
        Ok(Events {
            reader: BufReader::new(response.into_reader()),
        })
    }
}

/// The events of one connection to `GET /v1/events`.
pub struct Events {
    reader: BufReader<Box<dyn Read + Send + Sync + 'static>>,
}

impl Iterator for Events {
    type Item = ApiResult<Event>;

    fn next(&mut self) -> Option<ApiResult<Event>> {
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None,
                // An empty line only keeps the connection open.
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => {
                    return Some(
                        serde_json::from_str(&line)
                            .map_err(|e| ApiError(format!("cannot read an event: {e}"))),
                    );
                }
                Err(e) => return Some(Err(ApiError(e.to_string()))),
            }
        }
    }
}

/// A message from [`watch`].
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// The event stream is open. The events that the server sent while no
    /// stream was open are lost, so read the views again.
    Connected,
    Event(Event),
    /// The event stream stopped. [`watch`] connects again after 2 seconds.
    Disconnected(String),
}

/// Read the event stream of `client` on a new thread, and send each message
/// to `tx`. The thread connects again after each error, and stops when the
/// receiver of `tx` is gone.
pub fn watch(client: Client, tx: mpsc::Sender<Message>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut connected = true;
        loop {
            let error = match client.events() {
                Ok(events) => {
                    connected = true;
                    if tx.send(Message::Connected).is_err() {
                        return;
                    }
                    let mut error = "the server closed the event stream".to_string();
                    for event in events {
                        match event {
                            Ok(event) => {
                                if tx.send(Message::Event(event)).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                error = e.0;
                                break;
                            }
                        }
                    }
                    error
                }
                Err(e) => e.0,
            };
            // Report a failed connection once, not at each retry.
            if connected {
                connected = false;
                let message = format!("cannot reach {}: {error}", client.base());
                if tx.send(Message::Disconnected(message)).is_err() {
                    return;
                }
            }
            std::thread::sleep(RETRY);
        }
    })
}

impl Api for Client {
    fn status(&self) -> ApiResult<Status> {
        self.fetch(self.get(routes::STATUS))
    }

    fn projects(&self) -> ApiResult<ProjectsResponse> {
        self.fetch(self.get(routes::PROJECTS))
    }

    fn files(&self, project: Option<&Path>, since: Option<u64>) -> ApiResult<FilesResponse> {
        let request = Self::with_project(self.get(routes::FILES), project);
        self.fetch(Self::with_since(request, since))
    }

    fn agents(&self, project: Option<&Path>, since: Option<u64>) -> ApiResult<AgentsResponse> {
        let request = Self::with_project(self.get(routes::AGENTS), project);
        self.fetch(Self::with_since(request, since))
    }

    fn history(&self, filter: &Filter, page: &Page) -> ApiResult<HistoryResponse> {
        let request = Self::with_pairs(self.get(routes::HISTORY), &filter.query_pairs());
        self.fetch(Self::with_pairs(request, &page.query_pairs()))
    }

    fn entry(&self, id: &EntryId) -> ApiResult<Entry> {
        self.fetch(self.get(&format!("{}/{id}", routes::ENTRIES)))
    }

    fn diff(&self, ids: &[EntryId]) -> ApiResult<Vec<DiffLine>> {
        self.fetch(self.get(routes::DIFF).query("ids", &recap_api::join_ids(ids)))
    }

    fn summaries(&self, ids: &[EntryId]) -> ApiResult<HashMap<EntryId, DiffSummary>> {
        self.fetch(self.get(routes::SUMMARIES).query("ids", &recap_api::join_ids(ids)))
    }

    fn search(&self, kind: SearchKind, query: &str, filter: &Filter) -> ApiResult<SearchResponse> {
        let request = self
            .get(&format!("{}/{}", routes::SEARCH, kind.as_str()))
            .query("q", query);
        self.fetch(Self::with_pairs(request, &filter.query_pairs()))
    }

    fn running(&self) -> ApiResult<Vec<RunningSession>> {
        self.fetch(self.get(routes::RUNNING))
    }

    fn reload(&self) -> ApiResult<()> {
        self.post(routes::RELOAD).call().map_err(error)?;
        Ok(())
    }

    fn start_embedding(&self) -> ApiResult<()> {
        self.post(routes::EMBED_START).call().map_err(error)?;
        Ok(())
    }

    fn restore_session(&self, session_id: &str) -> ApiResult<()> {
        self.post(&format!("{}/{session_id}/restore", routes::SESSIONS)).call().map_err(error)?;
        Ok(())
    }

    fn blame(&self, request: &BlameRequest) -> ApiResult<Vec<HunkBlame>> {
        let response = self
            .post(routes::BLAME)
            .send_json(request)
            .map_err(error)?;
        response
            .into_json()
            .map_err(|e| ApiError(format!("cannot read the response: {e}")))
    }
}
