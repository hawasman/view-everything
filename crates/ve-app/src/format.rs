use chrono::{Local, TimeZone};

/// A byte count in binary units, e.g. `1.4 GB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let digits = if value < 10.0 {
        2
    } else if value < 100.0 {
        1
    } else {
        0
    };
    format!("{value:.digits$} {}", UNITS[unit])
}

/// A count with thousands separators, e.g. `1,234,567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A modification time in local time, e.g. `2025-03-14 12:05`; blank when unknown.
pub fn time(unix_nanos: i64) -> String {
    if unix_nanos == 0 {
        return String::new();
    }
    Local
        .timestamp_nanos(unix_nanos)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}
