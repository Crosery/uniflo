//! Typed, blocking Rust client for the [Uniflo](https://github.com/Crosery/uniflo) daemon's
//! local HTTP gateway (default `http://127.0.0.1:7311`).
//!
//! Uniflo is a local daemon that normalizes every agent-harness session on your machine (Claude
//! Code, Codex, OpenCode, …) into one schema and serves it over a loopback-only HTTP gateway.
//! This crate is a client for that gateway: point it at a running daemon and get typed sessions,
//! transcripts, search hits, usage reports and a live event stream, without hand-rolling HTTP
//! calls or JSON shapes.
//!
//! # Quick start
//!
//! ```no_run
//! let client = uniflo_client::Client::local()?;
//! for s in client.sessions("s:work", 20)? {
//!     println!("{} · {}", s.key, s.title.unwrap_or_default());
//! }
//! # Ok::<(), uniflo_client::Error>(())
//! ```
//!
//! [`Client::local`] reads `UNIFLO_URL` (default `http://127.0.0.1:7311`) and `UNIFLO_TOKEN` from
//! the environment; use [`Client::new`] to point at an explicit URL instead.
//!
//! # Scope
//!
//! Every method here is read-only: this crate has no write endpoints (session cleanup, archive
//! deletion, opening a terminal) in this first version. See `docs/api.md` in the Uniflo
//! repository for the full REST surface, and `docs/sdk.md` for how this crate relates to
//! `uniflo-schema` (just the wire types) and embedding Uniflo without a daemon at all
//! (`uniflo-core` + `uniflo-adapters`, optionally `uniflo-search` and `uniflo-gateway`).
//!
//! # Dependencies
//!
//! This crate depends on nothing but [`uniflo_schema`] (re-exported below), `serde` and
//! `serde_json`: the HTTP transport is a small `std`-only HTTP/1.1 client over `TcpStream`.

#![warn(missing_docs)]

mod error;
mod http;
mod stream;
mod types;

use serde::de::DeserializeOwned;
use std::time::Duration;

pub use error::{Error, Result};
pub use stream::EnvelopeStream;
pub use types::{EventsPage, EventsQuery, Health, SearchOptions, StreamOptions, UsageQuery};

/// The full wire-schema crate (`uniflo-schema`), re-exported so a dependency on this crate alone
/// is enough to name every type, including ones not flattened below.
pub use uniflo_schema;
/// One line of the live stream returned by [`Client::stream`] ([`EnvelopeStream`]).
pub use uniflo_schema::Envelope;
/// One normalized transcript item of a session; see [`uniflo_schema::Event`] for its fields and
/// [`uniflo_schema::Body`] for the per-kind payload (`tool_call`, `assistant_message`, …).
pub use uniflo_schema::Event;
/// A supported harness and its live counters, returned by [`Client::harnesses`].
pub use uniflo_schema::Harness;
/// The text content of one memory file, returned by [`Client::memory_file`].
pub use uniflo_schema::MemoryContent;
/// One agent memory / instruction file, returned by [`Client::memory`].
pub use uniflo_schema::MemoryFile;
/// One model seen across sessions, with price catalog match, returned by [`Client::models`].
pub use uniflo_schema::ModelUsage;
/// How to continue a session in its own harness, returned by [`Client::resume`].
pub use uniflo_schema::ResumeInfo;
/// One conversation of one harness, returned by [`Client::sessions`] and [`Client::session`].
pub use uniflo_schema::Session;
/// Turns, steps and items of one session with its sub-sessions, returned by
/// [`Client::session_trajectory`].
pub use uniflo_schema::SessionTrajectory;
/// Per-step usage and per-turn totals of one session, returned by [`Client::session_usage`].
pub use uniflo_schema::SessionUsageDetail;
/// Token / cost aggregation, returned by [`Client::usage`].
pub use uniflo_schema::UsageReport;
/// Full-text search results grouped by session, returned by [`Client::search`].
pub use uniflo_schema::search::SearchResponse;

