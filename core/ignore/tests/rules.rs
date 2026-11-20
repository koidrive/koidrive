// SPDX-License-Identifier: Apache-2.0

use koidrive_ignore::{
    Change, InvalidLine, Kind, LineError, LocalRules, LocalRulesError, Matcher, NameKey, Reason,
    RuleFile, Source, Status,
};

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        koidrive_engine::names::name_key(name)
    }
}

fn split(path: &str) -> Vec<String> {
    path.trim_end_matches('/')
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

fn kind(path: &str) -> Kind {
    if path.ends_with('/') {
        Kind::Dir
    } else {
        Kind::File
    }
}

fn matcher_with(local: LocalRules<Folded>, files: &[(&str, &str)]) -> Matcher<Folded> {
    let mut matcher = Matcher::new(local);
    for (dir, text) in files {
        matcher.set_rules(
            &split(dir),
            Source::Koiignore,
            Some(RuleFile::parse(text.as_bytes())),
            [],
        );
    }
    matcher
}

fn matcher(files: &[(&str, &str)]) -> Matcher<Folded> {
    matcher_with(LocalRules::default(), files)
}

fn status(matcher: &Matcher<Folded>, path: &str) -> Status {
    matcher.status(&split(path), kind(path))
}

fn excluded(matcher: &Matcher<Folded>, path: &str) -> bool {
    matches!(status(matcher, path), Status::Excluded(_))
}

fn rule(depth: usize, line: u32) -> Status {
    Status::Excluded(Reason::Rule {
        source: Source::Koiignore,
        depth,
        line,
    })
}

fn git_rule(depth: usize, line: u32) -> Status {
    Status::Excluded(Reason::Rule {
        source: Source::Gitignore,
        depth,
        line,
    })
}

fn sourced(local: LocalRules<Folded>, files: &[(&str, Source, &str)]) -> Matcher<Folded> {
    let mut matcher = Matcher::new(local);
    for (dir, source, text) in files {
        let rules = RuleFile::parse(text.as_bytes());
        matcher.set_rules(&split(dir), *source, Some(rules), []);
    }
    matcher
}

fn entries(paths: &[&str]) -> Vec<(Vec<String>, Kind)> {
    paths.iter().map(|p| (split(p), kind(p))).collect()
}

#[test]
fn patterns_apply_to_their_directory_subtree_only() {
    let m = matcher(&[("sub", "*.log\n/top.txt\n")]);
    assert_eq!(status(&m, "sub/a.log"), rule(1, 1));
    assert_eq!(status(&m, "sub/deep/b.log"), rule(1, 1));
    assert_eq!(status(&m, "sub/top.txt"), rule(1, 2));
    assert!(
        !excluded(&m, "sub/deep/top.txt"),
        "a leading slash anchors to the file's directory"
    );
    assert!(!excluded(&m, "a.log"));
    assert!(!excluded(&m, "other/a.log"));
}

#[test]
fn a_deeper_file_takes_precedence() {
    let m = matcher(&[("", "*.log\n"), ("sub", "!keep.log\n")]);
    assert_eq!(status(&m, "sub/keep.log"), Status::Included);
    assert_eq!(status(&m, "sub/deep/keep.log"), Status::Included);
    assert_eq!(status(&m, "sub/other.log"), rule(0, 1));
    assert_eq!(status(&m, "keep.log"), rule(0, 1));

    let m = matcher(&[("", "!*.tmp\n"), ("sub", "*.tmp\n")]);
    assert_eq!(status(&m, "sub/a.tmp"), rule(1, 1));
    assert_eq!(status(&m, "a.tmp"), Status::Included);
}

#[test]
fn the_last_matching_pattern_in_a_file_wins() {
    let m = matcher(&[("", "*.log\n!a.log\n")]);
    assert_eq!(status(&m, "a.log"), Status::Included);
    assert_eq!(status(&m, "b.log"), rule(0, 1));
    let m = matcher(&[("", "!a.log\n*.log\n")]);
    assert_eq!(status(&m, "a.log"), rule(0, 2));
}

#[test]
fn a_rules_file_in_an_included_directory_is_never_excluded() {
    for rules in [
        "*\n",
        ".koiignore\n",
        "/.koiignore\n",
        "**\n",
        ".*\n",
        "*/\n!sub/\n",
    ] {
        let m = matcher(&[("", rules), ("sub", "*\n")]);
        assert_eq!(status(&m, ".koiignore"), Status::Included, "{rules:?}");
        assert_eq!(
            status(&m, ".KOIIGNORE"),
            Status::Included,
            "compared by key: {rules:?}"
        );
    }
    let m = matcher(&[("", "*\n!sub/\n"), ("sub", ".koiignore\n")]);
    assert_eq!(status(&m, "sub/.koiignore"), Status::Included);
    assert_eq!(status(&m, "sub/other"), rule(0, 1));
    assert_eq!(
        status(&m, ".koiignore/"),
        rule(0, 1),
        "only a file is a rules file"
    );
}

#[test]
fn rules_inside_an_excluded_subtree_are_not_read() {
    let m = matcher(&[
        ("", "build/\n"),
        ("build", "!keep\n"),
        ("build/out", "!*\n"),
    ]);
    assert_eq!(status(&m, "build/keep"), rule(0, 1));
    assert_eq!(status(&m, "build/out/x"), rule(0, 1));
    assert_eq!(
        status(&m, "build/.koiignore"),
        rule(0, 1),
        "a rules file there is excluded with its directory"
    );
}

#[test]
fn a_path_under_an_excluded_directory_cannot_be_included() {
    let m = matcher(&[("", "dir/\n!dir/a\n")]);
    assert_eq!(status(&m, "dir/a"), rule(0, 1));
    let m = matcher(&[("", "dir/*\n!dir/a\n")]);
    assert_eq!(status(&m, "dir/a"), Status::Included);
    assert_eq!(status(&m, "dir/b"), rule(0, 1));
    assert_eq!(status(&m, "dir/"), Status::Included);
}

#[test]
fn built_in_exclusions_cannot_be_negated() {
    let rules = "!.DS_Store\n!.koidrive\n!.koidrive/\n!Thumbs.db\n!desktop.ini\n!Icon\r\r\n!*\n";
    let m = matcher(&[("", rules)]);
    for path in [
        ".DS_Store",
        "sub/.DS_Store",
        ".ds_store",
        "Thumbs.db",
        "a/b/THUMBS.DB",
        "desktop.ini",
        "Icon\r",
        "sub/Icon\r",
        ".koidrive/",
        ".KOIDRIVE/",
        ".koidrive/staging/x",
        ".koidrive",
    ] {
        assert_eq!(
            status(&m, path),
            Status::Excluded(Reason::BuiltIn),
            "{path:?}"
        );
    }
    assert_eq!(
        status(&m, "sub/.koidrive/"),
        Status::Included,
        "only the root's"
    );
    assert_eq!(status(&m, "Icon"), Status::Included);
}

#[test]
fn names_match_by_key_after_nfc() {
    let m = matcher(&[("", "Caf\u{e9}/\nstra\u{df}e.txt\n?.md\n[\u{e9}]x\n")]);
    assert!(
        excluded(&m, "cafe\u{301}/"),
        "an NFD name matches an NFC pattern"
    );
    assert!(excluded(&m, "CAF\u{c9}/"));
    assert!(excluded(&m, "CAFE\u{301}/x"));
    assert!(excluded(&m, "STRASSE.TXT"), "ß folds to ss");
    assert!(
        excluded(&m, "e\u{301}.md"),
        "? matches one composed character"
    );
    assert!(!excluded(&m, "ab.md"));
    assert!(excluded(&m, "E\u{301}x"), "bracket members fold too");

    let m = matcher(&[("", "e\u{301}*\n")]);
    assert!(
        excluded(&m, "\u{e9}t\u{e9}"),
        "an NFD pattern matches an NFC name"
    );
    let m = matcher(&[("", "[A-C]*\n[[:upper:]]1\n")]);
    assert!(excluded(&m, "b"));
    assert!(excluded(&m, "B"));
    assert!(excluded(&m, "x1"));
}

#[test]
fn equivalent_pattern_spellings_select_the_same_names() {
    let names = [
        "e", "E", "\u{e9}", "e\u{301}", "\u{c9}", "E\u{301}", "q", "q\u{301}", "Q\u{301}",
        "\u{e5}", "\u{c5}", "A\u{30a}", "k", "\u{212a}", "\u{212b}", "x", "ss", "\u{df}",
    ];
    let spellings: &[&[&str]] = &[
        &["[\u{e9}]", "[e\u{301}]", "[E\u{301}]", "[\u{c9}]"],
        &["[!\u{e9}]", "[!e\u{301}]"],
        &["[K-\u{c5}]", "[\u{212a}-\u{212b}]", "[K-A\u{30a}]"],
        &["[q\u{301}]", "[Q\u{301}]"],
        &["\u{e9}", "e\u{301}", "\\e\u{301}", "\\\u{e9}"],
        &["?", "[!/]"],
        &["\u{df}", "ss", "SS"],
    ];
    for group in spellings {
        let selected = |pattern: &str| -> Vec<&str> {
            let m = matcher(&[("", &format!("{pattern}\n"))]);
            names
                .iter()
                .copied()
                .filter(|name| excluded(&m, name))
                .collect()
        };
        let first = selected(group[0]);
        for pattern in &group[1..] {
            assert_eq!(
                selected(pattern),
                first,
                "{pattern:?} against {:?}",
                group[0]
            );
        }
    }

    let m = matcher(&[("", "[e\u{301}]\n")]);
    assert!(excluded(&m, "\u{e9}") && excluded(&m, "E\u{301}"));
    assert!(!excluded(&m, "e"), "a plain e is not \u{e9}");
    let m = matcher(&[("", "[q\u{301}]\n??x\n")]);
    assert!(
        excluded(&m, "q"),
        "[q\u{301}] is the set of q and U+0301, as in git"
    );
    assert!(!excluded(&m, "q\u{301}"));
    assert!(excluded(&m, "q\u{301}x"), "? reads one code point");
    let m = matcher(&[("", "[K-\u{c5}]\n")]);
    assert!(excluded(&m, "\u{e5}") && excluded(&m, "\u{212b}") && excluded(&m, "k"));
}

#[test]
fn wildcards_match_code_points_next_to_combining_marks() {
    let m = matcher(&[("", "q*\n")]);
    for path in ["q\u{301}", "q\u{301}.txt", "sub/q\u{301}", "Q\u{301}x"] {
        assert!(excluded(&m, path), "q* should exclude {path:?}");
    }
    assert!(!excluded(&m, "\u{e9}"));
    let m = matcher(&[("", "*\u{301}\n")]);
    assert!(excluded(&m, "q\u{301}"));
    assert!(excluded(&m, "sub/aq\u{301}"));
    assert!(!excluded(&m, "q"));
    let m = matcher(&[("", "q\n?\n")]);
    assert!(!excluded(&m, "q\u{301}"));
    let m = matcher(&[("", "q?\n")]);
    assert!(excluded(&m, "q\u{301}"), "? reads the mark");
    for pattern in [
        "?\u{301}",
        "[q]\u{301}",
        "[q]*",
        "[!x]\u{301}",
        "[a-z]*",
        "[q]\u{301}*",
    ] {
        let m = matcher(&[("", &format!("{pattern}\n"))]);
        assert!(excluded(&m, "q\u{301}"), "{pattern:?}");
    }
    let m = matcher(&[("", "*.txt\n")]);
    assert!(excluded(&m, "a\u{301}\u{302}.txt"));
}

#[test]
fn gitignore_syntax() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        ("# comment\n", &[], &["# comment"]),
        ("\\#a\n", &["#a"], &["a"]),
        ("\\!a\n", &["!a"], &["a"]),
        ("a  \n", &["a"], &["a  "]),
        ("a\\ \n", &["a "], &["a"]),
        ("a\r\n", &["a"], &["a\r"]),
        ("\u{feff}a\n", &["a"], &[]),
        ("dir/\n", &["dir/", "x/dir/"], &["dir"]),
        ("/a/b\n", &["a/b"], &["x/a/b"]),
        ("a/b\n", &["a/b"], &["x/a/b"]),
        ("**/b\n", &["b", "x/y/b"], &[]),
        ("a/**\n", &["a/x", "a/x/y"], &["a/"]),
        ("a/**/b\n", &["a/b", "a/x/y/b"], &["a/xb"]),
        ("a**b\n", &["ab", "axyb"], &["a/b"]),
        ("x/*/z\n", &["x/y/z"], &["x/y/w/z"]),
        ("[a-c]?\n", &["b1"], &["d1", "b"]),
        ("[!a-c]\n", &["d"], &["b"]),
        ("[]a]\n", &["]", "a"], &["b"]),
        ("[[:digit:]]*\n", &["1x"], &["x1"]),
        ("\\*\n", &["*"], &["a"]),
        ("*.[oa]\n", &["x.o", "x.a"], &["x.c"]),
    ];
    for (rules, hit, miss) in cases {
        let m = matcher(&[("", rules)]);
        for path in *hit {
            assert!(excluded(&m, path), "{rules:?} should exclude {path:?}");
        }
        for path in *miss {
            assert!(!excluded(&m, path), "{rules:?} should not exclude {path:?}");
        }
    }
    let twice = "\u{feff}\u{feff}a\n".as_bytes();
    assert_eq!(RuleFile::<Folded>::parse(twice).source(), twice);
}

