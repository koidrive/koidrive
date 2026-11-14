// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use koidrive_ignore::{Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Status};
use proptest::prelude::*;
use proptest::sample::Index;
use unicode_normalization::UnicodeNormalization;

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

#[derive(Clone, Debug)]
struct Case {
    entries: BTreeMap<Vec<String>, Kind>,
    files: BTreeMap<Vec<String>, String>,
}

type Verdict = Option<(usize, u32)>;

fn ours<K: NameKey>(case: &Case, spell: fn(&str) -> String) -> Vec<Verdict> {
    let respell = |path: &[String]| -> Vec<String> { path.iter().map(|n| spell(n)).collect() };
    let mut matcher = Matcher::<K>::new(LocalRules::default());
    for (dir, text) in &case.files {
        let rules = RuleFile::parse(spell(text).as_bytes());
        matcher.set_rules(&respell(dir), Some(rules), []);
    }
    case.entries
        .iter()
        .map(|(path, kind)| match matcher.status(&respell(path), *kind) {
            Status::Included => None,
            Status::Excluded(Reason::Rule { depth, line }) => Some((depth, line)),
            Status::Excluded(reason) => panic!("{path:?} excluded by {reason:?}"),
        })
        .collect()
}

fn git(case: &Case, ignorecase: bool, spell: fn(&str) -> String) -> Vec<Verdict> {
    let work = tempfile::tempdir().expect("temporary directory");
    let root = work.path();
    let joined = |path: &[String]| -> String { spell(&path.join("/")) };
    for (path, kind) in &case.entries {
        let at = root.join(joined(path));
        match kind {
            Kind::Dir => std::fs::create_dir_all(&at),
            Kind::File => std::fs::write(&at, b""),
        }
        .unwrap_or_else(|e| panic!("cannot create {at:?}: {e}"));
    }
    for (dir, text) in &case.files {
        let at = root.join(joined(dir));
        std::fs::write(at.join(".gitignore"), spell(text)).expect("write .gitignore");
    }
    run_git(root, &["init", "-q"], None);

    let mut input = Vec::new();
    for path in case.entries.keys() {
        input.extend_from_slice(joined(path).as_bytes());
        input.push(0);
    }
    let ignorecase = format!("core.ignorecase={ignorecase}");
    let output = run_git(
        root,
        &[
            "-c",
            &ignorecase,
            "check-ignore",
            "--no-index",
            "--verbose",
            "--non-matching",
            "-z",
            "--stdin",
        ],
        Some(&input),
    );
    let fields: Vec<&[u8]> = output.split(|&b| b == 0).collect();
    let records: Vec<&[&[u8]]> = fields.chunks(4).filter(|r| r.len() == 4).collect();
    assert_eq!(
        records.len(),
        case.entries.len(),
        "git check-ignore output: {:?}",
        String::from_utf8_lossy(&output)
    );
    records
        .into_iter()
        .zip(case.entries.keys())
        .map(|(record, path)| {
            let [source, line, pattern, echoed] = record else {
                unreachable!()
            };
            assert_eq!(
                *echoed,
                joined(path).as_bytes(),
                "git answered out of order"
            );
            if source.is_empty() || pattern.starts_with(b"!") {
                return None;
            }
            let source = std::str::from_utf8(source).expect("UTF-8 source");
            let depth = source.matches('/').count();
            let line = std::str::from_utf8(line)
                .expect("line")
                .parse()
                .expect("line number");
            Some((depth, line))
        })
        .collect()
}

fn run_git(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
    let mut git = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            git.env_remove(key);
        }
    }
    let mut child = git
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the git oracle needs git on PATH");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.unwrap_or_default())
        .expect("write to git");
    let output = child.wait_with_output().expect("git output");
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn same(text: &str) -> String {
    text.to_owned()
}

fn nfd(text: &str) -> String {
    text.nfd().collect()
}

const STAND_INS: [(char, char); 10] = [
    ('\u{e9}', '0'),
    ('\u{c9}', '1'),
    ('\u{fc}', '2'),
    ('\u{c5}', '3'),
    ('\u{301}', '4'),
    ('\u{302}', '5'),
    ('\u{e1}', '6'),
    ('\u{c1}', '7'),
    ('\u{1d8}', '8'),
    ('\u{1fa}', '9'),
];

