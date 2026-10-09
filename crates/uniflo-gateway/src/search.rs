//! `GET /v1/search` (full-text hits grouped by session) and `events?around=` (the transcript
//! window a hit jumps to).

use crate::{ApiError, AppState, ndjson_body, with_seq};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;
use uniflo_core::{Engine, HistoryQuery};
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
    let params = SearchParams {
        q,
        filter: p.filter,
        kinds: p.kinds.map(|k| k.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()),
        limit: p.limit.unwrap_or(20),
        offset: p.offset.unwrap_or(0),
    };
    match tokio::task::spawn_blocking(move || fts.search(&params)).await {
        Ok(Ok(r)) => with_seq(&s.engine, Json(r).into_response()),
        Ok(Err(SearchError::Query(m))) => ApiError(StatusCode::BAD_REQUEST, m).into_response(),
        Ok(Err(e)) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `limit` events centred on event `id`; same body as the paged events endpoint plus `around`.
pub(crate) async fn around(
    engine: Arc<Engine>,
    key: String,
    id: String,
    limit: usize,
    max_text: usize,
    ndjson: bool,
) -> Response {
    let (e, k, i) = (engine.clone(), key.clone(), id.clone());
    let res = tokio::task::spawn_blocking(move || {
        let Some(w) = e.around(&k, &i, limit)? else { return anyhow::Ok(None) };
        let first = w.events.first().and_then(|ev| ev.pos);
        let more = match first {
            Some(pos) => !e.history(&k, &HistoryQuery { before: Some(pos), limit: 1 })?.is_empty(),
            None => false,
        };
        anyhow::Ok(Some((w.events, first.filter(|_| more))))
    })
    .await;
    let (mut evs, next_before) = match res {
        Ok(Ok(Some(page))) => page,
        Ok(Ok(None)) => return ApiError(StatusCode::NOT_FOUND, format!("unknown event {id} in {key}")).into_response(),
        Ok(Err(err)) => return ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response(),
        Err(err) => return ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };
    for e in &mut evs {
        e.truncate_text(max_text);
    }
    let resp = if ndjson {
        ndjson_body(evs)
    } else {
        Json(serde_json::json!({ "session": key, "around": id, "events": evs, "next_before": next_before }))
            .into_response()
    };
    with_seq(&engine, resp)
}
