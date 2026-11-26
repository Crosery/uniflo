//! Optional cross-agent messaging routes (ADR-0012):
//! `GET /v1/messages`, `POST /v1/messages`, `POST /v1/messages/ack`,
//! `GET /v1/messages/stream`, `GET /v1/messages/peers`.

use crate::{ApiError, AppState, with_seq};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::convert::Infallible;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use uniflo_core::deliver::{DeliveryMode, deliver_message, list_peers};
use uniflo_core::message::{InboxQuery, MessageError, dir_matches, resolve_target};
use uniflo_schema::{AckMessagesRequest, SendMessageRequest};

const UNAVAILABLE: &str = "cross-agent message bus is unavailable on this gateway instance";

fn map_err(err: MessageError) -> ApiError {
    match err {
        MessageError::BadRequest(m) => ApiError(StatusCode::BAD_REQUEST, m),
        MessageError::NotFound(m) => ApiError(StatusCode::NOT_FOUND, m),
        MessageError::Ambiguous(m, _) => ApiError(StatusCode::CONFLICT, m),
        MessageError::Internal(m) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, m),
    }
}

#[derive(Deserialize)]
pub(crate) struct InboxParams {
    to: Option<String>,
    peer: Option<String>,
    cwd: Option<String>,
    from: Option<String>,
    unread: Option<bool>,
    undelivered: Option<bool>,
    limit: Option<usize>,
}

pub(crate) async fn list(State(s): State<AppState>, Query(p): Query<InboxParams>) -> Response {
    let Some(bus) = &s.messages else {
        return ApiError(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE.into()).into_response();
    };
    let q = InboxQuery {
        to: p.to,
        peer: p.peer,
        cwd: p.cwd,
        from: p.from,
        unread: p.unread.unwrap_or(false),
        undelivered: p.undelivered.unwrap_or(false),
        limit: p.limit.unwrap_or(50),
    };
    match bus.inbox(&s.engine, &q) {
        Ok(resp) => with_seq(&s.engine, Json(resp).into_response()),
        Err(e) => map_err(e).into_response(),
    }
}

pub(crate) async fn send(State(s): State<AppState>, Json(req): Json<SendMessageRequest>) -> Response {
    let Some(bus) = &s.messages else {
        return ApiError(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE.into()).into_response();
    };
    let mode = DeliveryMode::parse(req.deliver.as_deref());
    let wait_secs = req.wait_secs;
    let hop = req.hop.unwrap_or(0);
    let mut resp = match bus.send(&s.engine, req) {
        Ok(r) => r,
        Err(e) => return map_err(e).into_response(),
    };
    if mode != DeliveryMode::Inbox
        && let Some(target_sess) = s.engine.session_arc(&resp.target.key)
    {
        let out =
            deliver_message(s.engine.clone(), bus.clone(), target_sess, resp.message.clone(), mode, wait_secs, hop)
                .await;
        resp.delivery_channel = Some(out.channel);
        resp.delivery_detail = Some(out.detail);
        resp.reply = out.reply;
    }
    with_seq(&s.engine, Json(resp).into_response())
}

pub(crate) async fn ack(State(s): State<AppState>, Json(req): Json<AckMessagesRequest>) -> Response {
    let Some(bus) = &s.messages else {
        return ApiError(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE.into()).into_response();
    };
    match bus.ack(&s.engine, req) {
        Ok(resp) => with_seq(&s.engine, Json(resp).into_response()),
        Err(e) => map_err(e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct PeersParams {
    pid: Option<u32>,
    #[serde(rename = "self")]
    self_key: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn peers(State(s): State<AppState>, Query(p): Query<PeersParams>) -> Response {
    let Some(bus) = &s.messages else {
        return ApiError(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE.into()).into_response();
    };
    let engine = s.engine.clone();
    let bus = bus.clone();
    let limit = p.limit.unwrap_or(25);
    let resp = tokio::task::spawn_blocking(move || list_peers(&engine, &bus, p.pid, p.self_key.as_deref(), limit))
        .await
        .unwrap_or(uniflo_schema::PeersResponse { self_key: None, peers: Vec::new() });
    with_seq(&s.engine, Json(resp).into_response())
}

#[derive(Deserialize)]
pub(crate) struct StreamParams {
    to: Option<String>,
    peer: Option<String>,
    cwd: Option<String>,
}

pub(crate) async fn stream(State(s): State<AppState>, Query(p): Query<StreamParams>) -> Response {
    let Some(bus) = &s.messages else {
        return ApiError(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE.into()).into_response();
    };
    let to_key = match p.to.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(raw) => match resolve_target(&s.engine, raw) {
            Ok(sess) => Some(sess.key.clone()),
            Err(_) if raw.contains(':') && !raw.starts_with("cwd:") => Some(raw.to_owned()),
            Err(e) => return map_err(e).into_response(),
        },
        None => None,
    };
    let peer_key = match p.peer.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(raw) => match resolve_target(&s.engine, raw) {
            Ok(sess) => Some(sess.key.clone()),
            Err(_) if raw.contains(':') && !raw.starts_with("cwd:") => Some(raw.to_owned()),
            Err(e) => return map_err(e).into_response(),
        },
        None => None,
    };
    let cwd_norm = p.cwd.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(|c| {
        let n = c.replace('\\', "/");
        let t = n.trim_end_matches('/');
        if t.is_empty() && n.starts_with('/') { "/".to_owned() } else { t.to_owned() }
    });
    let engine = s.engine.clone();
    let rx = bus.subscribe();
    let st = BroadcastStream::new(rx).filter_map(move |item| {
        let msg = item.ok()?;
        if let Some(ref want) = to_key
            && msg.to != *want
        {
            return None;
        }
        if let Some(ref want_peer) = peer_key
            && msg.to != *want_peer
            && msg.from != *want_peer
        {
            return None;
        }
        if let Some(ref want_cwd) = cwd_norm {
            let ok = engine
                .session_arc(&msg.to)
                .and_then(|sess| sess.cwd.clone())
                .is_some_and(|c| dir_matches(&c, want_cwd));
            if !ok {
                return None;
            }
        }
        let data = serde_json::to_string(&*msg).unwrap_or_default();
        Some(Ok::<_, Infallible>(SseEvent::default().id(msg.id.clone()).event("message").data(data)))
    });
    Sse::new(st).keep_alive(KeepAlive::default()).into_response()
}
