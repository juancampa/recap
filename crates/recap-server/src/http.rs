//! The HTTP API: axum routes over [`Server`].
//!
//! Each handler runs its [`Api`] call on the blocking thread pool, because
//! the calls read files and hold locks. The server binds to 127.0.0.1 only
//! and has no authentication.
//!
//! `GET /v1/events` sends server-sent events by default, because the browser
//! `EventSource` needs them. `?format=ndjson` sends one JSON object on each
//! line instead, which `curl -N | jq` reads directly. Both formats send a
//! keep-alive every 15 seconds: an SSE comment, or an empty line.

use crate::server::Server;
use axum::body::Body;
use axum::body::HttpBody;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use recap_api::{
    Api, ApiError, BlameRequest, DiffLine, EntryId, Event, Filter, Page, SearchKind, routes,
};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tokio_stream::{Stream, StreamExt};

/// The interval between two keep-alive messages on the event stream.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The minimal web client. See `examples/web/`.
const WEB_EXAMPLE: &str = include_str!("../../../examples/web/index.html");

/// An error response: the message as plain text.
struct HttpError(StatusCode, String);

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

impl From<ApiError> for HttpError {
    fn from(e: ApiError) -> Self {
        HttpError(StatusCode::INTERNAL_SERVER_ERROR, e.0)
    }
}

type HttpResult<T> = Result<T, HttpError>;

/// Run `f` with the server on the blocking thread pool.
async fn blocking<T, F>(server: Server, f: F) -> HttpResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Server) -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&server))
        .await
        .map_err(|e| HttpError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(HttpError::from)
}

#[derive(Deserialize)]
struct ProjectQuery {
    project: Option<std::path::PathBuf>,
}

#[derive(Deserialize)]
struct IdsQuery {
    #[serde(default)]
    ids: String,
    /// `unified` returns the diff as plain text.
    format: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

#[derive(Deserialize)]
struct EventsQuery {
    /// `ndjson` sends one JSON object on each line.
    format: Option<String>,
}

/// All routes of the API, and the web example at `/`.
pub fn router(server: Server) -> Router {
    Router::new()
        .route("/", get(|| async { Html(WEB_EXAMPLE) }))
        .route(routes::STATUS, get(status))
        .route(routes::PROJECTS, get(projects))
        .route(routes::FILES, get(files))
        .route(routes::AGENTS, get(agents))
        .route(routes::HISTORY, get(history))
        .route(&format!("{}/{{id}}", routes::ENTRIES), get(entry))
        .route(routes::DIFF, get(diff))
        .route(routes::SUMMARIES, get(summaries))
        .route(&format!("{}/{{kind}}", routes::SEARCH), get(search))
        .route(routes::RUNNING, get(running))
        .route(routes::RELOAD, post(reload))
        .route(routes::EMBED_START, post(start_embedding))
        .route(routes::BLAME, post(blame))
        .route(
            &format!("{}/{{id}}/restore", routes::SESSIONS),
            post(restore_session),
        )
        .route(routes::EVENTS, get(events))
        .layer(middleware::from_fn(log_request))
        .with_state(server)
}

/// Log each request: the method, the path with the query, the status, the
/// time until the response headers, and the size of the body when the size
/// is known. For the event stream, the line tells that a client connected.
async fn log_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let uri = request.uri().clone();
    let response = next.run(request).await;
    let elapsed = crate::logger::ms(started.elapsed());
    let status = response.status();
    let size = response
        .body()
        .size_hint()
        .exact()
        .map(|n| format!(" {}", crate::logger::bytes(n)))
        .unwrap_or_default();
    let path = percent_encoding::percent_decode_str(&uri.to_string())
        .decode_utf8_lossy()
        .into_owned();
    if status.is_server_error() {
        log::warn!("{method} {path} {} {elapsed}{size}", status.as_u16());
    } else {
        log::info!("{method} {path} {} {elapsed}{size}", status.as_u16());
    }
    response
}

async fn status(State(s): State<Server>) -> HttpResult<impl IntoResponse> {
    Ok(Json(blocking(s, |s| s.status()).await?))
}

async fn projects(State(s): State<Server>) -> HttpResult<impl IntoResponse> {
    Ok(Json(blocking(s, |s| s.projects()).await?))
}

