// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::names::{MAX_NAME_BYTES, casefold, conflict_name, name_key, normalize_name};
use proptest::prelude::*;
use unicode_normalization::UnicodeNormalization;

#[test]
fn nfc_expansion_of_the_device_name_never_cuts_the_attempt_number() {
    let original = format!("x.{}", "e".repeat(200));
    for device in ["\u{0A5E}".repeat(40), "\u{1D160}".repeat(40)] {
        let names: Vec<String> = (1..=5)
            .map(|attempt| conflict_name(&original, &device, 1_794_268_740_000, attempt))
            .collect();
        for (i, name) in names.iter().enumerate() {
            assert!(name.len() <= MAX_NAME_BYTES, "{name}");
            if i > 0 {
                assert!(name.contains(&format!(" {})", i + 1)), "{name}");
            }
        }
        let distinct: std::collections::BTreeSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len());
    }
}

#[test]
fn name_key_matches_canonically_equivalent_names_that_differ_only_by_case() {
    assert_eq!(name_key("\u{0390}"), name_key("\u{03AA}\u{0301}"));
    assert_eq!(name_key("\u{0390}"), "\u{03B9}\u{0308}\u{0301}");
}

#[test]
fn every_ascii_character_keys_like_its_full_folding() {
    for c in (0..=0x7f_u8).map(char::from) {
        let s = c.to_string();
        let key: String = casefold(&s.nfd().collect::<String>()).nfd().collect();
        assert_eq!(name_key(&s), key, "{c:?}");
    }
}

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Zß.]{1,12}",
        "\\PC{1,40}",
        "[가-힣]{80,90}",
        "[a-z]{240,255}(\\.[a-z]{1,8})?",
    ]
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/names.proptest-regressions"),
        512,
    ))]

    #[test]
    fn name_key_is_idempotent(s in "\\PC{0,40}") {
        prop_assert_eq!(name_key(&name_key(&s)), name_key(&s));
        prop_assert_eq!(casefold(&casefold(&s)), casefold(&s));
    }

    #[test]
    fn name_key_is_the_nfd_of_the_casefold_of_the_nfd(s in prop_oneof!["[\\x00-\\x7f]{0,40}", name()]) {
        let nfd: String = s.nfd().collect();
        let key: String = casefold(&nfd).nfd().collect();
        prop_assert_eq!(name_key(&s), key);
    }

    #[test]
    fn canonically_equivalent_names_share_a_key(s in name()) {
        let nfc: String = s.nfc().collect();
        let nfd: String = s.nfd().collect();
        prop_assert_eq!(name_key(&nfc), name_key(&nfd));
    }

    #[test]
    fn normalizing_is_idempotent(s in name()) {
        if let Some(n) = normalize_name(&s) {
            prop_assert_eq!(normalize_name(&n), Some(n.clone()));
            prop_assert!(n.len() <= MAX_NAME_BYTES);
        }
    }

    #[test]
    fn conflict_names_are_valid_and_distinct(s in name(), device in "\\PC{0,40}", attempt in 1u32..5, day in 0i64..40_000) {
        let Some(original) = normalize_name(&s) else { return Ok(()) };
        let renamed = conflict_name(&original, &device, day * 86_400_000, attempt);
        prop_assert!(renamed.len() <= MAX_NAME_BYTES);
        prop_assert_eq!(normalize_name(&renamed), Some(renamed.clone()));
        prop_assert_ne!(name_key(&renamed), name_key(&original));
        prop_assert!(renamed.contains(" (conflict from "));
        if attempt > 1 {
            prop_assert_ne!(name_key(&renamed), name_key(&conflict_name(&original, &device, day * 86_400_000, attempt - 1)));
        }
    }
}
