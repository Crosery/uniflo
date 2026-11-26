//! HTTP gateway over the live index.
//!
//! | route | |
//! |---|---|
//! | `GET /v1/health` | liveness, seq, counts |
//! | `GET /v1/harnesses` | supported harnesses on this machine |
//! | `GET /v1/harnesses/{id}/icon.svg` | monochrome brand icon (`image/svg+xml`) |
//! | `GET /v1/sessions?q=&limit=&format=ndjson` | search (see `uniflo-search` syntax) |
//! | `GET /v1/sessions/{key}` | one session |
//! | `GET /v1/sessions/{key}/events?limit=&before=&oldest=&max_text=&format=ndjson` | transcript, newest page by default or oldest page |
//! | `GET /v1/sessions/{key}/events?around=<event id>&limit=` | transcript window centred on one event |
//! | `GET /v1/search?q=&filter=&kinds=&limit=&offset=` | full-text hits grouped by session (503 without an index) |
//! | `GET /v1/stream` (SSE) · `/v1/stream.ndjson` · `/v1/ws` | live envelopes; `since`, `session`, `harness`, `types`, `kinds`, `max_text` |
//! | `GET /v1/stats` | engine counters, unknown discriminators |
//! | `GET /v1/usage?group_by=&q=&since=&until=&tz=&under=&depth=&limit=&sort=` | token / cost aggregation |
//! | `GET /v1/sessions/{key}/usage` | per-step usage, per-turn totals |
//! | `GET /v1/sessions/{key}/trajectory` | turns → steps → items chain, sub-sessions hung on their calls |
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
mod cache;
pub mod cleanup;
mod guard;
pub mod icons;
mod message;
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
use cache::{CacheKey, CachedBody, NS_SESSIONS, ResponseCache, Singleflight, query_has_relative_time};
use futures_util::StreamExt;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use uniflo_core::cleanup::Cleanup;
use uniflo_core::message::MessageBus;
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, HistoryQuery};
use uniflo_schema::{Envelope, SCHEMA_VERSION};
use uniflo_search::fts::Fts;
use uniflo_search::{Query as SearchQuery, search_refs};

pub const DEFAULT_MAX_TEXT: usize = 32 * 1024;

#[derive(Clone)]
struct AppState {
    engine: Arc<Engine>,
    fts: Option<Arc<Fts>>,
    cleanup: Option<Arc<Cleanup>>,
    messages: Option<Arc<MessageBus>>,
    read_only: bool,
    cache: Arc<ResponseCache>,
    flights: Arc<Singleflight>,
    /// Caps concurrent CPU-heavy `spawn_blocking` tasks so bursts of cache-miss fuzzy/usage/FTS
    /// queries never starve Tokio worker threads serving `/v1/health`, cached lists, and streams.
    cpu_permits: Arc<tokio::sync::Semaphore>,
}

/// Optional services behind the router; a missing one answers 503.
#[derive(Default)]
pub struct Services {
    pub fts: Option<Arc<Fts>>,
    pub cleanup: Option<Arc<Cleanup>>,
    pub messages: Option<Arc<MessageBus>>,
}

/// Router without a full-text index: `/v1/search` answers 503.
pub fn router(engine: Arc<Engine>, guard: GuardOptions) -> Router {
    router_with_fts(engine, guard, None)
}

pub fn router_with_fts(engine: Arc<Engine>, guard: GuardOptions, fts: Option<Arc<Fts>>) -> Router {
    router_with(engine, guard, Services { fts, ..Default::default() })
}

pub fn router_with(engine: Arc<Engine>, guard: GuardOptions, services: Services) -> Router {
    let cpu_budget = std::thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(2).clamp(4, 8));
    let state = AppState {
        engine,
        fts: services.fts,
        cleanup: services.cleanup,
        messages: services.messages,
        read_only: guard.read_only,
        cache: Arc::new(ResponseCache::default()),
        flights: Arc::new(Singleflight::default()),
        cpu_permits: Arc::new(tokio::sync::Semaphore::new(cpu_budget)),
    };
    let guard = Arc::new(guard);
    Router::new()
        .route("/", get(index))
        .route("/demo", get(demo))
        .route("/v1/health", get(health))
        .route("/v1/harnesses", get(harnesses))
        .route("/v1/harnesses/{id}/icon.svg", get(icons::icon))
        .route("/v1/stats", get(stats))
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{key}", get(session))
        .route("/v1/sessions/{key}/events", get(events))
        .route("/v1/sessions/{key}/usage", get(usage::session_usage))
        .route("/v1/sessions/{key}/trajectory", get(usage::session_trajectory))
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
        .route("/v1/messages", get(message::list).post(message::send))
        .route("/v1/messages/ack", post(message::ack))
        .route("/v1/messages/stream", get(message::stream))
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

async fn index() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "uniflo",
        "version": env!("CARGO_PKG_VERSION"),
        "schema": SCHEMA_VERSION,
        "endpoints": ["/demo", "/v1/health", "/v1/harnesses", "/v1/harnesses/{id}/icon.svg", "/v1/sessions", "/v1/sessions/{key}", "/v1/sessions/{key}/events", "/v1/search", "/v1/stream", "/v1/stream.ndjson", "/v1/ws", "/v1/stats", "/v1/usage", "/v1/sessions/{key}/usage", "/v1/sessions/{key}/trajectory", "/v1/models", "/v1/pricing", "/v1/cleanup/plan", "/v1/cleanup/plans/{id}/execute", "/v1/archive", "/v1/archive/{key}", "/v1/sessions/{key}/resume", "/v1/sessions/{key}/open-terminal", "/v1/memory", "/v1/memory/file", "/v1/messages", "/v1/messages/ack", "/v1/messages/stream"],
    }))
}

