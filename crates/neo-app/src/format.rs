pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn rate(bps: f64) -> String {
    format!("{}/s", bytes(bps.max(0.0) as u64))
}

pub fn eta(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return String::new();
    }
    let s = secs.ceil() as u64;
    if s < 60 {
        format!("{s}s left")
    } else if s < 3600 {
        format!("{}m {}s left", s / 60, s % 60)
    } else {
        format!("{}h {}m left", s / 3600, (s % 3600) / 60)
    }
}

/// A Unix timestamp as YYYY-MM-DD (UTC).
pub fn date(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parses YYYY-MM-DD into the Unix time at the end of that day (UTC).
pub fn parse_date(text: &str) -> Option<u64> {
    let mut it = text.trim().split('-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    if civil_from_days(days) != (y, m, d) {
        return None; // e.g. 2026-02-30
    }
    Some(days as u64 * 86_400 + 86_399)
}

// Howard Hinnant's civil-date algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_roundtrip() {
        assert_eq!(super::date(0), "1970-01-01");
        let t = super::parse_date("2026-11-13").unwrap();
        assert_eq!(super::date(t), "2026-11-13");
        assert_eq!(super::parse_date("2026-02-30"), None);
        assert_eq!(super::parse_date("tomorrow"), None);
    }
}