/// The NFC text with each non-ASCII code point but a byte order mark replaced by a one-byte
/// stand-in. Git matches bytes, so on this spelling its `?` and bracket expressions read one code
/// point, which is what this crate does on the NFC text.
fn code_points(text: &str) -> String {
    text.nfc()
        .map(|c| {
            if c.is_ascii() || c == '\u{feff}' {
                return c;
            }
            STAND_INS
                .iter()
                .find(|(from, _)| *from == c)
                .unwrap_or_else(|| panic!("no stand-in for {c:?}"))
                .1
        })
        .collect()
}

fn compare<K: NameKey>(
    case: &Case,
    ignorecase: bool,
    spell: fn(&str) -> String,
    theirs: fn(&str) -> String,
) -> Result<(), TestCaseError> {
    let ours = ours::<K>(case, spell);
    let theirs = git(case, ignorecase, theirs);
    for ((path, kind), (ours, theirs)) in case.entries.iter().zip(ours.into_iter().zip(theirs)) {
        prop_assert_eq!(
            ours,
            theirs,
            "{:?} ({:?}) with rules files {:#?}",
            path,
            kind,
            case.files
        );
    }
    Ok(())
}

fn trimmed(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut last_space = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' => {
                last_space.get_or_insert(i);
            }
            b'\\' => {
                i += 1;
                if i == bytes.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    &line[..last_space.unwrap_or(line.len())]
}

/// Whether the line has a `**` right after the literal start of a pattern with a slash
/// (`foo**/bar`). Git 2.39 treats that `**` as special, git 2.54 as a plain `*`, as git
/// documents and this crate does, so the oracle leaves such lines out.
fn version_dependent(line: &str) -> bool {
    let line = trimmed(line);
    let body = line.strip_prefix('!').unwrap_or(line);
    let body = body.strip_suffix('/').unwrap_or(body);
    if !body.contains('/') {
        return false;
    }
    let body = body.strip_prefix('/').unwrap_or(body).as_bytes();
    let Some(prefix) = body.iter().position(|b| b"*?[\\".contains(b)) else {
        return false;
    };
    if prefix == 0 || body[prefix - 1] == b'/' || !body[prefix..].starts_with(b"**") {
        return false;
    }
    let end = prefix + body[prefix..].iter().take_while(|&&b| b == b'*').count();
    match body.get(end) {
        None | Some(b'/') => true,
        Some(b'\\') => body.get(end + 1) == Some(&b'/'),
        Some(_) => false,
    }
}

fn name() -> impl Strategy<Value = String> {
    "[abAB.*?\\[\\]\\\\!# -]{1,3}".prop_filter("not . or ..", |n| n != "." && n != "..")
}

/// Bracket expressions. Under folding, members are lowercase: git leaves them unfolded.
fn class(fold: bool) -> BoxedStrategy<String> {
    let mut classes = vec![
        "[ab]",
        "[!a]",
        "[^b]",
        "[a-c]",
        "[A-C]",
        "[Z-a]",
        "[]a]",
        "[!]]",
        "[a-]",
        "[-b]",
        "[*?]",
        "[\\]]",
        "[\\a-b]",
        "[a-c-e]",
        "[[:alpha:]]",
        "[[:upper:]]",
        "[[:lower:]]",
        "[[:punct:][:digit:]]",
        "[[:space:]]",
        "[[:alnum:]x]",
        "[[:]]",
        "[[:a]",
        "[b-a]",
        "[.-]",
        "[[]",
        "[[:nope:]]",
    ];
    if !fold {
        classes.extend(["[A]", "[aB]", "[!B]", "[\\A]", "[[:upper:]A]", "[ab", "[\\"]);
    }
    proptest::sample::select(classes)
        .prop_map(str::to_owned)
        .boxed()
}

fn piece(fold: bool) -> impl Strategy<Value = String> {
    let mut literals = vec![
        "a", "b", "A", "B", ".", "-", " ", "#", "!", "\\*", "\\?", "\\[", "\\]", "\\\\", "\\!",
        "\\#", "\\ ", "\\a", "\\-",
    ];
    if !fold {
        literals.push("\\A");
    }
    prop_oneof![
        4 => proptest::sample::select(literals).prop_map(str::to_owned),
        2 => Just("*".to_owned()),
        1 => Just("**".to_owned()),
        1 => Just("***".to_owned()),
        2 => Just("?".to_owned()),
        2 => class(fold),
    ]
}

