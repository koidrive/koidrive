// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use koidrive_ignore::{
    Change, Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Source, Status,
};
use proptest::prelude::*;
use proptest::sample::Index;

struct Exact;

impl NameKey for Exact {
    fn key(name: &str) -> String {
        name.to_owned()
    }
}

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        koidrive_engine::names::name_key(name)
    }
}

type Tree = BTreeMap<Vec<String>, Kind>;

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => "[aAb][aAb.]?",
        1 => Just(".koiignore".to_owned()),
        1 => Just(".DS_Store".to_owned()),
        1 => Just(".koidrive".to_owned()),
    ]
}

fn tree() -> impl Strategy<Value = Tree> {
    prop::collection::vec((prop::collection::vec(name(), 1..5), any::<bool>()), 1..40).prop_map(
        |paths| {
            let mut tree = Tree::new();
            'paths: for (path, is_dir) in paths {
                for depth in 1..=path.len() {
                    let kind = if depth < path.len() || is_dir {
                        Kind::Dir
                    } else {
                        Kind::File
                    };
                    match tree.get(&path[..depth]) {
                        Some(Kind::File) if depth < path.len() => continue 'paths,
                        Some(_) => {}
                        None => {
                            tree.insert(path[..depth].to_vec(), kind);
                        }
                    }
                }
            }
            tree
        },
    )
}

fn line() -> impl Strategy<Value = String> {
    prop_oneof![
        proptest::sample::select(vec![
            "*",
            "a",
            "A",
            "b",
            "*.a",
            "a*",
            "/a",
            "a/",
            "!a",
            "!*",
            "!b/",
            "a/b",
            "**/b",
            "a/**",
            "?",
            "[ab]",
            "!.koiignore",
            ".koiignore",
            "!.DS_Store",
            "*/",
            "!*/",
            "b/*",
            "!b/a",
        ])
        .prop_map(str::to_owned),
        "[aAb*?!/\\[\\]]{1,5}",
    ]
}

fn rules() -> impl Strategy<Value = String> {
    prop::collection::vec(line(), 0..5).prop_map(|lines| lines.join("\n"))
}

fn dirs(tree: &Tree) -> Vec<Vec<String>> {
    std::iter::once(Vec::new())
        .chain(
            tree.iter()
                .filter(|(_, k)| **k == Kind::Dir)
                .map(|(p, _)| p.clone()),
        )
        .collect()
}

#[derive(Clone, Debug)]
enum Update {
    Rules(Index, Source, Option<String>),
    Local(Vec<Index>, Vec<String>, bool),
}

fn source() -> impl Strategy<Value = Source> {
    prop::sample::select(Source::ALL.to_vec())
}

fn files() -> impl Strategy<Value = Vec<(Index, Source, String)>> {
    prop::collection::vec((any::<Index>(), source(), rules()), 0..4)
}

fn update() -> impl Strategy<Value = Update> {
    prop_oneof![
        3 => (any::<Index>(), source(), prop::option::of(rules()))
            .prop_map(|(d, s, r)| Update::Rules(d, s, r)),
        1 => (
            prop::collection::vec(any::<Index>(), 0..3),
            prop::collection::vec(line(), 0..3),
            any::<bool>(),
        )
            .prop_map(|(i, e, g)| Update::Local(i, e, g)),
    ]
}

fn local(tree: &Tree, include: &[Index], exclude: &[String]) -> LocalRules<Folded> {
    let paths: Vec<&Vec<String>> = tree.keys().collect();
    let include: Vec<String> = include.iter().map(|i| i.get(&paths).join("/")).collect();
    LocalRules::new(&include, exclude).unwrap_or_default()
}

fn statuses(matcher: &Matcher<Folded>, tree: &Tree) -> Vec<Status> {
    tree.iter()
        .map(|(path, kind)| matcher.status(path, *kind))
        .collect()
}

fn entries(tree: &Tree) -> Vec<(Vec<String>, Kind)> {
    tree.iter().map(|(p, k)| (p.clone(), *k)).collect()
}

fn build(
    tree: &Tree,
    files: &[(Index, Source, String)],
    local: LocalRules<Folded>,
) -> Matcher<Folded> {
    let dirs = dirs(tree);
    let mut matcher = Matcher::new(local);
    for (dir, source, text) in files {
        let rules = RuleFile::parse(text.as_bytes());
        matcher.set_rules(dir.get(&dirs), *source, Some(rules), []);
    }
    matcher
}

