//! `{{#time: format | date}}`: a date parser and a PHP-`date()`-style formatter (UTC only).

use crate::util::civil;

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => 28 + is_leap(y) as i64,
    }
}

fn month_by_name(w: &str) -> Option<i64> {
    let w = w.to_lowercase();
    if w.len() < 3 {
        return None;
    }
    MONTHS.iter().position(|m| {
        let m = m.to_lowercase();
        m == w || (w.len() == 3 && m.starts_with(&w)) || (w == "sept" && m == "september")
    })
    .map(|i| i as i64 + 1)
}

#[derive(Clone, Copy)]
struct Fields {
    y: i64,
    mo: i64,
    d: i64,
    h: i64,
    mi: i64,
    s: i64,
}

impl Fields {
    fn ts(&self) -> Option<i64> {
        if !(1..=12).contains(&self.mo) || self.d < 1 || self.d > days_in_month(self.y, self.mo) {
            return None;
        }
        if !(0..=9999).contains(&self.y) || !(0..24).contains(&self.h) || !(0..60).contains(&self.mi) || !(0..=60).contains(&self.s) {
            return None;
        }
        Some(days_from_civil(self.y, self.mo, self.d) * 86_400 + self.h * 3600 + self.mi * 60 + self.s)
    }
}

/// `HH:MM[:SS]`, returns the fields and an optional am/pm suffix handled by the caller.
fn parse_clock(t: &str) -> Option<(i64, i64, i64)> {
    let mut it = t.split(':');
    let h: i64 = it.next()?.parse().ok()?;
    let mi: i64 = it.next()?.parse().ok()?;
    let s: i64 = match it.next() {
        Some(x) => x.split('.').next()?.parse().ok()?,
        None => 0,
    };
    it.next().is_none().then_some((h, mi, s))
}

/// Offset in seconds for `Z`, `+02:00`, `-0530`, `+02`.
fn parse_zone(z: &str) -> Option<i64> {
    if z.eq_ignore_ascii_case("z") || z.eq_ignore_ascii_case("utc") || z.eq_ignore_ascii_case("gmt") {
        return Some(0);
    }
    let sign = match z.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = z[1..].chars().filter(|c| *c != ':').collect();
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, m): (i64, i64) = match digits.len() {
        2 => (digits.parse().ok()?, 0),
        4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
        _ => return None,
    };
    Some(sign * (h * 3600 + m * 60))
}

fn unit_seconds(u: &str) -> Option<(i64, i64)> {
    // (seconds, months)
    let u = u.trim_end_matches('s');
    Some(match u {
        "sec" | "second" => (1, 0),
        "min" | "minute" => (60, 0),
        "hour" => (3600, 0),
        "day" => (86_400, 0),
        "week" => (604_800, 0),
        "fortnight" => (1_209_600, 0),
        "month" => (0, 1),
        "year" => (0, 12),
        _ => return None,
    })
}

/// A time zone's state at one instant.
#[derive(Clone, Debug, Default)]
pub struct Local {
    /// Seconds east of UTC.
    pub offset: i64,
    pub abbr: String,
    pub name: String,
    pub dst: bool,
}

impl Local {
    pub fn utc() -> Local {
        Local { offset: 0, abbr: "UTC".into(), name: "UTC".into(), dst: false }
    }
}

pub fn valid_zone(tz: &str) -> bool {
    tz.parse::<chrono_tz::Tz>().is_ok()
}

/// Offset and abbreviation of IANA zone `tz` at `ts` (UTC for unknown zones).
pub fn local(tz: &str, ts: i64) -> Local {
    use chrono::{Offset, TimeZone};
    use chrono_tz::OffsetComponents;
    let Ok(zone) = tz.parse::<chrono_tz::Tz>() else { return Local::utc() };
    let Some(dt) = zone.timestamp_opt(ts, 0).single() else { return Local::utc() };
    let off = *dt.offset();
    Local { offset: off.fix().local_minus_utc() as i64, abbr: off.to_string(), name: zone.name().to_string(), dst: !off.dst_offset().is_zero() }
}

/// A wall-clock time in zone `tz` (given as if it were UTC) converted to a real timestamp.
pub fn from_wall(tz: &str, wall: i64) -> i64 {
    let guess = wall - local(tz, wall).offset;
    wall - local(tz, guess).offset
}

/// Parse a date/time string into a Unix timestamp (UTC). `None` = not understood.
pub fn parse(input: &str, now: i64) -> Option<i64> {
    parse_ex(input, now).map(|(ts, _)| ts)
}

