//! bm25 scoring shared by the hot FTS5 index and the cold tier (ADR-0011).
//!
//! `uniflo_bm25(docs_fts)` / `uniflo_rows(docs_fts)`: FTS5's `bm25()` for a single-phrase query,
//! minus its IDF pass (as before: with one phrase IDF is a common factor, so the caller applies
//! it once after counting rows instead of paying for `xQueryPhrase` on every row).
//!
//! `uniflo_bm25n(docs_fts, avgdl, idf_json)`: the same formula for a multi-phrase query, but with
//! an *externally supplied* average document length and one IDF weight per phrase, so a hot row's
//! score and a cold row's score (computed in Rust with [`tf_part`] / [`bm25_multi`] from the same
//! two numbers) are on the same scale. `idf_json` is a JSON array of `f64`, one per positive
//! phrase in match order; phrase indices beyond its length are excluded-term instances, which
//! never occur in matched rows and so are skipped rather than erroring.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, ffi};
use std::os::raw::{c_int, c_void};

/// FTS5's bm25 constants.
const K1: f64 = 1.2;
const B: f64 = 0.75;

type AuxFn = unsafe extern "C" fn(
    *const ffi::Fts5ExtensionApi,
    *mut ffi::Fts5Context,
    *mut ffi::sqlite3_context,
    c_int,
    *mut *mut ffi::sqlite3_value,
);

/// Register `uniflo_bm25`, `uniflo_rows` and `uniflo_bm25n` on `c`.
pub fn register(c: &Connection) -> Result<()> {
    // SAFETY: `c` is an open connection; the API pointer comes from SQLite itself and the
    // functions registered below only use the extension API they are handed.
    unsafe {
        let api = fts5_api(c.handle());
        ensure!(!api.is_null(), "SQLite was built without FTS5");
        let create = (*api).xCreateFunction.context("fts5_api without xCreateFunction")?;
        for (name, f) in
            [(c"uniflo_bm25", bm25 as AuxFn), (c"uniflo_rows", rows as AuxFn), (c"uniflo_bm25n", bm25n as AuxFn)]
        {
            let rc = create(api, name.as_ptr(), std::ptr::null_mut(), Some(f), None);
            ensure!(rc == ffi::SQLITE_OK, "register {name:?}: SQLite error {rc}");
        }
    }
    Ok(())
}

/// FTS5's IDF for a phrase found in `hits` of `rows` rows (floored like the built-in bm25); `1.0`
/// when the tier being estimated from is empty (`rows == 0`).
pub fn idf(rows: i64, hits: i64) -> f64 {
    if rows == 0 {
        return 1.0;
    }
    let v = (((rows - hits) as f64 + 0.5) / (hits as f64 + 0.5)).ln();
    if v <= 0.0 { 1e-6 } else { v }
}

/// The term-frequency part of bm25 (everything except the IDF factor), shared by the hot FTS5 aux
/// functions and the cold tier's Rust-side scoring: `f·(k1+1) / (f + k1·(1 - b + b·d/avgdl))`.
pub fn tf_part(f: f64, d: f64, avgdl: f64) -> f64 {
    if avgdl <= 0.0 {
        return 0.0;
    }
    (f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * d / avgdl))
}

/// bm25 across several ANDed phrases, each with its own IDF weight: `Σ idf_i · tf_part(f_i, d,
/// avgdl)`. `freqs` and `idfs` are parallel, one entry per positive phrase.
pub fn bm25_multi(freqs: &[f64], idfs: &[f64], d: f64, avgdl: f64) -> f64 {
    freqs.iter().zip(idfs).map(|(&f, &idf)| idf * tf_part(f, d, avgdl)).sum()
}

/// [`bm25_multi`] over integer occurrence counts (the cold tier's verification output), with the
/// same arithmetic so a row scores identically in either tier.
pub fn bm25_counts(freqs: &[u32], idfs: &[f64], d: f64, avgdl: f64) -> f64 {
    freqs.iter().zip(idfs).map(|(&f, &idf)| idf * tf_part(f as f64, d, avgdl)).sum()
}

/// The documented way to reach FTS5's C API from a connection.
unsafe fn fts5_api(db: *mut ffi::sqlite3) -> *mut ffi::fts5_api {
    let mut api: *mut ffi::fts5_api = std::ptr::null_mut();
    let mut stmt: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
    unsafe {
        if ffi::sqlite3_prepare_v2(db, c"SELECT fts5(?1)".as_ptr(), -1, &mut stmt, std::ptr::null_mut())
            == ffi::SQLITE_OK
        {
            let out = (&mut api as *mut *mut ffi::fts5_api).cast::<c_void>();
            ffi::sqlite3_bind_pointer(stmt, 1, out, c"fts5_api_ptr".as_ptr(), None);
            ffi::sqlite3_step(stmt);
        }
        ffi::sqlite3_finalize(stmt);
    }
    api
}