#[test]
fn an_escaped_slash_after_a_double_star_needs_a_directory() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        ("**\\/b\n", &["x/b", "x/y/b"], &["b"]),
        ("a/**\\/b\n", &["a/x/b", "a/x/y/b"], &["a/b"]),
        ("a\\/**/b\n", &["a/b", "a/x/b"], &["b"]),
    ];
    for (rules, hit, miss) in cases {
        let m = matcher(&[("", rules)]);
        for path in *hit {
            assert!(excluded(&m, path), "{rules:?} should exclude {path:?}");
        }
        for path in *miss {
            assert!(!excluded(&m, path), "{rules:?} should not exclude {path:?}");
        }
    }
    let nested = matcher(&[("d/e", "**\\/b\n")]);
    assert!(!excluded(&nested, "d/e/b"));
    assert!(excluded(&nested, "d/e/x/b"));
}

#[test]
fn unusable_lines_are_reported_and_skipped() {
    let file = RuleFile::<Folded>::parse(b"ok\n\xff\nfoo\\\n[ab\n[[:nope:]]\nx\0y\nlast");
    assert_eq!(
        file.invalid_lines(),
        [
            InvalidLine {
                line: 2,
                error: LineError::NotUtf8
            },
            InvalidLine {
                line: 3,
                error: LineError::TrailingBackslash
            },
            InvalidLine {
                line: 4,
                error: LineError::UnclosedBracket
            },
            InvalidLine {
                line: 5,
                error: LineError::UnknownClass("nope".to_owned())
            },
            InvalidLine {
                line: 6,
                error: LineError::ContainsNul
            },
        ]
    );
    let mut m = Matcher::new(LocalRules::default());
    m.set_rules(&split(""), Source::Koiignore, Some(file), []);
    assert_eq!(m.status(&split("ok"), Kind::File), rule(0, 1));
    assert_eq!(m.status(&split("last"), Kind::File), rule(0, 7));
}