/// A client for one Uniflo daemon. Cheap to clone; every method opens its own short-lived TCP
/// connection (the daemon always responds with `Connection: close`), except [`Client::stream`],
/// which keeps a connection open for as long as the returned [`EnvelopeStream`] is alive.
#[derive(Clone)]
pub struct Client {
    host: String,
    port: u16,
    token: Option<String>,
    timeout: Duration,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Default request timeout (connect, write and read) applied unless [`Client::with_timeout`] is
/// called. [`Client::stream`] never applies a read timeout, regardless of this value.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

impl Client {
    /// A client for the daemon at `url`, which must start with `http://` (the gateway only binds
    /// loopback addresses and never speaks TLS, see `docs/api.md#安全`). The port defaults to 80
    /// when omitted, matching every other `http://` client; Uniflo's own default port is 7311,
    /// so most callers want `http://127.0.0.1:7311` explicitly, or just [`Client::local`].
    ///
    /// ```
    /// let client = uniflo_client::Client::new("http://127.0.0.1:7311").unwrap();
    /// assert!(format!("{client:?}").contains("7311"));
    /// ```
    pub fn new(url: &str) -> Result<Client> {
        let rest =
            url.strip_prefix("http://").ok_or_else(|| Error::Url(format!("only http:// is supported: {url}")))?;
        let hostport = rest.split('/').next().unwrap_or(rest);
        if hostport.is_empty() {
            return Err(Error::Url(format!("missing host: {url}")));
        }
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => {
                let port = p.parse::<u16>().map_err(|e| Error::Url(format!("invalid port {p:?} in {url}: {e}")))?;
                (h.to_owned(), port)
            }
            None => (hostport.to_owned(), 80),
        };
        Ok(Client { host, port, token: None, timeout: DEFAULT_TIMEOUT })
    }

    /// A client for the local daemon, configured the same way the `uniflo` CLI is: the base URL
    /// from `UNIFLO_URL` (default `http://127.0.0.1:7311`) and the bearer token from
    /// `UNIFLO_TOKEN`, when either is set.
    pub fn local() -> Result<Client> {
        let url = std::env::var("UNIFLO_URL").unwrap_or_else(|_| "http://127.0.0.1:7311".to_owned());
        let mut client = Client::new(&url)?;
        if let Some(token) = std::env::var("UNIFLO_TOKEN").ok().filter(|t| !t.is_empty()) {
            client = client.with_token(token);
        }
        Ok(client)
    }

    /// Send `Authorization: Bearer <token>` with every request; required when the daemon was
    /// started with `--token` / `UNIFLO_TOKEN` (`docs/api.md#安全`).
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Connect / write / read timeout for every request (default [`DEFAULT_TIMEOUT`]), except
    /// [`Client::stream`]'s socket, which is never subject to a read timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `GET <path_and_query>` (e.g. `"/v1/stats"`), decoded as `T`. An escape hatch for routes
    /// this crate does not wrap yet, or extra query parameters a typed method leaves out (see
    /// [`UsageQuery`]'s docs for an example).
    ///
    /// ```no_run
    /// # use serde_json::Value;
    /// let client = uniflo_client::Client::local()?;
    /// let stats: Value = client.get_json("/v1/stats")?;
    /// println!("{stats}");
    /// # Ok::<(), uniflo_client::Error>(())
    /// ```
    pub fn get_json<T: DeserializeOwned>(&self, path_and_query: &str) -> Result<T> {
        let mut resp =
            http::get(&self.host, self.port, path_and_query, self.token.as_deref(), self.timeout, Some(self.timeout))?;
        let body = resp.body().map_err(Error::Io)?;
        if resp.status != 200 {
            return Err(Error::http(resp.status, &body));
        }
        serde_json::from_slice(&body).map_err(|e| Error::Decode { path: path_and_query.to_owned(), source: e })
    }

    /// `GET /v1/health`: liveness, schema version, session counts, whether the daemon is
    /// read-only, and whether a newer release is available.
    pub fn health(&self) -> Result<Health> {
        self.get_json("/v1/health")
    }

