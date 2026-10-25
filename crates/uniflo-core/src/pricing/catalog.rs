//! Catalog building blocks: upstream formats → [`CatalogEntry`], model-name matching, step cost.

use serde_json::Value;
use std::collections::HashMap;
use uniflo_schema::{CatalogEntry, MatchKind, PriceSegment, PriceTier};

/// Official vendors whose own list price is used as-is. Every other models.dev provider
/// (resellers and aggregators, including OpenRouter) is folded instead into one median-priced
/// consensus entry per model — a reseller's headline price is often its cheapest route, not
/// the vendor's price (e.g. OpenRouter lists glm-5.3 input at $0.079/M against Zhipu's official
/// $1.4/M, kimi-k3 at $0.239/M against $3/M), so no single reseller price is trustworthy on its
/// own; see [`parse_resellers`] and ADR-0010.
/// Order breaks ties when two vendors list the same model id.
pub const FIRST_PARTY: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "minimax",
    "moonshotai",
    "zai",
    "deepseek",
    "alibaba",
    "xai",
    "groq",
    "xiaomi",
    "meta",
    "stepfun",
    "mistral",
    "cohere",
    "perplexity",
];

/// LiteLLM `litellm_provider` → vendor, in priority order.
const LITELLM_PROVIDERS: &[(&str, &str)] = &[
    ("anthropic", "anthropic"),
    ("openai", "openai"),
    ("gemini", "google"),
    ("minimax", "minimax"),
    ("moonshot", "moonshotai"),
    ("zai", "zai"),
    ("deepseek", "deepseek"),
    ("dashscope", "alibaba"),
    ("xai", "xai"),
    ("groq", "groq"),
];

const LITELLM_MODES: &[&str] = &["chat", "responses", "completion"];

/// Anthropic's 5-minute cache write rate; used whenever a catalog has no cache write price.
pub const CACHE_WRITE_MULTIPLIER: f64 = 1.25;

pub const MODELS_DEV: &str = "models.dev";
pub const LITELLM: &str = "litellm";

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0)
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// Middle value of a non-empty, unsorted sample (average of the two middles when even-sized).
fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).expect("finite prices only"));
    let n = xs.len();
    if n % 2 == 1 { xs[n / 2] } else { round6((xs[n / 2 - 1] + xs[n / 2]) / 2.0) }
}

/// The key with the highest count, ties broken lexicographically smallest.
fn most_frequent(counts: &HashMap<String, u32>) -> Option<String> {
    counts.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|(k, _)| k.clone())
}