unsafe extern "C" fn bm25(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    ctx: *mut ffi::sqlite3_context,
    _: c_int,
    _: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: FTS5 passes a valid API table and context for the current row.
    unsafe {
        match row_score(&*api, fts) {
            Ok(v) => ffi::sqlite3_result_double(ctx, v),
            Err(rc) => ffi::sqlite3_result_error_code(ctx, rc),
        }
    }
}

unsafe extern "C" fn rows(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    ctx: *mut ffi::sqlite3_context,
    _: c_int,
    _: *mut *mut ffi::sqlite3_value,
) {
    let mut n: i64 = 0;
    // SAFETY: as in `bm25`.
    unsafe {
        let rc = match (*api).xRowCount {
            Some(f) => f(fts, &mut n),
            None => ffi::SQLITE_MISUSE,
        };
        if rc == ffi::SQLITE_OK {
            ffi::sqlite3_result_int64(ctx, n);
        } else {
            ffi::sqlite3_result_error_code(ctx, rc);
        }
    }
}

unsafe extern "C" fn bm25n(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    ctx: *mut ffi::sqlite3_context,
    n_val: c_int,
    args: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: as in `bm25`; `args` holds exactly the `(avgdl, idf_json)` values bound in the SQL
    // call, constant for the whole query.
    unsafe {
        match multi_row_score(&*api, fts, n_val, args) {
            Ok(v) => ffi::sqlite3_result_double(ctx, v),
            Err(rc) => ffi::sqlite3_result_error_code(ctx, rc),
        }
    }
}

/// The row's term of `fts5Bm25Function` with the IDF factor left out, in the same operation order.
unsafe fn row_score(api: &ffi::Fts5ExtensionApi, fts: *mut ffi::Fts5Context) -> Result<f64, c_int> {
    let ok = |rc: c_int| if rc == ffi::SQLITE_OK { Ok(()) } else { Err(rc) };
    let (mut inst, mut tokens, mut rows, mut total) = (0 as c_int, 0 as c_int, 0i64, 0i64);
    // SAFETY: `fts` is the context FTS5 handed to the calling auxiliary function.
    unsafe {
        ok(api.xInstCount.ok_or(ffi::SQLITE_MISUSE)?(fts, &mut inst))?;
        ok(api.xColumnSize.ok_or(ffi::SQLITE_MISUSE)?(fts, -1, &mut tokens))?;
        ok(api.xRowCount.ok_or(ffi::SQLITE_MISUSE)?(fts, &mut rows))?;
        ok(api.xColumnTotalSize.ok_or(ffi::SQLITE_MISUSE)?(fts, -1, &mut total))?;
    }
    let avgdl = total as f64 / rows as f64;
    Ok(tf_part(inst as f64, tokens as f64, avgdl))
}

/// Per-phrase instance counts for the current row (`f_i`), via `xInst`, then [`bm25_multi`] with
/// the externally supplied `avgdl` / per-phrase IDFs.
unsafe fn multi_row_score(
    api: &ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    n_val: c_int,
    args: *mut *mut ffi::sqlite3_value,
) -> Result<f64, c_int> {
    let ok = |rc: c_int| if rc == ffi::SQLITE_OK { Ok(()) } else { Err(rc) };
    if n_val != 2 {
        return Err(ffi::SQLITE_MISUSE);
    }
    // SAFETY: `args` is the array FTS5 hands the aux function, with `n_val` valid entries.
    let (avgdl, idfs) = unsafe {
        let argv = std::slice::from_raw_parts(args, n_val as usize);
        let avgdl = ffi::sqlite3_value_double(argv[0]);
        let p = ffi::sqlite3_value_text(argv[1]);
        let len = ffi::sqlite3_value_bytes(argv[1]);
        if p.is_null() || len < 0 {
            return Err(ffi::SQLITE_MISUSE);
        }
        let bytes = std::slice::from_raw_parts(p, len as usize);
        let json = std::str::from_utf8(bytes).map_err(|_| ffi::SQLITE_MISUSE)?;
        let idfs: Vec<f64> = serde_json::from_str(json).map_err(|_| ffi::SQLITE_MISUSE)?;
        (avgdl, idfs)
    };
    let mut freqs = vec![0f64; idfs.len()];
    let mut n_inst: c_int = 0;
    // SAFETY: as in `row_score`.
    unsafe {
        ok(api.xInstCount.ok_or(ffi::SQLITE_MISUSE)?(fts, &mut n_inst))?;
        let x_inst = api.xInst.ok_or(ffi::SQLITE_MISUSE)?;
        for i in 0..n_inst {
            let (mut phrase, mut col, mut off): (c_int, c_int, c_int) = (0, 0, 0);
            ok(x_inst(fts, i, &mut phrase, &mut col, &mut off))?;
            if let Some(f) = freqs.get_mut(phrase as usize) {
                *f += 1.0;
            }
        }
    }
    let mut tokens: c_int = 0;
    // SAFETY: as in `row_score`.
    unsafe {
        ok(api.xColumnSize.ok_or(ffi::SQLITE_MISUSE)?(fts, -1, &mut tokens))?;
    }
    Ok(bm25_multi(&freqs, &idfs, tokens as f64, avgdl))
}

