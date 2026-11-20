// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

use koidrive_engine::ignore::Key;
use koidrive_fs::GitIndex;
use sha1::{Digest, Sha1};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .env_clear()
        .envs(std::env::vars_os().filter(|(name, _)| !name.to_string_lossy().starts_with("GIT_")))
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

fn answers(index: &GitIndex<Key>, paths: &str) -> String {
    let answer = |path| match index.lists(Path::new(path)) {
        Some(true) => 'y',
        Some(false) => 'n',
        None => '?',
    };
    paths.split(' ').map(answer).collect()
}

fn holds(path: &Path, signature: &[u8]) -> bool {
    fs::read(path).unwrap().windows(4).any(|w| w == signature)
}

fn replace(path: &Path, bytes: &[u8]) {
    let next = path.with_extension("next");
    fs::write(&next, bytes).unwrap();
    fs::rename(next, path).unwrap();
}

fn index(version: u32, entries: &[(&[u8], u16)], extensions: &[u8]) -> Vec<u8> {
    let count = u32::try_from(entries.len()).unwrap().to_be_bytes();
    let mut bytes = [&b"DIRC"[..], &version.to_be_bytes(), &count].concat();
    for (name, flags) in entries {
        let start = bytes.len();
        bytes.extend([0; 24]);
        bytes.extend(0o100644_u32.to_be_bytes());
        bytes.extend([0; 32]);
        bytes.extend((flags | u16::try_from(name.len()).unwrap()).to_be_bytes());
        bytes.extend(if flags & 0x4000 != 0 {
            &[0; 2][..]
        } else {
            &[]
        });
        bytes.extend(if version == 4 { &[0][..] } else { &[] });
        bytes.extend([*name, &[0]].concat());
        if version < 4 {
            bytes.resize(start + (bytes.len() - start).next_multiple_of(8), 0);
        }
    }
    bytes.extend(extensions);
    let checksum = Sha1::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    bytes
}

fn with_extension(index: &[u8], signature: &[u8; 4]) -> Vec<u8> {
    let mut bytes = index[..index.len() - 20].to_vec();
    bytes.extend([&signature[..], &[0; 4]].concat());
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
    git(root, &["update-index", "--chmod=+x", "--add", "bin/run"]);
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
    let paths = "a.txt A.TXT bin/run dist/app.js DIST/App.JS dist/other.js e\u{301}.txt É.TXT link new.txt untracked.txt bin";
    assert_eq!(answers(&index, paths), "yyyyynyynynn");
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
    let stages = String::from_utf8(run(root, &["ls-files", "--stage"]).stdout).unwrap();
    assert!(stages.lines().all(|line| !line.contains(" 0\t")));
    assert_eq!(answers(&read(root), "f.txt g.txt"), "yn");
}

#[test]
fn reads_version_4_prefix_compressed_paths() {
    let dir = repo(&["dir/aaa.txt", "dir/aab.txt", "dir/sub/x", "other"]);
    let root = dir.path();
    git(root, &["add", "."]);
    let long = format!("{}/", "x".repeat(200)).repeat(10);
    let blob = "100644,e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    let infos: Vec<String> = (0..200).map(|n| format!("{blob},{long}{n}")).collect();
    let mut args = vec!["update-index", "--add"];
    args.extend(infos.iter().flat_map(|info| ["--cacheinfo", info]));
    git(root, &args);
    git(root, &["update-index", "--index-version", "4"]);
    let mut bytes = fs::read(root.join(".git/index")).unwrap();
    assert_eq!(&bytes[4..8], &[0, 0, 0, 4]);
    let end = bytes.len() - 20;
    bytes[end..].fill(0);
    fs::write(root.join(".git/index"), bytes).unwrap();
    let paths = format!("dir/aaa.txt dir/aab.txt dir/sub/x other {long}199 dir/aac.txt dir/aa");
    assert_eq!(answers(&read(root), &paths), "yyyyynn");
}