fn last_segment(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// models.dev `api.json`: `{provider: {models: {id: {cost: {input, output, cache_read?,
/// cache_write?, tiers?[{tier:{type:"context", size}, …}], context_over_200k?}, limit: {context, output}}}}}`.
/// [`FIRST_PARTY`] vendors are read as their own list price; every model name none of them
/// covers gets one [`parse_resellers`] consensus entry from the remaining providers instead.
/// A missing cache-read price is `NaN` until [`fill_missing`].
pub fn parse_models_dev(v: &Value) -> Vec<CatalogEntry> {
    let mut out: Vec<CatalogEntry> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for provider in FIRST_PARTY {
        let Some(models) = v.pointer(&format!("/{provider}/models")).and_then(Value::as_object) else { continue };
        let mut ids: Vec<&String> = models.keys().collect();
        ids.sort();
        for raw in ids {
            let m = &models[raw];
            let Some(cost) = m.get("cost") else { continue };
            let (Some(input), Some(output)) = (num(cost, "input"), num(cost, "output")) else { continue };
            let cache_read = num(cost, "cache_read").unwrap_or(f64::NAN);
            let mut tiers: Vec<PriceTier> = cost
                .get("tiers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|t| t.pointer("/tier/type").and_then(Value::as_str) == Some("context"))
                .filter_map(|t| {
                    Some(PriceTier {
                        above: t.pointer("/tier/size").and_then(Value::as_u64)?,
                        input: num(t, "input")?,
                        output: num(t, "output")?,
                        cache_read: num(t, "cache_read").unwrap_or(f64::NAN),
                        cache_write: num(t, "cache_write"),
                    })
                })
                .collect();
            if tiers.is_empty()
                && let Some(t) = cost.get("context_over_200k")
                && let (Some(i), Some(o)) = (num(t, "input"), num(t, "output"))
            {
                tiers.push(PriceTier {
                    above: 200_000,
                    input: i,
                    output: o,
                    cache_read: num(t, "cache_read").unwrap_or(f64::NAN),
                    cache_write: num(t, "cache_write"),
                });
            }
            tiers.sort_by_key(|t| t.above);
            let id = last_segment(raw).to_owned();
            if seen.insert(match_key(&id), ()).is_some() {
                continue;
            }
            out.push(CatalogEntry {
                id,
                provider: (*provider).to_owned(),
                prices: vec![PriceSegment {
                    from: 0,
                    until: None,
                    input,
                    output,
                    cache_read,
                    cache_write: num(cost, "cache_write"),
                    tiers,
                }],
                context_limit: m.pointer("/limit/context").and_then(Value::as_u64).filter(|n| *n > 0),
                output_limit: m.pointer("/limit/output").and_then(Value::as_u64).filter(|n| *n > 0),
                source: MODELS_DEV.into(),
                name: m.get("name").and_then(Value::as_str).map(str::to_owned),
                consensus: false,
            });
        }
    }
    out.extend(parse_resellers(v, &seen));
    out
}

/// One reseller's listing of a model, kept only long enough to fold into a median.
struct Listing {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    context_limit: Option<f64>,
    output_limit: Option<f64>,
}

/// Everything collected for one [`match_key`] group before it becomes one consensus entry.
#[derive(Default)]
struct Consensus {
    listings: Vec<Listing>,
    /// Original last-segment spelling → how many providers used exactly that spelling.
    spellings: HashMap<String, u32>,
    /// `vendor` prefix of ids shaped `vendor/model` (e.g. OpenRouter's `deepseek/deepseek-v4.1-flash`).
    vendors: HashMap<String, u32>,
}

/// One median-priced entry per model name that no [`FIRST_PARTY`] vendor lists, built from
/// every other models.dev provider (resellers, aggregators, including OpenRouter itself as one
/// vote). `:free` / `-free` ids and listings without a positive input and output price are
/// excluded so a free tier or a missing price never pulls the median down; see ADR-0010.
fn parse_resellers(v: &Value, seen: &HashMap<String, ()>) -> Vec<CatalogEntry> {
    let Some(obj) = v.as_object() else { return Vec::new() };
    let mut providers: Vec<&String> = obj.keys().filter(|p| !FIRST_PARTY.contains(&p.as_str())).collect();
    providers.sort();
    let mut groups: HashMap<String, Consensus> = HashMap::new();
    for provider in providers {
        let Some(models) = v.pointer(&format!("/{provider}/models")).and_then(Value::as_object) else { continue };
        let mut ids: Vec<&String> = models.keys().collect();
        ids.sort();
        for raw in ids {
            let id = last_segment(raw);
            if id.ends_with(":free") || id.ends_with("-free") {
                continue;
            }
            let key = match_key(id);
            if seen.contains_key(&key) {
                continue;
            }
            let m = &models[raw];
            let Some(cost) = m.get("cost") else { continue };
            let (Some(input), Some(output)) = (num(cost, "input"), num(cost, "output")) else { continue };
            if !(input > 0.0 && output > 0.0) {
                continue;
            }
            let g = groups.entry(key).or_default();
            *g.spellings.entry(id.to_owned()).or_insert(0) += 1;
            if let Some((vendor, _)) = raw.rsplit_once('/') {
                *g.vendors.entry(vendor.to_owned()).or_insert(0) += 1;
            }
            g.listings.push(Listing {
                input,
                output,
                cache_read: num(cost, "cache_read"),
                cache_write: num(cost, "cache_write"),
                context_limit: m.pointer("/limit/context").and_then(Value::as_u64).filter(|n| *n > 0).map(|n| n as f64),
                output_limit: m.pointer("/limit/output").and_then(Value::as_u64).filter(|n| *n > 0).map(|n| n as f64),
            });
        }
    }
    let mut keys: Vec<&String> = groups.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|key| {
            let g = &groups[key];
            let cache_read = {
                let xs: Vec<f64> = g.listings.iter().filter_map(|l| l.cache_read).collect();
                if xs.is_empty() { f64::NAN } else { median(xs) }
            };
            let cache_write = {
                let xs: Vec<f64> = g.listings.iter().filter_map(|l| l.cache_write).collect();
                (!xs.is_empty()).then(|| median(xs))
            };
            let context_limit = {
                let xs: Vec<f64> = g.listings.iter().filter_map(|l| l.context_limit).collect();
                (!xs.is_empty()).then(|| median(xs).round() as u64)
            };
            let output_limit = {
                let xs: Vec<f64> = g.listings.iter().filter_map(|l| l.output_limit).collect();
                (!xs.is_empty()).then(|| median(xs).round() as u64)
            };
            CatalogEntry {
                id: most_frequent(&g.spellings).unwrap_or_default(),
                provider: most_frequent(&g.vendors).unwrap_or_default(),
                prices: vec![PriceSegment {
                    from: 0,
                    until: None,
                    input: median(g.listings.iter().map(|l| l.input).collect()),
                    output: median(g.listings.iter().map(|l| l.output).collect()),
                    cache_read,
                    cache_write,
                    tiers: Vec::new(),
                }],
                context_limit,
                output_limit,
                source: MODELS_DEV.into(),
                name: None,
                consensus: true,
            }
        })
        .collect()
}

/// LiteLLM `model_prices_and_context_window.json`: per-token prices keyed by model name.
/// Like [`parse_models_dev`], a missing cache-read price is `NaN` until [`fill_missing`].
pub fn parse_litellm(v: &Value) -> Vec<CatalogEntry> {
    let Some(all) = v.as_object() else { return Vec::new() };
    let mut out: Vec<CatalogEntry> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for (lp, vendor) in LITELLM_PROVIDERS {
        let mut keys: Vec<&String> = all
            .iter()
            .filter(|(_, m)| m.get("litellm_provider").and_then(Value::as_str) == Some(*lp))
            .filter(|(_, m)| m.get("mode").and_then(Value::as_str).is_none_or(|x| LITELLM_MODES.contains(&x)))
            .map(|(k, _)| k)
            .collect();
        // Bare names before `provider/name` duplicates.
        keys.sort_by_key(|k| (k.contains('/'), (*k).clone()));
        for k in keys {
            let m = &all[k];
            let per_m = |key: &str| num(m, key).map(|x| round6(x * 1e6));
            let (Some(input), Some(output)) = (per_m("input_cost_per_token"), per_m("output_cost_per_token")) else {
                continue;
            };
            let cache_read = per_m("cache_read_input_token_cost").unwrap_or(f64::NAN);
            let mut tiers = Vec::new();
            if let Some(o) = m.as_object() {
                for key in o.keys() {
                    let Some(n) = key
                        .strip_prefix("input_cost_per_token_above_")
                        .and_then(|r| r.strip_suffix("k_tokens"))
                        .and_then(|n| n.parse::<u64>().ok())
                    else {
                        continue;
                    };
                    let sfx = format!("above_{n}k_tokens");
                    let (Some(ti), Some(to)) = (per_m(key), per_m(&format!("output_cost_per_token_{sfx}"))) else {
                        continue;
                    };
                    tiers.push(PriceTier {
                        above: n * 1000,
                        input: ti,
                        output: to,
                        cache_read: per_m(&format!("cache_read_input_token_cost_{sfx}")).unwrap_or(f64::NAN),
                        cache_write: per_m(&format!("cache_creation_input_token_cost_{sfx}")),
                    });
                }
            }
            tiers.sort_by_key(|t| t.above);
            let id = last_segment(k).to_owned();
            if seen.insert(match_key(&id), ()).is_some() {
                continue;
            }
            out.push(CatalogEntry {
                id,
                provider: (*vendor).to_owned(),
                prices: vec![PriceSegment {
                    from: 0,
                    until: None,
                    input,
                    output,
                    cache_read,
                    cache_write: per_m("cache_creation_input_token_cost"),
                    tiers,
                }],
                context_limit: m.get("max_input_tokens").and_then(Value::as_u64).filter(|n| *n > 0),
                output_limit: m.get("max_output_tokens").and_then(Value::as_u64).filter(|n| *n > 0),
                source: LITELLM.into(),
                name: None,
                consensus: false,
            });
        }
    }
    out
}

/// Missing cache-read prices (`NaN` from the parsers) become the full input price: an
/// unknown discount is billed conservatively, never as free. A tier without one keeps the
/// base discount ratio.
pub fn fill_missing(entries: &mut [CatalogEntry]) {
    for e in entries {
        for s in &mut e.prices {
            if !s.cache_read.is_finite() {
                s.cache_read = s.input;
            }
            let ratio = if s.input > 0.0 { s.cache_read / s.input } else { 1.0 };
            for t in &mut s.tiers {
                if !t.cache_read.is_finite() {
                    t.cache_read = round6(t.input * ratio);
                }
            }
        }
    }
}

/// Primary entries win; the fallback only fills ids the primary does not list.
pub fn merge(primary: Vec<CatalogEntry>, fallback: Vec<CatalogEntry>) -> Vec<CatalogEntry> {
    let mut have: HashMap<String, ()> = primary.iter().map(|e| (match_key(&e.id), ())).collect();
    let mut out = primary;
    for e in fallback {
        if have.insert(match_key(&e.id), ()).is_none() {
            out.push(e);
        }
    }
    out
}

// ---------------------------------------------------------------- matching

/// Step 1 of name normalization: drop `provider/` prefixes, bracketed suffixes (`[1m]`)
/// and trailing modifiers in parentheses (`(high)`).
pub fn base_name(model: &str) -> &str {
    let mut s = last_segment(model.trim()).trim();
    loop {
        let before = s;
        if s.ends_with(']')
            && let Some(i) = s.rfind('[')
        {
            s = s[..i].trim_end();
        }
        if s.ends_with(')')
            && let Some(i) = s.rfind('(')
        {
            s = s[..i].trim_end();
        }
        if s == before {
            return s;
        }
    }
}

/// Case-insensitive comparison key where `4.5` and `4-5` are the same version.
pub fn match_key(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.char_indices() {
        let digit_dot_digit =
            ch == '.' && i > 0 && b[i - 1].is_ascii_digit() && b.get(i + 1).is_some_and(u8::is_ascii_digit);
        out.push(if digit_dot_digit { '-' } else { ch.to_ascii_lowercase() });
    }
    out
}

/// Drop a release-date suffix: `-20250929`, `@20250929`, `-2025-09-29`.
pub fn strip_date(s: &str) -> &str {
    let b = s.as_bytes();
    let n = b.len();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if n > 9 && matches!(b[n - 9], b'-' | b'@') && digits(n - 8..n) {
        return &s[..n - 9];
    }
    if n > 11
        && b[n - 11] == b'-'
        && digits(n - 10..n - 6)
        && b[n - 6] == b'-'
        && digits(n - 5..n - 3)
        && b[n - 3] == b'-'
        && digits(n - 2..n)
    {
        return &s[..n - 11];
    }
    s
}

/// Comparison key for display names: ASCII letters and digits only, lowercase, with a trailing
/// build or date tag of 4–8 digits dropped. `DeepSeek V4.1 Flash 1017` and `deepseek-v4.1-flash`
/// both give `deepseekv41flash`.
fn display_key(s: &str) -> String {
    let mut k: String = s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    let tag = k.bytes().rev().take_while(u8::is_ascii_digit).count();
    if (4..=8).contains(&tag) && tag < k.len() {
        k.truncate(k.len() - tag);
    }
    k
}

/// `glm-5-2` → (`glm-#-#`, [5, 2]); names must start with a letter and carry a version.
fn skeleton(key: &str) -> Option<(String, Vec<u64>)> {
    if !key.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut sk = String::with_capacity(key.len());
    let mut nums = Vec::new();
    let mut cur: Option<u64> = None;
    for ch in key.chars() {
        match ch.to_digit(10) {
            Some(d) => cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add(d as u64)),
            None => {
                if let Some(n) = cur.take() {
                    sk.push('#');
                    nums.push(n);
                }
                sk.push(ch);
            }
        }
    }
    if let Some(n) = cur {
        sk.push('#');
        nums.push(n);
    }
    (!nums.is_empty()).then_some((sk, nums))
}

