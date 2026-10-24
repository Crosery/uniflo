//! [`Error`]: the one error type every fallible [`crate::Client`] method returns. `std`-only
//! (no `anyhow`), since this crate depends on nothing but `uniflo-schema`, `serde` and
//! `serde_json`.

use std::fmt;
use std::io;

/// Everything that can go wrong talking to the Uniflo daemon.
#[derive(Debug)]
pub enum Error {
    /// Could not establish the TCP connection (daemon not running, wrong port, DNS failure…).
    Connect(io::Error),
    /// An I/O failure after the connection was established (write, read, a stream that broke).
    Io(io::Error),
    /// A non-2xx HTTP response.
    Http {
        /// The HTTP status code.
        status: u16,
        /// The gateway's `{"error": "..."}` text when the body parsed as JSON with that shape,
        /// otherwise the raw response body.
        message: String,
    },
    /// The response body was not valid JSON for the type the caller asked for.
    Decode {
        /// The request path (and query), to make it obvious which call failed.
        path: String,
        /// The underlying JSON decode error.
        source: serde_json::Error,
    },
    /// The configured base URL could not be used (only `http://` is supported).
    Url(String),
}

impl Error {
    /// Build an [`Error::Http`] from a status code and a response body, extracting the
    /// gateway's `{"error": "..."}` message when present.
    pub(crate) fn http(status: u16, body: &[u8]) -> Error {
        let message = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
        Error::Http { status, message }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "failed to connect to the uniflo daemon: {e}"),
            Error::Io(e) => write!(f, "I/O error talking to the uniflo daemon: {e}"),
            Error::Http { status, message } => write!(f, "HTTP {status}: {message}"),
            Error::Decode { path, source } => write!(f, "failed to decode JSON response from {path}: {source}"),
            Error::Url(message) => write!(f, "invalid uniflo daemon URL: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Connect(e) | Error::Io(e) => Some(e),
            Error::Decode { source, .. } => Some(source),
            Error::Http { .. } | Error::Url(_) => None,
        }
    }
}

/// Convenience alias used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;