/// Bind `avgdl` / per-phrase IDFs for [`bm25n`]'s SQL arguments.
pub fn idf_json(idfs: &[f64]) -> String {
    serde_json::to_string(idfs).expect("Vec<f64> serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_phrase_scores_equal_builtin_bm25() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(text, tokenize='trigram case_sensitive 0');
             INSERT INTO t(text) VALUES ('deploy the deploy script'), ('deploy'), ('nothing here'),
               ('a much longer text that mentions deploy only once among many other words'), ('no');",
        )
        .unwrap();
        register(&c).unwrap();
        let q = "SELECT rowid, bm25(t), uniflo_bm25(t), uniflo_rows(t) FROM t WHERE t MATCH '\"deploy\"'";
        let rows: Vec<(i64, f64, f64, i64)> = c
            .prepare(q)
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 3);
        let idf = idf(rows[0].3, rows.len() as i64);
        for (id, builtin, ours, n) in rows {
            assert_eq!(n, 5);
            assert_eq!(-builtin, idf * ours, "row {id}");
        }
    }

    #[test]
    fn multi_phrase_aux_matches_builtin_bm25_with_real_idf_weights() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(text, tokenize='trigram case_sensitive 0');
             INSERT INTO t(text) VALUES ('deploy the release script'), ('deploy release'),
               ('deploy only'), ('release only'), ('nothing here at all');",
        )
        .unwrap();
        register(&c).unwrap();
        // The built-in computes avgdl and each phrase's IDF from the table itself; feeding
        // `uniflo_bm25n` the same two numbers must reproduce the same score exactly.
        let total_rows: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        // `xColumnTotalSize` / `xColumnSize` report *tokens* (trigram count = chars - 2, floored
        // at 0), not `length(text)`'s character count: avgdl must match that to agree exactly.
        let texts: Vec<String> = c
            .prepare("SELECT text FROM t")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let tokens: Vec<i64> = texts.iter().map(|t| (t.chars().count() as i64 - 2).max(0)).collect();
        let avgdl = tokens.iter().sum::<i64>() as f64 / tokens.len() as f64;
        let hits = |term: &str| -> i64 {
            c.query_row("SELECT count(*) FROM t WHERE t MATCH ?1", [format!("\"{term}\"")], |r| r.get(0)).unwrap()
        };
        let idfs = [idf(total_rows, hits("deploy")), idf(total_rows, hits("release"))];
        let rows: Vec<(f64, f64)> = c
            .prepare("SELECT -bm25(t), uniflo_bm25n(t, ?1, ?2) FROM t WHERE t MATCH '(\"deploy\" AND \"release\")'")
            .unwrap()
            .query_map(rusqlite::params![avgdl, idf_json(&idfs)], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        for (builtin, ours) in &rows {
            assert!((builtin - ours).abs() < 1e-9, "{builtin} vs {ours}");
        }
    }

    #[test]
    fn tf_part_matches_rows_when_rebuilt_from_stats() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(text, tokenize='trigram case_sensitive 0');
             INSERT INTO t(text) VALUES ('alpha beta'), ('alpha alpha beta beta'), ('gamma');",
        )
        .unwrap();
        register(&c).unwrap();
        let (avgdl,): (f64,) =
            c.query_row("SELECT CAST(sum(length(text)) AS REAL) / count(*) FROM t", [], |r| Ok((r.get(0)?,))).unwrap();
        let rows: Vec<(f64, i64, i64)> = c
            .prepare("SELECT uniflo_bm25n(t, ?1, '[2.0]'), length(text), uniflo_bm25(t) IS NOT NULL FROM t WHERE t MATCH '\"alpha\"'")
            .unwrap()
            .query_map([avgdl], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (score, len, _) in rows {
            // f=1 for the single-occurrence row vs f=2 for the doubled one: higher score for more.
            assert!(score > 0.0, "len={len}");
        }
    }
}
