// SPDX-License-Identifier: Apache-2.0

pub(crate) fn count(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// How many decisions wait for the person, as summary lines say it.
pub(crate) fn need_you(n: u64) -> String {
    format!("{n} {}", if n == 1 { "needs you" } else { "need you" })
}

/// Bytes in decimal units, such as 999 B, 1.0 kB or 1.5 GB.
pub(crate) fn size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["kB", "MB", "GB", "TB"] {
        value /= 1000.0;
        if value < 999.95 {
            return format!("{value:.1} {unit}");
        }
    }
    format!("{:.1} PB", value / 1000.0)
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// The text with control characters and bidirectional formatting characters escaped, such as `\u{1b}`. Another device
/// chose its name and platform, and a terminal would run the escape sequences in them or show forged rows.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                c.escape_unicode().to_string()
            }
            c if c.is_control() => c.escape_unicode().to_string(),
            c => c.to_string(),
        })
        .collect()
}
