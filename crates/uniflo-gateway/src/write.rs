//! Guard for endpoints that change anything (session cleanup, archive deletion, and any later
//! write endpoint). On top of the read guard (`guard.rs`), a write request must
//!
//! - name a loopback Host (`--allow-host` never extends to writes);
//! - carry no Origin, or a loopback one, or one listed in `--cors-origin` (`*` does not count);
//! - send `X-Uniflo-Write: 1` (a custom header, so a browser has to pass a CORS preflight);
//! - carry the token when one is configured.
//!
//! Anything else answers 403, and so does every write while the daemon runs `--read-only`.
//! Mount write routes on their own router layered with [`require_write`]:
//! `Router::new().route(…).route_layer(from_fn_with_state(guard, write::require_write))`.
//! Rules and rationale: `docs/api.md#写接口`, ADR-0006.

use crate::guard::{GuardOptions, deny, host_name, is_loopback, token_ok};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

pub const WRITE_HEADER: &str = "x-uniflo-write";

pub async fn require_write(State(g): State<Arc<GuardOptions>>, req: Request, next: Next) -> Response {
    match check(&g, &req) {
        Ok(()) => next.run(req).await,
        Err(why) => deny(StatusCode::FORBIDDEN, why),
    }
}

/// The write conditions; the error says which one failed.
pub fn check(g: &GuardOptions, req: &Request) -> Result<(), &'static str> {
    if g.read_only {
        return Err("read-only daemon: write endpoints are disabled");
    }
    let h = req.headers();
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !is_loopback(host_name(host)) {
        return Err("write requests need a loopback host");
    }
    if let Some(o) = h.get(header::ORIGIN) {
        let ok = o.to_str().is_ok_and(|o| {
            g.cors_origins.iter().any(|c| c == o)
                || match o.split_once("://") {
                    Some(("http" | "https", rest)) => is_loopback(host_name(rest.split('/').next().unwrap_or(rest))),
                    _ => false,
                }
        });
        if !ok {
            return Err("origin not allowed to write");
        }
    }
    if h.get(WRITE_HEADER).is_none_or(|v| v.as_bytes() != b"1") {
        return Err("write requests need the header X-Uniflo-Write: 1");
    }
    if let Some(want) = &g.token
        && !token_ok(req, want)
    {
        return Err("missing or wrong token");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn req(headers: &[(&str, &str)]) -> Request {
        let mut b = axum::http::Request::post("/v1/cleanup/plan");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn every_condition_is_required() {
        let g = GuardOptions { cors_origins: vec!["*".into(), "tauri://localhost".into()], ..Default::default() };
        let ok = [("host", "127.0.0.1:7311"), (WRITE_HEADER, "1")];
        assert_eq!(check(&g, &req(&ok)), Ok(()));
        assert!(check(&g, &req(&[("host", "127.0.0.1:7311")])).is_err(), "header missing");
        assert!(check(&g, &req(&[("host", "lan.example:7311"), (WRITE_HEADER, "1")])).is_err(), "non-loopback host");
        let origin = |o| req(&[("host", "localhost:7311"), (WRITE_HEADER, "1"), ("origin", o)]);
        assert!(check(&g, &origin("http://127.0.0.1:7311")).is_ok());
        assert!(check(&g, &origin("tauri://localhost")).is_ok(), "explicitly allowed");
        assert!(check(&g, &origin("https://evil.example")).is_err(), "`*` does not open writes");
        let tok = GuardOptions { token: Some("s".into()), ..Default::default() };
        assert!(check(&tok, &req(&ok)).is_err());
        assert!(check(&tok, &req(&[ok[0], ok[1], ("authorization", "Bearer s")])).is_ok());
        let ro = GuardOptions { read_only: true, ..Default::default() };
        assert!(check(&ro, &req(&ok)).is_err());
    }
}