#[test]
fn reads_sha256_indexes_with_the_object_format_of_the_common_dir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "--object-format=sha256"]);
    write(root, &["a.txt", "b.txt"]);
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "one"]);
    git(root, &["worktree", "add", "-q", "-b", "side", "wt"]);
    assert_eq!(answers(&read(root), "a.txt b.txt"), "yn");
    assert_eq!(answers(&read(&root.join("wt")), "a.txt b.txt"), "yn");
    git(root, &["config", "index.skipHash", "true"]);
    git(root, &["add", "b.txt"]);
    assert_eq!(answers(&read(root), "a.txt b.txt"), "yy");
    git(root, &["config", "extensions.objectFormat", "sha512"]);
    assert_eq!(answers(&read(root), "a.txt"), "?");
    let config = root.join(".git/config");
    let text = fs::read_to_string(&config).unwrap();
    let text = text.replace("[extensions]", "[x]\n\ty = a\\\\\n[extensions]\n# c \\");
    fs::write(&config, text.replace("sha512", "sha\"\\\n25\"6")).unwrap();
    assert_eq!(answers(&read(root), "a.txt"), "y");
    fs::write(&config, "\u{feff}[extensions]\n\tobjectformat = sha256\n").unwrap();
    assert_eq!(answers(&read(root), "a.txt"), "y");
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
    assert_eq!(answers(&read(root), "f00 f01 f02 f29 new"), "yynyy");
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
    assert_eq!(answers(&index, "f00"), "?");
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
    assert_eq!(
        answers(&read(root), "a/x top a/y b/y B/Y b/z/w b"),
        "yyn???n"
    );
}

#[test]
fn every_dot_git_entry_makes_a_work_tree_with_its_own_index() {
    let source = repo(&["s.txt"]);
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "-qm", "sub"]);
    let dir = repo(&["m.txt", "odd/x"]);
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
    std::os::unix::fs::symlink(root.join(".git"), root.join("odd/.git")).unwrap();
    assert!(GitIndex::<Key>::find(&root.join(".git")).is_none());
    let indexes: BTreeMap<PathBuf, GitIndex<Key>> = ["", "wt", "sub", "odd"]
        .into_iter()
        .map(|folder| (PathBuf::from(folder), read(&root.join(folder))))
        .collect();
    let answer = |(tree, path)| answers(&indexes[Path::new(tree)], path);
    let paths = [
        ("", "m.txt"),
        ("wt", "side.txt"),
        ("wt", "m.txt"),
        ("sub", "s.txt"),
        ("sub", "m.txt"),
        ("", "side.txt"),
        ("odd", "x"),
    ];
    assert_eq!(paths.map(answer).concat(), "yyyynn?");
    fs::copy(root.join(".git/index"), root.join("odd/index")).unwrap();
    for gitfile in ["nonsense", "gitdir: /nowhere", "gitdir: "] {
        fs::remove_file(root.join("odd/.git")).unwrap();
        fs::write(root.join("odd/.git"), gitfile).unwrap();
        assert_eq!(answers(&read(&root.join("odd")), "x"), "?", "{gitfile}");
    }
}

#[test]
fn an_unreadable_index_keeps_the_last_answers() {
    let dir = repo(&["a.txt"]);
    let root = dir.path();
    git(root, &["add", "."]);
    let path = root.join(".git/index");
    let good = fs::read(&path).unwrap();
    let mut index = read(root);
    let mut corrupt = good.clone();
    corrupt[40] ^= 1;
    let unknown = with_extension(&good, b"abcd");
    let lock = root.join(".git/index.lock");
    let check = |index: &mut GitIndex<Key>, case: &str| {
        assert!(!index.refresh() && !index.readable(), "{case}");
        assert_eq!(answers(index, "a.txt b.txt"), "yn", "{case}");
        assert_eq!(answers(&read(root), "a.txt"), "?", "{case}");
        let _ = fs::remove_file(&lock);
        replace(&path, &good);
        assert!(!index.refresh() && index.readable(), "{case}");
    };
    replace(&path, &corrupt);
    check(&mut index, "corrupt");
    replace(&path, &unknown);
    check(&mut index, "required extension");
    fs::remove_file(&path).unwrap();
    check(&mut index, "missing");
    fs::write(&lock, "").unwrap();
    check(&mut index, "being replaced");
    replace(&path, &with_extension(&good, b"ABCD"));
    assert!(!index.refresh() && index.readable());
}

#[test]
fn a_name_git_refuses_is_not_listed() {
    let dir = repo(&[]);
    let names = b"../x /abs a//b a/./b dir/ dist/../../x .git/config a/.GIT/x b\xff o\xef\xbf\xbd";
    let entries: Vec<(&[u8], u16)> = names
        .split(|byte| *byte == b' ')
        .map(|name| (name, 0))
        .collect();
    fs::write(dir.path().join(".git/index"), index(2, &entries, &[])).unwrap();
    let index = read(dir.path());
    assert_eq!(index.listed().collect::<Vec<_>>(), ["o\u{fffd}"]);
    let lossy = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"o\xff");
    assert_eq!(index.lists(Path::new(lossy)), Some(false));
}