/// Like `parse`; the flag says the input named a calendar date or clock time without a zone
/// (so a caller working in local time should read it as wall-clock time).
pub fn parse_ex(input: &str, now: i64) -> Option<(i64, bool)> {
    let s = input.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("now") {
        return Some((now, false));
    }
    if let Some(u) = s.strip_prefix('@') {
        return u.trim().parse().ok().map(|t| (t, false));
    }
    if s.len() == 14 && s.chars().all(|c| c.is_ascii_digit()) {
        let n = |a: usize, b: usize| s[a..b].parse::<i64>().ok();
        return Fields { y: n(0, 4)?, mo: n(4, 6)?, d: n(6, 8)?, h: n(8, 10)?, mi: n(10, 12)?, s: n(12, 14)? }.ts().map(|t| (t, false));
    }
    let (ny, nmo, nd, ..) = civil(now);
    let mut f = Fields { y: ny, mo: nmo as i64, d: nd as i64, h: 0, mi: 0, s: 0 };
    let (mut have_date, mut have_time) = (false, false);
    let (mut add_secs, mut add_months, mut zone, mut had_zone) = (0i64, 0i64, 0i64, false);
    // "2024-03-05T12:30:00Z" -> split the T; commas are noise.
    let norm = s.replace(',', " ");
    let mut toks: Vec<String> = Vec::new();
    for raw in norm.split_whitespace() {
        let b = raw.as_bytes();
        if b.len() > 11 && b[4] == b'-' && (b[10] == b'T' || b[10] == b't') {
            toks.push(raw[..10].to_string());
            let rest = &raw[11..];
            match rest.find(['Z', 'z', '+', '-']) {
                Some(i) => {
                    toks.push(rest[..i].to_string());
                    toks.push(rest[i..].to_string());
                }
                None => toks.push(rest.to_string()),
            }
        } else {
            toks.push(raw.to_string());
        }
    }
    let mut i = 0;
    let mut pending_num: Option<i64> = None;
    let mut have_year = false;
    while i < toks.len() {
        let t = toks[i].as_str();
        let lower = t.to_lowercase();
        i += 1;
        // relative: "+3 days", "-1 week", "3 days ago", "next month", "last year"
        let signed = (t.starts_with('+') || t.starts_with('-')) && t.len() > 1 && t[1..].chars().all(|c| c.is_ascii_digit()) && have_time_or_rel(&toks, i);
        if signed || matches!(lower.as_str(), "next" | "last" | "previous") {
            let n: i64 = match lower.as_str() {
                "next" => 1,
                "last" | "previous" => -1,
                _ => t.parse().ok()?,
            };
            let unit = toks.get(i)?.to_lowercase();
            let (secs, months) = unit_seconds(&unit)?;
            i += 1;
            add_secs += n * secs;
            add_months += n * months;
            continue;
        }
        if lower == "ago" {
            add_secs = -add_secs;
            add_months = -add_months;
            continue;
        }
        match lower.as_str() {
            "today" | "midnight" => {
                have_time = true;
                continue;
            }
            "yesterday" => {
                add_secs -= 86_400;
                have_time = true;
                continue;
            }
            "tomorrow" => {
                add_secs += 86_400;
                have_time = true;
                continue;
            }
            "noon" => {
                f.h = 12;
                have_time = true;
                continue;
            }
            "am" | "pm" => {
                if lower == "pm" && f.h < 12 {
                    f.h += 12;
                } else if lower == "am" && f.h == 12 {
                    f.h = 0;
                }
                continue;
            }
            "at" | "on" | "of" | "the" => continue,
            _ => {}
        }
        if let Some(z) = parse_zone(t).filter(|_| have_time && !t.chars().all(|c| c.is_ascii_digit())) {
            zone = z;
            had_zone = true;
            continue;
        }
        if t.contains(':') {
            let (h, mi, sec) = parse_clock(t)?;
            f.h = h;
            f.mi = mi;
            f.s = sec;
            have_time = true;
            continue;
        }
        // YYYY-MM-DD, YYYY/MM/DD, YYYY-MM
        let parts: Vec<&str> = t.split(['-', '/']).collect();
        if parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) && !have_date {
            if parts[0].len() == 4 {
                f.y = parts[0].parse().ok()?;
                f.mo = parts[1].parse().ok()?;
                f.d = parts.get(2).map_or(Some(1), |d| d.parse().ok())?;
            } else if parts.len() == 3 && parts[2].len() == 4 {
                // DD-MM-YYYY (European order, as PHP does for dashes)
                f.d = parts[0].parse().ok()?;
                f.mo = parts[1].parse().ok()?;
                f.y = parts[2].parse().ok()?;
            } else {
                return None;
            }
            have_date = true;
            have_year = true;
            continue;
        }
        if let Some(m) = month_by_name(&lower) {
            f.mo = m;
            if !have_date {
                f.d = 1;
            }
            have_date = true;
            if let Some(n) = pending_num.take() {
                f.d = n;
            }
            continue;
        }
        // weekday names are accepted and ignored ("Monday, 5 March 2024")
        if DAYS.iter().any(|d| d.to_lowercase() == lower || (lower.len() == 3 && d.to_lowercase().starts_with(&lower))) {
            continue;
        }
        let digits = lower.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            let n: i64 = digits.parse().ok()?;
            let suffix = &lower[digits.len()..];
            if !matches!(suffix, "" | "st" | "nd" | "rd" | "th") {
                // "3 days" without a sign
                if let Some((secs, months)) = unit_seconds(suffix) {
                    add_secs += n * secs;
                    add_months += n * months;
                    continue;
                }
                return None;
            }
            // "3 days" / "3 days ago"
            if let Some((secs, months)) = toks.get(i).and_then(|u| unit_seconds(&u.to_lowercase())) {
                i += 1;
                add_secs += n * secs;
                add_months += n * months;
                continue;
            }
            if digits.len() == 4 && suffix.is_empty() && !have_year {
                f.y = n;
                have_year = true;
                if !have_date {
                    f.mo = 1;
                    f.d = 1;
                }
                have_date = true;
            } else if have_date && pending_num.is_none() && (1..=31).contains(&n) && toks.get(i).is_none_or(|x| month_by_name(x).is_none()) {
                // "March 5"
                f.d = n;
            } else if (1..=31).contains(&n) {
                // "5 March"
                pending_num = Some(n);
            } else {
                return None;
            }
            continue;
        }
        return None;
    }
    if pending_num.is_some() {
        return None;
    }
    if !have_date && !have_time && add_secs == 0 && add_months == 0 {
        return None;
    }
    if !have_date && !have_time {
        // purely relative: keep the current time of day
        let (.., h, mi, sec) = civil(now);
        f.h = h as i64;
        f.mi = mi as i64;
        f.s = sec as i64;
    }
    if add_months != 0 {
        let total = f.y * 12 + (f.mo - 1) + add_months;
        f.y = total.div_euclid(12);
        f.mo = total.rem_euclid(12) + 1;
        f.d = f.d.min(days_in_month(f.y, f.mo));
    }
    Some((f.ts()? + add_secs - zone, (have_date || have_time) && !had_zone))
}