#[test]
fn the_include_list_narrows_sync_to_its_paths() {
    let local = LocalRules::new(&["Projects/", "/Work/Notes.txt"], &[] as &[&str]).unwrap();
    let m = matcher_with(local, &[("", "*.log\n")]);
    assert_eq!(status(&m, "Projects/"), Status::Included);
    assert_eq!(
        status(&m, "projects/a/b"),
        Status::Included,
        "compared by key"
    );
    assert_eq!(
        status(&m, "Projects/x.log"),
        rule(0, 1),
        "root rules still apply"
    );
    assert_eq!(status(&m, ".koiignore"), Status::Included);
    assert_eq!(
        status(&m, "Work/"),
        Status::Included,
        "on the way to an include"
    );
    assert_eq!(status(&m, "Work/.koiignore"), Status::Included);
    assert_eq!(status(&m, "Work/Notes.txt"), Status::Included);
    for path in ["Other/", "readme.md", "Work/other", "Projects", "Work"] {
        assert_eq!(
            status(&m, path),
            Status::Excluded(Reason::Local),
            "{path:?}"
        );
    }
}

#[test]
fn the_local_list_and_koiignore_files_only_add_exclusions() {
    let local = LocalRules::new(&[] as &[&str], &["*.mp4", "!keep.log"]).unwrap();
    let m = matcher_with(local, &[("", "!a.mp4\n*.log\n")]);
    assert_eq!(status(&m, "a.mp4"), Status::Excluded(Reason::Local));
    assert_eq!(status(&m, "sub/b.mp4"), Status::Excluded(Reason::Local));
    assert_eq!(status(&m, "keep.log"), rule(0, 2));
    assert_eq!(status(&m, "x.txt"), Status::Included);
}