    /// `GET /v1/sessions?q=&limit=`. `q` is the session search syntax (`docs/search.md`, e.g.
    /// `"h:claude s:work since:7d"`); an empty string lists everything. Sorted by relevance when
    /// `q` is non-empty, otherwise by most-recently-updated first.
    pub fn sessions(&self, q: &str, limit: usize) -> Result<Vec<Session>> {
        let mut qs = http::QueryString::new();
        qs.push_opt("q", Some(q));
        qs.push_usize("limit", Some(limit));
        self.get_json(&format!("/v1/sessions{}", qs.finish()))
    }

    /// `GET /v1/sessions/{key}`. `key` looks like `"claude:4f1c…"`; 404 surfaces as
    /// [`Error::Http`] with `status: 404`.
    pub fn session(&self, key: &str) -> Result<Session> {
        self.get_json(&format!("/v1/sessions/{}", http::percent_encode(key)))
    }

    /// `GET /v1/sessions/{key}/events`: one page of the transcript (newest page first, events
    /// within a page in time order), or a window centred on one event when
    /// [`EventsQuery::around`] is set.
    pub fn events(&self, key: &str, q: &EventsQuery) -> Result<EventsPage> {
        let mut qs = http::QueryString::new();
        qs.push_u64("before", q.before);
        qs.push_opt("around", q.around.as_deref());
        qs.push_usize("limit", q.limit);
        qs.push_usize("max_text", q.max_text);
        self.get_json(&format!("/v1/sessions/{}/events{}", http::percent_encode(key), qs.finish()))
    }

    /// `GET /v1/search?q=`: full-text hits grouped by session (`docs/search.md#全文检索` for the
    /// query syntax). 503 (surfaced as [`Error::Http`]) when the daemon was started with
    /// `--no-fts` or the index failed to open.
    pub fn search(&self, q: &str, opts: &SearchOptions) -> Result<SearchResponse> {
        let mut qs = http::QueryString::new();
        qs.push("q", q);
        qs.push_opt("filter", opts.filter.as_deref());
        if !opts.kinds.is_empty() {
            qs.push("kinds", &opts.kinds.join(","));
        }
        qs.push_usize("limit", opts.limit);
        qs.push_usize("offset", opts.offset);
        self.get_json(&format!("/v1/search{}", qs.finish()))
    }

    /// `GET /v1/usage`: token and cost totals aggregated by one dimension (`docs/api.md#v1usage-参数`).
    pub fn usage(&self, q: &UsageQuery) -> Result<UsageReport> {
        let mut qs = http::QueryString::new();
        qs.push_opt("group_by", q.group_by.as_deref());
        qs.push_opt("q", q.q.as_deref());
        qs.push_opt("since", q.since.as_deref());
        qs.push_opt("until", q.until.as_deref());
        qs.push_opt("under", q.under.as_deref());
        qs.push_usize("depth", q.depth);
        qs.push_usize("limit", q.limit);
        qs.push_opt("tz", q.tz.as_deref());
        self.get_json(&format!("/v1/usage{}", qs.finish()))
    }

    /// `GET /v1/sessions/{key}/usage`: every priced step of one session plus per-turn totals.
    pub fn session_usage(&self, key: &str) -> Result<SessionUsageDetail> {
        self.get_json(&format!("/v1/sessions/{}/usage", http::percent_encode(key)))
    }

    /// `GET /v1/sessions/{key}/trajectory`: the call chain of one session as turns → steps →
    /// items, with its direct sub-sessions hung on the calls that started them.
    pub fn session_trajectory(&self, key: &str) -> Result<SessionTrajectory> {
        self.get_json(&format!("/v1/sessions/{}/trajectory", http::percent_encode(key)))
    }

    /// `GET /v1/models`: every model seen across sessions, with its price-catalog match and
    /// totals.
    pub fn models(&self) -> Result<Vec<ModelUsage>> {
        self.get_json("/v1/models")
    }

    /// `GET /v1/harnesses`: every supported harness with sessions on this machine.
    pub fn harnesses(&self) -> Result<Vec<Harness>> {
        self.get_json("/v1/harnesses")
    }

