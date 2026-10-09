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
    let shown = r.results.len();
    let mut tail = format!("{shown} of {} matching sessions", r.total);
    if r.indexing {
        tail.push_str(&format!(" · index still building ({}/{} sessions)", r.progress.done, r.progress.total));
    }
    eprintln!("{tail}");
    Ok(())
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