fn have_time_or_rel(toks: &[String], next: usize) -> bool {
    toks.get(next).is_some_and(|u| unit_seconds(&u.to_lowercase()).is_some())
}

fn iso_week(y: i64, yday: i64, wday_mon1: i64) -> (i64, i64) {
    // ISO 8601: week 1 contains the first Thursday.
    let week = (yday - wday_mon1 + 10) / 7;
    if week < 1 {
        let py = y - 1;
        let pdays = 365 + is_leap(py) as i64;
        ((yday + pdays - wday_mon1 + 10) / 7, py)
    } else if week == 53 && (yday - (365 + is_leap(y) as i64) - wday_mon1 + 10) / 7 >= 1 {
        (1, y + 1)
    } else {
        (week, y)
    }
}

fn roman(mut n: i64) -> String {
    if !(1..4000).contains(&n) {
        return n.to_string();
    }
    let mut out = String::new();
    for (v, s) in [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")] {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

/// Format a timestamp with PHP `date()` codes (plus MediaWiki's `xg`, `xr`, `"literal"`), in UTC.
pub fn format(fmt: &str, ts: i64) -> String {
    format_in(fmt, ts, &Local::utc())
}

/// Format `ts` as seen in the zone state `z`.
pub fn format_in(fmt: &str, real_ts: i64, z: &Local) -> String {
    let ts = real_ts + z.offset;
    let (osign, oabs) = (if z.offset < 0 { '-' } else { '+' }, z.offset.abs());
    let (oh, om) = (oabs / 3600, oabs % 3600 / 60);
    let (y, mo, d, h, mi, s) = civil(ts);
    let (mo_i, d_i) = (mo as i64, d as i64);
    let days = ts.div_euclid(86_400);
    let wday = (days + 4).rem_euclid(7); // 0 = Sunday
    let wday_mon1 = if wday == 0 { 7 } else { wday };
    let yday = days - days_from_civil(y, 1, 1) + 1; // 1-based
    let (week, iso_year) = iso_week(y, yday, wday_mon1);
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    let mut out = String::new();
    let mut roman_next = false;
    let num = |out: &mut String, v: i64, width: usize, roman_next: &mut bool| {
        if std::mem::take(roman_next) {
            out.push_str(&roman(v));
        } else {
            out.push_str(&format!("{v:0width$}"));
        }
    };
    let cs: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        i += 1;
        match c {
            'd' => num(&mut out, d_i, 2, &mut roman_next),
            'j' => num(&mut out, d_i, 1, &mut roman_next),
            'D' => out.push_str(&DAYS[wday as usize][..3]),
            'l' => out.push_str(DAYS[wday as usize]),
            'N' => num(&mut out, wday_mon1, 1, &mut roman_next),
            'w' => num(&mut out, wday, 1, &mut roman_next),
            'z' => num(&mut out, yday - 1, 1, &mut roman_next),
            'W' => num(&mut out, week, 2, &mut roman_next),
            'F' => out.push_str(MONTHS[(mo - 1) as usize]),
            'M' => out.push_str(&MONTHS[(mo - 1) as usize][..3]),
            'm' => num(&mut out, mo_i, 2, &mut roman_next),
            'n' => num(&mut out, mo_i, 1, &mut roman_next),
            't' => num(&mut out, days_in_month(y, mo_i), 1, &mut roman_next),
            'L' => out.push(if is_leap(y) { '1' } else { '0' }),
            'o' => num(&mut out, iso_year, 4, &mut roman_next),
            'Y' => num(&mut out, y, 4, &mut roman_next),
            'y' => num(&mut out, y.rem_euclid(100), 2, &mut roman_next),
            'a' => out.push_str(if h < 12 { "am" } else { "pm" }),
            'A' => out.push_str(if h < 12 { "AM" } else { "PM" }),
            'g' => num(&mut out, h12 as i64, 1, &mut roman_next),
            'G' => num(&mut out, h as i64, 1, &mut roman_next),
            'h' => num(&mut out, h12 as i64, 2, &mut roman_next),
            'H' => num(&mut out, h as i64, 2, &mut roman_next),
            'i' => num(&mut out, mi as i64, 2, &mut roman_next),
            's' => num(&mut out, s as i64, 2, &mut roman_next),
            'U' => out.push_str(&real_ts.to_string()),
            'e' => out.push_str(&z.name),
            'T' => out.push_str(&z.abbr),
            'I' => out.push(if z.dst { '1' } else { '0' }),
            'Z' => out.push_str(&z.offset.to_string()),
            'O' => out.push_str(&format!("{osign}{oh:02}{om:02}")),
            'P' => out.push_str(&format!("{osign}{oh:02}:{om:02}")),
            'c' => out.push_str(&format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{osign}{oh:02}:{om:02}")),
            'r' => out.push_str(&format!("{}, {d:02} {} {y:04} {h:02}:{mi:02}:{s:02} {osign}{oh:02}{om:02}", &DAYS[wday as usize][..3], &MONTHS[(mo - 1) as usize][..3])),
            'x' => match cs.get(i) {
                Some('g') => {
                    i += 1;
                    out.push_str(MONTHS[(mo - 1) as usize]);
                }
                Some('r') => {
                    i += 1;
                    roman_next = true;
                }
                Some('n') | Some('N') => i += 1,
                Some('x') => {
                    i += 1;
                    out.push('x');
                }
                _ => out.push('x'),
            },
            '\\' => {
                if let Some(n) = cs.get(i) {
                    out.push(*n);
                    i += 1;
                }
            }
            '"' => match cs[i..].iter().position(|c| *c == '"') {
                Some(e) => {
                    out.extend(&cs[i..i + e]);
                    i += e + 1;
                }
                None => out.push('"'),
            },
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_553_200; // 2026-11-13T07:00:00Z, a Friday

    fn f(fmt: &str, date: &str) -> String {
        format(fmt, parse(date, NOW).unwrap_or_else(|| panic!("unparsed: {date}")))
    }

    #[test]
    fn formats() {
        assert_eq!(f("Y-m-d H:i:s", "2024-03-05 14:07:09"), "2024-03-05 14:07:09");
        assert_eq!(f("j F Y", "2024-03-05"), "5 March 2024");
        assert_eq!(f("D, d M y", "2024-03-05"), "Tue, 05 Mar 24");
        assert_eq!(f("l N w z t L", "2024-02-29"), "Thursday 4 4 59 29 1");
        assert_eq!(f("g:i a", "2024-03-05 00:05"), "12:05 am");
        assert_eq!(f("h A", "2024-03-05 13:00"), "01 PM");
        assert_eq!(f("U", "@86400"), "86400");
        assert_eq!(f("c", "2024-03-05T10:00:00Z"), "2024-03-05T10:00:00+00:00");
        assert_eq!(f("\"Year\" Y \\Y xrY", "2024-01-01"), "Year 2024 Y MMXXIV");
        assert_eq!(f("Y", ""), "2026");
    }

    #[test]
    fn iso_weeks() {
        assert_eq!(f("o-\\WW", "2021-01-03"), "2020-W53");
        assert_eq!(f("o-\\WW", "2024-12-30"), "2025-W01");
        assert_eq!(f("o-\\WW", "2026-11-13"), "2026-W46");
    }

    #[test]
    fn parses_many_shapes() {
        assert_eq!(f("Y-m-d", "5 March 2024"), "2024-03-05");
        assert_eq!(f("Y-m-d", "March 5, 2024"), "2024-03-05");
        assert_eq!(f("Y-m-d", "Tuesday, 5th Mar 2024"), "2024-03-05");
        assert_eq!(f("Y-m-d", "March 2024"), "2024-03-01");
        assert_eq!(f("Y-m-d", "2024"), "2024-01-01");
        assert_eq!(f("Y-m-d", "20240305123000"), "2024-03-05");
        assert_eq!(f("Y-m-d H:i", "2024-03-05T12:30:00+02:00"), "2024-03-05 10:30");
        assert_eq!(f("Y-m-d H:i", "5 March 2024 3:15 pm"), "2024-03-05 15:15");
        assert_eq!(f("Y-m-d", "05-03-2024"), "2024-03-05");
    }

    #[test]
    fn relative_dates() {
        assert_eq!(f("Y-m-d", "now"), "2026-11-13");
        assert_eq!(f("Y-m-d", "tomorrow"), "2026-11-14");
        assert_eq!(f("Y-m-d", "yesterday"), "2026-11-12");
        assert_eq!(f("Y-m-d H:i", "+1 day"), "2026-11-14 07:00");
        assert_eq!(f("Y-m-d", "2024-01-31 +1 month"), "2024-02-29");
        assert_eq!(f("Y-m-d", "2024-03-05 -1 week"), "2024-02-27");
        assert_eq!(f("Y-m-d", "3 days ago"), "2026-11-10");
        assert_eq!(f("Y-m-d", "next year"), "2027-11-13");
    }

    #[test]
    fn time_zones() {
        // 2024-07-01 12:00 UTC is 14:00 in Berlin (CEST), 2024-01-01 12:00 UTC is 13:00 (CET)
        let summer = parse("2024-07-01 12:00", NOW).unwrap();
        let z = local("Europe/Berlin", summer);
        assert_eq!((z.offset, z.abbr.as_str(), z.dst), (7200, "CEST", true));
        assert_eq!(format_in("Y-m-d H:i T P e I", summer, &z), "2024-07-01 14:00 CEST +02:00 Europe/Berlin 1");
        let winter = parse("2024-01-01 12:00", NOW).unwrap();
        assert_eq!(format_in("H:i T O", winter, &local("Europe/Berlin", winter)), "13:00 CET +0100");
        assert_eq!(format_in("H:i c", winter, &local("America/Chicago", winter)), "06:00 2024-01-01T06:00:00-06:00");
        assert_eq!(format_in("U", winter, &local("Asia/Kolkata", winter)), winter.to_string());
        // unknown zones behave as UTC
        assert_eq!(local("Mars/Olympus", winter).offset, 0);
        assert!(valid_zone("Asia/Tokyo") && !valid_zone("Nope"));
        // wall-clock conversion
        assert_eq!(from_wall("Europe/Berlin", summer), summer - 7200);
        assert!(parse_ex("2024-07-01 12:00", NOW).unwrap().1);
        assert!(!parse_ex("2024-07-01T12:00:00Z", NOW).unwrap().1);
        assert!(!parse_ex("+1 day", NOW).unwrap().1);
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["not a date", "2024-13-01", "2024-02-30", "32 March 2024", "25:00"] {
            assert!(parse(bad, NOW).is_none(), "{bad}");
        }
    }
}
