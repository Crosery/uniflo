//! HTTP gateway over the live index.
//!
//! | route | |
//! |---|---|
//! | `GET /v1/health` | liveness, seq, counts |
//! | `GET /v1/harnesses` | supported harnesses on this machine |
//! | `GET /v1/sessions?q=&limit=&format=ndjson` | search (see `uniflo-search` syntax) |
//! | `GET /v1/sessions/{key}` | one session |
//! | `GET /v1/sessions/{key}/events?limit=&before=&max_text=&format=ndjson` | transcript, newest page first |
//! | `GET /v1/sessions/{key}/events?around=<event id>&limit=` | transcript window centred on one event |
//! | `GET /v1/search?q=&filter=&kinds=&limit=&offset=` | full-text hits grouped by session (503 without an index) |
//! | `GET /v1/stream` (SSE) · `/v1/stream.ndjson` · `/v1/ws` | live envelopes; `since`, `session`, `harness`, `types`, `kinds`, `max_text` |
//! | `GET /v1/stats` | engine counters, unknown discriminators |
//! | `GET /v1/usage?group_by=&q=&since=&until=&tz=&under=&depth=&limit=&sort=` | token / cost aggregation |
//! | `GET /v1/sessions/{key}/usage` | per-step usage, per-turn totals |
//! | `GET /v1/models?q=` · `GET /v1/pricing` | models seen with prices · catalog sync status |
//! | `POST /v1/cleanup/plan` · `POST /v1/cleanup/plans/{id}/execute` | session cleanup (write) |
//! | `GET /v1/archive` · `DELETE /v1/archive/{key}` (write) | archived sessions |
//! | `GET /v1/sessions/{key}/resume` | command that continues the session in its harness |
//! | `POST /v1/sessions/{key}/open-terminal?terminal=` | macOS: run that command in a new terminal window (write route) |
//! | `GET /v1/memory?cwd=` · `GET /v1/memory/file?path=` | agent memory / instruction files · one of them |
//! | `GET /demo` | bundled single-page demo client (`examples/web/index.html`) |
//!
//! Snapshots carry `x-uniflo-seq`; subscribe with `since=<that>` for a gap-free view.
//! Security: loopback Host only (DNS-rebinding guard), browser Origins limited to
//! loopback + `--cors-origin`, optional bearer token (`Authorization` or `?token=`). Every write
//! request (any method but GET / HEAD / OPTIONS) also passes [`write::check`].

pub mod agent;
pub mod cleanup;
mod guard;
mod search;
mod stream;
pub mod usage;
pub mod write;

pub use guard::GuardOptions;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use uniflo_core::cleanup::Cleanup;
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, HistoryQuery};
use uniflo_schema::{Envelope, SCHEMA_VERSION};
use uniflo_search::fts::Fts;
use uniflo_search::{Query as SearchQuery, search};

pub const DEFAULT_MAX_TEXT: usize = 32 * 1024;

#[derive(Clone)]
struct AppState {
    engine: Arc<Engine>,
    fts: Option<Arc<Fts>>,
    cleanup: Option<Arc<Cleanup>>,
    read_only: bool,
}

/// Optional services behind the router; a missing one answers 503.
#[derive(Default)]
pub struct Services {
    pub fts: Option<Arc<Fts>>,
    pub cleanup: Option<Arc<Cleanup>>,
}

/// Router without a full-text index: `/v1/search` answers 503.
pub fn router(engine: Arc<Engine>, guard: GuardOptions) -> Router {
    router_with_fts(engine, guard, None)
}

pub fn router_with_fts(engine: Arc<Engine>, guard: GuardOptions, fts: Option<Arc<Fts>>) -> Router {
    router_with(engine, guard, Services { fts, ..Default::default() })
}

