//! Session cleanup and archive endpoints over [`uniflo_core::cleanup::Cleanup`]:
//! `POST /v1/cleanup/plan`, `POST /v1/cleanup/plans/{id}/execute`, `DELETE /v1/archive/{key}`
//! (all behind [`crate::write::require_write`]) and `GET /v1/archive`.

use crate::{ApiError, AppState, with_seq};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use uniflo_core::Engine;
use uniflo_core::cleanup::ExecError;
use uniflo_core::util::now_ms;
use uniflo_schema::cleanup::CleanupRequest;
use uniflo_search::{Query as SearchQuery, search};

pub const DISABLED: &str = "session cleanup is unavailable in this gateway (no archive directory)";

/// Keys named in the request plus every session the query matches (`docs/search.md`).
pub fn select(engine: &Engine, req: &CleanupRequest) -> Vec<String> {
    let mut keys = req.sessions.clone();
    if let Some(q) = req.q.as_deref().filter(|q| !q.trim().is_empty()) {
        let all = engine.sessions();
        keys.extend(search(&all, &SearchQuery::parse(q, now_ms()), usize::MAX).iter().map(|h| h.session.key.clone()));
    }
    keys
}

fn disabled() -> Response {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, DISABLED.into()).into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

pub(crate) async fn plan(State(s): State<AppState>, body: Result<Json<CleanupRequest>, JsonRejection>) -> Response {
    let Some(c) = s.cleanup.clone() else { return disabled() };
    let req = match body {
        Ok(Json(r)) => r,
        Err(e) => return ApiError(StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    if req.sessions.is_empty() && req.q.as_deref().is_none_or(|q| q.trim().is_empty()) {
        return ApiError(StatusCode::BAD_REQUEST, "give `sessions` (keys) or `q` (a session query)".into())
            .into_response();
    }
    let engine = s.engine.clone();
    match tokio::task::spawn_blocking(move || c.plan(&select(&engine, &req))).await {
        Ok(p) => with_seq(&s.engine, Json(p).into_response()),
        Err(e) => internal(e),
    }
}

pub(crate) async fn execute(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(c) = s.cleanup.clone() else { return disabled() };
    match tokio::task::spawn_blocking(move || c.execute(&id)).await {
        Ok(Ok(r)) => with_seq(&s.engine, Json(r).into_response()),
        Ok(Err(ExecError::Unknown)) => {
            ApiError(StatusCode::NOT_FOUND, "unknown plan (or it already ran)".into()).into_response()
        }
        Ok(Err(ExecError::Expired)) => ApiError(StatusCode::GONE, "plan expired; plan again".into()).into_response(),
        Err(e) => internal(e),
    }
}

pub(crate) async fn archives(State(s): State<AppState>) -> Response {
    match &s.cleanup {
        Some(c) => with_seq(&s.engine, Json(c.archives()).into_response()),
        None => disabled(),
    }
}

pub(crate) async fn remove(State(s): State<AppState>, Path(key): Path<String>) -> Response {
    let Some(c) = s.cleanup.clone() else { return disabled() };
    let k = key.clone();
    match tokio::task::spawn_blocking(move || c.remove_archive(&k)).await {
        Ok(Ok(Some(r))) => with_seq(&s.engine, Json(r).into_response()),
        Ok(Ok(None)) => ApiError(StatusCode::NOT_FOUND, format!("no archive for {key}")).into_response(),
        Ok(Err(e)) => internal(format!("{e:#}")),
        Err(e) => internal(e),
    }
}