#[test]
fn local_lists_that_cannot_be_used_are_refused() {
    for include in ["", "/", "a//b", "../x", "a/./b"] {
        assert_eq!(
            LocalRules::<Folded>::new(&[include], &[] as &[&str]).unwrap_err(),
            LocalRulesError::Include {
                index: 0,
                entry: include.to_owned()
            }
        );
    }
    assert_eq!(
        LocalRules::<Folded>::new(&[] as &[&str], &["ok", "a\nb"]).unwrap_err(),
        LocalRulesError::Exclude {
            index: 1,
            entry: "a\nb".to_owned(),
            error: None
        }
    );
    assert_eq!(
        LocalRules::<Folded>::new(&[] as &[&str], &["ok", "# fine", "[ab"]).unwrap_err(),
        LocalRulesError::Exclude {
            index: 2,
            entry: "[ab".to_owned(),
            error: Some(LineError::UnclosedBracket)
        }
    );
}

#[test]
fn a_new_rule_excludes_without_deleting_and_removing_it_includes_again() {
    let tree = entries(&[
        "a.log",
        "sub/",
        "sub/b.log",
        "sub/c.txt",
        "sub/.koiignore",
        "other/d.log",
    ]);
    let mut m = matcher(&[]);
    let added = m.set_rules(
        &split("sub"),
        Source::Koiignore,
        Some(RuleFile::parse(b"*\n")),
        tree.clone(),
    );
    assert_eq!(
        added,
        [
            Change::Excluded {
                path: split("sub/b.log"),
                reason: Reason::Rule {
                    source: Source::Koiignore,
                    depth: 1,
                    line: 1
                }
            },
            Change::Excluded {
                path: split("sub/c.txt"),
                reason: Reason::Rule {
                    source: Source::Koiignore,
                    depth: 1,
                    line: 1
                }
            },
        ],
        "only the subtree changes, and the rules file stays"
    );
    let removed = m.set_rules(&split("sub"), Source::Koiignore, None, tree);
    assert_eq!(
        removed,
        [
            Change::Included {
                path: split("sub/b.log")
            },
            Change::Included {
                path: split("sub/c.txt")
            }
        ]
    );
}