async fn files(State(s): State<Server>, Query(q): Query<ProjectQuery>) -> HttpResult<impl IntoResponse> {
    Ok(Json(blocking(s, move |s| s.files(q.project.as_deref())).await?))
}

async fn agents(State(s): State<Server>, Query(q): Query<ProjectQuery>) -> HttpResult<impl IntoResponse> {
    Ok(Json(blocking(s, move |s| s.agents(q.project.as_deref())).await?))
}

async fn history(
    State(s): State<Server>,
    Query(filter): Query<Filter>,
    Query(page): Query<Page>,
) -> HttpResult<impl IntoResponse> {
    Ok(Json(blocking(s, move |s| s.history(&filter, &page)).await?))
}

async fn entry(
    State(s): State<Server>,
    UrlPath(id): UrlPath<String>,
) -> HttpResult<impl IntoResponse> {
    let id = EntryId(id);
    let found = blocking(s, move |s| Ok(s.entry(&id).ok())).await?;
    match found {
        Some(entry) => Ok(Json(entry)),
        None => Err(HttpError(
            StatusCode::NOT_FOUND,
            "no entry with this id".to_string(),
        )),
    }
}

async fn diff(State(s): State<Server>, Query(q): Query<IdsQuery>) -> HttpResult<Response> {
    let ids = recap_api::split_ids(&q.ids);
    let lines = blocking(s, move |s| s.diff(&ids)).await?;
    Ok(match q.format.as_deref() {
        Some("unified") => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            unified(&lines),
        )
            .into_response(),
        _ => Json(lines).into_response(),
    })
}

/// The diff lines as text: `+` and `-` in front of the changed lines, a space
/// in front of a context line, `>` in front of a prompt line and `<` in front
/// of a response line.
fn unified(lines: &[DiffLine]) -> String {
    let mut out = String::new();
    for line in lines {
        match line {
            DiffLine::Added(s) => out.push_str(&format!("+{s}\n")),
            DiffLine::Removed(s) => out.push_str(&format!("-{s}\n")),
            DiffLine::Context(s) => out.push_str(&format!(" {s}\n")),
            DiffLine::Header(s) => out.push_str(&format!("{s}\n")),
            DiffLine::Prompt(s) => out.push_str(&format!("> {s}\n")),
            DiffLine::Response(s) => out.push_str(&format!("< {s}\n")),
            DiffLine::ChildSeparator(id) => out.push_str(&format!("=== {id}\n")),
        }
    }
    out
}

async fn summaries(
    State(s): State<Server>,
    Query(q): Query<IdsQuery>,
) -> HttpResult<impl IntoResponse> {
    let ids = recap_api::split_ids(&q.ids);
    Ok(Json(blocking(s, move |s| s.summaries(&ids)).await?))
}

async fn search(
    State(s): State<Server>,
    UrlPath(kind): UrlPath<SearchKind>,
    Query(q): Query<SearchQuery>,
    Query(filter): Query<Filter>,
) -> HttpResult<impl IntoResponse> {
    Ok(Json(
        blocking(s, move |s| s.search(kind, &q.q, &filter)).await?,
    ))
}

async fn running(State(s): State<Server>) -> HttpResult<impl IntoResponse> {
    Ok(Json(blocking(s, |s| s.running()).await?))
}

