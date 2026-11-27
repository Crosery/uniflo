//! `GET /v1/search` (full-text hits grouped by session) and `events?around=` (the transcript
//! window a hit jumps to).

use crate::cache::{CacheKey, CachedBody, NS_SEARCH, query_has_relative_time};
use crate::{ApiError, AppState, with_seq};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uniflo_core::HistoryQuery;
use uniflo_search::fts::{SearchError, SearchParams};

pub const DISABLED: &str =
    "full-text search is disabled: the daemon runs with --no-fts (or the index failed to open, see its log)";

#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    q: Option<String>,
    filter: Option<String>,
    /// Comma-separated event kinds.
    kinds: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub(crate) async fn search(State(s): State<AppState>, Query(p): Query<SearchQuery>) -> Response {
    let Some(fts) = s.fts.clone() else {
        return ApiError(StatusCode::SERVICE_UNAVAILABLE, DISABLED.into()).into_response();
    };
    let q = p.q.unwrap_or_default();
    if q.trim().is_empty() {
        return ApiError(StatusCode::BAD_REQUEST, "missing q".into()).into_response();
    }
    let limit = p.limit.unwrap_or(20);
    let offset = p.offset.unwrap_or(0);
    let filter_raw = p.filter.as_deref().unwrap_or("");
    let kinds_raw = p.kinds.as_deref().unwrap_or("");
    let gen_tag = (fts.generation(), s.engine.state_gen(), 0);
    let key_str = format!("{q}\0{filter_raw}\0{kinds_raw}\0{limit}\0{offset}");
    let cacheable = !query_has_relative_time(filter_raw);
    let cache_key = CacheKey { ns: NS_SEARCH, gen_tag, key: key_str.clone() };
    if cacheable && let Some(hit) = s.cache.get(&cache_key) {
        return with_seq(&s.engine, hit.into_response());
    }
    let params = SearchParams {
        q,
        filter: p.filter,
        kinds: p.kinds.map(|k| k.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()),
        limit,
        offset,
    };
    let flight_key = format!("fts:{}:{}:{key_str}", gen_tag.0, gen_tag.1);
    let cache = s.cache.clone();
    let permits = s.cpu_permits.clone();
    let res = s
        .flights
        .run(flight_key, move || async move {
            let _permit = permits.acquire_owned().await.ok();
            let out = match tokio::task::spawn_blocking(move || fts.search(&params)).await {
                Ok(Ok(r)) => CachedBody::json(&r),
                Ok(Err(SearchError::Query(m))) => Err((StatusCode::BAD_REQUEST, m)),
                Ok(Err(e)) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
            };
            if cacheable && let Ok(ref body) = out {
                cache.insert(cache_key, body.clone());
            }
            out
        })
        .await;
    match res {
        Ok(body) => with_seq(&s.engine, body.into_response()),
        Err((code, msg)) => ApiError(code, msg).into_response(),
    }
}

/// `limit` events centred on event `id`; same body as the paged events endpoint plus `around`.
pub(crate) async fn around(
    s: &AppState,
    key: String,
    id: String,
    limit: usize,
    max_text: usize,
    ndjson: bool,
) -> Response {
    let flight_key = format!("around:{key}:{id}:{limit}:{max_text}:{ndjson}");
    let engine = s.engine.clone();
    let permits = s.io_permits.clone();
    let res = s
        .flights
        .run(flight_key, move || async move {
            let _permit = permits.acquire_owned().await.ok();
            let (k, i) = (key.clone(), id.clone());
            let page = tokio::task::spawn_blocking(move || {
                let Some(w) = engine.around(&k, &i, limit)? else { return anyhow::Ok(None) };
                let first = w.events.first().and_then(|ev| ev.pos);
                let more = match first {
                    Some(0) | None => false,
                    Some(pos) => !engine.history(&k, &HistoryQuery { before: Some(pos), limit: 1 })?.is_empty(),
                };
                let mut evs = w.events;
                for e in &mut evs {
                    e.truncate_text(max_text);
                }
                let next_before = first.filter(|_| more);
                if ndjson {
                    anyhow::Ok(Some(CachedBody::ndjson(evs)))
                } else {
                    let body = serde_json::to_vec(&serde_json::json!({
                        "session": k,
                        "around": i,
                        "events": evs,
                        "next_before": next_before,
                    }))?;
                    anyhow::Ok(Some(CachedBody { content_type: "application/json", body: Bytes::from(body) }))
                }
            })
            .await;
            match page {
                Ok(Ok(Some(body))) => Ok(body),
                Ok(Ok(None)) => Err((StatusCode::NOT_FOUND, format!("unknown event {id} in {key}"))),
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
