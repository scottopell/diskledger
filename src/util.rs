use std::time::{SystemTime, UNIX_EPOCH};

pub type Res<T> = Result<T, String>;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// "12.3G", "456M", "7K". Binary units.
pub fn human(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= K * K * K {
        format!("{:.1}G", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.0}M", b / (K * K))
    } else if b >= K {
        format!("{:.0}K", b / K)
    } else {
        format!("{}B", bytes)
    }
}

pub fn human_delta(d: i64) -> String {
    if d == 0 {
        "0".to_string()
    } else {
        format!("{}{}", if d > 0 { "+" } else { "-" }, human(d.unsigned_abs()))
    }
}

/// "500M", "2G", "1024" (bytes).
pub fn parse_size(s: &str) -> Res<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1u64 << 20),
        Some('G') => (&s[..s.len() - 1], 1u64 << 30),
        Some('T') => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1),
    };
    let v: f64 = num.parse().map_err(|_| format!("bad size '{s}' (use e.g. 500M, 2G)"))?;
    Ok((v * mult as f64) as u64)
}

/// "90m", "2h", "3d" -> seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        _ => return None,
    };
    num.parse::<u64>().ok().map(|v| v * mult)
}

pub fn fmt_age(secs: u64) -> String {
    if secs >= 2 * 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 2 * 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 120 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// UTC timestamp, "2026-11-15T17:04:05Z".
pub fn fmt_ts(ts: u64) -> String {
    let days = (ts / 86400) as i64;
    let rem = ts % 86400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Escape a path for one TSV field.
pub fn tsv_esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

pub fn tsv_unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => o.push('\t'),
                Some('n') => o.push('\n'),
                Some(other) => o.push(other),
                None => {}
            }
        } else {
            o.push(c);
        }
    }
    o
}

/// Parent of "/a/b/c" is "/a/b".
pub fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').filter(|&i| i > 0).map(|i| &path[..i])
}

pub fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(parse_size("500m").unwrap(), 500 << 20);
        assert_eq!(parse_size("17").unwrap(), 17);
        assert!(parse_size("abc").is_err());
        assert_eq!(human(3 << 30), "3.0G");
        assert_eq!(human_delta(-(5 << 20)), "-5M");
    }

    #[test]
    fn times() {
        assert_eq!(fmt_ts(0), "1970-01-01T00:00:00Z");
        assert_eq!(fmt_ts(1_794_700_800), "2026-11-15T00:00:00Z");
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("x"), None);
    }

    #[test]
    fn tsv_roundtrip() {
        let p = "/a\tb/c\\d\ne";
        assert_eq!(tsv_unesc(&tsv_esc(p)), p);
    }
}
