//! Usage and pricing routes: `/v1/usage`, `/v1/sessions/{key}/usage`, `/v1/models`,
//! `/v1/pricing`. [`UsageParams::report`] is also what `uniflo usage --local` runs, so the
//! CLI and REST answer from the same code.

use super::{ApiError, AppState, with_seq};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uniflo_core::Engine;
use uniflo_core::usage::report::{self, GroupBy, Sort, expand_home, parse_time};
use uniflo_core::usage::tz::Tz;
use uniflo_core::util::{home, now_ms};
use uniflo_schema::{Session, UsageReport};
use uniflo_search::{Query as SearchQuery, search};

/// `GET /v1/usage` query parameters (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageParams {
    /// Dimension, default `harness`.
    pub group_by: Option<String>,
    /// Session search syntax; its `since:` / `before:` become the event-time window.
    pub q: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub tz: Option<String>,
    pub under: Option<String>,
    pub depth: Option<usize>,
    pub limit: Option<usize>,
    pub sort: Option<String>,
    /// Only steps of this `group_by=model` key (catalog id or base name).
    #[serde(default)]
    pub model: Option<String>,
}

impl UsageParams {
    /// Run the aggregation; `Err` carries a message for a 400.
    pub fn report(&self, engine: &Engine) -> Result<UsageReport, String> {
        let now = now_ms();
        let tz = Tz::parse(self.tz.as_deref().unwrap_or(""))?;
        let home = home().to_string_lossy().into_owned();
        let mut sq = SearchQuery::parse(&expand_home(self.q.as_deref().unwrap_or(""), &home), now);
        let (q_since, q_until) = sq.take_window();
        let since = self.since.as_deref().map(|v| parse_time(v, now, &tz)).transpose()?;
        let until = self.until.as_deref().map(|v| parse_time(v, now, &tz)).transpose()?;
        let q = report::Query {
            group_by: GroupBy::parse(self.group_by.as_deref().unwrap_or("harness"))?,
            since: [since, q_since].into_iter().flatten().max(),
            until: [until, q_until].into_iter().flatten().min(),
            tz,
            under: self.under.as_deref().filter(|u| !u.is_empty()).map(|u| tilde(u, &home)),
            depth: self.depth,
            limit: self.limit,
            sort: Sort::parse(self.sort.as_deref().unwrap_or(""))?,
            model: self.model.clone().filter(|m| !m.is_empty()),
        };
        let all = engine.sessions();
        let picked: Vec<&Session> = search(&all, &sq, usize::MAX).into_iter().map(|h| h.session).collect();
        Ok(engine.usage_report(&picked, &q))
    }
}

fn tilde(p: &str, home: &str) -> String {
    match p.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => p.to_owned(),
    }
}

pub(super) async fn usage(State(s): State<AppState>, Query(p): Query<UsageParams>) -> Response {
    let engine = s.engine.clone();
    match tokio::task::spawn_blocking(move || p.report(&engine)).await {
        Ok(Ok(r)) => with_seq(&s.engine, Json(r).into_response()),
        Ok(Err(msg)) => ApiError(StatusCode::BAD_REQUEST, msg).into_response(),
        Err(err) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

pub(super) async fn session_usage(State(s): State<AppState>, Path(key): Path<String>) -> Response {
    if s.engine.session(&key).is_none() {
        return ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response();
    }
    let engine = s.engine.clone();
    match tokio::task::spawn_blocking(move || engine.session_usage(&key)).await {
        Ok(Ok(d)) => with_seq(&s.engine, Json(d).into_response()),
        Ok(Err(err)) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response(),
        Err(err) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct ModelParams {
    q: Option<String>,
}

pub(super) async fn models(State(s): State<AppState>, Query(p): Query<ModelParams>) -> Response {
    let engine = s.engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let home = home().to_string_lossy().into_owned();
        let sq = SearchQuery::parse(&expand_home(p.q.as_deref().unwrap_or(""), &home), now_ms());
        let all = engine.sessions();
        let picked: Vec<&Session> = search(&all, &sq, usize::MAX).into_iter().map(|h| h.session).collect();
        engine.models(&picked)
    })
    .await;
    match res {
        Ok(m) => Json(m).into_response(),
        Err(err) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

pub(super) async fn pricing(State(s): State<AppState>) -> Response {
    let engine = s.engine.clone();
    match tokio::task::spawn_blocking(move || engine.pricing_status()).await {
        Ok(st) => Json(st).into_response(),
        Err(err) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}
