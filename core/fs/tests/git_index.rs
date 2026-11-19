// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use koidrive_engine::ignore::Key;
use koidrive_fs::{GitIndex, owner};
use sha1::{Digest, Sha1};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=koi", "-c", "user.email=koi@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .output()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = run(dir, args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "git {args:?}: {stderr}");
}

fn write(dir: &Path, files: &[&str]) {
    for file in files {
        let path = dir.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, file).unwrap();
    }
}

fn repo(files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    write(dir.path(), files);
    dir
}

fn read(folder: &Path) -> GitIndex<Key> {
    let mut index = GitIndex::find(folder).expect("a work tree");
    index.refresh();
    index
}

fn lists(index: &GitIndex<Key>, paths: &[&str]) -> Vec<Option<bool>> {
    (paths.iter())
        .map(|path| index.lists(Path::new(path)))
        .collect()
}

fn holds(path: &Path, signature: &[u8]) -> bool {
    fs::read(path).unwrap().windows(4).any(|w| w == signature)
}

fn with_extension(index: &[u8], signature: &[u8; 4]) -> Vec<u8> {
    let mut bytes = index[..index.len() - 20].to_vec();
    bytes.extend_from_slice(signature);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    let checksum = Sha1::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    bytes
}

#[test]
fn lists_regular_files_by_name_key_and_skips_optional_extensions() {
    let dir = repo(&[
        ".gitignore",
        "a.txt",
        "bin/run",
        "dist/app.js",
        "dist/other.js",
        "é.txt",
    ]);
    let root = dir.path();
    fs::write(root.join(".gitignore"), "dist/\n").unwrap();
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    fs::set_permissions(
        root.join("bin/run"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    git(root, &["config", "index.recordEndOfIndexEntries", "true"]);
    git(root, &["config", "core.untrackedCache", "true"]);
    git(root, &["add", "."]);
    git(root, &["add", "-f", "dist/app.js"]);
    git(root, &["commit", "-qm", "one"]);
    write(root, &["new.txt", "untracked.txt"]);
    git(root, &["add", "-N", "new.txt"]);
    git(root, &["status", "--porcelain"]);
    assert!(holds(&root.join(".git/index"), b"EOIE"));
    let index = read(root);
    assert!(index.readable());
    let paths = [
        "a.txt",
        "A.TXT",
        "bin/run",
        "dist/app.js",
        "DIST/App.JS",
        "dist/other.js",
    ];
    assert_eq!(
        lists(&index, &paths),
        [
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(false)
        ]
    );
    let paths = [
        "e\u{301}.txt",
        "É.TXT",
        "link",
        "new.txt",
        "untracked.txt",
        "bin",
    ];
    assert_eq!(
        lists(&index, &paths),
        [
            Some(true),
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(false)
        ]
    );
}

#[test]
fn a_conflicted_file_stays_listed() {
    let dir = repo(&["f.txt"]);
    let root = dir.path();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "side"]);
    fs::write(root.join("f.txt"), "side").unwrap();
    git(root, &["commit", "-qam", "side"]);
    git(root, &["switch", "-q", "main"]);
    fs::write(root.join("f.txt"), "main").unwrap();
    git(root, &["commit", "-qam", "main"]);
    assert!(!run(root, &["merge", "side"]).status.success());
    let stages = run(root, &["ls-files", "--stage"]).stdout;
    assert!(
        String::from_utf8(stages)
            .unwrap()
            .lines()
            .all(|line| !line.contains(" 0\t"))
    );
    assert_eq!(
        lists(&read(root), &["f.txt", "g.txt"]),
        [Some(true), Some(false)]
    );
}

#[test]
fn reads_version_4_prefix_compressed_paths() {
    let dir = repo(&["dir/aaa.txt", "dir/aab.txt", "dir/sub/x", "other"]);
    let root = dir.path();
    git(root, &["add", "."]);
    git(root, &["config", "index.skipHash", "true"]);
    git(root, &["update-index", "--index-version", "4"]);
    let bytes = fs::read(root.join(".git/index")).unwrap();
    assert_eq!(bytes[4..8], [0, 0, 0, 4]);
    assert_eq!(bytes[bytes.len() - 20..], [0; 20]);
    let paths = [
        "dir/aaa.txt",
        "dir/aab.txt",
        "dir/sub/x",
        "other",
        "dir/aac.txt",
        "dir/aa",
    ];
    assert_eq!(
        lists(&read(root), &paths),
        [
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(false)
        ]
    );
}