pub fn router_with(engine: Arc<Engine>, guard: GuardOptions, services: Services) -> Router {
    let state = AppState { engine, fts: services.fts, cleanup: services.cleanup, read_only: guard.read_only };
    let guard = Arc::new(guard);
    Router::new()
        .route("/", get(index))
        .route("/demo", get(demo))
        .route("/v1/health", get(health))
        .route("/v1/harnesses", get(harnesses))
        .route("/v1/stats", get(stats))
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{key}", get(session))
        .route("/v1/sessions/{key}/events", get(events))
        .route("/v1/sessions/{key}/usage", get(usage::session_usage))
        .route("/v1/usage", get(usage::usage))
        .route("/v1/models", get(usage::models))
        .route("/v1/pricing", get(usage::pricing))
        .route("/v1/search", get(search::search))
        .route("/v1/sessions/{key}/resume", get(agent::resume))
        .route("/v1/sessions/{key}/open-terminal", axum::routing::post(agent::open_terminal))
        .route("/v1/memory", get(agent::memory))
        .route("/v1/memory/file", get(agent::memory_file))
        .route("/v1/stream", get(sse))
        .route("/v1/stream.ndjson", get(ndjson))
        .route("/v1/ws", get(ws))
        .route("/v1/archive", get(cleanup::archives))
        .route("/v1/cleanup/plan", post(cleanup::plan))
        .route("/v1/cleanup/plans/{id}/execute", post(cleanup::execute))
        .route("/v1/archive/{key}", delete(cleanup::remove))
        .layer(axum::middleware::from_fn_with_state(guard, guard::guard))
        .with_state(state)
}

/// Serve until the future resolves (Ctrl-C in the CLI).
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    axum::serve(listener, router).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn with_seq(engine: &Engine, mut resp: Response) -> Response {
    if let Ok(v) = HeaderValue::from_str(&engine.seq().to_string()) {
        resp.headers_mut().insert("x-uniflo-seq", v);
    }
    resp
}

fn ndjson_body<T: serde::Serialize>(items: impl IntoIterator<Item = T>) -> Response {
    let mut out = Vec::new();
    for it in items {
        if serde_json::to_writer(&mut out, &it).is_ok() {
            out.push(b'\n');
        }
    }
    ([(header::CONTENT_TYPE, "application/x-ndjson")], out).into_response()
}

async fn index() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "uniflo",
        "version": env!("CARGO_PKG_VERSION"),
        "schema": SCHEMA_VERSION,
        "endpoints": ["/demo", "/v1/health", "/v1/harnesses", "/v1/sessions", "/v1/sessions/{key}", "/v1/sessions/{key}/events", "/v1/search", "/v1/stream", "/v1/stream.ndjson", "/v1/ws", "/v1/stats", "/v1/usage", "/v1/sessions/{key}/usage", "/v1/models", "/v1/pricing", "/v1/cleanup/plan", "/v1/cleanup/plans/{id}/execute", "/v1/archive", "/v1/archive/{key}", "/v1/sessions/{key}/resume", "/v1/sessions/{key}/open-terminal", "/v1/memory", "/v1/memory/file"],
    }))
}

async fn demo() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn health(State(s): State<AppState>) -> impl IntoResponse {
    let st = s.engine.stats();
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "schema": SCHEMA_VERSION,
        "seq": st.seq,
        "sessions": st.sessions,
        "working": st.working,
        "uptime_ms": st.uptime_ms,
        "update_available": st.update.as_ref().is_some_and(|u| u.available),
        "latest_version": st.update.as_ref().and_then(|u| u.latest.clone()),
        "latest_prerelease": st.update.as_ref().and_then(|u| u.latest_prerelease.clone()),
        "read_only": s.read_only,
    }))
}

async fn harnesses(State(s): State<AppState>) -> impl IntoResponse {
    Json(s.engine.harnesses())
}

async fn stats(State(s): State<AppState>) -> impl IntoResponse {
    let mut v = serde_json::to_value(s.engine.stats()).unwrap_or_default();
    v["fts"] = serde_json::to_value(s.fts.as_ref().map(|f| f.status())).unwrap_or_default();
    Json(v)
}

#[derive(Deserialize)]
struct ListParams {
    q: Option<String>,
    limit: Option<usize>,
    format: Option<String>,
}

async fn sessions(State(s): State<AppState>, Query(p): Query<ListParams>) -> Response {
    let all = s.engine.sessions();
    let q = SearchQuery::parse(p.q.as_deref().unwrap_or(""), now_ms());
    let hits = search(&all, &q, p.limit.unwrap_or(100).min(100_000));
    let list: Vec<_> = hits.iter().map(|h| h.session).collect();
    let resp = if p.format.as_deref() == Some("ndjson") { ndjson_body(list) } else { Json(list).into_response() };
    with_seq(&s.engine, resp)
}

