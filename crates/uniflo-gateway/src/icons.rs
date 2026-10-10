//! Harness brand icons: `GET /v1/harnesses/{id}/icon.svg` and `Harness.icon`. The monochrome
//! lobe-icons symbols (MIT) and vendor marks live once, in the demo page's `#harness-icons` sprite; the gateway
//! cuts them out of the embedded page so the page and the API never drift apart.

use crate::ApiError;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::sync::OnceLock;

const PAGE: &str = include_str!("index.html");

/// Inner markup of `<symbol id="hi-{id}" …>` by harness id.
fn symbols() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut out = HashMap::new();
        let start = PAGE.find("<svg id=\"harness-icons\"").unwrap_or(PAGE.len());
        let mut rest = &PAGE[start..];
        rest = &rest[..rest.find("</svg>").unwrap_or(0)];
        while let Some(i) = rest.find("<symbol id=\"hi-") {
            rest = &rest[i + "<symbol id=\"hi-".len()..];
            let (Some(q), Some(open), Some(close)) = (rest.find('"'), rest.find('>'), rest.find("</symbol>")) else {
                break;
            };
            if q < open && open < close {
                out.insert(&rest[..q], &rest[open + 1..close]);
            }
            rest = &rest[close..];
        }
        out
    })
}

/// `/v1/harnesses/{id}/icon.svg` when `id` has a brand icon.
pub fn path(id: &str) -> Option<String> {
    symbols().contains_key(id).then(|| format!("/v1/harnesses/{id}/icon.svg"))
}

/// Standalone SVG document; paths use `currentColor` (black in an `<img>`; inline it or use it
/// as a CSS mask to follow the theme).
pub fn svg(id: &str) -> Option<String> {
    symbols().get(id).map(|inner| {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" fill=\"currentColor\" fill-rule=\"evenodd\">{inner}</svg>"
        )
    })
}

pub(crate) async fn icon(Path(id): Path<String>) -> Response {
    match svg(&id) {
        Some(body) => {
            ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "public, max-age=86400")], body)
                .into_response()
        }
        None => ApiError(StatusCode::NOT_FOUND, format!("no icon for harness {id}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_symbols_become_standalone_documents() {
        assert!(symbols().len() >= 20, "{:?}", symbols().keys());
        let s = svg("claude").unwrap();
        assert!(s.starts_with("<svg xmlns=") && s.ends_with("/></svg>"), "{s}");
        assert!(s.contains("<path d=") || s.contains("<path clip-rule="), "{s}");
        assert!(!s.contains("<symbol"));
        assert_eq!(path("codex").as_deref(), Some("/v1/harnesses/codex/icon.svg"));
        for id in ["pi", "omp", "workbuddy", "factory", "reasonix", "dsh", "zcode", "craft"] {
            assert!(svg(id).is_some_and(|s| s.contains("<path")), "{id}");
        }
        assert_eq!(path("prime"), None, "no brand icon: clients draw a letter block");
        assert_eq!(svg("../index"), None);
    }
}
