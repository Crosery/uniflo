//! Request option structs and the response types that have no `uniflo-schema` type of their own
//! (`/v1/health` and the events page are plain JSON built in the gateway, not wire-schema types).

use serde::Deserialize;

/// `GET /v1/health`. Every field is `#[serde(default)]`-tolerant: the gateway may add fields
/// (wire schema v1 only grows, see `docs/decisions/ADR-0003-wire-schema-只增不改.md`), and this
/// struct must keep decoding a newer daemon's response.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Health {
    /// The daemon is up and answering requests.
    #[serde(default)]
    pub ok: bool,
    /// `uniflo` crate version of the daemon.
    #[serde(default)]
    pub version: String,
    /// Wire schema version (`uniflo_schema::SCHEMA_VERSION`) the daemon speaks.
    #[serde(default)]
    pub schema: u32,
    /// Latest global sequence number; usable as `since=` for [`crate::Client::stream`].
    #[serde(default)]
    pub seq: u64,
    /// Total indexed sessions.
    #[serde(default)]
    pub sessions: usize,
    /// Sessions currently in `work` status.
    #[serde(default)]
    pub working: usize,
    /// How long the daemon has been running.
    #[serde(default)]
    pub uptime_ms: u64,
    /// A newer release of `uniflo` is available on crates.io.
    #[serde(default)]
    pub update_available: bool,
    /// The latest published version, when `update_available` (or a periodic check) found one.
    #[serde(default)]
    pub latest_version: Option<String>,
    /// The latest prerelease version seen, if any (never implies `update_available`).
    #[serde(default)]
    pub latest_prerelease: Option<String>,
    /// The daemon was started with `--read-only`: every write endpoint answers 403.
    #[serde(default)]
    pub read_only: bool,
}

/// `GET /v1/sessions/{key}/events`: one page of the transcript, or (with [`EventsQuery::around`])
/// a window centred on one event.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct EventsPage {
    /// The session key this page belongs to.
    pub session: String,
    /// The page's events, in time order.
    #[serde(default)]
    pub events: Vec<uniflo_schema::Event>,
    /// Pass as `before` to page further back; `None` exactly when nothing older exists.
    #[serde(default)]
    pub next_before: Option<u64>,
    /// Present when the page was requested with `around`.
    #[serde(default)]
    pub around: Option<String>,
}

/// Parameters of [`crate::Client::events`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventsQuery {
    /// Page further back than this cursor (`EventsPage::next_before` of an earlier page).
    pub before: Option<u64>,
    /// Centre the page on this event id instead of paging from the newest event.
    pub around: Option<String>,
    /// Defaults to 200 server-side, capped at 10000.
    pub limit: Option<usize>,
    /// Truncate free-text fields to this many bytes; `0` = unlimited. Defaults to 32768.
    pub max_text: Option<usize>,
}

/// Parameters of [`crate::Client::search`] beyond the query text itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchOptions {
    /// Session search syntax (`docs/search.md`) to narrow which sessions are scanned.
    pub filter: Option<String>,
    /// Only these event kinds (`tool_call`, `assistant_message`, …).
    pub kinds: Vec<String>,
    /// Sessions per page, default 20, max 200.
    pub limit: Option<usize>,
    /// Pages past the first; in units of `limit`.
    pub offset: Option<usize>,
}

/// Parameters of [`crate::Client::usage`]. The gateway also accepts `sort` and `model`; reach
/// those with [`crate::Client::get_json`] if you need them (`/v1/usage?sort=...&model=...`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageQuery {
    /// Dimension: `harness` (default), `model`, `project`, `cwd`, `dir`, `day`, `hour`,
    /// `weekday`, `session`, `weekday_hour`.
    pub group_by: Option<String>,
    /// Session search syntax (`docs/search.md`); its `since:` / `before:` become the event-time
    /// window.
    pub q: Option<String>,
    /// `30m`, `2h`, `7d`, `1w` (relative to now), `YYYY-MM-DD`, or epoch milliseconds.
    pub since: Option<String>,
    /// Same format as `since`; the window is `[since, until)`.
    pub until: Option<String>,
    /// Only sessions whose cwd is at or under this directory.
    pub under: Option<String>,
    /// `group_by=dir` drill-down depth, default 1.
    pub depth: Option<usize>,
    /// Keep only the top N rows; the rest fold into a `(other)` row.
    pub limit: Option<usize>,
    /// `local` (default), `UTC`, `+08:00`, or an IANA zone name.
    pub tz: Option<String>,
}

/// Parameters of [`crate::Client::stream`]. Every field is the comma-separated form the gateway
/// expects on the wire.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamOptions {
    /// Replay envelopes after this sequence number before switching to live ones.
    pub since: Option<u64>,
    /// Comma-separated session keys; only envelopes about these sessions.
    pub session: Option<String>,
    /// Comma-separated harness ids.
    pub harness: Option<String>,
    /// Comma-separated envelope types (`session,event,removed`); `hello` and `lagged` always
    /// come through.
    pub types: Option<String>,
    /// Comma-separated event kinds (`tool_call,assistant_message,…`).
    pub kinds: Option<String>,
    /// Truncate event text to this many bytes; `0` = unlimited. Defaults to 32768.
    pub max_text: Option<usize>,
}
