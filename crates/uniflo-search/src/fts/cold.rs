//! Cold tier: sessions older than `hot_days` are cut into ≤ 64 KB blocks, each one zstd frame of
//! per-row metadata followed by the row texts, plus a contentless trigram index over the blocks
//! (`blk_fts`, rowid = block id) used only to find *candidate blocks* — matching inside a block is
//! verified in Rust ([`super::verify`]) since `detail=none` cannot run phrase queries.
//!
//! A session's cold content is compared against a whole-session fingerprint (not a row-by-row
//! diff against the old blocks): recomputing it needs no decompression, and an unchanged session
//! is common (a version bump's refresh pass touches every session, but content rarely changed).

use super::store;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use std::io::Read;

/// A block's directory row (`blk`), without its compressed payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blk {
    pub bid: i64,
    pub sid: i64,
    pub first: i64,
    pub last: i64,
    pub rows: i64,
    pub kinds: i64,
    pub tokens: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WriteStats {
    pub blocks: i64,
    pub rows: i64,
    pub tokens: i64,
}

fn compress(raw: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(raw, ruzstd::encoding::CompressionLevel::Fastest)
}

fn decompress(z: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(z).map_err(|e| anyhow::anyhow!("zstd: {e}"))?.read_to_end(&mut out)?;
    Ok(out)
}

/// Decompress and decode one block's raw `blk_data.z` bytes, for [`super::cache::BlockCache`]
/// (which also wants the decompressed byte size for its budget).
pub fn decode_compressed(z: &[u8]) -> Result<(usize, Vec<store::Row>)> {
    let payload = decompress(z)?;
    let rows = decode_block(&payload)?;
    Ok((payload.len(), rows))
}

