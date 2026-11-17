// SPDX-License-Identifier: Apache-2.0

#![no_main]

use koidrive_ignore::{Kind, LocalRules, Matcher, NameKey, RuleFile, Source, Status};
use libfuzzer_sys::fuzz_target;

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        koidrive_engine::names::name_key(name)
    }
}

struct Exact;

impl NameKey for Exact {
    fn key(name: &str) -> String {
        name.to_owned()
    }
}

fn check<K: NameKey>(data: &[u8]) {
    let file = RuleFile::<K>::parse(data);
    let lines: Vec<String> = data
        .split(|&b| b == b'\n')
        .take(64)
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect();
    let local = LocalRules::<K>::new(&lines, &lines).unwrap_or_default();
    let mut matcher = Matcher::new(local);
    matcher.set_rules(&[] as &[&str], Source::Koiignore, Some(file), []);
    for line in &lines {
        let path: Vec<&str> = line.split('/').filter(|c| !c.is_empty()).collect();
        for kind in [Kind::File, Kind::Dir] {
            let _ = matcher.status(&path, kind);
        }
    }
    assert_eq!(matcher.status(&[".koiignore"], Kind::File), Status::Included);
    assert_eq!(matcher.status(&[".gitignore"], Kind::File), Status::Included);
}

fuzz_target!(|data: &[u8]| {
    check::<Folded>(data);
    check::<Exact>(data);
});
