//! Time zones for day / hour / weekday buckets: `local` (default), `UTC`, fixed offsets
//! (`+08:00`, `-0530`) and IANA names read from the system zoneinfo (TZif + POSIX footer).

use chrono::{Datelike, Offset, TimeZone, Timelike};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub enum Tz {
    #[default]
    Local,
    Fixed(i32),
    Zone(String, Arc<Zone>),
}

/// Civil time of an instant in a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    /// ISO weekday, Monday = 1.
    pub weekday: u32,
}

impl Tz {
    pub fn parse(s: &str) -> Result<Tz, String> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("local") {
            return Ok(Tz::Local);
        }
        if matches!(s.to_ascii_uppercase().as_str(), "UTC" | "Z" | "GMT" | "ETC/UTC") {
            return Ok(Tz::Fixed(0));
        }
        if let Some(off) = fixed_offset(s) {
            return Ok(Tz::Fixed(off));
        }
        if s.contains("..") || s.starts_with('/') || !s.chars().all(|c| c.is_ascii_alphanumeric() || "/_-+".contains(c))
        {
            return Err(format!("unknown time zone {s:?}"));
        }
        for dir in ["/usr/share/zoneinfo", "/usr/lib/zoneinfo", "/usr/share/lib/zoneinfo"] {
            if let Ok(bytes) = std::fs::read(std::path::Path::new(dir).join(s)) {
                return Zone::parse(&bytes)
                    .map(|z| Tz::Zone(s.to_owned(), Arc::new(z)))
                    .ok_or_else(|| format!("bad zoneinfo for {s}"));
            }
        }
        Err(format!("unknown time zone {s:?} (use local, UTC, ±HH:MM or an IANA name)"))
    }

    pub fn name(&self) -> String {
        match self {
            Tz::Local => "local".into(),
            Tz::Fixed(0) => "UTC".into(),
            Tz::Fixed(o) => {
                let a = o.unsigned_abs();
                format!("{}{:02}:{:02}", if *o < 0 { '-' } else { '+' }, a / 3600, a % 3600 / 60)
            }
            Tz::Zone(n, _) => n.clone(),
        }
    }

    /// Seconds east of UTC at `ts_ms`.
    pub fn offset_at(&self, ts_ms: i64) -> i32 {
        match self {
            Tz::Local => {
                chrono::Local.timestamp_millis_opt(ts_ms).single().map_or(0, |d| d.offset().fix().local_minus_utc())
            }
            Tz::Fixed(o) => *o,
            Tz::Zone(_, z) => z.offset_at(ts_ms.div_euclid(1000)),
        }
    }

    pub fn civil(&self, ts_ms: i64) -> Civil {
        let local = ts_ms.div_euclid(1000) + self.offset_at(ts_ms) as i64;
        let d = chrono::DateTime::from_timestamp(local, 0).unwrap_or_default().naive_utc();
        Civil {
            year: d.year(),
            month: d.month(),
            day: d.day(),
            hour: d.hour(),
            weekday: d.weekday().number_from_monday(),
        }
    }
}

/// `+08:00`, `+0800`, `+8`, `-05:30` → seconds east of UTC.
fn fixed_offset(s: &str) -> Option<i32> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, m) = if digits.len() <= 2 {
        (digits.parse::<i32>().ok()?, 0)
    } else {
        let split = digits.len() - 2;
        (digits[..split].parse::<i32>().ok()?, digits[split..].parse::<i32>().ok()?)
    };
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// A parsed TZif file: explicit transitions, then the POSIX rule of its footer.
#[derive(Debug)]
pub struct Zone {
    transitions: Vec<(i64, i32)>,
    initial: i32,
    rule: Option<Rule>,
}

