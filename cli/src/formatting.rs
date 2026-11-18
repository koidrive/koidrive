// SPDX-License-Identifier: Apache-2.0

pub(crate) fn ago(ms: i64, now_ms: i64) -> String {
    if ms == 0 {
        return "never".to_owned();
    }
    let minutes = (now_ms - ms).max(0) / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..60 => format!("{minutes} min ago"),
        60..2880 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

pub(crate) fn local_time(ms: i64, now_ms: i64) -> String {
    let local = |ms: i64| {
        let seconds = ms.div_euclid(1000) as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let converted = unsafe { libc::localtime_r(&seconds, &mut tm) };
        (!converted.is_null()).then_some(tm)
    };
    let (Some(at), Some(now)) = (local(ms), local(now_ms)) else {
        return ago(ms, now_ms);
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS[at.tm_mon.clamp(0, 11) as usize];
    let year = match at.tm_year == now.tm_year {
        true => String::new(),
        false => format!(" {}", at.tm_year + 1900),
    };
    format!(
        "{month} {}{year}, {:02}:{:02}",
        at.tm_mday, at.tm_hour, at.tm_min
    )
}

pub(crate) fn count(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

pub(crate) fn need_you(n: u64) -> String {
    format!("{n} {}", if n == 1 { "needs you" } else { "need you" })
}

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
