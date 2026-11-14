//! The HTTP routes of the triage data and of the web UI.
//!
//! The routes under `/v1/triage/` are for triagent and for each
//! other client. Triagent changes the data with `POST /v1/triage/apply`,
//! and the server refuses the changes that only the user can make.
//!
//! The routes under `/ui/` are for the web UI of the user. Each request
//! must send the token of the server in the `X-Recap-Token` header. The
//! server writes the token into the page at `/` and at the path of each
//! view (see [`VIEWS`]). Triagent can call
//! only the `/v1/` routes with `curl`, so it cannot get the token and
//! cannot, for example, approve its own check scripts.

use crate::server::Server;
use crate::triage_api::StateQuery;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use recap_api::ApiError;
use recap_api::triage::{Actor, Batch};
use serde::Deserialize;
use std::path::PathBuf;

/// The header of the token of the `/ui/` routes.
const TOKEN_HEADER: &str = "x-recap-token";
/// The text in `index.html` that the server replaces with the token.
const TOKEN_PLACEHOLDER: &str = "__RECAP_TOKEN__";

/// The files of the web UI, in the binary.
const WEB_FILES: &[(&str, &str, &str)] = &[
    ("index.html", "text/html; charset=utf-8", include_str!("../../../web/index.html")),
    ("style.css", "text/css; charset=utf-8", include_str!("../../../web/style.css")),
    ("app.js", "text/javascript; charset=utf-8", include_str!("../../../web/app.js")),
    ("views.js", "text/javascript; charset=utf-8", include_str!("../../../web/views.js")),
    ("chat.js", "text/javascript; charset=utf-8", include_str!("../../../web/chat.js")),
    ("markdown.js", "text/javascript; charset=utf-8", include_str!("../../../web/markdown.js")),
    ("text.js", "text/javascript; charset=utf-8", include_str!("../../../web/text.js")),
    ("highlight.js", "text/javascript; charset=utf-8", include_str!("../../../web/highlight.js")),
    ("vendor/preact.js", "text/javascript; charset=utf-8", include_str!("../../../web/vendor/preact.js")),
];

/// The views of the web UI. The page is also at `/<view>`, so the address
/// of a view works after a reload. `web/app.js` has the same list.
const VIEWS: &[&str] = &["items", "topics", "checks", "activity"];

/// A directory that holds the files of the web UI. With it, the server
/// reads each file from the disk at each request, so a page reload shows a
/// change. Without it, the server sends the files in the binary.
#[derive(Clone, Default)]
pub struct WebDir(pub Option<PathBuf>);

fn fail(status: StatusCode, e: impl std::fmt::Display) -> Response {
    (status, e.to_string()).into_response()
}

fn api_error(e: ApiError) -> Response {
    fail(StatusCode::BAD_REQUEST, e.0)
}

/// Run `f` with the server on the blocking thread pool.
async fn blocking<T, F>(server: Server, f: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(&Server) -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&server))
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map_err(api_error)
}

/// The triage routes and the web UI.
pub fn router(server: Server, web_dir: WebDir) -> Router<Server> {
    let ui = Router::new()
        .route("/apply", post(ui_apply))
        .route("/changes", get(changes))
        .route("/checks", get(checks))
        .route("/checks/{name}/approve", post(approve))
        .route("/sessions/{chain}/focus", post(focus))
        .route("/chat/threads", get(threads))
        .route("/chat/threads/{id}", get(thread))
        .route("/chat/messages", post(chat_send))
        .layer(middleware::from_fn_with_state(server, check_token));
    let mut router = Router::new().route("/", get(index));
    for view in VIEWS {
        router = router.route(&format!("/{view}"), get(index));
    }
    router
        .route("/web/{*path}", get(web_file))
        .route("/v1/triage/state", get(state))
        .route("/v1/triage/apply", post(agent_apply))
        .nest("/ui", ui)
        .layer(Extension(web_dir))
}

/// Refuse a request without the token of the server.
async fn check_token(State(server): State<Server>, request: Request, next: Next) -> Response {
    let sent = request.headers().get(TOKEN_HEADER).and_then(|v| v.to_str().ok());
    if sent != Some(server.ui_token()) {
        return fail(StatusCode::FORBIDDEN, "this route is for the web UI of the user");
    }
    next.run(request).await
}