#[test]
fn rules_set_inside_an_excluded_directory_wait_until_it_is_included() {
    let tree = entries(&["build/", "build/a.o", "build/keep.txt"]);
    let mut m = matcher(&[("", "build/\n")]);
    let changes = m.set_rules(
        &split("build"),
        Source::Koiignore,
        Some(RuleFile::parse(b"*.o\n")),
        tree.clone(),
    );
    assert!(changes.is_empty());
    assert_eq!(status(&m, "build/a.o"), rule(0, 1));
    let changes = m.set_rules(&split(""), Source::Koiignore, None, tree);
    assert_eq!(
        changes,
        [
            Change::Included {
                path: split("build")
            },
            Change::Included {
                path: split("build/keep.txt")
            },
        ]
    );
    assert_eq!(status(&m, "build/a.o"), rule(1, 1));
}

#[test]
fn changing_the_local_list_reports_the_difference() {
    let tree = entries(&["Projects/", "Projects/a", "Music/", "Music/b.mp3", "c.txt"]);
    let mut m = matcher(&[]);
    let local = LocalRules::new(&["Projects"], &[] as &[&str]).unwrap();
    let changes = m.set_local(local, tree.clone());
    assert_eq!(
        changes,
        [
            Change::Excluded {
                path: split("Music"),
                reason: Reason::Local
            },
            Change::Excluded {
                path: split("Music/b.mp3"),
                reason: Reason::Local
            },
            Change::Excluded {
                path: split("c.txt"),
                reason: Reason::Local
            },
        ]
    );
    let changes = m.set_local(LocalRules::default(), tree);
    assert_eq!(changes.len(), 3);
    assert!(changes.iter().all(|c| matches!(c, Change::Included { .. })));
}

#[test]
fn a_scope_walk_refuses_excluded_directories() {
    let m = matcher(&[("", "node_modules/\n"), ("app", "*.tmp\n")]);
    let root = m.root();
    assert_eq!(
        root.enter("node_modules").unwrap_err(),
        Reason::Rule {
            source: Source::Koiignore,
            depth: 0,
            line: 1
        }
    );
    let app = root.enter("app").unwrap();
    assert_eq!(app.status("x.tmp", Kind::File), rule(1, 1));
    assert_eq!(app.status("x.txt", Kind::File), Status::Included);
    assert!(app.enter("node_modules").is_err());
}