impl Zone {
    fn parse(b: &[u8]) -> Option<Zone> {
        let be32 = |o: usize| b.get(o..o + 4).map(|x| i32::from_be_bytes([x[0], x[1], x[2], x[3]]));
        if b.get(..4)? != b"TZif" {
            return None;
        }
        let version = *b.get(4)?;
        let counts = |at: usize| -> Option<[usize; 6]> {
            let mut c = [0usize; 6];
            for (i, v) in c.iter_mut().enumerate() {
                *v = be32(at + 20 + i * 4)? as usize;
            }
            Some(c)
        };
        let [isut, isstd, leap, time, typ, chars] = counts(0)?;
        let v1_len = time * 5 + typ * 6 + chars + leap * 8 + isstd + isut;
        let (base, tsize, c) = if version >= b'2' {
            let at = 44 + v1_len;
            (at + 44, 8usize, counts(at)?)
        } else {
            (44, 4usize, [isut, isstd, leap, time, typ, chars])
        };
        let [isut, isstd, leap, time, typ, chars] = c;
        let tt = |i: usize| -> Option<i64> {
            let o = base + i * tsize;
            Some(if tsize == 8 { i64::from_be_bytes(b.get(o..o + 8)?.try_into().ok()?) } else { be32(o)? as i64 })
        };
        let idx_at = base + time * tsize;
        let types_at = idx_at + time;
        let offs: Vec<i32> = (0..typ).map(|i| be32(types_at + i * 6)).collect::<Option<_>>()?;
        let isdst = |i: usize| b.get(types_at + i * 6 + 4).copied().unwrap_or(0) != 0;
        let mut transitions = Vec::with_capacity(time);
        for i in 0..time {
            let ty = *b.get(idx_at + i)? as usize;
            transitions.push((tt(i)?, *offs.get(ty)?));
        }
        let initial = (0..typ).find(|&i| !isdst(i)).and_then(|i| offs.get(i).copied()).unwrap_or(0);
        let end = types_at + typ * 6 + chars + leap * (tsize + 4) + isstd + isut;
        let rule = if version >= b'2' {
            b.get(end..).and_then(|f| std::str::from_utf8(f).ok()).and_then(|f| Rule::parse(f.trim_matches('\n')))
        } else {
            None
        };
        Some(Zone { transitions, initial, rule })
    }

    fn offset_at(&self, t: i64) -> i32 {
        if let Some(r) = &self.rule
            && self.transitions.last().is_none_or(|(at, _)| t >= *at)
        {
            return r.offset_at(t);
        }
        match self.transitions.partition_point(|(at, _)| *at <= t) {
            0 => self.initial,
            i => self.transitions[i - 1].1,
        }
    }
}

/// `Mm.w.d/time`: month, week (5 = last), weekday (0 = Sunday), seconds after midnight.
type When = (u32, u32, u32, i32);

/// POSIX TZ string `STD offset [DST [offset] ,Mm.w.d[/time],Mm.w.d[/time]]`.
#[derive(Debug)]
struct Rule {
    std: i32,
    dst: Option<(i32, When, When)>,
}

impl Rule {
    fn parse(s: &str) -> Option<Rule> {
        let mut rest = s;
        let name = |r: &mut &str| -> Option<()> {
            if let Some(q) = r.strip_prefix('<') {
                let end = q.find('>')?;
                *r = &q[end + 1..];
            } else {
                let n = r.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(r.len());
                if n < 3 {
                    return None;
                }
                *r = &r[n..];
            }
            Some(())
        };
        let offset = |r: &mut &str| -> Option<i32> {
            let n = r.find(|c: char| !(c.is_ascii_digit() || "+-:".contains(c))).unwrap_or(r.len());
            let (tok, after) = r.split_at(n);
            *r = after;
            posix_time(tok)
        };
        name(&mut rest)?;
        let std = -offset(&mut rest)?;
        if rest.is_empty() {
            return Some(Rule { std, dst: None });
        }
        name(&mut rest)?;
        let dst_off = if rest.starts_with(',') { std + 3600 } else { -offset(&mut rest)? };
        let mut parts = rest.strip_prefix(',')?.split(',');
        let date = |p: &str| -> Option<When> {
            let (d, time) = p.split_once('/').map_or((p, None), |(d, t)| (d, Some(t)));
            let mut it = d.strip_prefix('M')?.split('.');
            let m = it.next()?.parse().ok()?;
            let w = it.next()?.parse().ok()?;
            let wd = it.next()?.parse().ok()?;
            Some((m, w, wd, time.map_or(Some(7200), posix_time)?))
        };
        let start = date(parts.next()?)?;
        let end = date(parts.next()?)?;
        Some(Rule { std, dst: Some((dst_off, start, end)) })
    }