const PATTERN_PIECES: &[&[&str]] = &[
    &["\u{e9}", "e\u{301}"],
    &["\u{c9}", "E\u{301}"],
    &["\u{c5}", "A\u{30a}", "\u{212b}"],
    &["K", "\u{212a}"],
    &["q\u{301}"],
    &["a"],
    &["e"],
    &["?"],
    &["*"],
    &["/"],
    &["**"],
    &["[\u{e9}a]", "[e\u{301}a]"],
    &["[!\u{c9}]", "[!E\u{301}]"],
    &["[K-\u{c5}]", "[\u{212a}-\u{212b}]", "[K-A\u{30a}]"],
    &["[q\u{301}e]"],
    &["\\\u{e9}", "\\e\u{301}"],
];

const NAME_PIECES: &[&[&str]] = &[
    &["\u{e9}", "e\u{301}"],
    &["\u{c9}", "E\u{301}"],
    &["\u{c5}", "A\u{30a}", "\u{212b}"],
    &["K", "\u{212a}"],
    &["q\u{301}"],
    &["\u{e5}", "a\u{30a}"],
    &["a"],
    &["e"],
    &["q"],
    &["k"],
];

fn spellings(pieces: &[&[&str]], picks: &[(Index, Index)]) -> (String, String) {
    picks
        .iter()
        .map(|(piece, spelling)| {
            let choices = piece.get(pieces);
            (choices[0], *spelling.get(choices))
        })
        .fold((String::new(), String::new()), |(mut a, mut b), (x, y)| {
            a.push_str(x);
            b.push_str(y);
            (a, b)
        })
}