#[test]
fn a_long_globstar_chain_matches_without_recursion() {
    let m = matcher(&[("", &format!("{}a\n", "**/".repeat(50_000)))]);
    assert_eq!(status(&m, "x/y/a"), rule(0, 1));
    assert_eq!(status(&m, "b"), Status::Included);
}

#[test]
fn rules_files_are_named_by_key() {
    assert_eq!(Source::of::<Folded>(".GitIgnore"), Some(Source::Gitignore));
    assert_eq!(Source::of::<Folded>(".koiignore"), Some(Source::Koiignore));
    assert_eq!(Source::of::<Folded>("gitignore"), None);
}

#[test]
fn koiignore_patterns_decide_before_gitignore_patterns() {
    let m = sourced(
        LocalRules::default(),
        &[
            (
                "",
                Source::Koiignore,
                "!.env
*.psd
!build/
",
            ),
            (
                "app",
                Source::Gitignore,
                ".env
*.tmp
build/
!*.psd
",
            ),
        ],
    );
    assert_eq!(status(&m, "app/.env"), Status::Included);
    assert_eq!(status(&m, "app/cover.psd"), rule(0, 2));
    assert_eq!(status(&m, "app/a.tmp"), git_rule(1, 2));
    assert_eq!(status(&m, "app/build/"), Status::Included);
    assert_eq!(status(&m, "app/build/x.tmp"), git_rule(1, 2));
    assert_eq!(status(&m, "app/build/x.txt"), Status::Included);
}

#[test]
fn a_file_in_a_directory_a_gitignore_excludes_cannot_be_included_again() {
    let m = sourced(
        LocalRules::default(),
        &[
            (
                "",
                Source::Gitignore,
                "build/
!build/keep.txt
out/
",
            ),
            (
                "",
                Source::Koiignore,
                "!out/keep.txt
",
            ),
            (
                "build",
                Source::Gitignore,
                "!*
",
            ),
        ],
    );
    assert_eq!(status(&m, "build/keep.txt"), git_rule(0, 1));
    assert_eq!(status(&m, "out/keep.txt"), git_rule(0, 3));
}

#[test]
fn rules_files_of_both_kinds_are_always_included() {
    for gitignore in [true, false] {
        let m = sourced(
            LocalRules::default().with_gitignore(gitignore),
            &[
                (
                    "",
                    Source::Koiignore,
                    "*
!*/
",
                ),
                (
                    "",
                    Source::Gitignore,
                    "*
",
                ),
            ],
        );
        for path in [
            ".gitignore",
            ".koiignore",
            "sub/.GITIGNORE",
            "sub/.koiignore",
        ] {
            assert_eq!(status(&m, path), Status::Included, "{path}");
        }
        assert_eq!(status(&m, "sub/a"), rule(0, 1));
    }
}

const GIT_DIR: Status = Status::Excluded(Reason::Default);

#[test]
fn git_directories_are_left_out_at_any_depth_whether_the_device_follows_gitignore_files_or_not() {
    for gitignore in [true, false] {
        let m = sourced(LocalRules::default().with_gitignore(gitignore), &[]);
        for path in [
            ".git/",
            ".git/config",
            ".git/hooks/pre-commit",
            "app/.git/",
            "app/vendor/lib/.git/objects/pack/a.pack",
        ] {
            assert_eq!(status(&m, path), GIT_DIR, "{path}, gitignore {gitignore}");
        }
        for path in [
            ".git",
            "app/sub/.git",
            ".github/",
            "app.git/",
            ".git.bak/",
            ".gitignore",
            "app/.gitmodules",
        ] {
            assert_eq!(
                status(&m, path),
                Status::Included,
                "{path}, gitignore {gitignore}"
            );
        }
    }
}

#[test]
fn a_git_directory_name_is_compared_by_key_like_the_built_in_names() {
    fn left_out<K: NameKey>(name: &str, reason: Reason) -> bool {
        Matcher::<K>::new(LocalRules::default()).status(&[name], Kind::Dir)
            == Status::Excluded(reason)
    }
    struct Exact;
    impl NameKey for Exact {
        fn key(name: &str) -> String {
            name.to_owned()
        }
    }
    for (git, state) in [(".GIT", ".KOIDRIVE"), (".Git", ".Koidrive")] {
        assert!(left_out::<Folded>(git, Reason::Default), "{git}");
        assert!(left_out::<Folded>(state, Reason::BuiltIn), "{state}");
        assert!(!left_out::<Exact>(git, Reason::Default), "{git}");
        assert!(!left_out::<Exact>(state, Reason::BuiltIn), "{state}");
    }
}

