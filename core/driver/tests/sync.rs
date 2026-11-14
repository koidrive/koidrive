// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use koidrive_chunk::ChunkerConfig;
use koidrive_driver::{Driver, FolderState, FolderStatus, Paths};
use koidrive_net::{Api, Auth, Credentials, Event, Session, SessionConfig};
use koidrive_proto::v1::{ClaimRequest, CreateSpaceRequest, SpaceInfo};
use tokio::sync::OnceCell;
use url::Url;

const WAIT: Duration = Duration::from_secs(30);

fn server_url() -> Url {
    let url = std::env::var("KOIDRIVE_TEST_SERVER").expect(
        "KOIDRIVE_TEST_SERVER is not set; run core/driver/scripts/integration-test.sh, which starts the Worker",
    );
    url.parse().expect("KOIDRIVE_TEST_SERVER is a URL")
}

fn api() -> Api {
    Api::local_development(server_url()).unwrap()
}

static OWNER: OnceCell<Credentials> = OnceCell::const_new();

async fn owner() -> Arc<Auth> {
    let credentials = OWNER.get_or_init(claim).await.clone();
    Arc::new(Auth::new(api(), credentials))
}

async fn claim() -> Credentials {
    let log =
        std::env::var("KOIDRIVE_TEST_SERVER_LOG").expect("KOIDRIVE_TEST_SERVER_LOG is not set");
    let setup = reqwest::get(server_url().join("setup").unwrap())
        .await
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
    let started = Instant::now();
    let code = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(claim_code) {
            break code;
        }
        assert!(started.elapsed() < WAIT, "no claim code in {log}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let request = ClaimRequest {
        code,
        user_name: "Owner".into(),
        device_name: "owner".into(),
        platform: "test".into(),
        setup_attempt: String::new(),
        request_id: String::new(),
    };
    Credentials::from(&api().claim(&request).await.expect("the claim succeeds"))
}

fn claim_code(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claim code") {
        return None;
    }
    line.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .rfind(|word| {
            word.contains('-')
                && word.len() >= 8
                && word.split('-').all(|group| {
                    !group.is_empty() && group.chars().all(|c| c.is_ascii_alphanumeric())
                })
        })
        .map(str::to_owned)
}

fn log() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "koidrive_driver=debug,koidrive_net=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

async fn new_space(owner: &Arc<Auth>, name: &str) -> SpaceInfo {
    let request = CreateSpaceRequest {
        name: name.into(),
        request_id: String::new(),
    };
    owner.api().create_space(owner, &request).await.unwrap()
}

struct Device {
    home: tempfile::TempDir,
    driver: Option<Driver>,
}

impl Device {
    async fn linked(owner: &Arc<Auth>, space: &SpaceInfo, name: &str) -> Device {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("folder")).unwrap();
        let mut device = Device { home, driver: None };
        let driver = device.open();
        let pairing = owner.api().pair_start(owner).await.unwrap();
        driver
            .pair(server_url().as_str(), &pairing.code, name)
            .await
            .unwrap();
        driver
            .link(&device.folder(), &space.space_id)
            .await
            .unwrap();
        driver.start();
        device.driver = Some(driver);
        device
    }

    fn open(&self) -> Driver {
        Driver::open(Paths::under(&self.home.path().join("profile"))).unwrap()
    }

    fn folder(&self) -> PathBuf {
        self.home.path().join("folder")
    }

    fn status(&self) -> FolderStatus {
        let driver = self.driver.as_ref().expect("the device is running");
        driver.status().folders.remove(0)
    }

    async fn stop(&mut self) {
        let driver = self.driver.take().expect("the device is running");
        driver.stop().await;
    }

    fn start(&mut self) {
        assert!(self.driver.is_none(), "the device is stopped");
        let driver = self.open();
        driver.start();
        self.driver = Some(driver);
    }

    async fn restart(&mut self) {
        self.stop().await;
        self.start();
    }
}

#[derive(PartialEq, Eq)]
enum Entry {
    Folder,
    File { executable: bool, bytes: Vec<u8> },
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Entry::Folder => write!(f, "folder"),
            Entry::File { executable, bytes } => {
                let executable = if *executable { ", executable" } else { "" };
                write!(f, "file of {} bytes{executable}", bytes.len())
            }
        }
    }
}

type Tree = BTreeMap<PathBuf, Entry>;

fn tree(root: &Path) -> io::Result<Tree> {
    fn walk(root: &Path, dir: &Path, tree: &mut Tree) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if relative == Path::new(".koidrive") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                tree.insert(relative, Entry::Folder);
                walk(root, &path, tree)?;
            } else {
                let executable = metadata.permissions().mode() & 0o111 != 0;
                let bytes = fs::read(&path)?;
                tree.insert(relative, Entry::File { executable, bytes });
            }
        }
        Ok(())
    }
    let mut tree = Tree::new();
    walk(root, root, &mut tree)?;
    Ok(tree)
}

