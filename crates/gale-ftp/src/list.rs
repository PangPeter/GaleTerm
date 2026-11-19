//! Directory listing parsers: `MLSD` (RFC 3659) and the `LIST` formats seen
//! in the wild (Unix `ls -l` and IIS/DOS).

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: Option<u64>,
    /// Seconds since the Unix epoch (server time, taken as UTC).
    pub modified: Option<i64>,
    pub permissions: Option<u32>,
}

impl FtpEntry {
    fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            is_dir: false,
            is_symlink: false,
            size: None,
            modified: None,
            permissions: None,
        }
    }
}

fn keep(entry: &FtpEntry) -> bool {
    !entry.name.is_empty() && entry.name != "." && entry.name != ".."
}

pub fn parse_mlsd(text: &str) -> Vec<FtpEntry> {
    text.lines()
        .filter_map(parse_mlsd_line)
        .filter(keep)
        .collect()
}

pub fn parse_list(text: &str, now: DateTime<Utc>) -> Vec<FtpEntry> {
    text.lines()
        .filter_map(|line| parse_unix_line(line, now).or_else(|| parse_dos_line(line)))
        .filter(keep)
        .collect()
}

/// `type=file;size=12;modify=20240101120000;UNIX.mode=0644; name`
fn parse_mlsd_line(line: &str) -> Option<FtpEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (facts, name) = line.split_once(' ')?;
    let mut entry = FtpEntry::named(name);
    for fact in facts.split(';').filter(|f| !f.is_empty()) {
        let (key, value) = fact.split_once('=')?;
        match key.to_ascii_lowercase().as_str() {
            "type" => {
                let value = value.to_ascii_lowercase();
                if value == "cdir" || value == "pdir" {
                    return None;
                }
                entry.is_dir = value == "dir";
                entry.is_symlink = value.starts_with("os.unix=slink") || value.starts_with("os.unix=symlink");
            }
            "size" | "sizd" => entry.size = value.parse().ok(),
            "modify" => entry.modified = parse_mlsd_time(value),
            "unix.mode" => entry.permissions = u32::from_str_radix(value, 8).ok(),
            _ => {}
        }
    }
    if entry.is_dir {
        entry.size = None;
    }
    Some(entry)
}

fn parse_mlsd_time(value: &str) -> Option<i64> {
    let digits = value.split('.').next()?;
    NaiveDateTime::parse_from_str(digits, "%Y%m%d%H%M%S")
        .ok()
        .map(|t| t.and_utc().timestamp())
}

/// Byte spans of whitespace-separated tokens.
fn token_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    for (ix, ch) in line.char_indices() {
        match (ch.is_whitespace(), start) {
            (true, Some(s)) => {
                spans.push((s, ix));
                start = None;
            }
            (false, None) => start = Some(ix),
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, line.len()));
    }
    spans
}

fn month(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = token.to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == lower).map(|i| i as u32 + 1)
}

fn mode_bits(perms: &str) -> Option<u32> {
    let bytes = perms.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let mut mode = 0u32;
    for (ix, &b) in bytes[1..10].iter().enumerate() {
        let bit = 1 << (8 - ix);
        let set = match ix % 3 {
            0 => b == b'r',
            1 => b == b'w',
            _ => matches!(b, b'x' | b's' | b't'),
        };
        if set {
            mode |= bit;
        }
    }
    Some(mode)
}

/// `drwxr-xr-x  2 user group 4096 Jan  1 12:00 name` (group may be absent).
fn parse_unix_line(line: &str, now: DateTime<Utc>) -> Option<FtpEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let spans = token_spans(line);
    let perms = &line[spans.first()?.0..spans.first()?.1];
    let kind = perms.chars().next()?;
    if perms.len() < 10 || !"dl-bcps".contains(kind) {
        return None;
    }
    let tok = |i: usize| spans.get(i).map(|&(s, e)| &line[s..e]);

    // The month is the first token after the size; locating it copes with
    // listings that omit the group or link count.
    let m = (3..spans.len().saturating_sub(3))
        .find(|&i| {
            month(tok(i).unwrap_or_default()).is_some()
                && tok(i - 1).is_some_and(|t| t.parse::<u64>().is_ok())
        })?;
    let size: u64 = tok(m - 1)?.parse().ok()?;
    let mon = month(tok(m)?)?;
    let day: u32 = tok(m + 1)?.parse().ok()?;
    let year_or_time = tok(m + 2)?;
    let (year, hour, minute) = match year_or_time.split_once(':') {
        Some((h, mi)) => (now.year(), h.parse().ok()?, mi.parse().ok()?),
        None => (year_or_time.parse().ok()?, 0, 0),
    };
    let mut date = NaiveDate::from_ymd_opt(year, mon, day)?.and_hms_opt(hour, minute, 0)?;
    // "Mon DD HH:MM" means within the last ~6 months; a date ahead of now is last year's.
    if year_or_time.contains(':') && date.and_utc() > now + chrono::Duration::days(1) {
        date = NaiveDate::from_ymd_opt(year - 1, mon, day)?.and_hms_opt(hour, minute, 0)?;
    }

    let name_start = spans.get(m + 3)?.0;
    let mut name = &line[name_start..];
    let is_symlink = kind == 'l';
    if is_symlink {
        if let Some((link, _target)) = name.split_once(" -> ") {
            name = link;
        }
    }
    Some(FtpEntry {
        name: name.to_string(),
        is_dir: kind == 'd',
        is_symlink,
        size: (kind != 'd').then_some(size),
        modified: Some(date.and_utc().timestamp()),
        permissions: mode_bits(perms),
    })
}

