//! Request guard: loopback Host (DNS-rebinding defense), Origin allow-list, optional token.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct GuardOptions {
    /// Required bearer token (`Authorization: Bearer …` or `?token=`). `None` = open on loopback.
    pub token: Option<String>,
    /// Extra browser origins allowed besides loopback ones (`*` allows any).
    pub cors_origins: Vec<String>,
    /// Extra Host header values accepted besides loopback names.
    pub allowed_hosts: Vec<String>,
}

fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    host.rsplit_once(':').map_or(host, |(h, port)| if port.chars().all(|c| c.is_ascii_digit()) { h } else { host })
}

fn is_loopback(name: &str) -> bool {
    matches!(name, "localhost" | "127.0.0.1" | "::1") || name.ends_with(".localhost")
}

fn origin_allowed(origin: &str, g: &GuardOptions) -> bool {
    if g.cors_origins.iter().any(|o| o == "*" || o == origin) {
        return true;
    }
    match origin.split_once("://") {
        Some(("http" | "https", rest)) => is_loopback(host_name(rest.split('/').next().unwrap_or(rest))),
        _ => false,
    }
}

fn token_ok(req: &Request, want: &str) -> bool {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_eq(t, want));
    bearer
        || req.uri().query().is_some_and(|q| {
            q.split('&').filter_map(|kv| kv.split_once('=')).any(|(k, v)| k == "token" && constant_eq(v, want))
        })
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn deny(code: StatusCode, msg: &str) -> Response {
    (code, axum::Json(serde_json::json!({ "error": msg }))).into_response()
}

fn cors_headers(h: &mut HeaderMap, origin: &HeaderValue) {
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, OPTIONS"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, last-event-id, content-type"),
    );
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("x-uniflo-seq"));
}

pub async fn guard(State(g): State<Arc<GuardOptions>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let name = host_name(host);
    if !is_loopback(name) && !g.allowed_hosts.iter().any(|h| h == host || h == name) {
        return deny(StatusCode::FORBIDDEN, "host not allowed");
    }
    let origin = req.headers().get(header::ORIGIN).cloned();
    if let Some(o) = &origin
        && !o.to_str().is_ok_and(|s| origin_allowed(s, &g))
    {
        return deny(StatusCode::FORBIDDEN, "origin not allowed");
    }
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        if let Some(o) = &origin {
            cors_headers(resp.headers_mut(), o);
        }
        return resp;
    }
    if let Some(want) = &g.token
        && !token_ok(&req, want)
    {
        return deny(StatusCode::UNAUTHORIZED, "missing or wrong token");
    }
    let mut resp = next.run(req).await;
    if let Some(o) = &origin {
        cors_headers(resp.headers_mut(), o);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing() {
        assert_eq!(host_name("127.0.0.1:7311"), "127.0.0.1");
        assert_eq!(host_name("[::1]:7311"), "::1");
        assert_eq!(host_name("localhost"), "localhost");
        assert!(is_loopback("app.localhost"));
        assert!(!is_loopback("evil.com"));
    }

    #[test]
    fn origins() {
        let g = GuardOptions { cors_origins: vec!["tauri://localhost".into()], ..Default::default() };
        assert!(origin_allowed("http://localhost:5173", &g));
        assert!(origin_allowed("http://127.0.0.1:3000", &g));
        assert!(origin_allowed("tauri://localhost", &g));
        assert!(!origin_allowed("https://evil.com", &g));
        assert!(!origin_allowed("http://localhost.evil.com", &g));
        assert!(!origin_allowed("null", &g));
    }
}