#[test]
fn an_index_git_refuses_to_read_is_unreadable() {
    let dir = repo(&[]);
    let root = dir.path();
    let path = root.join(".git/index");
    fs::write(&path, index(3, &[(b"a", 0x4000)], &[])).unwrap();
    assert_eq!(answers(&read(root), "a"), "y");
    fs::write(&path, index(2, &[(b"a", 0x4000)], &[])).unwrap();
    assert!(!read(root).readable(), "extended flags in version 2");
    fs::write(&path, index(4, &vec![(&b"a"[..], 0xfff); 4097], &[])).unwrap();
    assert!(!read(root).readable(), "a path longer than 4096 bytes");
    let shared = index(2, &[(b"a", 0)], &[]);
    let base = &shared[shared.len() - 20..];
    let hex: String = base.iter().map(|byte| format!("{byte:02x}")).collect();
    fs::write(root.join(format!(".git/sharedindex.{hex}")), &shared).unwrap();
    let none = [0; 12];
    let first = [
        &[0, 0, 0, 1, 0, 0, 0, 2][..],
        &(1u64 << 33).to_be_bytes(),
        &1u64.to_be_bytes(),
        &[0; 4],
    ]
    .concat();
    for (deleted, case) in [(&none[..], "y"), (&first, "unreadable")] {
        let link = [base, deleted, &first].concat();
        let size = u32::try_from(link.len()).unwrap().to_be_bytes();
        fs::write(
            &path,
            index(2, &[(b"", 0)], &[&b"link"[..], &size, &link].concat()),
        )
        .unwrap();
        let index = read(root);
        let answer = if index.readable() {
            answers(&index, "a")
        } else {
            "unreadable".into()
        };
        assert_eq!(answer, case);
    }
}

#[test]
fn reads_only_regular_git_files_of_a_bounded_size() {
    let names = ["index", "sharedindex", "commondir", "config"];
    let cases = names.map(|name| ["fifo", "link", "large"].map(|kind| (name, kind)));
    for (name, kind) in cases
        .into_iter()
        .flatten()
        .filter(|case| *case != ("sharedindex", "large"))
    {
        let case = format!("{name} {kind}");
        let dir = repo(&["a.txt"]);
        let root = dir.path().to_path_buf();
        git(&root, &["add", "."]);
        git(&root, &["update-index", "--split-index"]);
        fs::write(root.join(".git/commondir"), ".\n").unwrap();
        assert_eq!(answers(&read(&root), "a.txt"), "y", "{case}");
        let shared = |entry: &fs::DirEntry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("sharedindex.")
        };
        let path = match name {
            "sharedindex" => fs::read_dir(root.join(".git"))
                .unwrap()
                .map(Result::unwrap)
                .find(shared)
                .unwrap()
                .path(),
            _ => root.join(".git").join(name),
        };
        let moved = root.join("moved");
        fs::rename(&path, &moved).unwrap();
        let mut bytes = fs::read(&moved).unwrap();
        match (kind, name) {
            ("fifo", _) => assert!(
                Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            ),
            ("link", _) => std::os::unix::fs::symlink(&moved, &path).unwrap(),
            (_, "index") => {
                bytes.truncate(bytes.len() - 20);
                bytes.extend([&b"ZZZZ"[..], &(1u32 << 30).to_be_bytes()].concat());
                fs::write(&path, &bytes).unwrap();
                let file = fs::File::options().write(true).open(&path).unwrap();
                file.set_len(bytes.len() as u64 + (1 << 30) + 20).unwrap();
            }
            _ => {
                bytes.resize((1 << 20) + 1, b'\n');
                fs::write(&path, bytes).unwrap();
            }
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || sender.send(read(&root).readable()));
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok(false),
            "{case}"
        );
    }
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
    assert_eq!(answers(&index, "a.txt b.txt"), "yn");
    assert!(!index.refresh());
    let before = fs::metadata(&path).unwrap();
    let file = fs::File::options().write(true).open(&replacement).unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    assert!(index.refresh());
    assert_eq!(answers(&index, "a.txt b.txt"), "ny");
}