    fn offset_at(&self, t: i64) -> i32 {
        let Some((dst, start, end)) = self.dst else { return self.std };
        let year = chrono::DateTime::from_timestamp(t + self.std as i64, 0).map_or(1970, |d| d.year());
        let on = transition(year, start) - self.std as i64;
        let off = transition(year, end) - dst as i64;
        let in_dst = if on < off { t >= on && t < off } else { !(t >= off && t < on) };
        if in_dst { dst } else { self.std }
    }
}

/// `[+-]hh[:mm[:ss]]` → seconds.
fn posix_time(s: &str) -> Option<i32> {
    let (sign, body) = match s.as_bytes().first()? {
        b'-' => (-1, &s[1..]),
        b'+' => (1, &s[1..]),
        _ => (1, s),
    };
    let mut secs = 0;
    for (i, part) in body.split(':').enumerate() {
        let v: i32 = part.parse().ok()?;
        secs += v * [3600, 60, 1].get(i)?;
    }
    Some(sign * secs)
}

/// Local wall-clock seconds (as if UTC) of `Mm.w.d/time` in `year`.
fn transition(year: i32, (m, w, wd, time): When) -> i64 {
    let Some(first) = chrono::NaiveDate::from_ymd_opt(year, m, 1) else { return 0 };
    let shift = (wd + 7 - first.weekday().num_days_from_sunday()) % 7;
    let mut day = 1 + shift + (w.saturating_sub(1)) * 7;
    let len =
        chrono::NaiveDate::from_ymd_opt(if m == 12 { year + 1 } else { year }, if m == 12 { 1 } else { m + 1 }, 1)
            .and_then(|n| n.pred_opt())
            .map_or(28, |d| d.day());
    while day > len {
        day -= 7;
    }
    let date = chrono::NaiveDate::from_ymd_opt(year, m, day).unwrap_or(first);
    date.and_hms_opt(0, 0, 0).map_or(0, |d| d.and_utc().timestamp()) + time as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_offsets_and_names() {
        assert_eq!(Tz::parse("+08:00").unwrap().offset_at(0), 8 * 3600);
        assert_eq!(Tz::parse("-0530").unwrap().offset_at(0), -(5 * 3600 + 1800));
        assert_eq!(Tz::parse("utc").unwrap().name(), "UTC");
        assert_eq!(Tz::parse("+8").unwrap().name(), "+08:00");
        assert!(Tz::parse("../etc/passwd").is_err());
        assert!(Tz::parse("Nowhere/Atlantis").is_err());
    }

    #[test]
    fn civil_fields() {
        // 2026-10-04T16:30:00Z: Sunday in UTC, Monday 00:30 in +08:00.
        let t = 1_791_131_400_000;
        let utc = Tz::Fixed(0).civil(t);
        assert_eq!((utc.day, utc.hour, utc.weekday), (4, 16, 7));
        let cn = Tz::Fixed(8 * 3600).civil(t);
        assert_eq!((cn.day, cn.hour, cn.weekday), (5, 0, 1));
    }

    #[test]
    fn posix_rules() {
        let r = Rule::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2026-07-01T12:00Z is summer (EDT, -4h); 2026-01-15T12:00Z is winter (EST, -5h).
        assert_eq!(r.offset_at(1_782_907_200), -4 * 3600);
        assert_eq!(r.offset_at(1_768_478_400), -5 * 3600);
        assert_eq!(Rule::parse("CST-8").unwrap().offset_at(0), 8 * 3600);
        let syd = Rule::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(syd.offset_at(1_768_478_400), 11 * 3600, "southern summer");
        assert_eq!(syd.offset_at(1_782_907_200), 10 * 3600);
    }

    #[test]
    fn system_zoneinfo_when_present() {
        if !std::path::Path::new("/usr/share/zoneinfo/America/New_York").exists() {
            return;
        }
        let ny = Tz::parse("America/New_York").unwrap();
        assert_eq!(ny.offset_at(1_782_907_200_000), -4 * 3600);
        assert_eq!(ny.offset_at(1_768_478_400_000), -5 * 3600);
        assert_eq!(Tz::parse("Asia/Shanghai").unwrap().offset_at(1_768_478_400_000), 8 * 3600);
    }
}