    /// `GET /v1/sessions/{key}/resume`: the command that continues this session in its own
    /// harness. Nothing is executed; `ResumeInfo::supported` is `false` when the harness or
    /// session id does not support resuming.
    pub fn resume(&self, key: &str) -> Result<ResumeInfo> {
        self.get_json(&format!("/v1/sessions/{}/resume", http::percent_encode(key)))
    }

    /// `GET /v1/memory`: agent memory / instruction files an agent would load. `cwd` scopes the
    /// project-level files (git root down to `cwd`); `None` lists only global files.
    pub fn memory(&self, cwd: Option<&str>) -> Result<Vec<uniflo_schema::MemoryFile>> {
        let mut qs = http::QueryString::new();
        qs.push_opt("cwd", cwd);
        self.get_json(&format!("/v1/memory{}", qs.finish()))
    }

    /// `GET /v1/memory/file?path=`: the content of one file [`Client::memory`] listed (at most
    /// 256 KB; `MemoryContent::truncated` is set when the file is larger).
    pub fn memory_file(&self, path: &str) -> Result<uniflo_schema::MemoryContent> {
        let mut qs = http::QueryString::new();
        qs.push("path", path);
        self.get_json(&format!("/v1/memory/file{}", qs.finish()))
    }

    /// `GET /v1/stream.ndjson`: live [`Envelope`]s as they happen. Returns immediately with an
    /// iterator; each `next()` call blocks until another envelope arrives. See
    /// `examples/watch.rs` for printing tool calls as they happen.
    pub fn stream(&self, opts: &StreamOptions) -> Result<EnvelopeStream> {
        let mut qs = http::QueryString::new();
        qs.push_u64("since", opts.since);
        qs.push_opt("session", opts.session.as_deref());
        qs.push_opt("harness", opts.harness.as_deref());
        qs.push_opt("types", opts.types.as_deref());
        qs.push_opt("kinds", opts.kinds.as_deref());
        qs.push_usize("max_text", opts.max_text);
        let path = format!("/v1/stream.ndjson{}", qs.finish());
        let mut resp = http::get(&self.host, self.port, &path, self.token.as_deref(), self.timeout, None)?;
        if resp.status != 200 {
            let body = resp.body().map_err(Error::Io)?;
            return Err(Error::http(resp.status, &body));
        }
        Ok(EnvelopeStream::new(resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let c = Client::new("http://127.0.0.1:7311").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("127.0.0.1", 7311));
        let c = Client::new("http://example.com").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("example.com", 80));
        let c = Client::new("http://localhost:9/v1/health").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("localhost", 9));
        assert!(Client::new("https://127.0.0.1:7311").is_err());
        assert!(Client::new("http://").is_err());
        assert!(Client::new("http://127.0.0.1:not-a-port").is_err());
    }

    #[test]
    fn local_reads_env() {
        // SAFETY: test-only env mutation, no other test in this crate reads these variables
        // concurrently (uniflo-client has no multi-threaded test that touches them).
        unsafe {
            std::env::set_var("UNIFLO_URL", "http://127.0.0.1:9999");
            std::env::set_var("UNIFLO_TOKEN", "secret");
        }
        let c = Client::local().unwrap();
        assert_eq!((c.host.as_str(), c.port), ("127.0.0.1", 9999));
        assert_eq!(c.token.as_deref(), Some("secret"));
        assert!(!format!("{c:?}").contains("secret"), "token must not leak into Debug output");
        unsafe {
            std::env::remove_var("UNIFLO_URL");
            std::env::remove_var("UNIFLO_TOKEN");
        }
    }

    #[test]
    fn with_token_and_timeout_are_builders() {
        let c = Client::new("http://127.0.0.1:7311").unwrap().with_token("tok").with_timeout(Duration::from_millis(5));
        assert_eq!(c.token.as_deref(), Some("tok"));
        assert_eq!(c.timeout, Duration::from_millis(5));
    }
}