/// `01-02-23  10:00AM       <DIR>          name` / `… 1234 name`
fn parse_dos_line(line: &str) -> Option<FtpEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let spans = token_spans(line);
    if spans.len() < 4 {
        return None;
    }
    let tok = |i: usize| &line[spans[i].0..spans[i].1];
    let mut date_parts = tok(0).split('-');
    let mon: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    let mut year: i32 = date_parts.next()?.parse().ok()?;
    if year < 100 {
        year += if year < 70 { 2000 } else { 1900 };
    }
    let time = tok(1).to_ascii_uppercase();
    let (clock, pm) = match (time.strip_suffix("PM"), time.strip_suffix("AM")) {
        (Some(t), _) => (t, true),
        (_, Some(t)) => (t, false),
        _ => (time.as_str(), false),
    };
    let (h, mi) = clock.split_once(':')?;
    let mut hour: u32 = h.parse().ok()?;
    let minute: u32 = mi.parse().ok()?;
    if pm && hour < 12 {
        hour += 12;
    } else if !pm && hour == 12 && time.ends_with("AM") {
        hour = 0;
    }
    let modified = NaiveDate::from_ymd_opt(year, mon, day)?
        .and_hms_opt(hour, minute, 0)?
        .and_utc()
        .timestamp();
    let is_dir = tok(2).eq_ignore_ascii_case("<DIR>");
    let size = if is_dir { None } else { Some(tok(2).parse().ok()?) };
    Some(FtpEntry {
        name: line[spans[3].0..].to_string(),
        is_dir,
        is_symlink: false,
        size,
        modified: Some(modified),
        permissions: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 10, 12, 0, 0).unwrap()
    }

    #[test]
    fn mlsd_lines() {
        let text = "type=cdir;modify=20240101000000; .\r\n\
                    type=dir;modify=20240102030405;UNIX.mode=0755; my docs\r\n\
                    type=file;size=1234;modify=20240102030405.123;UNIX.mode=0644; a b.txt\r\n\
                    type=OS.unix=slink:/etc;modify=20240102030405; etc-link\r\n";
        let entries = parse_mlsd(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "my docs");
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].permissions, Some(0o755));
        assert_eq!(entries[1].name, "a b.txt");
        assert_eq!(entries[1].size, Some(1234));
        assert_eq!(entries[1].modified, Some(1_704_164_645));
        assert!(entries[2].is_symlink);
    }

    #[test]
    fn unix_list_lines() {
        let text = "total 12\r\n\
                    drwxr-xr-x    2 ftp      ftp          4096 Jan 05 12:30 dir one\r\n\
                    -rw-r--r--    1 ftp      ftp      10485760 Dec 31  2023 big.iso\r\n\
                    lrwxrwxrwx    1 0        0               7 Oct 02 09:00 lib -> usr/lib\r\n\
                    -rw-r--r--   1 owner   42 Feb  1 08:00 nogroup.txt\r\n\
                    drwxr-xr-x    2 ftp      ftp          4096 Jan 05 12:30 .\r\n";
        let entries = parse_list(text, now());
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["dir one", "big.iso", "lib", "nogroup.txt"]);
        assert!(entries[0].is_dir && entries[0].size.is_none());
        assert_eq!(entries[0].permissions, Some(0o755));
        assert_eq!(
            entries[0].modified,
            Some(Utc.with_ymd_and_hms(2026, 1, 5, 12, 30, 0).unwrap().timestamp())
        );
        assert_eq!(entries[1].size, Some(10_485_760));
        // "Oct 02" with a time is after "now" (March), so it was last year.
        assert_eq!(
            entries[2].modified,
            Some(Utc.with_ymd_and_hms(2025, 10, 2, 9, 0, 0).unwrap().timestamp())
        );
        assert!(entries[2].is_symlink);
        assert_eq!(entries[3].size, Some(42));
    }

    #[test]
    fn dos_list_lines() {
        let text = "01-02-23  10:00AM       <DIR>          Program Files\r\n\
                    12-31-2023  01:05PM            123456 report.pdf\r\n";
        let entries = parse_list(text, now());
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "Program Files");
        assert_eq!(entries[1].size, Some(123_456));
        assert_eq!(
            entries[1].modified,
            Some(Utc.with_ymd_and_hms(2023, 12, 31, 13, 5, 0).unwrap().timestamp())
        );
    }
}