/// Words that mark a product tier/variant rather than the model family itself. Dropping one of
/// these off the *end* of a name is never allowed (`qwen3.8-max` must not fall back to
/// `qwen3.8`, `gpt-5-mini` must not fall back to `gpt-5`) because it would silently price a
/// bigger or smaller tier as if it were the base model.
const TIER_WORDS: &[&str] = &[
    "mini",
    "nano",
    "micro",
    "lite",
    "flash",
    "pro",
    "max",
    "plus",
    "turbo",
    "ultra",
    "small",
    "medium",
    "large",
    "xl",
    "haiku",
    "sonnet",
    "opus",
    "air",
    "fast",
    "instant",
    "coder",
    "codex",
    "vision",
    "omni",
    "audio",
    "image",
    "embed",
    "embedding",
    "reasoner",
    "chat",
    "base",
];

fn is_tier_word(w: &str) -> bool {
    TIER_WORDS.iter().any(|t| t.eq_ignore_ascii_case(w))
}

/// Byte spans of the words in `s`, splitting on `-` and `:` (the separators themselves are
/// dropped but their position is implicit: joining the spans of a contiguous range reproduces
/// the original separators between them).
fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    for (i, ch) in s.char_indices() {
        if ch == '-' || ch == ':' {
            if let Some(from) = start.take() {
                spans.push((from, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(from) = start {
        spans.push((from, s.len()));
    }
    spans
}

/// Contiguous word-range substrings of `base`, strictly shorter than the whole name, longest
/// first; within one length the window that drops the most *leading* words comes first (so a
/// harness prefix like `qcn-` or `cline-` is tried before trimming any suffix). A window must
/// start with an ASCII letter, carry an ASCII digit or span at least two words (`oc-space-bunny`
/// → `space-bunny`, while `mimo-auto` never shrinks to a bare `mimo`), and may not drop a
/// [`TIER_WORDS`] word off the end — handles prefixes (`qcn-qwen3.8-flash`), suffixes (`gemini-3.8-flash-high`,
/// `claude-opus-4-6-thinking`, `qwen3.6-plus-free`) and both at once.
fn name_windows(base: &str) -> Vec<&str> {
    let spans = word_spans(base);
    let n = spans.len();
    let mut out = Vec::new();
    if n < 2 {
        return out;
    }
    for len in (1..n).rev() {
        for start in (0..=(n - len)).rev() {
            let end = start + len;
            let dropped_trailing_is_tier = (end..n).any(|k| is_tier_word(&base[spans[k].0..spans[k].1]));
            if dropped_trailing_is_tier {
                continue;
            }
            let win = &base[spans[start].0..spans[end - 1].1];
            if win.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && (len >= 2 || win.bytes().any(|b| b.is_ascii_digit()))
            {
                out.push(win);
            }
        }
    }
    out
}

/// Entries plus lookup indexes; later layers are merged in by the caller.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
    exact: HashMap<String, usize>,
    /// [`display_key`] of a vendor's display name → its listings.
    names: HashMap<String, Vec<usize>>,
    families: HashMap<String, Vec<(usize, Vec<u64>)>>,
}

impl Catalog {
    pub fn new(entries: Vec<CatalogEntry>) -> Catalog {
        let mut exact: HashMap<String, usize> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            exact.entry(match_key(&e.id)).or_insert(i);
        }
        // Dated ids also answer to their undated name; the newest date wins.
        let mut dateless: Vec<(String, usize)> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            let k = match_key(strip_date(&e.id));
            if k != match_key(&e.id) {
                dateless.push((k, i));
            }
        }
        dateless.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| entries[b.1].id.cmp(&entries[a.1].id)));
        for (k, i) in dateless {
            exact.entry(k).or_insert(i);
        }
        let mut families: HashMap<String, Vec<(usize, Vec<u64>)>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if let Some((sk, nums)) = skeleton(&match_key(strip_date(&e.id))) {
                families.entry(sk).or_default().push((i, nums));
            }
        }
        let mut names: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if let Some(n) = e.name.as_deref().filter(|_| !e.consensus) {
                names.entry(display_key(n)).or_default().push(i);
            }
        }
        Catalog { entries, exact, names, families }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Vendor id on the full name → vendor display name → reseller median on the full name
    /// (all after normalization, incl. dateless) → the same on a trimmed name window → nearest
    /// version of the same name skeleton (`approx`) on the full name → nearest version on a
    /// window → none. Only a vendor id hit on the full name is `exact`. A display-name hit is
    /// `approx` (the id differs, e.g. harnesses say `deepseek-v4.1-flash` where the vendor lists
    /// `deepseek-flash`), and so is a window hit: it already matched by dropping harness/reseller
    /// prefixes or suffixes (see [`name_windows`]).
    pub fn resolve(&self, model: &str) -> (MatchKind, Option<usize>) {
        let base = base_name(model);
        if base.is_empty() {
            return (MatchKind::None, None);
        }
        if let Some((i, by_id)) = self.lookup(base) {
            return (if by_id { MatchKind::Exact } else { MatchKind::Approx }, Some(i));
        }
        for win in name_windows(base) {
            if let Some((i, _)) = self.lookup(win) {
                return (MatchKind::Approx, Some(i));
            }
        }
        if let Some(i) = self.nearest_version(&match_key(strip_date(base))) {
            return (MatchKind::Approx, Some(i));
        }
        for win in name_windows(base) {
            if let Some(i) = self.nearest_version(&match_key(strip_date(win))) {
                return (MatchKind::Approx, Some(i));
            }
        }
        (MatchKind::None, None)
    }

    /// A vendor's own listing outranks a reseller median: the vendor id, then the vendor's
    /// display name, and only then the median found by id. `true` = matched by id.
    fn lookup(&self, name: &str) -> Option<(usize, bool)> {
        let by_id = self.exact_lookup(name);
        match by_id {
            Some(i) if !self.entries[i].consensus => Some((i, true)),
            _ => self.display_lookup(name).map(|i| (i, false)).or(by_id.map(|i| (i, true))),
        }
    }

    /// The vendor listing whose display name equals `name` under [`display_key`]. When several
    /// vendors list it, the one whose provider prefixes the name owns it (`deepseek…`); failing
    /// that, they must agree on the price.
    fn display_lookup(&self, name: &str) -> Option<usize> {
        let key = display_key(name);
        let found = self.names.get(&key)?;
        if let [only] = found.as_slice() {
            return Some(*only);
        }
        let owners: Vec<usize> =
            found.iter().copied().filter(|&i| key.starts_with(&self.entries[i].provider)).collect();
        if let [owner] = owners.as_slice() {
            return Some(*owner);
        }
        let first = &self.entries[found[0]].prices;
        let agree = found.iter().all(|&i| {
            let (a, b) = (first.last(), self.entries[i].prices.last());
            a.zip(b).is_some_and(|(a, b)| a.input == b.input && a.output == b.output && a.cache_read == b.cache_read)
        });
        agree.then_some(found[0])
    }

    /// `name`, normalized and with its date suffix also tried, looked up in the exact index.
    fn exact_lookup(&self, name: &str) -> Option<usize> {
        let dateless = match_key(strip_date(name));
        self.exact.get(&match_key(name)).or_else(|| self.exact.get(&dateless)).copied()
    }

    /// Closest version within `dateless`'s name skeleton (see [`skeleton`]), ties broken by id.
    fn nearest_version(&self, dateless: &str) -> Option<usize> {
        let (sk, want) = skeleton(dateless)?;
        self.families.get(&sk).and_then(|cands| {
            cands
                .iter()
                .map(|(i, got)| (got.iter().zip(&want).map(|(a, b)| a.abs_diff(*b)).sum::<u64>(), *i))
                .min_by(|a, b| a.0.cmp(&b.0).then_with(|| self.entries[a.1].id.cmp(&self.entries[b.1].id)))
                .map(|(_, i)| i)
        })
    }
}