fn structured(fold: bool) -> impl Strategy<Value = String> {
    (
        any::<bool>(),
        prop_oneof![3 => Just(""), 1 => Just("/"), 1 => Just("\\/")],
        prop::collection::vec(prop::collection::vec(piece(fold), 1..4), 1..4),
        prop_oneof![3 => Just(""), 1 => Just("/"), 1 => Just("//")],
        prop_oneof![3 => Just(""), 1 => Just(" "), 1 => Just("  "), 1 => Just("\\ "), 1 => Just("\t")],
    )
        .prop_map(|(negated, lead, segments, trail, spaces)| {
            let body: Vec<String> = segments.into_iter().map(|s| s.concat()).collect();
            format!(
                "{}{lead}{}{trail}{spaces}",
                if negated { "!" } else { "" },
                body.join("/")
            )
        })
}

fn line(fold: bool) -> impl Strategy<Value = String> {
    let raw = if fold {
        "[ab*?\\[\\]\\\\/!# :-]{0,8}"
    } else {
        "[aAb*?\\[\\]\\\\/!# :-]{0,8}"
    };
    prop_oneof![
        6 => structured(fold),
        2 => raw,
        1 => Just(String::new()),
        1 => Just("# comment".to_owned()),
    ]
    .prop_filter("version-dependent in git", |line| !version_dependent(line))
}

/// Whether git with `core.ignorecase` folds every uppercase letter of `line` as this crate does. Git leaves an escaped
/// letter and a single member of a bracket expression as written, and folds a range end.
fn git_folds_every_letter(line: &str) -> bool {
    let line: Vec<char> = line.chars().collect();
    let upper = |at: usize| line.get(at).is_some_and(char::is_ascii_uppercase);
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            '\\' if upper(i + 1) => return false,
            '\\' => i += 2,
            '[' => {
                i += 1;
                if matches!(line.get(i), Some('!' | '^')) {
                    i += 1;
                }
                let mut first = true;
                loop {
                    match line.get(i) {
                        None => return true,
                        Some(']') if !first => break,
                        Some('\\') if upper(i + 1) => return false,
                        Some('\\') => i += 2,
                        Some('[') if line.get(i + 1) == Some(&':') => {
                            let end = (i + 2..line.len()).find(|&j| line[j] == ']');
                            match end {
                                Some(end) if line[end - 1] == ':' && end > i + 2 => i = end + 1,
                                _ => i += 1,
                            }
                        }
                        Some(_)
                            if line.get(i + 1) == Some(&'-')
                                && !matches!(line.get(i + 2), None | Some(']')) =>
                        {
                            if line[i + 2] == '\\' && upper(i + 3) {
                                return false;
                            }
                            i += if line[i + 2] == '\\' { 4 } else { 3 };
                        }
                        Some(_) if upper(i) => return false,
                        Some(_) => i += 1,
                    }
                    first = false;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    true
}

fn unicode_name() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => "[aAéÉüÅ.-]{1,3}".prop_filter("not . or ..", |n| n != "." && n != ".."),
        1 => proptest::sample::select(vec!["q\u{301}", "q\u{301}.a", "aq\u{301}", "q\u{301}\u{302}"])
            .prop_map(str::to_owned),
    ]
}

fn unicode_line() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        4 => proptest::sample::select(vec!["a", "A", "é", "É", "ü", "Å", "\\é", "-", ".", "q", "\u{301}"])
            .prop_map(str::to_owned),
        2 => Just("*".to_owned()),
        1 => Just("**".to_owned()),
        1 => Just("?".to_owned()),
        2 => proptest::sample::select(vec!["[\u{e9}]", "[!q]", "[q\u{301}]", "[a-z]", "[!\u{e9}a]", "[\u{301}]"])
            .prop_map(str::to_owned),
    ];
    (
        any::<bool>(),
        prop_oneof![3 => Just(""), 1 => Just("/")],
        prop::collection::vec(prop::collection::vec(piece, 1..4), 1..4),
        prop_oneof![3 => Just(""), 1 => Just("/")],
    )
        .prop_map(|(negated, lead, segments, trail)| {
            let body: Vec<String> = segments.into_iter().map(|s| s.concat()).collect();
            format!(
                "{}{lead}{}{trail}",
                if negated { "!" } else { "" },
                body.join("/")
            )
        })
        .prop_filter("version-dependent in git", |line| !version_dependent(line))
}

fn case(fold: bool) -> impl Strategy<Value = Case> {
    case_with(name().boxed(), line(fold).boxed())
}