fn push_varint(buf: &mut Vec<u8>, mut x: u64) {
    loop {
        let b = (x & 0x7f) as u8;
        x >>= 7;
        if x == 0 {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let (mut x, mut shift) = (0u64, 0u32);
    loop {
        let b = *buf.get(*pos).context("truncated cold block (varint)")?;
        *pos += 1;
        x |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(x);
        }
        shift += 7;
        bail_on_overflow(shift)?;
    }
}

fn bail_on_overflow(shift: u32) -> Result<()> {
    if shift > 63 {
        bail!("truncated cold block (varint overflow)");
    }
    Ok(())
}

fn push_zigzag(buf: &mut Vec<u8>, x: i64) {
    push_varint(buf, ((x << 1) ^ (x >> 63)) as u64);
}

fn read_zigzag(buf: &[u8], pos: &mut usize) -> Result<i64> {
    let u = read_varint(buf, pos)?;
    Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
}

/// Encode one block's rows (metadata, then the concatenated texts) before compression.
fn encode_block(rows: &[store::Row]) -> Vec<u8> {
    let mut meta = Vec::new();
    push_varint(&mut meta, rows.len() as u64);
    let (mut prev_id, mut prev_ts) = (0i64, 0i64);
    for r in rows {
        push_zigzag(&mut meta, r.id - prev_id);
        push_zigzag(&mut meta, r.ts - prev_ts);
        prev_id = r.id;
        prev_ts = r.ts;
        match r.pos {
            Some(p) => {
                meta.push(1);
                push_zigzag(&mut meta, p);
            }
            None => meta.push(0),
        }
        let eb = r.event.as_bytes();
        push_varint(&mut meta, eb.len() as u64);
        meta.extend_from_slice(eb);
        push_varint(&mut meta, r.text.len() as u64);
    }
    meta.reserve(rows.iter().map(|r| r.text.len()).sum());
    for r in rows {
        meta.extend_from_slice(r.text.as_bytes());
    }
    meta
}

fn decode_block(payload: &[u8]) -> Result<Vec<store::Row>> {
    let mut pos = 0usize;
    let n = read_varint(payload, &mut pos)? as usize;
    struct Hdr {
        id: i64,
        ts: i64,
        pos: Option<i64>,
        event: String,
        text_len: usize,
    }
    let mut hdrs = Vec::with_capacity(n);
    let (mut id, mut ts) = (0i64, 0i64);
    for _ in 0..n {
        id += read_zigzag(payload, &mut pos)?;
        ts += read_zigzag(payload, &mut pos)?;
        let has_pos = *payload.get(pos).context("truncated cold block (pos flag)")?;
        pos += 1;
        let p = if has_pos != 0 { Some(read_zigzag(payload, &mut pos)?) } else { None };
        let elen = read_varint(payload, &mut pos)? as usize;
        let event_bytes = payload.get(pos..pos + elen).context("truncated cold block (event)")?;
        let event = String::from_utf8(event_bytes.to_vec()).context("cold block event id not UTF-8")?;
        pos += elen;
        let text_len = read_varint(payload, &mut pos)? as usize;
        hdrs.push(Hdr { id, ts, pos: p, event, text_len });
    }
    let mut rows = Vec::with_capacity(n);
    for h in hdrs {
        let text_bytes = payload.get(pos..pos + h.text_len).context("truncated cold block (text)")?;
        let text = String::from_utf8(text_bytes.to_vec()).context("cold block text not UTF-8")?;
        pos += h.text_len;
        rows.push(store::Row { id: h.id, event: h.event, ts: h.ts, pos: h.pos, text });
    }
    Ok(rows)
}

/// Cut rows (already in the order they will be written) into runs of at most `max` bytes of
/// text; a single row over `max` gets a block of its own.
fn cut(rows: &[store::Row], max: usize) -> Vec<std::ops::Range<usize>> {
    let (mut out, mut lo, mut bytes) = (Vec::new(), 0usize, 0usize);
    for (i, row) in rows.iter().enumerate() {
        if bytes > 0 && bytes + row.text.len() > max {
            out.push(lo..i);
            lo = i;
            bytes = 0;
        }
        bytes += row.text.len() + 1;
    }
    if lo < rows.len() {
        out.push(lo..rows.len());
    }
    out
}

/// A session's rows, hashed by content: used to skip rewriting cold blocks whose content has not
/// changed (e.g. a version-bump refresh pass, or a repeat `index_session`). Each row contributes
/// its kind, event id, time, position and text, but not its sequence number or place in the list:
/// a migrated or compacted session numbers its rows in arrival order, a fresh cold pass newest
/// first, and the same content must still compare equal. FNV-1a, so the value is stable across
/// builds (the fingerprints are stored).
pub fn fingerprint(rows: &[store::Row]) -> i64 {
    let mut sum = rows.len() as u64;
    for r in rows {
        let mut h = Fnv::default();
        h.write(&store::kind_of(r.id).to_le_bytes());
        h.write(&r.ts.to_le_bytes());
        h.write(&r.pos.unwrap_or(-1).to_le_bytes());
        h.write(r.event.as_bytes());
        h.write(&[0]);
        h.write(r.text.as_bytes());
        sum = sum.wrapping_add(h.0);
    }
    sum as i64
}

struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Write `rows` (already ordered) as new cold blocks for `sid`. Does not touch counters or
/// `sess`; callers bump [`store::bump_cold`] and `sess.cold` / `sess.fingerprint` themselves.
pub fn write_blocks(c: &Connection, sid: i64, rows: &[store::Row]) -> Result<WriteStats> {
    if rows.is_empty() {
        return Ok(WriteStats::default());
    }
    let mut ib = c.prepare_cached(
        "INSERT INTO blk (sid, first, last, rows, kinds, tokens, bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut iz = c.prepare_cached("INSERT INTO blk_data (bid, z) VALUES (?1, ?2)")?;
    let mut iff = c.prepare_cached("INSERT INTO blk_fts (rowid, text) VALUES (?1, ?2)")?;
    let mut stats = WriteStats { rows: rows.len() as i64, ..Default::default() };
    for range in cut(rows, store::COLD_BLOCK_MAX) {
        let chunk = &rows[range];
        let payload = encode_block(chunk);
        let z = compress(&payload);
        let kinds = chunk.iter().fold(0i64, |acc, r| acc | store::kind_bit(store::kind_of(r.id)));
        let tokens: i64 = chunk.iter().map(|r| store::token_count(&r.text)).sum();
        ib.execute(params![
            sid,
            chunk.first().unwrap().id,
            chunk.last().unwrap().id,
            chunk.len() as i64,
            kinds,
            tokens,
            z.len() as i64
        ])?;
        let bid = c.last_insert_rowid();
        iz.execute(params![bid, z])?;
        let joined = chunk.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("\u{1}");
        iff.execute(params![bid, joined])?;
        stats.blocks += 1;
        stats.tokens += tokens;
    }
    Ok(stats)
}

/// All of a session's cold rows, decompressed, in block order.
pub fn read_blocks(c: &Connection, sid: i64) -> Result<Vec<store::Row>> {
    let bids: Vec<i64> = c
        .prepare_cached("SELECT bid FROM blk WHERE sid = ?1 ORDER BY first")?
        .query_map([sid], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for bid in bids {
        let z: Vec<u8> = c.prepare_cached("SELECT z FROM blk_data WHERE bid = ?1")?.query_row([bid], |r| r.get(0))?;
        out.extend(decode_block(&decompress(&z)?)?);
    }
    Ok(out)
}

/// A session's block directory (no payloads), block order.
pub fn block_meta(c: &Connection, sid: i64) -> Result<Vec<Blk>> {
    let mut st = c.prepare_cached(
        "SELECT bid, sid, first, last, rows, kinds, tokens, bytes FROM blk WHERE sid = ?1 ORDER BY first",
    )?;
    let rows = st.query_map([sid], |r| {
        Ok(Blk {
            bid: r.get(0)?,
            sid: r.get(1)?,
            first: r.get(2)?,
            last: r.get(3)?,
            rows: r.get(4)?,
            kinds: r.get(5)?,
            tokens: r.get(6)?,
            bytes: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Remove all of a session's cold blocks; returns `(rows, tokens)` removed (for the caller to
/// subtract from [`store::Counters`]).
pub fn delete_blocks(c: &Connection, sid: i64) -> Result<(i64, i64)> {
    let bids: Vec<i64> = c
        .prepare_cached("SELECT bid FROM blk WHERE sid = ?1")?
        .query_map([sid], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if bids.is_empty() {
        return Ok((0, 0));
    }
    let (rows, tokens): (i64, i64) =
        c.query_row("SELECT coalesce(sum(rows), 0), coalesce(sum(tokens), 0) FROM blk WHERE sid = ?1", [sid], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    for bid in &bids {
        // `contentless_delete=1`: a plain DELETE by rowid tombstones the row, no content needed.
        c.prepare_cached("DELETE FROM blk_fts WHERE rowid = ?1")?.execute([bid])?;
        c.prepare_cached("DELETE FROM blk_data WHERE bid = ?1")?.execute([bid])?;
    }
    c.prepare_cached("DELETE FROM blk WHERE sid = ?1")?.execute([sid])?;
    Ok((rows, tokens))
}

/// Promote a cold session back to hot: decompress its blocks, reinsert the rows under their
/// original ids, and drop the cold blocks. Bumps both tiers' counters and `sess.cold`.
pub fn promote(c: &Connection, sid: i64) -> Result<()> {
    let rows = read_blocks(c, sid)?;
    let (cold_rows, cold_tokens) = delete_blocks(c, sid)?;
    store::bump_cold(c, -cold_rows, -cold_tokens)?;
    let mut hot_tokens = 0i64;
    for r in &rows {
        store::insert_hot_row(c, sid, r)?;
        hot_tokens += store::token_count(&r.text);
    }
    store::bump_hot(c, rows.len() as i64, hot_tokens)?;
    store::set_cold(c, sid, false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, event: &str, ts: i64, pos: Option<i64>, text: &str) -> store::Row {
        store::Row { id, event: event.into(), ts, pos, text: text.into() }
    }

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE blk (bid INTEGER PRIMARY KEY AUTOINCREMENT, sid INTEGER NOT NULL, first INTEGER NOT NULL, \
             last INTEGER NOT NULL, rows INTEGER NOT NULL, kinds INTEGER NOT NULL, tokens INTEGER NOT NULL, bytes \
             INTEGER NOT NULL);
             CREATE TABLE blk_data (bid INTEGER PRIMARY KEY, z BLOB NOT NULL);
             CREATE VIRTUAL TABLE blk_fts USING fts5(text, content='', contentless_delete=1, detail=none, \
             tokenize='trigram case_sensitive 0');
             CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE sess (sid INTEGER PRIMARY KEY, key TEXT, done_upd INTEGER, done_pos INTEGER, pinned \
             INTEGER NOT NULL DEFAULT 0, cold INTEGER NOT NULL DEFAULT 0, fingerprint INTEGER, meta TEXT);
             CREATE TABLE docs (id INTEGER PRIMARY KEY, sid INTEGER, event TEXT, ts INTEGER, pos INTEGER, text TEXT);
             CREATE VIRTUAL TABLE docs_fts USING fts5(text, content='docs', content_rowid='id', \
             tokenize='trigram case_sensitive 0');",
        )
        .unwrap();
        c.execute("INSERT INTO sess (sid, key) VALUES (1, 's')", []).unwrap();
        c
    }

    #[test]
    fn block_round_trips_metadata_and_text() {
        let rows = vec![
            row(store::row_id(1, 0, 1), "e0", 1000, Some(7), "hello 缓存"),
            row(store::row_id(1, 1, 2), "e1", 1005, None, &"x".repeat(50)),
        ];
        let payload = encode_block(&rows);
        let back = decode_block(&payload).unwrap();
        assert_eq!(back.len(), rows.len());
        for (a, b) in rows.iter().zip(back.iter()) {
            assert_eq!(
                (a.id, a.event.as_str(), a.ts, a.pos, a.text.as_str()),
                (b.id, b.event.as_str(), b.ts, b.pos, b.text.as_str())
            );
        }
    }

    #[test]
    fn cut_keeps_blocks_under_budget_and_isolates_huge_rows() {
        let rows: Vec<store::Row> = (0..5).map(|i| row(i, "e", 0, None, &"a".repeat(30))).collect();
        let ranges = cut(&rows, 100);
        for r in &ranges {
            let bytes: usize = rows[r.clone()].iter().map(|x| x.text.len() + 1).sum();
            assert!(bytes <= 100 || r.len() == 1);
        }
        let mut huge = vec![row(0, "e", 0, None, &"a".repeat(10))];
        huge.push(row(1, "big", 0, None, &"b".repeat(200)));
        huge.push(row(2, "e", 0, None, &"c".repeat(10)));
        let ranges = cut(&huge, 64);
        assert!(ranges.iter().any(|r| r.len() == 1 && huge[r.start].event == "big"), "{ranges:?}");
    }

    #[test]
    fn write_read_and_delete_blocks_round_trip() {
        let c = conn();
        let rows: Vec<store::Row> = (0..40)
            .map(|i| row(store::row_id(1, i, 1), &format!("e{i}"), 1000 + i, Some(i), &format!("message {i} deploy")))
            .collect();
        let stats = write_blocks(&c, 1, &rows).unwrap();
        assert!(stats.blocks >= 1 && stats.rows == 40);
        let back = read_blocks(&c, 1).unwrap();
        assert_eq!(back.len(), 40);
        assert_eq!(back[0].event, "e0");
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM blk_fts WHERE blk_fts MATCH '\"dep\" AND \"epl\" AND \"plo\" AND \"loy\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, stats.blocks);
        let (drows, dtokens) = delete_blocks(&c, 1).unwrap();
        assert_eq!(drows, 40);
        assert!(dtokens > 0);
        assert_eq!(read_blocks(&c, 1).unwrap().len(), 0);
        let left: i64 = c.query_row("SELECT count(*) FROM blk", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_content() {
        let a = vec![row(1, "e0", 1, None, "alpha")];
        let b = vec![row(1, "e0", 1, None, "alpha")];
        let c2 = vec![row(1, "e0", 1, None, "alphabet")];
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&c2));
        // The hash is stored, so it must not change between builds or toolchains.
        assert_eq!(fingerprint(&[row(store::row_id(1, 0, 1), "e0", 1, None, "alpha")]), 9_032_083_803_131_918_877);
    }

    #[test]
    fn fingerprint_ignores_row_numbering_and_order_but_not_kind_or_metadata() {
        let r = |sid, seq, kind, event: &str, ts, text: &str| row(store::row_id(sid, seq, kind), event, ts, None, text);
        let arrival = vec![r(1, 0, 1, "e0", 10, "alpha"), r(1, 1, 2, "e1", 20, "beta")];
        let newest_first = vec![r(7, 0, 2, "e1", 20, "beta"), r(7, 1, 1, "e0", 10, "alpha")];
        assert_eq!(fingerprint(&arrival), fingerprint(&newest_first));
        let other_kind = vec![r(1, 0, 2, "e0", 10, "alpha"), r(1, 1, 2, "e1", 20, "beta")];
        assert_ne!(fingerprint(&arrival), fingerprint(&other_kind));
        let other_ts = vec![r(1, 0, 1, "e0", 11, "alpha"), r(1, 1, 2, "e1", 20, "beta")];
        assert_ne!(fingerprint(&arrival), fingerprint(&other_ts));
        assert_ne!(fingerprint(&arrival), fingerprint(&arrival[..1]));
    }

    #[test]
    fn promote_restores_original_ids_and_clears_cold() {
        let c = conn();
        let rows: Vec<store::Row> = (0..3)
            .map(|i| row(store::row_id(1, i, 1), &format!("e{i}"), 1000 + i, None, &format!("text {i}")))
            .collect();
        write_blocks(&c, 1, &rows).unwrap();
        store::bump_cold(&c, rows.len() as i64, rows.iter().map(|r| store::token_count(&r.text)).sum()).unwrap();
        store::set_cold(&c, 1, true).unwrap();
        promote(&c, 1).unwrap();
        assert!(!store::is_cold(&c, 1).unwrap());
        let ids: Vec<i64> = c
            .prepare("SELECT id FROM docs ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, rows.iter().map(|r| r.id).collect::<Vec<_>>());
        assert_eq!(read_blocks(&c, 1).unwrap().len(), 0);
        let ct = store::counters(&c).unwrap();
        assert_eq!((ct.hot_rows, ct.cold_rows), (3, 0));
    }
}