// ---------------------------------------------------------------- cost

/// Prices in effect at `ts`: the last segment starting at or before it (the first one for
/// usage older than every segment).
pub fn segment_at(e: &CatalogEntry, ts: i64) -> Option<&PriceSegment> {
    let mut chosen = e.prices.first()?;
    for s in &e.prices {
        if s.from <= ts {
            chosen = s;
        } else {
            break;
        }
    }
    Some(chosen)
}

/// Token counts that drive a price (see `docs/schema.md#usage-口径`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// USD for one step: `(input×in + output×out + cache_read×cr + cache_write×cw) / 1e6`, with the
/// whole step billed at the highest tier whose `above` the full prompt exceeds.
pub fn step_cost(s: &PriceSegment, t: Tokens) -> f64 {
    let prompt = t.input + t.cache_read + t.cache_write;
    let tier = s.tiers.iter().filter(|x| prompt > x.above).max_by_key(|x| x.above);
    let (i, o, cr, cw) = match tier {
        Some(x) => (x.input, x.output, x.cache_read, x.cache_write.unwrap_or(x.input * CACHE_WRITE_MULTIPLIER)),
        None => (s.input, s.output, s.cache_read, s.cache_write.unwrap_or(s.input * CACHE_WRITE_MULTIPLIER)),
    };
    (t.input as f64 * i + t.output as f64 * o + t.cache_read as f64 * cr + t.cache_write as f64 * cw) / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.into(),
            prices: vec![PriceSegment { input: 1.0, output: 2.0, cache_read: 0.1, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn names_normalize_and_match() {
        let c = Catalog::new(vec![entry("claude-sonnet-4-5"), entry("glm-5.2")]);
        let id = |m: &str| {
            let (k, i) = c.resolve(m);
            (k, i.map(|i| c.entries[i].id.clone()))
        };
        let exact = (MatchKind::Exact, Some("claude-sonnet-4-5".to_string()));
        assert_eq!(id("anthropic/claude-sonnet-4.5"), exact);
        assert_eq!(id("claude-sonnet-4-5-20250929"), exact);
        assert_eq!(id("claude-sonnet-4-5[1m]"), exact);
        assert_eq!(id("Claude-Sonnet-4.5 (high)"), exact);
        assert_eq!(id("glm-5.3"), (MatchKind::Approx, Some("glm-5.2".into())));
        assert_eq!(id("totally-unknown"), (MatchKind::None, None));
        assert_eq!(id(""), (MatchKind::None, None));
    }

    #[test]
    fn dated_catalog_ids_answer_undated_names() {
        let c = Catalog::new(vec![
            entry("claude-3-5-haiku-20241022"),
            entry("gpt-4o-2024-05-13"),
            entry("gpt-4o-2024-08-06"),
        ]);
        assert_eq!(c.resolve("claude-3-5-haiku").0, MatchKind::Exact);
        let (k, i) = c.resolve("gpt-4o");
        assert_eq!((k, c.entries[i.unwrap()].id.as_str()), (MatchKind::Exact, "gpt-4o-2024-08-06"));
        assert_eq!(strip_date("x@20250101"), "x");
        assert_eq!(strip_date("v4"), "v4");
    }

    #[test]
    fn name_windows_match_harness_prefixes_and_suffixes() {
        let c = Catalog::new(vec![
            entry("qwen3.8-flash"),
            entry("deepseek-v4.1-flash"),
            entry("gemini-3.8-flash"),
            entry("claude-opus-4-6"),
            entry("qwen3.6-plus"),
        ]);
        let id = |m: &str| {
            let (k, i) = c.resolve(m);
            (k, i.map(|i| c.entries[i].id.clone()))
        };
        let approx = |id: &str| (MatchKind::Approx, Some(id.to_string()));
        // Harness/CLI wrappers prepend their own tag.
        assert_eq!(id("qcn-qwen3.8-flash"), approx("qwen3.8-flash"));
        assert_eq!(id("cline-deepseek-v4.1-flash"), approx("deepseek-v4.1-flash"));
        // A non-tier suffix (a qualifier, an expiry tag, a free-tier marker) is dropped fine.
        assert_eq!(id("gemini-3.8-flash-high"), approx("gemini-3.8-flash"));
        assert_eq!(id("gemini-3.8-flash-exp-a"), approx("gemini-3.8-flash"));
        assert_eq!(id("deepseek-v4.1-flash-expires-on-0910"), approx("deepseek-v4.1-flash"));
        assert_eq!(id("claude-opus-4-6-thinking"), approx("claude-opus-4-6"));
        assert_eq!(id("qwen/qwen3.6-plus:free"), approx("qwen3.6-plus"));
        assert_eq!(id("qwen3.6-plus-free"), approx("qwen3.6-plus"));
    }

    #[test]
    fn name_window_guard_blocks_tier_word_drop() {
        let c = Catalog::new(vec![entry("gpt-5"), entry("qwen3.8"), entry("mimo"), entry("space-bunny")]);
        assert_eq!(c.resolve("gpt-5").0, MatchKind::Exact);
        assert_eq!(
            c.resolve("gpt-5-mini"),
            (MatchKind::None, None),
            "mini is a tier word: must not fall back to gpt-5"
        );
        assert_eq!(
            c.resolve("qwen3.8-max"),
            (MatchKind::None, None),
            "max is a tier word: must not fall back to qwen3.8"
        );
        assert_eq!(c.resolve("mimo-auto"), (MatchKind::None, None), "a one-word window needs a digit");
        let (k, i) = c.resolve("oc-space-bunny");
        assert_eq!(
            (k, i.map(|i| c.entries[i].id.as_str())),
            (MatchKind::Approx, Some("space-bunny")),
            "two words need no digit"
        );
        assert_eq!(c.resolve("qfmodel"), (MatchKind::None, None), "single word: no window at all");
    }

    #[test]
    fn reseller_consensus_median_free_and_zero_and_first_party_priority() {
        let md = json!({
            "anthropic": {"models": {"claude-x-1": {"cost": {"input": 3.0, "output": 15.0}}}},
            "resellerA": {"models": {
                "claude-x-1": {"cost": {"input": 1.0, "output": 1.0}},
                "foo-1": {"cost": {"input": 1.0, "output": 2.0}}
            }},
            "resellerB": {"models": {"foo-1": {"cost": {"input": 2.0, "output": 4.0}}}},
            "resellerC": {"models": {"foo-1": {"cost": {"input": 3.0, "output": 6.0}}}},
            "resellerD": {"models": {"foo-1:free": {"cost": {"input": 0.0, "output": 0.0}}}},
            "resellerE": {"models": {"bar-1": {"cost": {"input": 0.0, "output": 0.0}}}}
        });
        let e = parse_models_dev(&md);
        assert_eq!(
            e.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["claude-x-1", "foo-1"],
            "a reseller price never overrides the first-party one; :free and zero-priced listings are skipped"
        );
        assert_eq!(e[0].prices[0].input, 3.0, "first-party price, not resellerA's 1.0");
        assert_eq!((e[1].prices[0].input, e[1].prices[0].output), (2.0, 4.0), "median of 1/2/3 and 2/4/6");
        assert_eq!(e[1].source, MODELS_DEV);
    }

    fn vendor(id: &str, provider: &str, name: &str, input: f64, output: f64) -> CatalogEntry {
        CatalogEntry {
            provider: provider.into(),
            name: Some(name.into()),
            prices: vec![PriceSegment { input, output, cache_read: input / 10.0, ..Default::default() }],
            ..entry(id)
        }
    }

    #[test]
    fn vendor_display_name_beats_reseller_median() {
        // models.dev lists the vendor's own V4.1 Flash as `deepseek-flash`; harnesses say `deepseek-v4.1-flash`
        let reseller = CatalogEntry { consensus: true, ..entry("deepseek-v4.1-flash") };
        let c = Catalog::new(vec![
            reseller,
            vendor("deepseek-flash", "deepseek", "DeepSeek V4.1 Flash 1017", 0.15, 0.6),
            vendor("deepseek-v4-pro", "deepseek", "DeepSeek V4 Pro", 0.66, 1.98),
        ]);
        let id = |m: &str| {
            let (k, i) = c.resolve(m);
            (k, i.map(|i| c.entries[i].id.clone()))
        };
        let want = (MatchKind::Approx, Some("deepseek-flash".to_string()));
        assert_eq!(id("deepseek-v4.1-flash"), want, "the vendor's price replaces the reseller median");
        assert_eq!(id("deepseek-v4.1-flash-1017"), want, "a build tag does not matter");
        assert_eq!(id("cline-deepseek-v4.1-flash"), want, "nor does a harness prefix");
        assert_eq!(id("deepseek-v4-pro"), (MatchKind::Exact, Some("deepseek-v4-pro".into())));
        // an override or a vendor id under the same name is never replaced by a display-name hit
        let c = Catalog::new(vec![
            entry("deepseek-v4.1-flash"),
            vendor("deepseek-flash", "deepseek", "DeepSeek V4.1 Flash", 0.15, 0.6),
        ]);
        assert_eq!(c.resolve("deepseek-v4.1-flash").0, MatchKind::Exact);
        assert_eq!(c.entries[c.resolve("deepseek-v4.1-flash").1.unwrap()].prices[0].input, 1.0);
        // without a vendor listing the median still answers
        let c = Catalog::new(vec![CatalogEntry { consensus: true, ..entry("hy4-preview") }]);
        assert_eq!(c.resolve("hy4-preview").0, MatchKind::Exact);
    }

    #[test]
    fn display_name_shared_by_vendors_needs_an_owner_or_one_price() {
        let owned = Catalog::new(vec![
            vendor("kimi-a", "alibaba", "Kimi K3", 9.0, 9.0),
            vendor("kimi-k3", "moonshotai", "Kimi K3", 3.0, 15.0),
        ]);
        // neither provider prefixes `kimik3` and the prices differ: no guess
        assert_eq!(owned.resolve("kimi k3").0, MatchKind::None);
        let agree =
            Catalog::new(vec![vendor("a", "x", "Same Name 1", 1.0, 2.0), vendor("b", "y", "Same Name 1", 1.0, 2.0)]);
        assert_eq!(agree.resolve("same-name-1").0, MatchKind::Approx);
        let owner = Catalog::new(vec![
            vendor("flash-x", "alibaba", "DeepSeek Flash 9", 0.3, 1.2),
            vendor("flash-9", "deepseek", "DeepSeek Flash 9", 0.15, 0.6),
        ]);
        let hit = owner.resolve("deepseek-flash-9").1.unwrap();
        assert_eq!(owner.entries[hit].provider, "deepseek", "the vendor named in the model owns it");
    }

    #[test]
    fn models_dev_keeps_vendor_display_names_and_flags_medians() {
        let md = json!({
            "deepseek": {"models": {"deepseek-flash": {"name": "DeepSeek V4.1 Flash 1017", "cost": {"input": 0.15, "output": 0.6}}}},
            "resellerA": {"models": {"foo-1": {"name": "Foo 1", "cost": {"input": 1.0, "output": 2.0}}}}
        });
        let e = parse_models_dev(&md);
        assert_eq!((e[0].name.as_deref(), e[0].consensus), (Some("DeepSeek V4.1 Flash 1017"), false));
        assert_eq!((e[1].name.as_deref(), e[1].consensus), (None, true));
    }

    #[test]
    fn display_keys_drop_punctuation_and_trailing_build_tags() {
        assert_eq!(display_key("DeepSeek V4.1 Flash 1017"), "deepseekv41flash");
        assert_eq!(display_key("deepseek-v4.1-flash-20251101"), "deepseekv41flash");
        assert_eq!(display_key("GPT-5.6"), "gpt56");
        assert_eq!(display_key("o3"), "o3");
        assert_eq!(display_key("2025"), "2025", "a key that is only digits stays");
    }

    #[test]
    fn cost_formula_tiers_and_cache_write_fallback() {
        let s = PriceSegment {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            tiers: vec![PriceTier { above: 200_000, input: 6.0, output: 22.5, cache_read: 0.6, cache_write: None }],
            ..Default::default()
        };
        let t = Tokens { input: 1_000, output: 500, cache_read: 10_000, cache_write: 2_000 };
        let want = (1_000.0 * 3.0 + 500.0 * 15.0 + 10_000.0 * 0.3 + 2_000.0 * 3.0 * 1.25) / 1e6;
        assert!((step_cost(&s, t) - want).abs() < 1e-12);
        let big = Tokens { input: 150_000, output: 10, cache_read: 50_000, cache_write: 1 };
        let want = (150_000.0 * 6.0 + 10.0 * 22.5 + 50_000.0 * 0.6 + 6.0 * 1.25) / 1e6;
        assert!((step_cost(&s, big) - want).abs() < 1e-12, "whole request billed at the tier");
        let edge = Tokens { input: 200_000, ..Default::default() };
        assert!((step_cost(&s, edge) - 0.6).abs() < 1e-12, "exactly `above` stays in the base tier");
    }

    #[test]
    fn segments_by_time() {
        let mut e = entry("m");
        e.prices[0].until = Some(100);
        e.prices.push(PriceSegment { from: 100, input: 9.0, ..Default::default() });
        assert_eq!(segment_at(&e, -5).unwrap().input, 1.0);
        assert_eq!(segment_at(&e, 99).unwrap().input, 1.0);
        assert_eq!(segment_at(&e, 100).unwrap().input, 9.0);
    }

    #[test]
    fn upstream_formats() {
        let md = json!({
            "anthropic": {"models": {"claude-x-1": {"cost": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75}, "limit": {"context": 200000, "output": 64000}}}},
            "google": {"models": {"gemini-y-2": {"cost": {"input": 1.25, "output": 10, "tiers": [{"input": 2.5, "output": 15, "tier": {"type": "context", "size": 200000}}]}, "limit": {"context": 1000000, "output": 65536}}}},
            "openrouter": {"models": {"resold": {"cost": {"input": 9, "output": 9}}}},
            "openai": {"models": {"nocost": {"limit": {"context": 1}}}}
        });
        let mut e = parse_models_dev(&md);
        fill_missing(&mut e);
        assert_eq!(e.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["claude-x-1", "gemini-y-2", "resold"]);
        assert_eq!(e[0].prices[0].cache_write, Some(3.75));
        assert_eq!(e[1].prices[0].cache_read, 1.25, "missing cache price = full input price");
        assert_eq!(e[1].prices[0].tiers[0].above, 200_000);
        assert_eq!(e[1].context_limit, Some(1_000_000));
        // openrouter is not a first-party vendor: its listing becomes a reseller-consensus entry
        // (median over one vote = that vote), not a dropped/ignored one.
        assert_eq!(e[2].source, MODELS_DEV);
        assert_eq!(e[2].provider, "", "no `vendor/` prefix in the raw id");
        assert_eq!((e[2].prices[0].input, e[2].prices[0].output), (9.0, 9.0));

        let ll = json!({
            "claude-x-1": {"litellm_provider": "anthropic", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
            "gemini/gemini-z": {"litellm_provider": "gemini", "mode": "chat", "input_cost_per_token": 1.25e-6, "output_cost_per_token": 1e-5,
                "input_cost_per_token_above_200k_tokens": 2.5e-6, "output_cost_per_token_above_200k_tokens": 1.5e-5, "max_input_tokens": 1048576},
            "text-embedding-x": {"litellm_provider": "openai", "mode": "embedding", "input_cost_per_token": 1e-7, "output_cost_per_token": 0},
            "azure/gpt": {"litellm_provider": "azure", "mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6}
        });
        let mut l = parse_litellm(&ll);
        fill_missing(&mut l);
        assert_eq!(l.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["claude-x-1", "gemini-z"]);
        assert_eq!(l[1].prices[0].tiers[0].input, 2.5);
        assert_eq!(l[1].provider, "google");
        let m = merge(e, l);
        assert_eq!(m.len(), 4, "primary keeps claude-x-1, gemini-y-2, resold, fallback adds gemini-z");
        assert_eq!(m[0].source, MODELS_DEV);
    }
}