fn case_with(
    name: BoxedStrategy<String>,
    line: BoxedStrategy<String>,
) -> impl Strategy<Value = Case> {
    let paths = prop::collection::vec((prop::collection::vec(name, 1..5), any::<bool>()), 1..30);
    let files = prop::collection::vec(
        (
            any::<Index>(),
            prop::collection::vec(line, 0..8),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        ),
        0..5,
    );
    (paths, files).prop_map(|(paths, files)| {
        let mut entries: BTreeMap<Vec<String>, Kind> = BTreeMap::new();
        // Siblings that differ only by case would collide on a case-insensitive volume, so a later
        // spelling takes the first one's.
        let mut spellings: BTreeMap<(Vec<String>, String), String> = BTreeMap::new();
        'paths: for (raw, is_dir) in paths {
            let mut path = Vec::new();
            for (i, name) in raw.iter().enumerate() {
                let spelling = spellings
                    .entry((path.clone(), name.to_lowercase()))
                    .or_insert_with(|| name.clone())
                    .clone();
                path.push(spelling);
                let kind = if i + 1 < raw.len() || is_dir {
                    Kind::Dir
                } else {
                    Kind::File
                };
                match entries.get(&path) {
                    Some(Kind::File) if i + 1 < raw.len() => continue 'paths,
                    Some(_) => {}
                    None => {
                        entries.insert(path.clone(), kind);
                    }
                }
            }
        }
        let dirs: Vec<Vec<String>> = std::iter::once(Vec::new())
            .chain(
                entries
                    .iter()
                    .filter(|(_, k)| **k == Kind::Dir)
                    .map(|(p, _)| p.clone()),
            )
            .collect();
        let files = files
            .into_iter()
            .map(|(dir, lines, crlf, bom, final_newline)| {
                let mut text = String::new();
                if bom {
                    text.push('\u{feff}');
                }
                text.push_str(&lines.join(if crlf { "\r\n" } else { "\n" }));
                if final_newline {
                    text.push('\n');
                }
                (dir.get(&dirs).clone(), text)
            })
            .collect();
        Case { entries, files }
    })
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/git_oracle.proptest-regressions"),
        ProptestConfig::default().cases,
    ))]
    #[test]
    fn agrees_with_git_case_sensitive(case in case(false)) {
        compare::<Exact>(&case, false, same, same)?;
    }

    #[test]
    fn agrees_with_git_ignorecase(case in case(true)) {
        compare::<Folded>(&case, true, same, same)?;
    }

    #[test]
    fn nfd_spellings_agree_with_git_on_nfc_code_points(case in case_with(unicode_name().boxed(), unicode_line().boxed())) {
        compare::<Exact>(&case, false, nfd, code_points)?;
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/git_oracle.proptest-regressions"),
        10_000,
    ))]

    #[test]
    fn ignorecase_lines_hold_no_letter_git_leaves_unfolded(line in line(true)) {
        prop_assert!(git_folds_every_letter(&line), "{:?}", line);
    }
}

#[test]
fn a_folded_escaped_member_agrees_with_git_on_its_lowercase_spelling() {
    let case = |text: &str| Case {
        entries: ["B", "a", "["]
            .into_iter()
            .map(|name| (vec![name.to_owned()], Kind::File))
            .collect(),
        files: BTreeMap::from([(Vec::new(), format!("{text}\n"))]),
    };
    let (line, lowercase) = (case("[\\B[Z-a]"), case("[\\b[Z-a]"));
    assert_eq!(ours::<Exact>(&line, same), git(&line, false, same));
    let folded = ours::<Folded>(&line, same);
    assert_eq!(folded, [Some((0, 1)); 3]);
    assert_eq!(folded, git(&lowercase, true, same));
    assert_ne!(folded, git(&line, true, same), "git leaves \\B unfolded");
}

