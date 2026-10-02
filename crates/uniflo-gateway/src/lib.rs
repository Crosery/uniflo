//! HTTP gateway over the live index.
//!
//! | route | |
//! |---|---|
//! | `GET /v1/health` | liveness, seq, counts |
//! | `GET /v1/harnesses` | supported harnesses on this machine |
//! | `GET /v1/sessions?q=&limit=&format=ndjson` | search (see `uniflo-search` syntax) |
//! | `GET /v1/sessions/{key}` | one session |
//! | `GET /v1/sessions/{key}/events?limit=&before=&max_text=&format=ndjson` | transcript, newest page first |
//! | `GET /v1/stream` (SSE) · `/v1/stream.ndjson` · `/v1/ws` | live envelopes; `since`, `session`, `harness`, `types`, `kinds`, `max_text` |
//! | `GET /v1/stats` | engine counters, unknown discriminators |
//! | `GET /demo` | bundled single-page demo client (`examples/web/index.html`) |
//!
//! Snapshots carry `x-uniflo-seq`; subscribe with `since=<that>` for a gap-free view.
//! Security: loopback Host only (DNS-rebinding guard), browser Origins limited to
//! loopback + `--cors-origin`, optional bearer token (`Authorization` or `?token=`).

mod guard;
mod stream;

pub use guard::GuardOptions;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, HistoryQuery};
use uniflo_schema::{Envelope, SCHEMA_VERSION};
use uniflo_search::{Query as SearchQuery, search};

pub const DEFAULT_MAX_TEXT: usize = 32 * 1024;

#[derive(Clone)]
struct AppState {
    engine: Arc<Engine>,
}

pub fn router(engine: Arc<Engine>, guard: GuardOptions) -> Router {
    let state = AppState { engine };
    Router::new()
        .route("/", get(index))
        .route("/demo", get(demo))
        .route("/v1/health", get(health))
        .route("/v1/harnesses", get(harnesses))
        .route("/v1/stats", get(stats))
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{key}", get(session))
        .route("/v1/sessions/{key}/events", get(events))
        .route("/v1/stream", get(sse))
        .route("/v1/stream.ndjson", get(ndjson))
        .route("/v1/ws", get(ws))
        .layer(axum::middleware::from_fn_with_state(Arc::new(guard), guard::guard))
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
        "endpoints": ["/demo", "/v1/health", "/v1/harnesses", "/v1/sessions", "/v1/sessions/{key}", "/v1/sessions/{key}/events", "/v1/stream", "/v1/stream.ndjson", "/v1/ws", "/v1/stats"],
    }))
}

async fn demo() -> Html<&'static str> {
    Html(include_str!("../../../examples/web/index.html"))
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
    }))
}

async fn harnesses(State(s): State<AppState>) -> impl IntoResponse {
    Json(s.engine.harnesses())
}

async fn stats(State(s): State<AppState>) -> impl IntoResponse {
    Json(s.engine.stats())
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
    max_text: Option<usize>,
    format: Option<String>,
}

async fn events(State(s): State<AppState>, Path(key): Path<String>, Query(p): Query<EventParams>) -> Response {
    if s.engine.session(&key).is_none() {
        return ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response();
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
