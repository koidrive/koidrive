// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use koidrive_fs::{Change, EntryKind, Ignore, Ignored, RULES_FILE, Rules, Snapshot};
use koidrive_ignore::{Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Status};

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        koidrive_engine::names::name_key(name)
    }
}

struct Matched(Matcher<Folded>);

fn names(path: &Path) -> Vec<String> {
    path.iter().map(|n| n.to_string_lossy().into_owned()).collect()
}

impl Ignore for Matched {
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>) {
        self.0.set_rules(&names(dir), rules.map(RuleFile::parse), []);
    }
    fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        let kind = match kind {
            EntryKind::File => Kind::File,
            EntryKind::Dir => Kind::Dir,
        };
        match self.0.status(&names(path), kind) {
            Status::Included => Ignored::No,
            Status::Excluded(Reason::BuiltIn) => Ignored::BuiltIn,
            Status::Excluded(_) => Ignored::Rule,
        }
    }
    fn key(&self, name: &OsStr) -> OsString {
        Folded::key(&name.to_string_lossy()).into()
    }
}

fn rules() -> Rules {
    Rules::new(Box::new(Matched(Matcher::new(LocalRules::default()))))
}

fn write(root: &Path, path: &str, body: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn scratch() -> tempfile::TempDir {
    let base = std::env::var("ATTACK_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}

struct Outcome {
    changes: Vec<Change>,
    rescans: usize,
    mutated: bool,
}

fn chain(depth: usize) -> String {
    (0..depth).map(|i| format!("c{i}")).collect::<Vec<_>>().join("/")
}

fn about_f(change: &Change) -> bool {
    match change {
        Change::Removed { path, .. } => path == Path::new("f.txt"),
        Change::Ignored { from, .. } => from == Path::new("f.txt"),
        _ => false,
    }
}

fn drive(setup: &dyn Fn(&Path), to: &str, budget: usize, between: &dyn Fn(&Path, usize), whole: bool, limit: usize) -> Option<(Change, usize)> {
    let dir = scratch();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    write(root, "f.txt", "f");
    setup(root);
    let mut rules = rules().with_search_budget(budget);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    match to {
        "" => fs::remove_file(root.join("f.txt")).unwrap(),
        to => fs::rename(root.join("f.txt"), root.join(to)).unwrap(),
    }
    let mut paths = vec![PathBuf::new()];
    for rescans in 1..=limit {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        if let Some(c) = rescan.changes.iter().find(|c| about_f(c)) {
            return Some((c.clone(), rescans));
        }
        between(root, rescans);
        paths = if whole { vec![PathBuf::new()] } else { rescan.unsettled };
    }
    None
}

fn sweep(name: &str, setup: &dyn Fn(&Path), first: &str, last: &str, budgets: std::ops::RangeInclusive<usize>, between: &dyn Fn(&Path), whole: bool) {
    let mut bad = Vec::new();
    let mut total = 0;
    for budget in budgets {
        let hit = std::cell::Cell::new(false);
        let r = drive(setup, first, budget, &|root, n| if n == 1 { between(root); hit.set(true) }, whole, 5000);
        if !hit.get() { continue; }
        total += 1;
        let ok = matches!(&r, Some((Change::Ignored { to, .. }, _)) if to == Path::new(last));
        if !ok { bad.push(format!("budget {budget}: {r:?}")); }
    }
    eprintln!("{name}: {total} budgets mutated mid-walk, {} wrong", bad.len());
    for b in bad.iter().take(6) { eprintln!("  {b}"); }
    assert!(bad.is_empty(), "{name}: {} wrong", bad.len());
}

#[test]
fn attack2_scanned_folder_above_the_ignored_folder_replaced() {
    sweep(
        "scanned folder above replaced",
        &|root| {
            write(root, "proj/keep.txt", "k");
            for i in 0..5 { write(root, &format!("proj/ig/p/n{i}"), "x"); }
        },
        "proj/ig/p/f.txt",
        "proj/ig/p/f.txt",
        1..=30,
        &|root| {
            fs::rename(root.join("proj"), root.join("old")).unwrap();
            fs::create_dir_all(root.join("proj/ig/p")).unwrap();
            fs::rename(root.join("old/ig/p/f.txt"), root.join("proj/ig/p/f.txt")).unwrap();
        },
        true,
    );
}

#[test]
fn attack2_ignored_ancestor_above_a_reached_folder_replaced() {
    let at = format!("ig/p/{}", chain(22));
    let at2 = at.clone();
    sweep(
        "ancestor above a reached folder replaced",
        &move |root| {
            write(root, &format!("{at}/{}/leaf", chain(10)), "x");
            for i in 0..5 { write(root, &format!("{at}/n{i}"), "x"); }
        },
        &format!("{at2}/f.txt"),
        &format!("{at2}/f.txt"),
        1..=60,
        &|root| {
            let at = format!("ig/p/{}", chain(22));
            fs::rename(root.join("ig/p"), root.join("ig/old")).unwrap();
            fs::create_dir_all(root.join(&at)).unwrap();
            fs::rename(root.join(format!("ig/old/{}/f.txt", chain(22))), root.join(format!("{at}/f.txt"))).unwrap();
        },
        false,
    );
}

#[test]
fn attack2_churning_ignored_folder_still_settles() {
    let mut stuck = Vec::new();
    for budget in [3, 10, 30] {
        let r = drive(
            &|root| { write(root, &format!("ig/a/{}/leaf", chain(20)), "x"); for i in 0..50 { write(root, &format!("ig/z{i}"), "x"); } },
            "",
            budget,
            &|root, _| {
                fs::remove_dir_all(root.join("ig/a")).unwrap();
                write(root, &format!("ig/a/{}/leaf", chain(20)), "x");
            },
            false,
            3000,
        );
        eprintln!("churn budget {budget}: {r:?}");
        if r.is_none() { stuck.push(budget); }
    }
    assert!(stuck.is_empty(), "never settled at budgets {stuck:?}");
}