#[test]
fn curated_lines_agree_with_git() {
    let tree = [
        "a/b/c.txt",
        "a/b/d/",
        "a/x.log",
        "a/foo/bar",
        "a/foo/x/bar",
        "a/foox/bar",
        "foo/bar",
        "foo/a/b/bar",
        "b.log",
        "#x",
        "!x",
        " lead",
        "trail ",
        "[x]",
        "a*b",
        "q?",
        "back\\slash",
        "x/y/z/",
        "x/y/w",
        "abc/",
        "abc/def/",
        "abc/def/ghi",
        "Upper/Case.TXT",
    ];
    let lines = [
        "*.log",
        "/b.log",
        "a/b/",
        "a/b",
        "b/",
        "foo/**/bar",
        "**/bar",
        "**\\/bar",
        "**\\/b.log",
        "foo/**\\/bar",
        "a\\/**/bar",
        "a/**",
        "**",
        "*",
        "a/*",
        "a/**/",
        "**/b/**",
        "a/**/b",
        "\\#x",
        "\\!x",
        "#x",
        "!x",
        " lead",
        "trail\\ ",
        "trail ",
        "\\[x\\]",
        "[[]x]",
        "a\\*b",
        "a*b",
        "q\\?",
        "back\\\\slash",
        "[a-c]*",
        "[!a]*",
        "x/*/z",
        "x/*",
        "/x/y/",
        "abc/**/ghi",
        "abc/***/ghi",
        "a**",
        "**/",
        "*/",
        "/*",
        "?",
        "??",
        "[[:alpha:]]*",
        "[[:upper:]]*",
        "[[:digit:][:punct:]]*",
        "upper/case.txt",
        "UPPER/",
        "*.TXT",
        "a/b/c.txt/",
        "x/y/z/w",
    ];
    let mut entries = BTreeMap::new();
    for path in tree {
        let kind = if path.ends_with('/') {
            Kind::Dir
        } else {
            Kind::File
        };
        let parts: Vec<String> = path
            .trim_end_matches('/')
            .split('/')
            .map(str::to_owned)
            .collect();
        for depth in 1..parts.len() {
            entries.insert(parts[..depth].to_vec(), Kind::Dir);
        }
        entries.insert(parts, kind);
    }
    for line in lines {
        for text in [format!("{line}\n"), format!("{line}\n!*.txt\n!a/\n")] {
            let case = Case {
                entries: entries.clone(),
                files: BTreeMap::from([(Vec::new(), text.clone())]),
            };
            for fold in [false, true] {
                let ours = if fold {
                    ours::<Folded>(&case, same)
                } else {
                    ours::<Exact>(&case, same)
                };
                let theirs = git(&case, fold, same);
                for ((path, _), (o, t)) in case.entries.iter().zip(ours.iter().zip(&theirs)) {
                    assert_eq!(o, t, "rules {text:?}, path {path:?}, folding {fold}");
                }
            }
        }
    }
}

#[test]
fn combining_mark_boundaries_agree_with_git() {
    let names = [
        "q",
        "q\u{301}",
        "q\u{301}.txt",
        "q\u{301}\u{302}",
        "aq\u{301}",
        "\u{301}",
        "xq\u{301}",
        "sub/q\u{301}",
    ];
    let mut patterns: Vec<String> = [
        "?",
        "??",
        "?\u{301}",
        "[q]\u{301}",
        "[!x]\u{301}",
        "[q]*",
        "[a-z]*",
        "[q]\u{301}*",
        "q?",
        "[q\u{301}]",
        "*\u{301}",
        "q*",
    ]
    .map(str::to_owned)
    .to_vec();
    let tokens = ["q", "?", "*", "[q]", "[!x]", "\u{301}"];
    for a in tokens {
        for b in tokens {
            patterns.push(format!("{a}{b}"));
        }
    }
    let mut entries = BTreeMap::new();
    for name in names {
        let parts: Vec<String> = name.split('/').map(str::to_owned).collect();
        if parts.len() > 1 {
            entries.insert(parts[..1].to_vec(), Kind::Dir);
        }
        entries.insert(parts, Kind::File);
    }
    let reviewed = ["?\u{301}", "[q]\u{301}", "[q]*"];
    let mut compared = 0;
    let mut byte_differences = 0;
    for pattern in &patterns {
        let case = Case {
            entries: entries.clone(),
            files: BTreeMap::from([(Vec::new(), format!("{pattern}\n"))]),
        };
        let exact = git(&case, false, code_points);
        let folded = git(&case, true, code_points);
        let utf8 = git(&case, false, same);
        let ours = [
            ("NFC", ours::<Exact>(&case, same), &exact),
            ("NFD", ours::<Exact>(&case, nfd), &exact),
            ("folded NFC", ours::<Folded>(&case, same), &folded),
            ("folded NFD", ours::<Folded>(&case, nfd), &folded),
        ];
        for (i, (path, kind)) in case.entries.iter().enumerate() {
            if *kind == Kind::Dir {
                continue;
            }
            compared += 1;
            for (spelling, ours, theirs) in &ours {
                assert_eq!(
                    ours[i], theirs[i],
                    "{spelling} {pattern:?} against {path:?}"
                );
            }
            if ours[0].1[i] == utf8[i] {
                continue;
            }
            byte_differences += 1;
            assert!(
                !reviewed.contains(&pattern.as_str())
                    && (pattern.contains('?') || pattern.contains('['))
                    && !path.concat().is_ascii(),
                "{pattern:?} against {path:?}: git on UTF-8 says {:?}",
                utf8[i]
            );
        }
    }
    assert_eq!(compared, 384);
    assert_eq!(byte_differences, 27);
}