/// The file `name` of the web UI.
fn web_content(web_dir: &WebDir, name: &str) -> Option<(&'static str, String)> {
    let (_, content_type, embedded) = WEB_FILES.iter().find(|(n, _, _)| *n == name)?;
    let text = match &web_dir.0 {
        Some(dir) => std::fs::read_to_string(dir.join(name)).ok()?,
        None => embedded.to_string(),
    };
    Some((content_type, text))
}

async fn index(State(server): State<Server>, Extension(web_dir): Extension<WebDir>) -> Response {
    match web_content(&web_dir, "index.html") {
        Some((_, text)) => Html(text.replace(TOKEN_PLACEHOLDER, server.ui_token())).into_response(),
        None => fail(StatusCode::NOT_FOUND, "no index.html"),
    }
}

async fn web_file(Extension(web_dir): Extension<WebDir>, UrlPath(path): UrlPath<String>) -> Response {
    // This route does not send the page with the token.
    match web_content(&web_dir, &path).filter(|_| path != "index.html") {
        Some((content_type, text)) => (
            [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")],
            text,
        )
            .into_response(),
        None => fail(StatusCode::NOT_FOUND, "no such file"),
    }
}

async fn state(State(s): State<Server>, Query(query): Query<StateQuery>) -> Result<Response, Response> {
    Ok(Json(blocking(s, move |s| s.triage_state(&query)).await?).into_response())
}

/// The body is JSON with any content type, because `curl -d` sends
/// `application/x-www-form-urlencoded`.
async fn apply(server: Server, actor: Actor, body: String) -> Result<Response, Response> {
    let batch: Batch = serde_json::from_str(&body)
        .map_err(|e| fail(StatusCode::BAD_REQUEST, format!("the batch is not valid JSON: {e}")))?;
    Ok(Json(blocking(server, move |s| s.triage_apply(actor, &batch)).await?).into_response())
}

async fn agent_apply(State(s): State<Server>, body: String) -> Result<Response, Response> {
    apply(s, Actor::Triagent, body).await
}

async fn ui_apply(State(s): State<Server>, body: String) -> Result<Response, Response> {
    apply(s, Actor::User, body).await
}

#[derive(Deserialize)]
struct ChangesQuery {
    #[serde(default)]
    after: u64,
    to: Option<u64>,
    actor: Option<Actor>,
}

async fn changes(State(s): State<Server>, Query(q): Query<ChangesQuery>) -> Result<Response, Response> {
    Ok(Json(blocking(s, move |s| s.triage_changes(q.after, q.to, q.actor)).await?).into_response())
}

async fn checks(State(s): State<Server>) -> Result<Response, Response> {
    Ok(Json(blocking(s, |s| s.triage_checks()).await?).into_response())
}

async fn approve(State(s): State<Server>, UrlPath(name): UrlPath<String>) -> Result<StatusCode, Response> {
    blocking(s, move |s| s.triage_approve(&name)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn focus(State(s): State<Server>, UrlPath(chain): UrlPath<String>) -> Result<StatusCode, Response> {
    blocking(s, move |s| s.focus_chain(&chain)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn threads(State(s): State<Server>) -> Result<Response, Response> {
    Ok(Json(blocking(s, |s| s.chat_threads()).await?).into_response())
}

async fn thread(State(s): State<Server>, UrlPath(id): UrlPath<i64>) -> Result<Response, Response> {
    Ok(Json(blocking(s, move |s| s.chat_thread(id)).await?).into_response())
}

#[derive(Deserialize)]
struct ChatSend {
    /// None starts a new thread.
    thread: Option<i64>,
    text: String,
    /// The model that writes the reply. None gives the default model.
    model: Option<recap_api::triage::Model>,
}

async fn chat_send(State(s): State<Server>, Json(send): Json<ChatSend>) -> Result<Response, Response> {
    Ok(Json(blocking(s, move |s| s.chat_send(send.thread, &send.text, send.model.unwrap_or_default())).await?).into_response())
}