#[test]
fn a_koiignore_pattern_includes_git_directories_again_and_a_gitignore_pattern_does_not() {
    let root = sourced(
        LocalRules::default(),
        &[
            ("", Source::Koiignore, "!.git/\n"),
            ("", Source::Gitignore, ".git/\n"),
        ],
    );
    for path in [".git/", ".git/config", "app/.git/", "app/.git/HEAD"] {
        assert_eq!(status(&root, path), Status::Included, "{path}");
    }

    let nested = sourced(
        LocalRules::default(),
        &[("app", Source::Koiignore, "!.git/\n")],
    );
    for path in ["app/.git/", "app/.git/HEAD", "app/lib/.git/"] {
        assert_eq!(status(&nested, path), Status::Included, "{path}");
    }
    for path in [".git/", "other/.git/HEAD"] {
        assert_eq!(status(&nested, path), GIT_DIR, "{path}");
    }

    for gitignore in [true, false] {
        let m = sourced(
            LocalRules::default().with_gitignore(gitignore),
            &[
                ("", Source::Gitignore, "!.git/\n!*\n"),
                ("app", Source::Gitignore, "!.git\n"),
            ],
        );
        for path in [".git/", "app/.git/", "app/.git/HEAD"] {
            assert_eq!(status(&m, path), GIT_DIR, "{path}, gitignore {gitignore}");
        }
    }

    let m = sourced(
        LocalRules::default(),
        &[
            ("", Source::Koiignore, "!.git/\n"),
            ("app", Source::Koiignore, ".git/\n"),
        ],
    );
    assert_eq!(status(&m, "app/.git/"), rule(1, 1));
    assert_eq!(status(&m, "lib/.git/"), Status::Included);
}

#[test]
fn a_file_the_index_lists_is_included_below_a_folder_a_gitignore_excludes() {
    let local = LocalRules::new(&[] as &[&str], &["dist/local.js"]).unwrap();
    let mut matcher = sourced(
        local,
        &[
            ("", Source::Gitignore, "dist/\n*.log\n"),
            ("", Source::Koiignore, "dist/koi.js\n"),
            ("repo/dist", Source::Gitignore, "!*\n"),
        ],
    );
    let listed = "dist/sub/app.js dist/koi.js dist/local.js dist/.DS_Store a.log";
    matcher.set_tracked(&[] as &[&str], listed.split(' '));
    matcher.set_tracked(&["repo"], ["dist/x.js"]);
    for path in "dist/ DIST/Sub/ dist/sub/app.js Dist/SUB/App.js a.log repo/dist/x.js".split(' ') {
        assert_eq!(status(&matcher, path), Status::Included, "{path}");
    }
    let left_out =
        "dist/other.js dist/sub/other.js dist/sub dist/other/ dist/.gitignore repo/dist/y.js";
    for path in left_out.split(' ') {
        assert_eq!(status(&matcher, path), git_rule(0, 1), "{path}");
    }
    assert_eq!(status(&matcher, "b.log"), git_rule(0, 2));
    assert_eq!(status(&matcher, "dist/koi.js"), rule(0, 1));
    assert_eq!(
        status(&matcher, "dist/local.js"),
        Status::Excluded(Reason::Local)
    );
    assert_eq!(
        status(&matcher, "dist/.DS_Store"),
        Status::Excluded(Reason::BuiltIn)
    );
    matcher.set_tracked(&[] as &[&str], [] as [&str; 0]);
    assert_eq!(status(&matcher, "dist/sub/app.js"), git_rule(0, 1));
    assert_eq!(status(&matcher, "repo/dist/x.js"), Status::Included);
    let ignoring = LocalRules::default().with_gitignore(false);
    let mut matcher = sourced(ignoring, &[("", Source::Gitignore, "dist/\n")]);
    matcher.set_tracked(&[] as &[&str], ["dist/a.js"]);
    assert_eq!(status(&matcher, "dist/b.js"), Status::Included);
}