async fn converged(a: &Device, b: &Device) -> (Tree, u64) {
    let started = Instant::now();
    loop {
        let (in_a, in_b) = (tree(&a.folder()), tree(&b.folder()));
        let (status_a, status_b) = (a.status(), b.status());
        let done = |status: &FolderStatus| {
            status.state == FolderState::UpToDate && status.last_seq == status.head_seq
        };
        let same = matches!((&in_a, &in_b), (Ok(in_a), Ok(in_b)) if in_a == in_b);
        if same && done(&status_a) && done(&status_b) && status_a.last_seq == status_b.last_seq {
            return (in_a.unwrap(), status_a.last_seq.unwrap_or(0));
        }
        assert!(
            started.elapsed() < WAIT,
            "the devices did not converge within {WAIT:?}\nA: {status_a:#?}\n{in_a:#?}\nB: {status_b:#?}\n{in_b:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn generated(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut bytes = Vec::with_capacity(len + 8);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

fn file(bytes: &[u8]) -> Entry {
    Entry::File {
        executable: false,
        bytes: bytes.to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_file_created_on_one_device_appears_on_the_other() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "create").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;

    fs::write(a.folder().join("notes.txt"), b"written on a\n").unwrap();
    let (synced, _) = converged(&a, &b).await;
    let expected = Tree::from([("notes.txt".into(), file(b"written on a\n"))]);
    assert_eq!(synced, expected);

    fs::create_dir(b.folder().join("replies")).unwrap();
    fs::write(b.folder().join("replies/reply.txt"), b"written on b\n").unwrap();
    fs::write(b.folder().join("replies/empty"), b"").unwrap();
    let script = b.folder().join("run.sh");
    fs::write(&script, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let (synced, seq) = converged(&a, &b).await;
    let expected = Tree::from([
        ("notes.txt".into(), file(b"written on a\n")),
        ("replies".into(), Entry::Folder),
        ("replies/empty".into(), file(b"")),
        ("replies/reply.txt".into(), file(b"written on b\n")),
        (
            "run.sh".into(),
            Entry::File {
                executable: true,
                bytes: b"#!/bin/sh\n".to_vec(),
            },
        ),
    ]);
    assert_eq!(synced, expected);

    a.restart().await;
    b.restart().await;
    converged(&a, &b).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (after, seq_after) = converged(&a, &b).await;
    assert_eq!(
        (after, seq_after),
        (expected, seq),
        "a restart changes neither the folders nor the space"
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_file_of_several_chunks_arrives_with_the_same_bytes() {
    log();
    let bytes = generated(8 << 20);
    let chunks = koidrive_chunk::chunks(bytes.as_slice(), ChunkerConfig::V0).count();
    assert!(chunks > 1, "the file is one chunk");
    let owner = owner().await;
    let space = new_space(&owner, "several chunks").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;

    let before = koidrive_driver::volume();
    fs::write(a.folder().join("large.bin"), &bytes).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced, Tree::from([("large.bin".into(), file(&bytes))]));
    let after = koidrive_driver::volume();
    let size = bytes.len() as u64;
    assert!(
        after.bytes_uploaded - before.bytes_uploaded >= size,
        "{before:?} {after:?}"
    );
    assert!(
        after.bytes_downloaded - before.bytes_downloaded >= size,
        "{before:?} {after:?}"
    );
    assert!(
        after.commits_sent > before.commits_sent,
        "{before:?} {after:?}"
    );
    assert!(
        after.commits_received > before.commits_received,
        "{before:?} {after:?}"
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn an_edit_replaces_the_file_on_the_other_device() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "edit").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::write(a.folder().join("notes.txt"), b"first\n").unwrap();
    converged(&a, &b).await;

    fs::write(b.folder().join("notes.txt"), b"written in place on b\n").unwrap();
    let (synced, _) = converged(&a, &b).await;
    let expected = Tree::from([("notes.txt".into(), file(b"written in place on b\n"))]);
    assert_eq!(synced, expected);

    let saved = a.folder().join(".notes.txt.swp");
    fs::write(&saved, b"saved by rename on a\n").unwrap();
    fs::rename(&saved, a.folder().join("notes.txt")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    let expected = Tree::from([("notes.txt".into(), file(b"saved by rename on a\n"))]);
    assert_eq!(synced, expected);
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_rename_moves_the_file_on_the_other_device() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "rename").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::create_dir_all(a.folder().join("docs/drafts")).unwrap();
    fs::write(a.folder().join("docs/plan.txt"), b"plan\n").unwrap();
    fs::write(a.folder().join("docs/drafts/one.txt"), b"one\n").unwrap();
    fs::write(a.folder().join("top.txt"), b"top\n").unwrap();
    converged(&a, &b).await;

    fs::rename(
        b.folder().join("top.txt"),
        b.folder().join("docs/moved.txt"),
    )
    .unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(
        synced,
        Tree::from([
            ("docs".into(), Entry::Folder),
            ("docs/drafts".into(), Entry::Folder),
            ("docs/drafts/one.txt".into(), file(b"one\n")),
            ("docs/moved.txt".into(), file(b"top\n")),
            ("docs/plan.txt".into(), file(b"plan\n")),
        ])
    );

    fs::rename(a.folder().join("docs"), a.folder().join("papers")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(
        synced,
        Tree::from([
            ("papers".into(), Entry::Folder),
            ("papers/drafts".into(), Entry::Folder),
            ("papers/drafts/one.txt".into(), file(b"one\n")),
            ("papers/moved.txt".into(), file(b"top\n")),
            ("papers/plan.txt".into(), file(b"plan\n")),
        ])
    );
    a.stop().await;
    b.stop().await;
}

async fn trashed(owner: &Arc<Auth>, space: &SpaceInfo) -> Vec<String> {
    let config = SessionConfig::new(&space.space_id, "driver sync test", "0", 1);
    let (session, mut events) = Session::start(owner.clone(), config).unwrap();
    loop {
        match tokio::time::timeout(WAIT, events.next())
            .await
            .unwrap()
            .unwrap()
        {
            Event::Connected(_) => break,
            Event::Stopped(reason) => panic!("the owner's session stopped: {reason}"),
            _ => {}
        }
    }
    let page = session.list_trash("", 1000).await.unwrap();
    session.close().await;
    page.nodes.into_iter().map(|node| node.name).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_folder_moved_over_an_empty_folder_keeps_its_files() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "move over an empty folder").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::create_dir_all(a.folder().join("drafts/inner")).unwrap();
    fs::write(a.folder().join("drafts/.koiignore"), b"*.tmp\n").unwrap();
    fs::write(a.folder().join("drafts/one.txt"), b"one\n").unwrap();
    fs::write(a.folder().join("drafts/inner/two.txt"), b"two\n").unwrap();
    fs::create_dir(a.folder().join("final")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced.len(), 6, "{synced:#?}");

    fs::rename(a.folder().join("drafts"), a.folder().join("final")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(
        synced,
        Tree::from([
            ("final".into(), Entry::Folder),
            ("final/.koiignore".into(), file(b"*.tmp\n")),
            ("final/inner".into(), Entry::Folder),
            ("final/inner/two.txt".into(), file(b"two\n")),
            ("final/one.txt".into(), file(b"one\n")),
        ])
    );
    assert_eq!(trashed(&owner, &space).await, ["final"]);
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_folder_moved_over_an_empty_folder_with_unreadable_rules_keeps_its_files() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "move with unreadable rules").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::create_dir_all(a.folder().join("drafts/inner")).unwrap();
    fs::write(a.folder().join("drafts/.koiignore"), b"*.tmp\n").unwrap();
    fs::write(a.folder().join("drafts/one.txt"), b"one\n").unwrap();
    fs::write(a.folder().join("drafts/inner/two.txt"), b"two\n").unwrap();
    fs::create_dir(a.folder().join("final")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced.len(), 6, "{synced:#?}");
    let moved = Tree::from([
        ("final".into(), Entry::Folder),
        ("final/.koiignore".into(), file(b"*.tmp\n")),
        ("final/inner".into(), Entry::Folder),
        ("final/inner/two.txt".into(), file(b"two\n")),
        ("final/one.txt".into(), file(b"one\n")),
    ]);

    let rules = a.folder().join("drafts/.koiignore");
    fs::set_permissions(&rules, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&rules).is_ok() {
        eprintln!("skipped: the rules file is still readable, as for root");
        a.stop().await;
        b.stop().await;
        return;
    }
    fs::rename(a.folder().join("drafts"), a.folder().join("final")).unwrap();
    let started = Instant::now();
    while tree(&b.folder()).ok().as_ref() != Some(&moved) {
        assert!(
            started.elapsed() < WAIT,
            "the move did not reach the other device within {WAIT:?}\n{:#?}",
            tree(&b.folder())
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(trashed(&owner, &space).await, ["final"]);

    let rules = a.folder().join("final/.koiignore");
    fs::set_permissions(&rules, fs::Permissions::from_mode(0o644)).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced, moved);
    assert_eq!(trashed(&owner, &space).await, ["final"]);
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_second_hard_link_arrives_as_a_file_of_its_own() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "hard link").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::write(a.folder().join("one.txt"), b"linked\n").unwrap();
    converged(&a, &b).await;

    fs::hard_link(a.folder().join("one.txt"), a.folder().join("two.txt")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(
        synced,
        Tree::from([
            ("one.txt".into(), file(b"linked\n")),
            ("two.txt".into(), file(b"linked\n")),
        ])
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_deleted_file_is_removed_on_the_other_device() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "delete a file").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;

    fs::write(a.folder().join("draft.txt"), b"deleted on b\n").unwrap();
    fs::write(a.folder().join("kept.txt"), b"kept\n").unwrap();
    converged(&a, &b).await;

    fs::remove_file(b.folder().join("draft.txt")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced, Tree::from([("kept.txt".into(), file(b"kept\n"))]));
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_deleted_folder_is_removed_with_its_children_on_the_other_device() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "delete a folder").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;

    fs::create_dir_all(b.folder().join("album/inner")).unwrap();
    fs::write(b.folder().join("album/one.txt"), b"one\n").unwrap();
    fs::write(b.folder().join("album/inner/two.txt"), b"two\n").unwrap();
    fs::write(b.folder().join("kept.txt"), b"kept\n").unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced.len(), 5, "{synced:#?}");

    fs::remove_dir_all(a.folder().join("album")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced, Tree::from([("kept.txt".into(), file(b"kept\n"))]));
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn two_edits_of_one_file_keep_both_contents() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "conflict").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::write(a.folder().join("notes.txt"), b"first\n").unwrap();
    let (_, seq) = converged(&a, &b).await;

    b.stop().await;
    fs::write(a.folder().join("notes.txt"), b"edited on a\n").unwrap();
    let started = Instant::now();
    while !(a.status().state == FolderState::UpToDate && a.status().last_seq > Some(seq)) {
        assert!(
            started.elapsed() < WAIT,
            "a did not commit its edit: {:#?}",
            a.status()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    fs::write(b.folder().join("notes.txt"), b"edited on b\n").unwrap();
    b.start();

    let (synced, _) = converged(&a, &b).await;
    let names: Vec<&Path> = synced.keys().map(PathBuf::as_path).collect();
    assert_eq!(names.len(), 2, "{synced:#?}");
    assert_eq!(synced[Path::new("notes.txt")], file(b"edited on a\n"));
    let copy = names
        .iter()
        .find(|name| **name != Path::new("notes.txt"))
        .unwrap();
    let copy_name = copy.to_str().unwrap();
    assert!(
        copy_name.starts_with("notes (conflict from b ") && copy_name.ends_with(").txt"),
        "{copy_name}"
    );
    assert_eq!(synced[*copy], file(b"edited on b\n"));
    a.stop().await;
    b.stop().await;
}

fn put_in_trash(device: &Device, name: &str, entry: &str, when: std::time::SystemTime) {
    let root = device.folder();
    let space = koidrive_fs::Space::bind(fs::File::open(&root).unwrap().into(), &root).unwrap();
    let location = koidrive_fs::Location::At {
        base: koidrive_fs::Base::Root,
        dirs: Vec::new(),
        name: name.into(),
    };
    let (folder, name) = space.parent(&location).unwrap();
    let config = ChunkerConfig::V0;
    let refs = koidrive_chunk::chunks(fs::read(root.join(name)).unwrap().as_slice(), config)
        .map(|chunk| chunk.unwrap().chunk_ref())
        .collect();
    let content = koidrive_chunk::content(refs, 0, false, &config);
    let content = prost::Message::encode_to_vec(&content);
    space
        .trash(&folder, name, entry, name, Some(&content), when)
        .unwrap();
}

async fn reclaimed(device: &Device, entry: &str) {
    let path = device.folder().join(".koidrive/trash").join(entry);
    let started = Instant::now();
    while path.exists() {
        assert!(
            started.elapsed() < WAIT,
            "{entry} is still in the local trash"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_started_folder_reclaims_its_local_trash_by_age_and_by_the_configured_cap() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "local trash").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    a.stop().await;
    fs::write(a.folder().join("old.txt"), b"old\n").unwrap();
    let week_ago = std::time::SystemTime::now() - koidrive_fs::TRASH_RETENTION;
    put_in_trash(&a, "old.txt", "e1", week_ago);
    a.start();
    reclaimed(&a, "e1").await;

    a.stop().await;
    fs::write(a.folder().join("new.txt"), b"new\n").unwrap();
    put_in_trash(&a, "new.txt", "e2", std::time::SystemTime::now());
    fs::write(
        a.home.path().join("profile/config.toml"),
        "[trash]\nmax_size_mb = 0\n",
    )
    .unwrap();
    a.start();
    reclaimed(&a, "e2").await;
    let names: Vec<_> = fs::read_dir(a.folder())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [".koidrive"], "nothing came back to the root");
    a.stop().await;
}
