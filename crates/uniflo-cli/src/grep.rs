//! `uniflo grep`: full-text search over event bodies, one block per session.

use crate::render::{self, Style};
use crate::{Source, enc};
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::time::Duration;
use uniflo_core::Engine;
use uniflo_schema::search::SearchResponse;
use uniflo_search::fts::{Fts, FtsOptions, SearchParams};

pub fn run(src: &Source, terms: &[String], filter: Option<&str>, limit: usize, json: bool) -> Result<()> {
    let q = terms.join(" ");
    let raw: serde_json::Value = match src {
        Source::Daemon(c) => {
            let mut path = format!("/v1/search?q={}&limit={limit}", enc(&q));
            if let Some(f) = filter {
                path.push_str(&format!("&filter={}", enc(f)));
            }
            c.get_json(&path)?
        }
        Source::Local(engine) => {
            let p = SearchParams { q, filter: filter.map(str::to_owned), limit, ..Default::default() };
            serde_json::to_value(local(engine, &p)?)?
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&raw)?);
        return Ok(());
    }
    let r: SearchResponse = serde_json::from_value(raw)?;
    let st = Style::detect();
    for s in &r.results {
        println!("{}", render::search_session_line(&st, s));
        for h in &s.hits {
            println!("  {}", render::search_hit_line(&st, h));
        }
    }
    for line in footer(&r) {
        eprintln!("{line}");
    }
    Ok(())
}

/// Status lines after the hits: counts (`+` when the scan stopped early), index progress, and a
/// notice when a short-term scan ran out of time.
fn footer(r: &SearchResponse) -> Vec<String> {
    let more = if r.scanned_until.is_some() { "+" } else { "" };
    let mut tail = format!("{} of {}{more} matching sessions", r.results.len(), r.total);
    if r.indexing {
        tail.push_str(&format!(" · index still building ({}/{} sessions)", r.progress.done, r.progress.total));
    }
    let mut out = vec![tail];
    if r.partial {
        let until = r.scanned_until.map_or_else(|| "?".to_owned(), render::date_time);
        out.push(format!(
            "search stopped at its time budget: sessions last active before {until} were not searched \
             (add a term of 3+ characters to search everything)"
        ));
    }
    out
}

/// No daemon: catch the on-disk index up in-process (first run builds it), then query.
fn local(engine: &Arc<Engine>, p: &SearchParams) -> Result<SearchResponse> {
    let fts = Fts::start(engine.clone(), FtsOptions { pause: 0.0, ..Default::default() })?;
    let st = fts.status();
    if st.indexing {
        eprintln!(
            "uniflo: updating the full-text index {} ({} of {} sessions to read)…",
            st.path,
            st.progress.total - st.progress.done,
            st.progress.total
        );
    }
    while !fts.wait_idle(Duration::from_secs(5)) {
        let st = fts.status();
        eprintln!("  {}/{} sessions, {} events", st.progress.done, st.progress.total, st.progress.events);
    }
    fts.search(p).map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniflo_schema::search::{IndexProgress, SearchOrder};

    fn response(total: usize, partial: bool, scanned_until: Option<i64>) -> SearchResponse {
        SearchResponse {
            q: "缓存".into(),
            filter: None,
            order: SearchOrder::Recent,
            total,
            offset: 0,
            limit: 20,
            indexing: false,
            progress: IndexProgress::default(),
            results: vec![],
            partial,
            scanned_until,
        }
    }

    #[test]
    fn footer_flags_lower_bounds_and_partial_scans() {
        assert_eq!(footer(&response(3, false, None)), vec!["0 of 3 matching sessions"]);
        assert_eq!(footer(&response(21, false, Some(1_790_000_000_000))), vec!["0 of 21+ matching sessions"]);
        let lines = footer(&response(2, true, Some(1_790_000_000_000)));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("2+ matching sessions"));
        assert!(lines[1].starts_with("search stopped at its time budget") && lines[1].contains("3+ characters"));
    }
}