fn spelled_statuses<K: NameKey>(rules: &str, paths: &[(Vec<String>, Kind)]) -> Vec<Status> {
    let mut matcher = Matcher::<K>::new(LocalRules::default());
    matcher.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(rules.as_bytes())),
        [],
    );
    paths
        .iter()
        .map(|(path, kind)| matcher.status(path, *kind))
        .collect()
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/properties.proptest-regressions"),
        ProptestConfig::default().cases,
    ))]
    #[test]
    fn updates_report_exactly_what_changed(
        tree in tree(),
        files in files(),
        updates in prop::collection::vec(update(), 1..4),
    ) {
        let mut matcher = build(&tree, &files, LocalRules::default());
        for update in updates {
            let before = statuses(&matcher, &tree);
            let (changes, dir) = match &update {
                Update::Rules(dir, source, text) => {
                    let dir = dir.get(&dirs(&tree)).clone();
                    let rules = text.as_ref().map(|t| RuleFile::parse(t.as_bytes()));
                    (matcher.set_rules(&dir, *source, rules, entries(&tree)), dir)
                }
                Update::Local(include, exclude, gitignore) => {
                    let local = local(&tree, include, exclude).with_gitignore(*gitignore);
                    (matcher.set_local(local, entries(&tree)), Vec::new())
                }
            };
            let after = statuses(&matcher, &tree);
            let expected: Vec<Change> = tree
                .keys()
                .zip(before.iter().zip(&after))
                .filter_map(|(path, statuses)| match statuses {
                    (Status::Excluded(_), Status::Included) => Some(Change::Included { path: path.clone() }),
                    (Status::Included, Status::Excluded(reason)) => {
                        Some(Change::Excluded { path: path.clone(), reason: *reason })
                    }
                    _ => None,
                })
                .collect();
            prop_assert_eq!(&changes, &expected, "after {:?}", update);
            for change in &changes {
                let (Change::Included { path } | Change::Excluded { path, .. }) = change;
                prop_assert!(path.len() > dir.len());
                prop_assert!(path.iter().zip(&dir).all(|(a, b)| a.eq_ignore_ascii_case(b)));
            }
        }
    }

    #[test]
    fn gitignore_files_change_nothing_while_the_device_does_not_follow_them(
        tree in tree(),
        files in files(),
        gitignore in prop::collection::vec((any::<Index>(), rules()), 1..4),
    ) {
        let koiignore: Vec<_> = (files.into_iter())
            .filter(|(_, source, _)| *source == Source::Koiignore)
            .collect();
        let both: Vec<_> = (koiignore.iter().cloned())
            .chain(gitignore.into_iter().map(|(dir, text)| (dir, Source::Gitignore, text)))
            .collect();
        let off = LocalRules::default().with_gitignore(false);
        let without = build(&tree, &koiignore, off.clone());
        let with = build(&tree, &both, off);
        prop_assert_eq!(statuses(&with, &tree), statuses(&without, &tree));
        let on = build(&tree, &both, LocalRules::default());
        for (path, kind) in &tree {
            if without.status(path, *kind) != Status::Included {
                prop_assert_ne!(on.status(path, *kind), Status::Included, "{:?}", path);
            }
        }
    }

    #[test]
    fn exclusion_invariants_hold(
        tree in tree(),
        files in files(),
        include in prop::collection::vec(any::<Index>(), 0..3),
        exclude in prop::collection::vec(line(), 0..3),
    ) {
        let local = local(&tree, &include, &exclude);
        let matcher = build(&tree, &files, local);
        let status: BTreeMap<&Vec<String>, Status> =
            tree.iter().map(|(p, k)| (p, matcher.status(p, *k))).collect();
        for (path, kind) in &tree {
            let name = path.last().unwrap().to_ascii_lowercase();
            let parent = &path[..path.len() - 1];
            let parent_included = parent.is_empty()
                || status[&parent.to_vec()] == Status::Included;
            if !parent_included {
                prop_assert_eq!(status[path], status[&parent.to_vec()]);
            } else if name == ".ds_store" || (path.len() == 1 && name == ".koidrive") {
                prop_assert_eq!(status[path], Status::Excluded(Reason::BuiltIn));
            } else if *kind == Kind::File && name == ".koiignore" {
                prop_assert_eq!(status[path], Status::Included);
            }
        }
    }

    #[test]
    fn any_bytes_parse(
        bytes in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..400),
            prop::collection::vec(proptest::sample::select(b"*?[]!^/\\:#\n\r\t -aZ\xc3\xa9\0".to_vec()), 0..400),
        ],
        path in prop::collection::vec("[a-zA-Z*?\\[\\]!:#\\\\ é-]{1,4}", 1..4),
        is_dir in any::<bool>(),
    ) {
        let file = RuleFile::<Folded>::parse(&bytes);
        let lines = bytes.split(|&b| b == b'\n').count() as u32;
        let numbers: Vec<u32> = file.invalid_lines().iter().map(|l| l.line).collect();
        prop_assert!(numbers.windows(2).all(|w| w[0] < w[1]));
        prop_assert!(numbers.iter().all(|&n| n >= 1 && n <= lines));
        let mut matcher = Matcher::new(LocalRules::default());
        matcher.set_rules(&[] as &[&str], Source::Koiignore, Some(file), []);
        let kind = if is_dir { Kind::Dir } else { Kind::File };
        let _ = matcher.status(&path, kind);
        prop_assert_eq!(matcher.status(&[".koiignore"], Kind::File), Status::Included);
    }
    #[test]
    fn equivalent_spellings_give_the_same_verdicts(
        lines in prop::collection::vec(prop::collection::vec((any::<Index>(), any::<Index>()), 1..6), 1..4),
        paths in prop::collection::vec(
            (prop::collection::vec(prop::collection::vec((any::<Index>(), any::<Index>()), 1..4), 1..4), any::<bool>()),
            1..12,
        ),
    ) {
        let (rules_nfc, rules_other): (Vec<String>, Vec<String>) =
            lines.iter().map(|picks| spellings(PATTERN_PIECES, picks)).unzip();
        let (rules_nfc, rules_other) = (rules_nfc.join("\n"), rules_other.join("\n"));
        let mut nfc_paths = Vec::new();
        let mut other_paths = Vec::new();
        for (components, is_dir) in &paths {
            let kind = if *is_dir { Kind::Dir } else { Kind::File };
            let (a, b): (Vec<String>, Vec<String>) =
                components.iter().map(|picks| spellings(NAME_PIECES, picks)).unzip();
            nfc_paths.push((a, kind));
            other_paths.push((b, kind));
        }
        let expected = spelled_statuses::<Folded>(&rules_nfc, &nfc_paths);
        prop_assert_eq!(&spelled_statuses::<Folded>(&rules_other, &other_paths), &expected);
        prop_assert_eq!(&spelled_statuses::<Folded>(&rules_nfc, &other_paths), &expected);
        prop_assert_eq!(&spelled_statuses::<Folded>(&rules_other, &nfc_paths), &expected);
        let expected = spelled_statuses::<Exact>(&rules_nfc, &nfc_paths);
        prop_assert_eq!(&spelled_statuses::<Exact>(&rules_other, &other_paths), &expected);
    }
}