async fn demo() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-cache, must-revalidate")], Html(include_str!("index.html")))
}

async fn health(State(s): State<AppState>) -> impl IntoResponse {
    let st = s.engine.health_snapshot();
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
    let mut list = s.engine.harnesses();
    for h in &mut list {
        h.icon = icons::path(&h.id);
    }
    Json(list)
}

async fn stats(State(s): State<AppState>) -> impl IntoResponse {
    let engine = s.engine.clone();
    let fts = s.fts.clone();
    let v = tokio::task::spawn_blocking(move || {
        let mut v = serde_json::to_value(engine.stats()).unwrap_or_default();
        v["fts"] = serde_json::to_value(fts.as_ref().map(|f| f.status())).unwrap_or_default();
        v
    })
    .await
    .unwrap_or_default();
    Json(v)
}

#[derive(Deserialize)]
struct ListParams {
    q: Option<String>,
    limit: Option<usize>,
    format: Option<String>,
}

async fn sessions(State(s): State<AppState>, Query(p): Query<ListParams>) -> Response {
    let q_raw = p.q.as_deref().unwrap_or("");
    let limit = p.limit.unwrap_or(100).min(100_000);
    let ndjson = p.format.as_deref() == Some("ndjson");
    let snap = s.engine.sessions_snapshot();
    let cacheable = !query_has_relative_time(q_raw);
    let cache_key =
        CacheKey { ns: NS_SESSIONS, gen_tag: (snap.generation, 0, 0), key: format!("{q_raw}\0{limit}\0{ndjson}") };
    if cacheable && let Some(hit) = s.cache.get(&cache_key) {
        return with_seq(&s.engine, hit.into_response());
    }
    let q = SearchQuery::parse(q_raw, now_ms());
    let has_text = !q.text.is_empty();
    let flight_key = format!("sess:{}:{q_raw}\0{limit}\0{ndjson}", snap.generation);
    let cache = s.cache.clone();
    let cache_key_save = cache_key.clone();
    let permits = s.cpu_permits.clone();
    let res = s
        .flights
        .run(flight_key, move || async move {
            let compute = move || {
                let hits = search_refs(&snap.sorted, &q, limit);
                let list: Vec<_> = hits.iter().map(|h| h.session).collect();
                if ndjson { Ok(CachedBody::ndjson(list)) } else { CachedBody::json(&list) }
            };
            let out = if has_text {
                let _permit = permits.acquire_owned().await.ok();
                match tokio::task::spawn_blocking(compute).await {
                    Ok(r) => r,
                    Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                }
            } else {
                compute()
            };
            if cacheable && let Ok(ref body) = out {
                cache.insert(cache_key_save, body.clone());
            }
            out
        })
        .await;
    match res {
        Ok(body) => with_seq(&s.engine, body.into_response()),
        Err((code, msg)) => ApiError(code, msg).into_response(),
    }
}

async fn session(State(s): State<AppState>, Path(key): Path<String>) -> Response {
    match s.engine.session_arc(&key) {
        Some(sess) => with_seq(&s.engine, Json(&*sess).into_response()),
        None => ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response(),
    }
}

#[derive(Deserialize)]
struct EventParams {
    limit: Option<usize>,
    before: Option<u64>,
    /// Return the oldest events instead of the newest page.
    oldest: Option<bool>,
    /// Event id: return a window centred on it instead of a page.
    around: Option<String>,
    max_text: Option<usize>,
    format: Option<String>,
}

async fn events(State(s): State<AppState>, Path(key): Path<String>, Query(p): Query<EventParams>) -> Response {
    if s.engine.session_arc(&key).is_none() {
        return ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response();
    }
    if let Some(id) = p.around {
        let (limit, max) = (p.limit.unwrap_or(200).clamp(1, 10_000), p.max_text.unwrap_or(DEFAULT_MAX_TEXT));
        return search::around(&s, key, id, limit, max, p.format.as_deref() == Some("ndjson")).await;
    }
    let limit = p.limit.unwrap_or(200).clamp(1, 10_000);
    let oldest = p.oldest.unwrap_or(false);
    let max = p.max_text.unwrap_or(DEFAULT_MAX_TEXT);
    let ndjson = p.format.as_deref() == Some("ndjson");
    let q = HistoryQuery { before: p.before, limit };
    let flight_key = format!("ev:{key}:{limit}:{:?}:{oldest}:{max}:{ndjson}", p.before);
    let engine = s.engine.clone();
    let res = s
        .flights
        .run(flight_key, move || async move {
            let k = key.clone();
            let page = tokio::task::spawn_blocking(move || {
                let (mut evs, next_before) =
                    if oldest { (engine.oldest(&k, limit)?, None) } else { engine.history_page(&k, &q)? };
                for e in &mut evs {
                    e.truncate_text(max);
                }
                if ndjson {
                    anyhow::Ok(CachedBody::ndjson(evs))
                } else {
                    let body = serde_json::to_vec(&serde_json::json!({
                        "session": k,
                        "events": evs,
                        "next_before": next_before,
                    }))?;
                    anyhow::Ok(CachedBody { content_type: "application/json", body: Bytes::from(body) })
                }
            })
            .await;
            match page {
                Ok(Ok(body)) => Ok(body),
                Ok(Err(err)) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))),
                Err(err) => Err((StatusCode::INTERNAL_SERVER_ERROR, err.to_string())),
            }
        })
        .await;
    match res {
        Ok(body) => with_seq(&s.engine, body.into_response()),
        Err((code, msg)) => ApiError(code, msg).into_response(),
    }
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