#[test]
fn reads_a_split_index_and_its_shared_index() {
    let names: Vec<String> = (0..30).map(|n| format!("f{n:02}")).collect();
    let dir = repo(&names.iter().map(String::as_str).collect::<Vec<_>>());
    let root = dir.path();
    git(root, &["config", "splitIndex.maxPercentChange", "100"]);
    git(root, &["add", "."]);
    git(root, &["update-index", "--split-index"]);
    fs::write(root.join("f01"), "changed").unwrap();
    write(root, &["new"]);
    git(root, &["add", "f01", "new"]);
    git(root, &["rm", "-q", "--cached", "f02"]);
    assert!(holds(&root.join(".git/index"), b"link"));
    let paths = ["f00", "f01", "f02", "f29", "new"];
    assert_eq!(
        lists(&read(root), &paths),
        [Some(true), Some(true), Some(false), Some(true), Some(true)]
    );
    for entry in fs::read_dir(root.join(".git")).unwrap() {
        let entry = entry.unwrap();
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("sharedindex.")
        {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let index = read(root);
    assert!(!index.readable());
    assert_eq!(lists(&index, &["f00"]), [None]);
}

#[test]
fn a_path_below_a_sparse_directory_has_no_answer() {
    let dir = repo(&["a/x", "b/y", "b/z/w", "top"]);
    let root = dir.path();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "one"]);
    git(
        root,
        &["sparse-checkout", "set", "--cone", "--sparse-index", "a"],
    );
    assert!(holds(&root.join(".git/index"), b"sdir"));
    let paths = ["a/x", "top", "a/y", "b/y", "B/Y", "b/z/w", "b"];
    assert_eq!(
        lists(&read(root), &paths),
        [
            Some(true),
            Some(true),
            Some(false),
            None,
            None,
            None,
            Some(false)
        ]
    );
}

#[test]
fn linked_worktrees_and_submodules_answer_for_their_own_paths() {
    let source = repo(&["s.txt"]);
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "-qm", "sub"]);
    let dir = repo(&["m.txt"]);
    let root = dir.path();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "main"]);
    git(root, &["worktree", "add", "-q", "-b", "side", "wt"]);
    write(&root.join("wt"), &["side.txt"]);
    git(&root.join("wt"), &["add", "side.txt"]);
    git(
        root,
        &[
            "submodule",
            "add",
            "-q",
            source.path().to_str().unwrap(),
            "sub",
        ],
    );
    assert!(root.join("sub/.git").is_file() && root.join("wt/.git").is_file());
    assert!(GitIndex::<Key>::find(&root.join(".git")).is_none());
    let indexes: BTreeMap<PathBuf, GitIndex<Key>> = ["", "wt", "sub"]
        .into_iter()
        .map(|folder| (PathBuf::from(folder), read(&root.join(folder))))
        .collect();
    let answer = |path: &str| owner(&indexes, Path::new(path)).map(|(index, rel)| index.lists(rel));
    assert_eq!(answer("m.txt"), Some(Some(true)));
    assert_eq!(answer("wt/side.txt"), Some(Some(true)));
    assert_eq!(answer("wt/m.txt"), Some(Some(true)));
    assert_eq!(answer("sub/s.txt"), Some(Some(true)));
    assert_eq!(answer("sub/m.txt"), Some(Some(false)));
    assert_eq!(answer("side.txt"), Some(Some(false)));
}

#[test]
fn an_unreadable_index_keeps_the_last_answers() {
    let dir = repo(&["a.txt"]);
    let root = dir.path();
    git(root, &["add", "."]);
    let path = root.join(".git/index");
    let good = fs::read(&path).unwrap();
    let mut index = read(root);
    assert_eq!(lists(&index, &["a.txt"]), [Some(true)]);

    let mut corrupt = good.clone();
    corrupt[40] ^= 1;
    let unknown = with_extension(&good, b"abcd");
    let lock = root.join(".git/index.lock");
    let check = |index: &mut GitIndex<Key>, case: &str| {
        assert!(!index.refresh(), "{case}");
        assert!(!index.readable(), "{case}");
        assert_eq!(
            lists(index, &["a.txt", "b.txt"]),
            [Some(true), Some(false)],
            "{case}"
        );
        assert!(read(root).lists(Path::new("a.txt")).is_none(), "{case}");
        let _ = fs::remove_file(&lock);
        fs::write(&path, &good).unwrap();
        assert!(!index.refresh(), "{case}");
        assert!(index.readable(), "{case}");
    };
    fs::write(&path, &corrupt).unwrap();
    check(&mut index, "corrupt");
    fs::write(&path, &unknown).unwrap();
    check(&mut index, "required extension");
    fs::remove_file(&path).unwrap();
    check(&mut index, "missing");
    fs::write(&lock, "").unwrap();
    check(&mut index, "being replaced");
    fs::write(&path, with_extension(&good, b"ABCD")).unwrap();
    assert!(!index.refresh());
    assert!(index.readable());
}

#[test]
fn an_index_replaced_with_the_same_size_and_time_is_read_again() {
    let dir = repo(&["a.txt", "b.txt"]);
    let root = dir.path();
    let path = root.join(".git/index");
    git(root, &["add", "b.txt"]);
    let replacement = root.join("replacement");
    fs::copy(&path, &replacement).unwrap();
    git(root, &["rm", "-q", "--cached", "b.txt"]);
    git(root, &["add", "a.txt"]);
    let mut index = read(root);
    assert_eq!(
        lists(&index, &["a.txt", "b.txt"]),
        [Some(true), Some(false)]
    );
    assert!(!index.refresh());

    let before = fs::metadata(&path).unwrap();
    let file = fs::File::options().write(true).open(&replacement).unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(
        (after.len(), after.modified().unwrap()),
        (before.len(), before.modified().unwrap())
    );
    assert!(index.refresh());
    assert_eq!(
        lists(&index, &["a.txt", "b.txt"]),
        [Some(false), Some(true)]
    );
}
