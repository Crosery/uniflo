//! `uniflo_bm25(docs_fts)`: FTS5's `bm25()` for a single-phrase query, minus its IDF pass.
//!
//! Before ranking the first row, the built-in function counts each phrase's rows with one more
//! evaluation of the phrase over the whole table, close to doubling the query. With one phrase
//! the IDF is a common factor and that count is the number of rows the query returns, so the
//! caller multiplies by [`idf`] afterwards: the same scores in one pass.
//! `uniflo_rows(docs_fts)` returns the table's row count for that.

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

/// Register `uniflo_bm25` and `uniflo_rows` on `c`.
pub fn register(c: &Connection) -> Result<()> {
    // SAFETY: `c` is an open connection; the API pointer comes from SQLite itself and the
    // functions registered below only use the extension API they are handed.
    unsafe {
        let api = fts5_api(c.handle());
        ensure!(!api.is_null(), "SQLite was built without FTS5");
        let create = (*api).xCreateFunction.context("fts5_api without xCreateFunction")?;
        for (name, f) in [(c"uniflo_bm25", bm25 as AuxFn), (c"uniflo_rows", rows as AuxFn)] {
            let rc = create(api, name.as_ptr(), std::ptr::null_mut(), Some(f), None);
            ensure!(rc == ffi::SQLITE_OK, "register {name:?}: SQLite error {rc}");
        }
    }
    Ok(())
}

/// FTS5's IDF for a phrase found in `hits` of `rows` rows (floored like the built-in bm25).
pub fn idf(rows: i64, hits: i64) -> f64 {
    let v = (((rows - hits) as f64 + 0.5) / (hits as f64 + 0.5)).ln();
    if v <= 0.0 { 1e-6 } else { v }
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

/// The row's term of fts5Bm25Function with the IDF factor left out, in the same operation order.
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
    let (f, d) = (inst as f64, tokens as f64);
    Ok((f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * d / avgdl)))
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
}
