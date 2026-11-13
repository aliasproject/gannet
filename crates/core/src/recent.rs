//! Recently used files: the list GTK apps, browsers and office apps keep in
//! `$XDG_DATA_HOME/recently-used.xbel`. Gannet only reads it.

use std::path::PathBuf;

use crate::uri::from_file_uri;

/// The list's file name, in `$XDG_DATA_HOME`.
pub const FILE: &str = "recently-used.xbel";

/// The most files the Recent view lists: the newest ones. Each is checked on disk, so a
/// long list would be slow to show.
pub const LIMIT: usize = 1000;

/// The most entries checked on disk to find them, so a list of mostly deleted files is still
/// quick to show.
pub const CHECK_LIMIT: usize = 4 * LIMIT;

/// The local files in the list, newest first, each with when it was last used, in seconds
/// since the epoch. A file listed twice keeps its newer time. Web addresses and anything
/// that isn't a local file are left out.
pub fn parse(xbel: &[u8]) -> Vec<(PathBuf, i64)> {
    let text = String::from_utf8_lossy(xbel);
    let mut out: Vec<(PathBuf, i64)> = Vec::new();
    let mut seen: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    let mut rest = &*text;
    while let Some(at) = rest.find('<') {
        rest = &rest[at..];
        // Comments and CDATA can hold anything, including what looks like a bookmark.
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.find("-->").map_or("", |e| &r[e + 3..]);
            continue;
        }
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            rest = r.find("]]>").map_or("", |e| &r[e + 3..]);
            continue;
        }
        let Some(end) = tag_end(rest) else { break };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        let Some(attrs) = tag.strip_prefix("bookmark").filter(|a| a.starts_with(|c: char| c.is_ascii_whitespace())) else { continue };
        let attrs = attributes(attrs);
        let get = |name: &str| attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str());
        let Some(path) = get("href").and_then(from_file_uri) else { continue };
        let Some(used) = ["visited", "modified", "added"].iter().filter_map(|n| get(n).and_then(parse_utc)).max() else { continue };
        match seen.get(&path) {
            Some(&i) => out[i].1 = out[i].1.max(used),
            None => {
                seen.insert(path.clone(), out.len());
                out.push((path, used));
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.1));
    out
}

/// Where a tag that starts `text` ends, skipping a `>` inside a quoted value.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// A tag's `name="value"` pairs, with the values unescaped.
fn attributes(mut text: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    loop {
        text = text.trim_start();
        let Some(eq) = text.find('=') else { return out };
        let name = text[..eq].trim();
        let after = text[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { return out };
        let Some(close) = after[1..].find(q) else { return out };
        out.push((name, unescape(&after[1..1 + close])));
        text = &after[close + 2..];
    }
}

/// Undoes XML's escapes: the five named ones and numbered characters.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(semi) = rest.find(';') else { break };
        let entity = &rest[1..semi];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")).map_or_else(|| entity.strip_prefix('#').and_then(|n| n.parse().ok()), |h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Seconds since the epoch for a UTC time like `2026-11-12T20:43:55.886123Z`, as GLib
/// writes. A time with an offset, like `+02:00`, is moved to UTC.
pub fn parse_utc(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> { text.get(r.clone()).filter(|s| s.bytes().all(|c| c.is_ascii_digit())).and_then(|s| s.parse().ok()) };
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d, h, mi, s) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let mut zone = &text[19..];
    if let Some(r) = zone.strip_prefix('.') {
        zone = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match zone {
        "Z" | "z" | "" => 0,
        // ASCII first, so the cuts below fall between characters.
        z if z.len() == 6 && z.is_ascii() && (z.starts_with('+') || z.starts_with('-')) && &z[3..4] == ":" => {
            let (oh, om) = (z[1..3].parse::<i64>().ok()?, z[4..6].parse::<i64>().ok()?);
            let o = oh * 3600 + om * 60;
            if z.starts_with('-') { -o } else { o }
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s - offset)
}

/// Days from 1970-01-01 to a date in the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_read_as_utc() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-11-12T20:43:55.886123Z"), Some(1794516235));
        assert_eq!(parse_utc("2000-02-29T12:00:00+02:00"), Some(951818400));
        assert_eq!(parse_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_utc("yesterday"), None);
        assert_eq!(parse_utc("2026-01-01T00:00:00+0é:0"), None, "a garbled zone is no time, not a panic");
    }

    #[test]
    fn local_files_come_newest_first_with_their_latest_use() {
        let xbel = br#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0" xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks">
  <bookmark href="file:///home/a/old.txt" added="2026-01-01T00:00:00Z" modified="2026-01-01T00:00:00Z" visited="2026-01-01T00:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="text/plain"/></metadata></info>
  </bookmark>
  <!-- <bookmark href="file:///home/a/commented.txt" visited="2027-01-01T00:00:00Z"/> -->
  <bookmark href="https://example.com/page" visited="2026-06-01T00:00:00Z"/>
  <bookmark href="file:///home/a/my%20photo.png" added="2026-03-01T00:00:00Z" modified="2026-05-01T10:00:00.5Z" visited="2026-04-01T00:00:00Z"/>
  <bookmark visited='2026-02-01T00:00:00Z' href='file:///home/a/Tom&amp;Jerry.odt'/>
  <bookmark href="file:///home/a/old.txt" visited="2026-07-01T00:00:00Z"/>
  <bookmark href="file:///home/a/undated.txt"/>
</xbel>"#;
        let got = parse(xbel);
        let names: Vec<_> = got.iter().map(|(p, _)| p.to_str().unwrap()).collect();
        assert_eq!(names, ["/home/a/old.txt", "/home/a/my photo.png", "/home/a/Tom&Jerry.odt"]);
        assert_eq!(got[0].1, parse_utc("2026-07-01T00:00:00Z").unwrap(), "a file listed twice keeps its newer time");
        assert_eq!(got[1].1, parse_utc("2026-05-01T10:00:00Z").unwrap(), "the latest of its times counts");
    }

    #[test]
    fn a_broken_list_gives_what_it_can() {
        assert!(parse(b"").is_empty());
        assert!(parse(b"<xbel><bookmark href=\"file:///a\" visited=\"2026-01-01T00:00:00Z\"").is_empty());
        assert_eq!(parse(b"<bookmark href=\"file:///a>b\" visited=\"2026-01-01T00:00:00Z\"/>").len(), 1, "a > in a value doesn't end the tag");
        assert!(parse(b"<bookmarks href=\"file:///a\" visited=\"2026-01-01T00:00:00Z\"/>").is_empty());
    }
}
