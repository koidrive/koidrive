// SPDX-License-Identifier: Apache-2.0

use unicode_normalization::UnicodeNormalization;

use crate::casefold_data::CASE_FOLDING;

pub const MAX_NAME_BYTES: usize = 255;
const MAX_DEVICE_NAME_CHARS: usize = 32;
const MAX_DEVICE_NAME_BYTES: usize = 128;

pub fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match CASE_FOLDING.binary_search_by_key(&u32::from(c), |&(code, _)| code) {
            Ok(i) => out.push_str(CASE_FOLDING[i].1),
            Err(_) => out.push(c),
        }
    }
    out
}

pub fn nfc(name: &str) -> String {
    name.nfc().collect()
}

/// Whether two names are the same name: equal, or equal in NFC (L keeps the spelling on disk).
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || nfc(a) == nfc(b)
}

/// The key under which no two live children of a directory may collide: canonical caseless matching,
/// `NFD(casefold(NFD(name)))`, so canonically equivalent names that differ only by case share a key.
pub fn name_key(name: &str) -> String {
    casefold(&name.nfd().collect::<String>()).nfd().collect()
}

/// RFC 0001 section 2: clients keep their local state in `.koidrive/` at the root of the synced
/// folder, so no node may take that name, in any case or normalization, directly under the root.
pub fn is_reserved_at_root(name: &str) -> bool {
    name_key(name) == ".koidrive"
}

/// The NFC form of a valid node name, or `None` when the name is not allowed.
pub fn normalize_name(name: &str) -> Option<String> {
    let name = nfc(name);
    let valid = (1..=MAX_NAME_BYTES).contains(&name.len())
        && !name.contains(['/', '\0'])
        && name != "."
        && name != "..";
    valid.then_some(name)
}

/// `<stem> (conflict from <device> <YYYY-MM-DD>)<ext>`, with ` <attempt>` before `)` from the second attempt on.
/// The device name is capped at 128 bytes after NFC, so the date and the attempt number always fit; then the
/// stem, and after it the extension, is shortened when the result would exceed 255 bytes once normalized to
/// NFC. NFC can lengthen a name, for example by decomposing a composition exclusion such as U+0A5E or
/// U+1D160 in the device name, and without the cap it could push the attempt number out of the name.
pub fn conflict_name(name: &str, device_name: &str, at_ms: i64, attempt: u32) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let mut device: Vec<char> = device_name
        .chars()
        .filter(|&c| c != '/' && c != '\0')
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    while nfc(&device.iter().collect::<String>()).len() > MAX_DEVICE_NAME_BYTES {
        device.pop();
    }
    let device: String = device.into_iter().collect();
    let device = if device.is_empty() {
        "unknown device"
    } else {
        &device
    };
    let attempt = if attempt > 1 {
        format!(" {attempt}")
    } else {
        String::new()
    };
    let suffix: Vec<char> = format!(
        " (conflict from {device} {}{attempt}){ext}",
        utc_date(at_ms)
    )
    .chars()
    .collect();
    let fits = |chars: &[char]| nfc(&chars.iter().collect::<String>()).len() <= MAX_NAME_BYTES;
    let mut stem: Vec<char> = stem.chars().collect();
    while !stem.is_empty() && !fits(&[stem.as_slice(), &suffix].concat()) {
        stem.pop();
    }
    let mut chars = [stem, suffix].concat();
    while !fits(&chars) {
        chars.pop();
    }
    nfc(&chars.into_iter().collect::<String>())
}

fn utc_date(at_ms: i64) -> String {
    let days = at_ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
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
    fn folds_full_case_mappings() {
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(name_key("STRASSE"), name_key("straße"));
        assert_eq!(casefold("\u{130}"), "i\u{307}");
        assert_eq!(casefold("ﬃ"), "ffi");
        assert_eq!(name_key("Cafe\u{301}"), name_key("CAFÉ"));
    }

    #[test]
    fn validates_names() {
        assert_eq!(normalize_name("Cafe\u{301}").as_deref(), Some("Café"));
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert_eq!(normalize_name(bad), None, "{bad:?}");
        }
        assert!(normalize_name(&"a".repeat(255)).is_some());
        assert!(normalize_name(&"a".repeat(256)).is_none());
        assert!(normalize_name(&"가".repeat(85)).is_some());
        assert!(normalize_name(&"가".repeat(86)).is_none());
    }

    #[test]
    fn formats_conflict_names_like_the_server() {
        let at = 1_794_268_740_000;
        assert_eq!(
            conflict_name("a.tar.gz", "Mac/Book", at, 1),
            "a.tar (conflict from MacBook 2026-11-09).gz"
        );
        assert_eq!(
            conflict_name(".bashrc", "pc", at, 2),
            ".bashrc (conflict from pc 2026-11-09 2)"
        );
        assert_eq!(
            conflict_name("x", &"d".repeat(40), at, 1),
            format!("x (conflict from {} 2026-11-09)", "d".repeat(32))
        );
        assert_eq!(
            conflict_name("x", "", at, 1),
            "x (conflict from unknown device 2026-11-09)"
        );
        assert!(conflict_name(&"가".repeat(85), "pc", at, 1).len() <= MAX_NAME_BYTES);
        let decomposing = conflict_name(&"가".repeat(85), "\u{a5e}", at, 1);
        assert!(
            decomposing.len() <= MAX_NAME_BYTES,
            "{} bytes",
            decomposing.len()
        );
    }

    #[test]
    fn computes_utc_dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(-1), "1969-12-31");
    }
}
