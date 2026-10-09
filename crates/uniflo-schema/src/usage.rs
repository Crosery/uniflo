//! Usage, cost and model-catalog types served by `/v1/usage`, `/v1/sessions/{key}/usage`,
//! `/v1/models` and `/v1/pricing`.
//!
//! Money is USD. Prices are USD per million tokens. For subscription users every amount is
//! the API-equivalent cost, not what they were billed.

use serde::{Deserialize, Serialize};

/// Running totals of one session (`Session.usage`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub steps: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Sum over priced steps; `null` when no step could be priced.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Steps with neither a harness-reported amount nor a catalog price (never counted as 0).
    pub unpriced_steps: u64,
    /// `input + cache_read + cache_write` of the latest step.
    pub last_context_tokens: u64,
    /// Context window of the latest step's model, from the catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
}

/// Where a step's amount came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// Reported by the harness (`usage.cost_usd`).
    Harness,
    /// Catalog price of the exact model.
    Catalog,
    /// Catalog price of the nearest version of the same model family.
    Approx,
}

/// How a model name resolved against the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Exact,
    Approx,
    None,
}

/// One aggregation bucket of `GET /v1/usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    pub key: String,
    pub label: String,
    /// Distinct sessions with a step or a prompt in this bucket.
    pub sessions: u64,
    pub steps: u64,
    /// Human prompts (non-synthetic user messages).
    pub prompts: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    pub unpriced_steps: u64,
}

/// Background usage indexing progress: totals are partial until `ready`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageProgress {
    pub ready: bool,
    pub done: u64,
    pub total: u64,
}

/// Freshness of the price catalog behind the amounts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PricingBrief {
    #[serde(default)]
    pub fetched_at: Option<i64>,
    pub stale: bool,
}

/// `GET /v1/usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub group_by: String,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    /// Time zone used for `day` / `hour` / `weekday` buckets.
    pub tz: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under: Option<String>,
    pub rows: Vec<UsageRow>,
    /// Sum of every row (rows folded by `limit` included).
    pub totals: UsageRow,
    pub pricing: PricingBrief,
    pub indexing: UsageProgress,
}

/// One model call of a session (`GET /v1/sessions/{key}/usage`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepUsage {
    /// Event id of the `usage` event.
    pub event: String,
    pub ts: i64,
    /// Counted from 1 at each human prompt / turn start; 0 = before the first one.
    pub turn: u32,
    #[serde(default)]
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub cost_source: Option<CostSource>,
    /// `input + cache_read + cache_write`: what the model saw this step.
    pub context_tokens: u64,
    #[serde(default)]
    pub context_limit: Option<u64>,
    /// `context_tokens / context_limit × 100`.
    #[serde(default)]
    pub context_pct: Option<f64>,
}

/// Sum of the steps of one turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnUsage {
    pub turn: u32,
    pub started_at: i64,
    pub prompts: u64,
    pub steps: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    pub unpriced_steps: u64,
}

/// `GET /v1/sessions/{key}/usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionUsageDetail {
    pub session: String,
    pub steps: Vec<StepUsage>,
    pub turns: Vec<TurnUsage>,
    pub totals: SessionUsage,
}

/// Rates above a whole-request prompt size: once `input + cache_read + cache_write`
/// exceeds `above`, the whole step is billed at this tier.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PriceTier {
    pub above: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// Prices in effect over `[from, until)` (epoch ms; `until` absent = still in effect).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PriceSegment {
    /// Absent in hand-written overrides = since forever.
    #[serde(default)]
    pub from: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// Absent: billed at `input × 1.25` (Anthropic 5-minute cache write rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<PriceTier>,
}

/// One model of the price catalog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    #[serde(default)]
    pub provider: String,
    /// Ascending by `from`.
    pub prices: Vec<PriceSegment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_limit: Option<u64>,
    /// `models.dev`, `litellm` or `override`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

/// One model name seen in sessions (`GET /v1/models`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// The name exactly as the harness reported it.
    pub model: String,
    #[serde(rename = "match")]
    pub match_kind: Option<MatchKind>,
    #[serde(default)]
    pub catalog_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub prices: Vec<PriceSegment>,
    #[serde(default)]
    pub context_limit: Option<u64>,
    #[serde(default)]
    pub output_limit: Option<u64>,
    pub sessions: u64,
    pub steps: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    pub unpriced_steps: u64,
}

/// `GET /v1/pricing`: catalog sync state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PricingStatus {
    /// `snapshot` (embedded, never synced) or the upstream sources of the synced catalog.
    pub source: String,
    /// Last successful sync (the snapshot's build time before any sync).
    #[serde(default)]
    pub fetched_at: Option<i64>,
    /// No successful sync for more than 24 hours.
    pub stale: bool,
    /// Failure of the latest attempt; `null` when it fully succeeded.
    #[serde(default)]
    pub error: Option<String>,
    /// Catalog entries in effect (overrides included).
    pub models: u64,
    /// Entries from `overrides.json`.
    pub overrides: u64,
    #[serde(default)]
    pub last_attempt: Option<i64>,
    /// Price changes waiting for a second consistent read.
    #[serde(default)]
    pub pending: u64,
    /// Whether this process syncs periodically.
    #[serde(default)]
    pub sync_enabled: bool,
}
