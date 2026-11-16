//! Human-readable formatting shared by the CLI and TUI.

/// `1536` → `"1.5 KB"`. Binary units, one decimal.
pub fn size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// `"100M"`, `"1.5G"`, `"512"` → bytes (binary units, as `size` prints them).
pub fn parse_size(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("{s:?} isn't a size like 500K, 100M or 2G"))?;
    let shift = match unit.trim().to_ascii_uppercase().trim_end_matches('B') {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(format!("{s:?} isn't a size like 500K, 100M or 2G")),
    };
    Ok((number * (1u64 << shift) as f64) as i64)
}

/// Unix seconds → `"YYYY-MM-DD"` (UTC), without pulling in a date library.
pub fn date(unix: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size("100M"), Ok(100 << 20));
        assert_eq!(parse_size("1.5gb"), Ok(3 << 29));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("5X").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_792_690_968), "2026-10-22");
        assert_eq!(date(-86_400), "1969-12-31");
    }
}