async fn reload(State(s): State<Server>) -> HttpResult<StatusCode> {
    blocking(s, |s| s.reload()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn start_embedding(State(s): State<Server>) -> HttpResult<StatusCode> {
    blocking(s, |s| s.start_embedding()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn restore_session(
    State(s): State<Server>,
    UrlPath(id): UrlPath<String>,
) -> HttpResult<StatusCode> {
    blocking(s, move |s| s.restore_session(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn blame(
    State(s): State<Server>,
    Json(request): Json<BlameRequest>,
) -> HttpResult<impl IntoResponse> {
    Ok(Json(blocking(s, move |s| s.blame(&request)).await?))
}

/// The events of the server. A receiver that falls behind loses events. It
/// then gets a `changed` event with the current version, so the client reads
/// the views again.
fn event_stream(server: Server, format: &'static str) -> impl Stream<Item = Event> {
    let rx = server.subscribe();
    let open = StreamLog {
        format,
        started: Instant::now(),
    };
    BroadcastStream::new(rx).map(move |item| {
        // The stream owns `open`, so the drop of the stream logs the close.
        let _ = &open;
        match item {
            Ok(event) => event,
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                log::warn!("an event stream lost {n} events. It gets one changed event");
                Event::Changed {
                    version: server.version(),
                }
            }
        }
    })
}

/// Logs the close of an event stream when axum drops the stream, that is
/// when the client disconnects.
struct StreamLog {
    format: &'static str,
    started: Instant,
}

impl Drop for StreamLog {
    fn drop(&mut self) {
        log::info!(
            "an {} event stream closed after {}",
            self.format,
            crate::logger::ms(self.started.elapsed())
        );
    }
}

async fn events(State(s): State<Server>, Query(q): Query<EventsQuery>) -> Response {
    let ndjson = q.format.as_deref() == Some("ndjson");
    let events = event_stream(s, if ndjson { "ndjson" } else { "SSE" });
    if ndjson {
        let lines = events.map(|event| {
            let mut line = serde_json::to_string(&event).unwrap_or_default();
            line.push('\n');
            line
        });
        let keep_alive =
            IntervalStream::new(tokio::time::interval(KEEP_ALIVE)).map(|_| "\n".to_string());
        let body = lines.merge(keep_alive).map(Ok::<_, Infallible>);
        return (
            [(header::CONTENT_TYPE, "application/x-ndjson")],
            Body::from_stream(body),
        )
            .into_response();
    }
    let body = events.map(|event| {
        Ok::<_, Infallible>(
            sse::Event::default().data(serde_json::to_string(&event).unwrap_or_default()),
        )
    });
    Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// Serve the API on 127.0.0.1:`port` until the process gets SIGINT or
/// SIGTERM. The function builds its own tokio runtime and blocks.
pub fn run(server: Server, port: u16) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        log::info!("serving on http://127.0.0.1:{port}");
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let serve = axum::serve(listener, router(server)).with_graceful_shutdown(async move {
            shutdown().await;
            let _ = stop_tx.send(true);
        });
        // The graceful shutdown waits for each open connection to close. An
        // event stream stays open until its client disconnects, and each TUI
        // holds one. So the server waits a short time and then stops.
        tokio::select! {
            result = serve => {
                result?;
                log::info!("stopped");
            }
            _ = async {
                let _ = stop_rx.wait_for(|stop| *stop).await;
                tokio::time::sleep(SHUTDOWN_WAIT).await;
            } => log::info!("stopped. The open event streams were closed"),
        }
        Ok(())
    })
}

/// The longest time that the server waits for open connections after
/// SIGINT or SIGTERM.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// Wait for SIGINT or SIGTERM.
async fn shutdown() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => log::info!("SIGINT: stopping"),
        _ = term.recv() => log::info!("SIGTERM: stopping"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEntry;
    use chrono::Local;
    use recap_api::{EntryFields, EntryKind, KindFilter};
    use recap_client::Client;
    use std::path::{Path, PathBuf};

    /// Serve `server` on a free port of 127.0.0.1 on a new thread, and
    /// return a client of it.
    fn serve(server: Server) -> Client {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap().port()).unwrap();
                axum::serve(listener, router(server)).await.unwrap();
            });
        });
        Client::new(recap_client::local_url(rx.recv().unwrap()))
    }

    fn prompt(text: &str, session: &str, minutes_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(minutes_ago),
            file_path: PathBuf::from(format!("claude:{session}:{text}")),
            snapshot_path: PathBuf::new(),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: recap_api::EditSource::ClaudeCode,
            kind: EntryKind::Prompt {
                text: text.to_string(),
                response: Some(format!("done: {text}")),
            },
            session_id: Some(session.to_string()),
            session_cwd: None,
        }
    }

    fn edit(root: &str, path: &str, snapshot: &Path, minutes_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(minutes_ago),
            file_path: PathBuf::from(path),
            snapshot_path: snapshot.to_path_buf(),
            base_path: None,
            project_roots: vec![PathBuf::from(root)],
            source: recap_api::EditSource::Neovim,
            kind: EntryKind::FileChange,
            session_id: None,
            session_cwd: None,
        }
    }

    /// Two prompts in /dev/proj, and two edits of one file in /dev/other.
    /// The second edit adds one line.
    fn fixture(name: &str) -> Server {
        let dir =
            std::env::temp_dir().join(format!("recap-http-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, "one\n").unwrap();
        std::fs::write(&b, "one\ntwo\n").unwrap();
        Server::from_entries(vec![
            prompt("fix the parser", "s1", 1),
            prompt("add a test", "s1", 2),
            edit("/dev/other", "/dev/other/x.rs", &b, 3),
            edit("/dev/other", "/dev/other/x.rs", &a, 4),
        ])
    }

    #[test]
    fn the_history_route_applies_the_filter_and_the_page() {
        let client = serve(fixture("history"));

        let all = client
            .history(&Filter::default(), &Page::default())
            .unwrap();
        let count = |r: &recap_api::HistoryResponse| {
            r.groups.iter().map(|g| g.entries.len()).sum::<usize>()
        };
        assert_eq!(count(&all), 4);

        let prompts = Filter {
            kind: KindFilter::Prompts,
            ..Filter::default()
        };
        let r = client.history(&prompts, &Page::default()).unwrap();
        assert_eq!(count(&r), 2);
        assert!(r.groups.iter().all(|g| g.entries[0].is_prompt()));

        let other = Filter {
            project: Some(PathBuf::from("/dev/other")),
            ..Filter::default()
        };
        let r = client.history(&other, &Page::default()).unwrap();
        assert_eq!(r.groups.len(), 1, "the two edits of one file are one group");
        assert_eq!(r.groups[0].entries.len(), 2);

        let newest = Page {
            limit: Some(1),
            ..Page::default()
        };
        let r = client.history(&Filter::default(), &newest).unwrap();
        assert_eq!(count(&r), 1);
        assert_eq!(r.groups[0].entries[0].prompt_text(), Some("fix the parser"));

        let unchanged = Page {
            since: Some(all.version),
            ..Page::default()
        };
        let r = client.history(&Filter::default(), &unchanged).unwrap();
        assert_eq!(count(&r), 0, "no entry changed after the current version");
        assert!(r.removed.is_empty());
    }

    #[test]
    fn the_diff_route_sends_json_or_unified_text() {
        let client = serve(fixture("diff"));
        let other = Filter {
            project: Some(PathBuf::from("/dev/other")),
            ..Filter::default()
        };
        let newest = client.history(&other, &Page::default()).unwrap().groups[0].entries[0].clone();

        let lines = client.diff(std::slice::from_ref(&newest.id)).unwrap();
        assert!(
            lines.contains(&DiffLine::Added("two".to_string())),
            "{lines:?}"
        );

        let url = format!(
            "{}{}?ids={}&format=unified",
            client.base(),
            routes::DIFF,
            newest.id
        );
        let text = ureq::get(&url).call().unwrap().into_string().unwrap();
        assert!(text.lines().any(|l| l == "+two"), "{text}");
    }

    #[test]
    fn the_entry_route_sends_404_for_an_unknown_id() {
        let client = serve(fixture("entry"));
        let url = format!("{}{}/0000000000000000", client.base(), routes::ENTRIES);
        match ureq::get(&url).call() {
            Err(ureq::Error::Status(code, _)) => assert_eq!(code, 404),
            other => panic!("expected 404, got {other:?}"),
        }

        let id = client
            .history(&Filter::default(), &Page::default())
            .unwrap()
            .groups[0]
            .entries[0]
            .id
            .clone();
        assert_eq!(
            client.entry(&id).unwrap().prompt_text(),
            Some("fix the parser")
        );
    }

    #[test]
    fn the_search_route_reads_the_kind_from_the_path() {
        let client = serve(fixture("search"));
        let r = client
            .search(SearchKind::Content, "parser", &Filter::default())
            .unwrap();
        assert_eq!(r.matches.len(), 1);
    }

    /// The ndjson stream sends one event on each line, in the order that
    /// the server sent them.
    #[test]
    fn the_event_stream_sends_ndjson_lines() {
        let server = fixture("events");
        let client = serve(server.clone());
        let mut events = client.events().unwrap();
        // The handler subscribes before it sends the response headers, so
        // the events that the server sends now reach this stream.
        server.send(Event::Loading { loading: true });
        server.send(Event::Changed { version: 7 });
        assert_eq!(
            events.next().unwrap().unwrap(),
            Event::Loading { loading: true }
        );
        assert_eq!(
            events.next().unwrap().unwrap(),
            Event::Changed { version: 7 }
        );
    }
}
