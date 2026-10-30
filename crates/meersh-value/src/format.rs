use crate::Value;

const NANOS_PER_SEC: f64 = 1e9;

pub fn format_filesize(bytes: i64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let sign = if bytes < 0 { "-" } else { "" };
    let mut size = bytes.unsigned_abs() as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{sign}{} B", bytes.unsigned_abs()) } else { format!("{sign}{size:.1} {}", UNITS[unit]) }
}

/// Shows the two most significant units, for example `3d 4h` or `2m 5s`.
pub fn format_duration(nanos: i64) -> String {
    const UNITS: [(&str, i64); 7] = [
        ("wk", 7 * 86_400_000_000_000),
        ("d", 86_400_000_000_000),
        ("h", 3_600_000_000_000),
        ("m", 60_000_000_000),
        ("s", 1_000_000_000),
        ("ms", 1_000_000),
        ("µs", 1_000),
    ];
    if nanos == 0 {
        return "0s".into();
    }
    let sign = if nanos < 0 { "-" } else { "" };
    let mut rest = nanos.unsigned_abs() as i64;
    let mut parts = Vec::new();
    for (name, size) in UNITS {
        if rest >= size {
            parts.push(format!("{}{name}", rest / size));
            rest %= size;
            if parts.len() == 2 {
                break;
            }
        } else if !parts.is_empty() {
            break;
        }
    }
    if parts.is_empty() {
        parts.push(format!("{rest}ns"));
    }
    format!("{sign}{}", parts.join(" "))
}

/// Parses words that are not plain strings: numbers, `true`, `null`, filesizes such as
/// `10kb` or `1.5gib` and durations such as `3day` or `10min`.
pub fn parse_literal(word: &str) -> Option<Value> {
    match word {
        "true" => return Some(Value::Bool(true)),
        "false" => return Some(Value::Bool(false)),
        "null" => return Some(Value::Nothing),
        _ => {}
    }
    let starts_numeric = {
        let mut chars = word.chars();
        match chars.next() {
            Some(c) if c.is_ascii_digit() => true,
            Some('-' | '.') => chars.next().is_some_and(|c| c.is_ascii_digit()),
            _ => false,
        }
    };
    if !starts_numeric {
        return None;
    }
    let clean = word.replace('_', "");
    if let Ok(i) = clean.parse::<i64>() {
        return Some(Value::Int(i));
    }
    if let Ok(f) = clean.parse::<f64>() {
        return Some(Value::Float(f));
    }
    let split = clean.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))?;
    let (number, unit) = clean.split_at(split);
    let number: f64 = number.parse().ok()?;
    let filesize = match unit.to_ascii_lowercase().as_str() {
        "b" => Some(1.0),
        "kb" => Some(1e3),
        "mb" => Some(1e6),
        "gb" => Some(1e9),
        "tb" => Some(1e12),
        "kib" => Some(1024.0),
        "mib" => Some(1024.0f64.powi(2)),
        "gib" => Some(1024.0f64.powi(3)),
        "tib" => Some(1024.0f64.powi(4)),
        _ => None,
    };
    if let Some(factor) = filesize {
        return Some(Value::Filesize((number * factor) as i64));
    }
    let seconds = match unit {
        "ns" => 1e-9,
        "us" | "µs" => 1e-6,
        "ms" => 1e-3,
        "s" | "sec" => 1.0,
        "min" => 60.0,
        "h" | "hr" => 3600.0,
        "d" | "day" => 86_400.0,
        "w" | "wk" => 7.0 * 86_400.0,
        // Calendar units are approximations, a month is 30 days and a year 365.
        "mo" | "month" | "months" => 30.0 * 86_400.0,
        "y" | "yr" | "year" | "years" => 365.0 * 86_400.0,
        _ => return None,
    };
    Some(Value::Duration((number * seconds * NANOS_PER_SEC) as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(parse_literal("42"), Some(Value::Int(42)));
        assert_eq!(parse_literal("1.5"), Some(Value::Float(1.5)));
        assert_eq!(parse_literal("10kb"), Some(Value::Filesize(10_000)));
        assert_eq!(parse_literal("2KiB"), Some(Value::Filesize(2048)));
        assert_eq!(parse_literal("3day"), Some(Value::Duration(3 * 86_400_000_000_000)));
        assert_eq!(parse_literal("6mo"), Some(Value::Duration(180 * 86_400_000_000_000)));
        assert_eq!(parse_literal("1yr"), Some(Value::Duration(365 * 86_400_000_000_000)));
        assert_eq!(parse_literal("src"), None);
        assert_eq!(parse_literal("2to3"), None);
    }

    #[test]
    fn display() {
        assert_eq!(format_filesize(812), "812 B");
        assert_eq!(format_filesize(1536), "1.5 KiB");
        assert_eq!(format_duration(90_000_000_000), "1m 30s");
    }
}