async fn session(State(s): State<AppState>, Path(key): Path<String>) -> Response {
    match s.engine.session(&key) {
        Some(sess) => with_seq(&s.engine, Json(sess).into_response()),
        None => ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response(),
    }
}

#[derive(Deserialize)]
struct EventParams {
    limit: Option<usize>,
    before: Option<u64>,
    /// Event id: return a window centred on it instead of a page.
    around: Option<String>,
    max_text: Option<usize>,
    format: Option<String>,
}

async fn events(State(s): State<AppState>, Path(key): Path<String>, Query(p): Query<EventParams>) -> Response {
    if s.engine.session(&key).is_none() {
        return ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response();
    }
    if let Some(id) = p.around {
        let (limit, max) = (p.limit.unwrap_or(200).clamp(1, 10_000), p.max_text.unwrap_or(DEFAULT_MAX_TEXT));
        return search::around(s.engine.clone(), key, id, limit, max, p.format.as_deref() == Some("ndjson")).await;
    }
    let q = HistoryQuery { before: p.before, limit: p.limit.unwrap_or(200).clamp(1, 10_000) };
    let engine = s.engine.clone();
    let k = key.clone();
    let res = tokio::task::spawn_blocking(move || {
        let evs = engine.history(&k, &q)?;
        // Page sizes vary (upserts dedupe, one line can yield several events), so probe one
        // event further back: `next_before` is null exactly when nothing older exists.
        let first = evs.first().and_then(|e| e.pos);
        let more = match first {
            Some(pos) => !engine.history(&k, &HistoryQuery { before: Some(pos), limit: 1 })?.is_empty(),
            None => false,
        };
        anyhow::Ok((evs, first.filter(|_| more)))
    })
    .await;
    let (mut evs, next_before) = match res {
        Ok(Ok(page)) => page,
        Ok(Err(err)) => return ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response(),
        Err(err) => return ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };
    let max = p.max_text.unwrap_or(DEFAULT_MAX_TEXT);
    for e in &mut evs {
        e.truncate_text(max);
    }
    let resp = if p.format.as_deref() == Some("ndjson") {
        ndjson_body(evs)
    } else {
        Json(serde_json::json!({ "session": key, "events": evs, "next_before": next_before })).into_response()
    };
    with_seq(&s.engine, resp)
}

fn last_event_id(h: &HeaderMap) -> Option<u64> {
    h.get("last-event-id")?.to_str().ok()?.parse().ok()
}

async fn sse(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<stream::StreamParams>,
) -> impl IntoResponse {
    // On reconnect EventSource resends the original URL (with its stale `since`) plus
    // `Last-Event-ID`, which is the newer position.
    let since = last_event_id(&headers).or(p.since);
    let st = stream::envelopes(&s.engine, since, stream::Filter::from(&p)).map(|env| {
        let data = serde_json::to_string(&*env).unwrap_or_default();
        Ok::<_, Infallible>(SseEvent::default().id(env.seq().to_string()).event(env.type_name()).data(data))
    });
    Sse::new(st).keep_alive(KeepAlive::default())
}

async fn ndjson(State(s): State<AppState>, Query(p): Query<stream::StreamParams>) -> Response {
    let st = stream::envelopes(&s.engine, p.since, stream::Filter::from(&p)).map(|env| {
        let mut line = serde_json::to_vec(&*env).unwrap_or_default();
        line.push(b'\n');
        Ok::<_, Infallible>(Bytes::from(line))
    });
    let mut resp = Response::new(Body::from_stream(st));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson"));
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

async fn ws(State(s): State<AppState>, Query(p): Query<stream::StreamParams>, up: WebSocketUpgrade) -> Response {
    let engine = s.engine.clone();
    up.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let mut st = Box::pin(stream::envelopes(&engine, p.since, stream::Filter::from(&p)));
        loop {
            tokio::select! {
                env = st.next() => {
                    let Some(env) = env else { break };
                    let text = serde_json::to_string(&*env).unwrap_or_default();
                    if socket.send(Message::Text(text.into())).await.is_err() { break; }
                }
                msg = socket.recv() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
    })
}

/// Hello envelope helper for clients that render a snapshot first.
pub fn hello(engine: &Engine) -> Envelope {
    engine.hello()
}
