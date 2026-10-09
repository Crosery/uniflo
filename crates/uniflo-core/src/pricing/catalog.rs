//! Catalog building blocks: upstream formats → [`CatalogEntry`], model-name matching, step cost.

use serde_json::Value;
use std::collections::HashMap;
use uniflo_schema::{CatalogEntry, MatchKind, PriceSegment, PriceTier};

/// Official vendors whose own list prices are used; resellers mark up or drop cache prices.
/// Order breaks ties when two vendors list the same model id.
pub const FIRST_PARTY: &[&str] =
    &["anthropic", "openai", "google", "minimax", "moonshotai", "zai", "deepseek", "alibaba", "xai", "groq"];

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

fn last_segment(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// models.dev `api.json`: `{provider: {models: {id: {cost: {input, output, cache_read?,
/// cache_write?, tiers?[{tier:{type:"context", size}, …}], context_over_200k?}, limit: {context, output}}}}}`.
/// Only [`FIRST_PARTY`] vendors are read. A missing cache-read price is `NaN` until [`fill_missing`].
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
            });
        }
    }
    out
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

/// Entries plus lookup indexes; later layers are merged in by the caller.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
    exact: HashMap<String, usize>,
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
        Catalog { entries, exact, families }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Exact (after normalization) → nearest version of the same name skeleton (`approx`) → none.
    pub fn resolve(&self, model: &str) -> (MatchKind, Option<usize>) {
        let base = base_name(model);
        if base.is_empty() {
            return (MatchKind::None, None);
        }
        let dateless = match_key(strip_date(base));
        if let Some(&i) = self.exact.get(&match_key(base)).or_else(|| self.exact.get(&dateless)) {
            return (MatchKind::Exact, Some(i));
        }
        let Some((sk, want)) = skeleton(&dateless) else { return (MatchKind::None, None) };
        let best = self.families.get(&sk).and_then(|cands| {
            cands
                .iter()
                .map(|(i, got)| (got.iter().zip(&want).map(|(a, b)| a.abs_diff(*b)).sum::<u64>(), *i))
                .min_by(|a, b| a.0.cmp(&b.0).then_with(|| self.entries[a.1].id.cmp(&self.entries[b.1].id)))
        });
        match best {
            Some((_, i)) => (MatchKind::Approx, Some(i)),
            None => (MatchKind::None, None),
        }
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
        assert_eq!(e.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["claude-x-1", "gemini-y-2"]);
        assert_eq!(e[0].prices[0].cache_write, Some(3.75));
        assert_eq!(e[1].prices[0].cache_read, 1.25, "missing cache price = full input price");
        assert_eq!(e[1].prices[0].tiers[0].above, 200_000);
        assert_eq!(e[1].context_limit, Some(1_000_000));

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
        assert_eq!(m.len(), 3, "primary keeps claude-x-1, fallback adds gemini-z");
        assert_eq!(m[0].source, MODELS_DEV);
    }
}
