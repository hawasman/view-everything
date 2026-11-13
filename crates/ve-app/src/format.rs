use chrono::{Local, TimeZone};

/// Sizes and counts read the same in the engine's messages and on every page.
pub use ve_engine::format::{count, size};

/// A modification time in local time, e.g. `2025-03-14 12:05`; blank when unknown.
pub fn time(unix_nanos: i64) -> String {
    if unix_nanos == 0 {
        return String::new();
    }
    Local.timestamp_nanos(unix_nanos).format("%Y-%m-%d %H:%M").to_string()
}
