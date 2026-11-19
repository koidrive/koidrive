use koidrive_driver::Credential;
use koidrive_proto::v1::{
    AccessToken, Content, Error, NodeStatus, RpcRequest, RpcResponse, TrashPage, TreePage,
    VersionList,
};
use prost::Message;

use super::*;
use crate::report::RequestedData;
use crate::setup::fake::{Fake, proto};

#[tokio::test]
async fn history_waits_for_a_daemon_that_holds_the_profile_before_its_socket_answers() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let driver = Driver::open(paths.clone()).unwrap();
    let socket = paths.socket();
    let daemon = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        ipc::serve(koidrive_ipc::bind(&socket).unwrap(), driver).await;
    });
    let folders = linked_folders(&paths).await.unwrap();
    daemon.abort();
    assert_eq!(folders.len(), 1);
}

const SPACE: &str = "0194f8a2-0000-7000-8000-00000000000a";
const PHOTOS: &str = "0194f8a2-0000-7000-8000-00000000000b";

#[test]
fn concise_restore_replay_does_not_assume_a_linked_folder() {
    let done = asked(
        Path::new("/home/ada/Work"),
        "report.docx",
        "restore-1".into(),
        None,
    );
    let note = done.output(true).1;
    assert!(!note.contains("waits for the folder to sync"), "{note}");
    assert!(!note.contains("koi history requests"), "{note}");
    assert!(note.contains("koi status"), "{note}");
    assert!(done.next.is_empty());

    let done = asked(
        Path::new("/home/ada/Work"),
        "report.docx",
        "restore-1".into(),
        Some(FolderState::Stopped),
    );
    assert!(done.output(true).1.contains("koi status"));
}

#[test]
fn a_restore_in_a_folder_whose_name_is_not_utf8_suggests_no_command_naming_it() {
    use std::os::unix::ffi::OsStrExt;

    let done = asked(
        Path::new(std::ffi::OsStr::from_bytes(b"/home/ada/Work\xff")),
        "report.docx",
        "restore-1".into(),
        Some(FolderState::UpToDate),
    );
    assert!(done.next.is_empty(), "{:?}", done.next);
}

#[test]
fn concise_pending_restores_keep_the_folder_problem_and_decisions() {
    let folder = FolderStatus {
        state: FolderState::Halted,
        problem: Some("Fix file access, then run koi folder restart /home/ada/Koidrive".into()),
        questions: 2,
        ..linked()
    };
    let restores = [RestoreStatus {
        id: "restore-1".into(),
        path: Some("report.docx".into()),
        asked_at_ms: 1000,
        request: RestoreRequest::FromTrash {
            node_id: "n1".into(),
            parent_id: None,
            name: None,
        },
        state: RestoreState::Pending { waits_for: None },
    }];
    let summary = restores_summary(&restores, &folder);
    assert!(
        summary.contains("Fix file access, then run koi folder restart"),
        "{summary}"
    );
    assert!(summary.contains("2 need you"), "{summary}");
    assert!(summary.contains("koi resolve"), "{summary}");
}

fn node(id: &str, parent: &str, name: &str, kind: NodeKind, size: u64, group: &str) -> NodeState {
    let status = if group.is_empty() {
        NodeStatus::Live
    } else {
        NodeStatus::Trashed
    };
    NodeState {
        node_id: id.into(),
        parent_id: parent.into(),
        name: name.into(),
        kind: kind as i32,
        status: status as i32,
        trash_group: group.into(),
        current: Some(version(&format!("{id}-1"), size, 0)),
        ..NodeState::default()
    }
}

fn version(id: &str, size: u64, created_at_ms: i64) -> VersionInfo {
    VersionInfo {
        version_id: id.into(),
        content: Some(Content {
            size,
            ..Content::default()
        }),
        created_at_ms,
        ..VersionInfo::default()
    }
}

fn names(path: &str) -> Vec<String> {
    path.split('/').map(String::from).collect()
}

fn answer(result: rpc_response::Result) -> RpcResponse {
    RpcResponse {
        req_id: 1,
        result: Some(result),
    }
}

/// A signed-in device of the server `fake`, which answers its token request first.
fn signed_in_to(fake: &Fake) -> Auth {
    let credential = Credential {
        server: fake.base.to_string(),
        user_id: "user".into(),
        user_name: None,
        device_id: "0194f8a2-0000-7000-8000-000000000001".into(),
        device_name: "laptop".into(),
        refresh_credential: "refresh".into(),
    };
    Auth::new(credential.api().unwrap(), credential.net())
}

fn token() -> crate::setup::fake::Reply {
    let token = AccessToken {
        token: "access".into(),
        expires_at_ms: i64::MAX,
        ..Default::default()
    };
    proto("POST", "/v1/token", token)
}

#[tokio::test]
async fn every_page_follows_the_cursor_to_the_last_page() {
    let mut asked = Vec::new();
    let all = every_page(async |cursor: String| {
        asked.push(cursor.clone());
        Ok(match cursor.as_str() {
            "" => (vec![1, 2], "b".to_owned()),
            "b" => (vec![3], "c".to_owned()),
            _ => (vec![4], String::new()),
        })
    })
    .await
    .unwrap();
    assert_eq!(all, [1, 2, 3, 4]);
    assert_eq!(asked, ["", "b", "c"]);

    let error = every_page(async |_cursor: String| Ok((vec![0], "same".to_owned())))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("same page cursor twice"),
        "{error:?}"
    );
}

#[tokio::test]
async fn versions_are_read_page_by_page_over_http() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let page = |ids: &[&str], next: &str| {
        let list = VersionList {
            versions: ids.iter().map(|id| version(id, 1, 0)).collect(),
            next_cursor: next.into(),
        };
        proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
    };
    let fake = Fake::start(vec![
        token(),
        page(&["v3", "v2"], "after-v2"),
        page(&["v1"], ""),
    ])
    .await;

    let kept = versions_of(&signed_in_to(&fake), SPACE, "node")
        .await
        .unwrap();

    let ids: Vec<_> = kept
        .iter()
        .map(|version| version.version_id.as_str())
        .collect();
    assert_eq!(ids, ["v3", "v2", "v1"]);
    let asked: Vec<_> = fake
        .seen()
        .iter()
        .filter(|seen| seen.path == rpc)
        .map(
            |seen| match RpcRequest::decode(seen.body.as_slice()).unwrap().call {
                Some(Call::ListVersions(list)) => (list.node_id, list.cursor),
                other => panic!("expected ListVersions, got {other:?}"),
            },
        )
        .collect();
    assert_eq!(
        asked,
        [
            ("node".to_owned(), String::new()),
            ("node".to_owned(), "after-v2".to_owned())
        ]
    );
}

#[tokio::test]
async fn the_tree_and_the_trash_are_read_over_http_and_a_refusal_stops_them() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let tree_page = TreePage {
        nodes: vec![node("docs", SPACE, "docs", NodeKind::Dir, 0, "")],
        ..TreePage::default()
    };
    let trash_page = TrashPage {
        nodes: vec![
            node("old", "docs", "old.txt", NodeKind::File, 12, "g"),
            node("older", "docs", "older.txt", NodeKind::File, 1, "h"),
        ],
        trashed: vec![
            Trashed {
                node_id: "old".into(),
                trashed_at_ms: 1_700,
                device_id: "desk".into(),
            },
            Trashed {
                node_id: "older".into(),
                trashed_at_ms: 0,
                device_id: String::new(),
            },
        ],
        ..TrashPage::default()
    };
    let refusal = Error {
        message: "not a member of this space".into(),
        ..Error::default()
    };
    let fake = Fake::start(vec![
        token(),
        proto("POST", &rpc, answer(rpc_response::Result::Tree(tree_page))),
        proto(
            "POST",
            &rpc,
            answer(rpc_response::Result::Trash(trash_page)),
        ),
        proto("POST", &rpc, answer(rpc_response::Result::Error(refusal))),
    ])
    .await;
    let auth = signed_in_to(&fake);

    let live = tree(&auth, SPACE).await.unwrap();
    let (trashed, deleted) = trash_nodes(&auth, SPACE).await.unwrap();
    let error = tree(&auth, SPACE).await.unwrap_err();

    assert_eq!(live[0].node_id, "docs");
    assert_eq!(trashed[0].node_id, "old");
    assert_eq!(
        (
            deleted.len(),
            deleted["old"].trashed_at_ms,
            deleted["old"].device_id.as_str()
        ),
        (1, 1_700, "desk")
    );
    assert_eq!(
        error.message,
        "the server refused: not a member of this space"
    );
}

#[test]
fn a_path_finds_its_live_node_by_name_key() {
    let live = vec![
        node("docs", SPACE, "Docs", NodeKind::Dir, 0, ""),
        node("cafe", "docs", "Café.txt", NodeKind::File, 5, ""),
        node("other", SPACE, "café.txt", NodeKind::File, 1, ""),
    ];
    let found = |path: &str| find(&live, SPACE, &names(path)).map(|node| node.node_id.as_str());

    assert_eq!(found("docs/CAFE\u{301}.TXT"), Some("cafe"));
    assert_eq!(found("Docs/Café.txt"), Some("cafe"));
    assert_eq!(found("café.txt"), Some("other"));
    assert_eq!(found("docs/missing.txt"), None);
    assert_eq!(found("café.txt/below"), None);
}

fn linked_on(server: &str) -> (tempfile::TempDir, Paths, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    std::fs::create_dir_all(&root).unwrap();
    Credential {
        server: server.into(),
        user_id: "user".into(),
        user_name: None,
        device_id: "d1".into(),
        device_name: "laptop".into(),
        refresh_credential: "refresh".into(),
    }
    .save_new(&paths.credential())
    .unwrap();
    let store = paths.store(SPACE).unwrap();
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    koidrive_store::Store::open(&store)
        .unwrap()
        .transaction(|tx| {
            tx.set_link(&koidrive_store::Link {
                space_id: SPACE.into(),
                space_name: "Projects".into(),
                root: root.clone(),
                root_dev: 1,
                root_volume: None,
                server_url: server.into(),
                device_id: "d1".into(),
                linked_at_ms: 1,
                found_marker: false,
            })
        })
        .unwrap();
    (dir, paths, root)
}

fn link(paths: &Paths, space_id: &str, space_name: &str, root: &Path) {
    let store = paths.store(space_id).unwrap();
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    koidrive_store::Store::open(&store)
        .unwrap()
        .transaction(|tx| {
            tx.set_link(&koidrive_store::Link {
                space_id: space_id.into(),
                space_name: space_name.into(),
                root: root.to_path_buf(),
                root_dev: 1,
                root_volume: None,
                server_url: "http://127.0.0.1:9/".into(),
                device_id: "d1".into(),
                linked_at_ms: 1,
                found_marker: false,
            })
        })
        .unwrap();
}

#[tokio::test]
async fn a_path_is_placed_by_the_real_file_it_leads_to() {
    let (dir, paths, root) = linked_on("http://127.0.0.1:9/");
    let base = root.parent().unwrap().to_path_buf();
    let photos = base.join("Photos");
    let outside = base.join("Outside");
    for folder in [root.join("Docs"), photos.clone(), outside.clone()] {
        std::fs::create_dir_all(folder).unwrap();
    }
    link(&paths, PHOTOS, "Photos", &dir.path().join("Photos"));
    let symlink =
        |link: &Path, destination: &Path| std::os::unix::fs::symlink(destination, link).unwrap();
    symlink(&root.join("bridge"), &photos);
    symlink(&root.join("away"), &outside);
    symlink(&root.join("gone"), &outside.join("Missing"));
    symlink(&root.join("up"), Path::new("../Outside/Missing"));
    symlink(&root.join("later"), Path::new("Docs/Later"));
    symlink(&base.join("Shortcut"), &root);
    for hop in 0..40 {
        let destination = match hop {
            39 => outside.join("Missing"),
            _ => PathBuf::from(format!("chain{}", hop + 1)),
        };
        symlink(&root.join(format!("chain{hop}")), &destination);
    }
    for links in [31, 32, 33] {
        for hop in 0..links {
            let destination = match hop + 1 == links {
                true => PathBuf::from("Docs/Missing"),
                false => PathBuf::from(format!("links{links}-{}", hop + 1)),
            };
            symlink(&root.join(format!("links{links}-{hop}")), &destination);
        }
    }
    let placed = |path: PathBuf| {
        let paths = paths.clone();
        async move {
            locate(&paths, &path)
                .await
                .map(|located| (located.folder.space_id, located.below.join("/")))
                .map_err(|error| error.reason)
        }
    };
    let refused = Err(Some(Reason::NotInLinkedFolder));

    assert_eq!(
        placed(root.join("Docs/Plan.key")).await,
        Ok((SPACE.into(), "Docs/Plan.key".into()))
    );
    assert_eq!(
        placed(dir.path().join("Koidrive/Docs/Plan.key")).await,
        Ok((SPACE.into(), "Docs/Plan.key".into()))
    );
    assert_eq!(
        placed(photos.join("Plan.key")).await,
        Ok((PHOTOS.into(), "Plan.key".into()))
    );
    assert_eq!(
        placed(root.join("bridge/Plan.key")).await,
        Ok((PHOTOS.into(), "Plan.key".into()))
    );
    assert_eq!(
        placed(base.join("Shortcut/Docs/New.key")).await,
        Ok((SPACE.into(), "Docs/New.key".into()))
    );
    assert_eq!(
        placed(root.join("later/Plan.key")).await,
        Ok((SPACE.into(), "Docs/Later/Plan.key".into()))
    );
    for path in [
        "away/Plan.key",
        "gone/Plan.key",
        "up/Plan.key",
        "chain0/Plan.key",
        "chain30/Plan.key",
    ] {
        assert_eq!(placed(root.join(path)).await, refused, "{path}");
        assert_eq!(
            placed(base.join("Shortcut").join(path)).await,
            refused,
            "{path}"
        );
    }
    assert_eq!(placed(base.join("Shortcut/../Plan.key")).await, refused);
    for links in [31, 32] {
        assert_eq!(
            placed(root.join(format!("links{links}-0/Plan.key"))).await,
            Ok((SPACE.into(), "Docs/Missing/Plan.key".into())),
            "{links} links"
        );
    }
    assert_eq!(placed(root.join("links33-0/Plan.key")).await, refused);
}

/// A profile signed in to a server that never answers, whose folder `Koidrive` syncs `SPACE`, and the record of the
/// finished request `done` that restored `a.txt` in `space_id`.
async fn silent_profile(
    space_id: &str,
) -> (tempfile::TempDir, Paths, PathBuf, tokio::net::TcpListener) {
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (dir, paths, root) = linked_on(&format!("http://{}/", silent.local_addr().unwrap()));
    let request::Begun::Claimed(claim) = request::begin(
        &paths,
        &RequestId::parse("done").unwrap(),
        &restore_key(&root),
    )
    .unwrap() else {
        panic!("the request was claimed before");
    };
    claim
        .finish(&Response::Restore {
            id: "restore-1".into(),
            space_id: space_id.into(),
            folder: root.clone(),
            path: "a.txt".into(),
        })
        .unwrap();
    (dir, paths, root, silent)
}

fn restore_key(root: &Path) -> Key {
    Key::restore(&root.join("a.txt"), None, None)
}

async fn restore_again(paths: &Paths, root: &Path, id: &str) -> Result<Done, CliError> {
    let file = root.join("a.txt");
    let run = restore(
        paths,
        &file,
        None,
        None,
        Some(RequestId::parse(id).unwrap()),
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("the run waited for the server")
}

#[tokio::test]
async fn each_refusal_of_history_names_its_reason() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let tree = || {
        let nodes = vec![
            node("docs", SPACE, "docs", NodeKind::Dir, 0, ""),
            node("a", "docs", "a.txt", NodeKind::File, 3, ""),
        ];
        let page = TreePage {
            nodes,
            ..TreePage::default()
        };
        proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
    };
    let trash = || {
        let nodes = vec![
            node("b1", "docs", "b.txt", NodeKind::File, 1, "g1"),
            node("b2", "docs", "b.txt", NodeKind::File, 2, "g2"),
            node("c", "docs", "c.txt", NodeKind::File, 2, "g3"),
        ];
        let page = TrashPage {
            nodes,
            ..TrashPage::default()
        };
        proto("POST", &rpc, answer(rpc_response::Result::Trash(page)))
    };
    let kept = || {
        let list = VersionList {
            versions: vec![version("a-1", 3, 2), version("a-0", 1, 1)],
            next_cursor: String::new(),
        };
        proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
    };
    let restore = |path: &'static str, version: Option<&str>, node: Option<&str>| {
        (
            path,
            Some((version.map(String::from), node.map(String::from))),
        )
    };
    let listing = |path: &'static str| (path, None);
    let cases = [
        (
            listing("../elsewhere.txt"),
            vec![],
            Reason::NotInLinkedFolder,
        ),
        (listing(""), vec![], Reason::LinkedFolderItself),
        (
            listing("docs/gone.txt"),
            vec![token(), tree()],
            Reason::NotFound,
        ),
        (listing("docs"), vec![token(), tree()], Reason::IsFolder),
        (restore("", None, None), vec![], Reason::LinkedFolderItself),
        (
            restore("docs/gone.txt", Some("v"), None),
            vec![token(), tree()],
            Reason::NotFound,
        ),
        (
            restore("docs", Some("v"), None),
            vec![token(), tree()],
            Reason::IsFolder,
        ),
        (
            restore("docs/a.txt", Some("a-1"), None),
            vec![token(), tree(), kept()],
            Reason::AlreadyCurrent,
        ),
        (
            restore("docs/a.txt", Some("a-9"), None),
            vec![token(), tree(), kept()],
            Reason::NoSuchVersion,
        ),
        (
            restore("docs/a.txt", None, None),
            vec![token(), tree(), trash()],
            Reason::NotDeleted,
        ),
        (
            restore("docs/gone.txt", None, None),
            vec![token(), tree(), trash()],
            Reason::NotFound,
        ),
        (
            restore("docs/b.txt", None, None),
            vec![token(), tree(), trash()],
            Reason::SeveralDeletions,
        ),
        (
            restore("docs/b.txt", None, Some("c")),
            vec![token(), tree(), trash()],
            Reason::NotInTrash,
        ),
    ];
    for ((path, restoring), replies, reason) in cases {
        let fake = Fake::start(replies).await;
        let (_dir, paths, root) = linked_on(fake.base.as_str());
        let path = root.join(path);
        let error = match restoring {
            None => versions(&paths, &path).await.map(drop).unwrap_err(),
            Some((version, node)) => checked(&paths, &path, version, node)
                .await
                .map(drop)
                .unwrap_err(),
        };
        assert_eq!(
            (error.code, error.reason),
            (koidrive_ipc::Code::Usage, Some(reason)),
            "{path:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_save_between_reading_the_tree_and_the_versions_reads_both_again() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let saved = |id: &str, seq: u64| VersionInfo {
        seq,
        ..version(id, seq, seq as i64)
    };
    let tree = |current: VersionInfo| {
        let page = TreePage {
            nodes: vec![NodeState {
                current: Some(current),
                ..node("a", SPACE, "a.txt", NodeKind::File, 3, "")
            }],
            ..TreePage::default()
        };
        proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
    };
    let kept = || {
        let list = VersionList {
            versions: vec![saved("a-2", 3), saved("a-1", 2)],
            next_cursor: String::new(),
        };
        proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
    };
    let no_devices = koidrive_proto::v1::DeviceList::default();
    let fake = Fake::start(vec![
        token(),
        tree(saved("a-1", 2)),
        kept(),
        tree(saved("a-2", 3)),
        kept(),
        proto("GET", "/v1/devices", no_devices),
        token(),
        tree(saved("a-1", 2)),
        kept(),
        tree(saved("a-2", 3)),
        kept(),
    ])
    .await;
    let (_dir, paths, root) = linked_on(fake.base.as_str());
    let file = root.join("a.txt");

    let done = versions(&paths, &file).await.unwrap();
    let refused = checked(&paths, &file, Some("a-2".into()), None)
        .await
        .unwrap_err();

    let current: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
        .map(|version| version["current"].clone())
        .collect();
    assert_eq!(current, [serde_json::json!(true), serde_json::json!(false)]);
    assert_eq!(refused.reason, Some(Reason::AlreadyCurrent), "{refused:?}");
}

#[tokio::test]
async fn a_file_that_keeps_changing_while_its_versions_are_read_asks_to_try_again() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let saved = |id: &str, seq: u64| VersionInfo {
        seq,
        ..version(id, seq, seq as i64)
    };
    let tree = || {
        let page = TreePage {
            nodes: vec![NodeState {
                current: Some(saved("a-1", 2)),
                ..node("a", SPACE, "a.txt", NodeKind::File, 3, "")
            }],
            ..TreePage::default()
        };
        proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
    };
    let kept = || {
        let list = VersionList {
            versions: vec![saved("a-2", 3), saved("a-1", 2)],
            next_cursor: String::new(),
        };
        proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
    };
    let mut replies = vec![token()];
    for _ in 0..4 {
        replies.extend([tree(), kept()]);
    }
    let fake = Fake::start(replies).await;
    let (_dir, paths, root) = linked_on(fake.base.as_str());

    let error = versions(&paths, &root.join("a.txt")).await.unwrap_err();

    assert_eq!(error.code, koidrive_ipc::Code::Unavailable, "{error:?}");
}

#[tokio::test]
async fn two_saves_in_one_commit_keep_the_current_version_the_tree_names() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let in_one_commit = |id: &str| VersionInfo {
        seq: 2,
        ..version(id, 3, 2)
    };
    let tree = || {
        let page = TreePage {
            nodes: vec![NodeState {
                current: Some(in_one_commit("a-1")),
                ..node("a", SPACE, "a.txt", NodeKind::File, 3, "")
            }],
            ..TreePage::default()
        };
        proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
    };
    let kept = || {
        let list = VersionList {
            versions: vec![in_one_commit("a-2"), in_one_commit("a-1")],
            next_cursor: String::new(),
        };
        proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
    };
    let no_devices = koidrive_proto::v1::DeviceList::default();
    let fake = Fake::start(vec![
        token(),
        tree(),
        kept(),
        proto("GET", "/v1/devices", no_devices),
        token(),
        tree(),
        kept(),
    ])
    .await;
    let (_dir, paths, root) = linked_on(fake.base.as_str());
    let file = root.join("a.txt");

    let done = versions(&paths, &file).await.unwrap();
    let older = checked(&paths, &file, Some("a-2".into()), None).await;

    let current: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
        .map(|version| version["current"].clone())
        .collect();
    assert_eq!(current, [serde_json::json!(false), serde_json::json!(true)]);
    assert!(older.is_ok(), "{:?}", older.err());
}

#[tokio::test]
async fn versions_without_the_device_list_name_no_device() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let tree = TreePage {
        nodes: vec![node("a", SPACE, "a.txt", NodeKind::File, 3, "")],
        ..TreePage::default()
    };
    let kept = VersionList {
        versions: vec![version("a-1", 3, 2), version("a-0", 3, 1)],
        next_cursor: String::new(),
    };
    let unavailable = crate::setup::fake::Reply {
        method: "GET",
        path: "/v1/devices".into(),
        status: 500,
        content_type: "text/plain",
        body: b"the device list is unavailable".to_vec(),
    };
    let fake = Fake::start(vec![
        token(),
        proto("POST", &rpc, answer(rpc_response::Result::Tree(tree))),
        proto("POST", &rpc, answer(rpc_response::Result::Versions(kept))),
        unavailable,
    ])
    .await;
    let (_dir, paths, root) = linked_on(fake.base.as_str());

    let done = versions(&paths, &root.join("a.txt")).await.unwrap();

    let names: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
        .map(|version| version["device_name"].clone())
        .collect();
    assert_eq!(names, [serde_json::Value::Null, serde_json::Value::Null]);
}

#[tokio::test]
async fn versions_name_the_device_that_saved_each() {
    let rpc = format!("/v1/spaces/{SPACE}/rpc");
    let tree = TreePage {
        nodes: vec![node("a", SPACE, "a.txt", NodeKind::File, 3, "")],
        ..TreePage::default()
    };
    let saved_by = |id: &str, device: &str| VersionInfo {
        device_id: device.into(),
        ..version(id, 3, 1)
    };
    let kept = VersionList {
        versions: vec![
            saved_by("a-2", "d1"),
            saved_by("a-1", "d0"),
            saved_by("a-0", "elsewhere"),
        ],
        next_cursor: String::new(),
    };
    let revoked = koidrive_proto::v1::DeviceInfo {
        device_id: "d0".into(),
        name: "old laptop".into(),
        revoked_at_ms: 5,
        ..Default::default()
    };
    let devices = koidrive_proto::v1::DeviceList {
        devices: vec![
            koidrive_proto::v1::DeviceInfo {
                device_id: "d1".into(),
                name: "laptop".into(),
                ..Default::default()
            },
            revoked,
        ],
        next_cursor: String::new(),
    };
    let fake = Fake::start(vec![
        token(),
        proto("POST", &rpc, answer(rpc_response::Result::Tree(tree))),
        proto("POST", &rpc, answer(rpc_response::Result::Versions(kept))),
        proto("GET", "/v1/devices", devices),
    ])
    .await;
    let (_dir, paths, root) = linked_on(fake.base.as_str());

    let done = versions(&paths, &root.join("a.txt")).await.unwrap();

    let names: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
        .map(|version| version["device_name"].clone())
        .collect();
    assert_eq!(
        names,
        [
            serde_json::json!("laptop"),
            serde_json::json!("old laptop"),
            serde_json::Value::Null
        ]
    );
}

#[test]
fn a_reason_reaches_the_envelope_and_an_error_without_one_leaves_it_out() {
    let validator =
        jsonschema::validator_for(&crate::schema::envelope("history restore").unwrap()).unwrap();
    let printed = |error: CliError| {
        let envelope = serde_json::to_value(crate::report::Envelope::failed(
            "history restore",
            error,
            None,
        ))
        .unwrap();
        assert!(validator.is_valid(&envelope), "{envelope}");
        envelope["error"].clone()
    };

    let refused = printed(CliError::usage("no such version").because(Reason::NoSuchVersion));
    let failed = printed(CliError::from("the server sent an empty answer"));

    assert_eq!(refused["reason"], "no_such_version");
    assert!(failed.get("reason").is_none(), "{failed}");
}

#[test]
fn a_request_names_its_path_when_it_was_asked_for_and_when_its_version_was_saved() {
    let restore = |path: Option<&str>| RestoreStatus {
        id: "0194f8a2-0000-7000-8000-000000000001".into(),
        path: path.map(Into::into),
        asked_at_ms: 1_738_000_000_000,
        request: RestoreRequest::Version {
            node_id: "note".into(),
            version_id: "note-0".into(),
            saved_at_ms: Some(1_737_000_000_000),
        },
        state: RestoreState::Restored,
    };

    let found = RequestData::of(&restore(Some("docs/note.txt")));
    let unknown = RequestData::of(&restore(None));

    assert_eq!(
        (found.path.as_deref(), found.asked_at_ms),
        (Some("docs/note.txt"), 1_738_000_000_000)
    );
    assert_eq!(
        found.request,
        RequestedData::Version {
            node_id: "note".into(),
            version_id: "note-0".into(),
            saved_at_ms: Some(1_737_000_000_000),
        }
    );
    assert_eq!(unknown.path, None);
}

#[tokio::test]
async fn a_finished_restore_is_reported_from_its_record_without_the_server() {
    let (_dir, paths, root, silent) = silent_profile(SPACE).await;
    let done = restore_again(&paths, &root, "done").await.unwrap();
    assert_eq!(done.text, "restore-1\n");
    assert_eq!(
        done.next,
        [koidrive_ipc::Next::of(&[
            "koi",
            "history",
            "requests",
            &root.display().to_string()
        ])]
    );

    let request::Begun::Claimed(claim) = request::begin(
        &paths,
        &RequestId::parse("going").unwrap(),
        &restore_key(&root),
    )
    .unwrap() else {
        panic!("the request was claimed before");
    };
    drop(claim);
    let error = restore_again(&paths, &root, "going").await.unwrap_err();
    assert_eq!(error.code, koidrive_ipc::Code::Interrupted, "{error:?}");

    let called = tokio::time::timeout(std::time::Duration::from_millis(100), silent.accept()).await;
    assert!(called.is_err(), "the server was called");
}

#[tokio::test]
async fn a_replayed_restore_offers_no_follow_up_once_its_folder_syncs_another_space() {
    let (_dir, paths, root, _silent) = silent_profile("0194f8a2-0000-7000-8000-00000000000b").await;
    let done = restore_again(&paths, &root, "done").await.unwrap();
    assert_eq!(done.text, "restore-1\n");
    assert_eq!(done.note, "Asked to restore a.txt (restore restore-1).\n");
    assert!(done.next.is_empty(), "{:?}", done.next);
}

#[tokio::test]
async fn without_a_daemon_the_commands_leave_a_fresh_profile_alone() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(&dir.path().join("profile"));
    let folder = dir.path().join("Sync");
    std::fs::create_dir(&folder).unwrap();

    let trashed = trash(&paths, &folder).await.unwrap_err();
    let kept = versions(&paths, &folder.join("a.txt")).await.unwrap_err();

    assert_eq!(
        trashed.code,
        koidrive_ipc::Code::AuthRequired,
        "{trashed:?}"
    );
    assert_eq!(kept.code, koidrive_ipc::Code::AuthRequired, "{kept:?}");
    assert!(!dir.path().join("profile").exists());
}

#[test]
fn a_path_is_normalized_without_reading_the_disk() {
    assert_eq!(
        lexical(Path::new("/home/ada/./Koidrive/docs/../link.txt")),
        Path::new("/home/ada/Koidrive/link.txt")
    );
}

#[test]
fn a_path_below_a_linked_folder_gives_its_names() {
    let root = Path::new("/home/ada/Koidrive");
    assert_eq!(
        below(root, Path::new("/home/ada/Koidrive/docs/a.txt")),
        Some(names("docs/a.txt"))
    );
    assert_eq!(below(root, root), Some(Vec::new()));
    assert_eq!(below(root, Path::new("/home/ada/Koidrive2/a.txt")), None);
    assert!(starts_with(&names("Docs/a.txt"), &names("docs")));
    assert!(!starts_with(&names("docs"), &names("docs/a.txt")));
}

#[test]
fn the_trash_lists_each_deletion_once_with_what_it_held() {
    let live = vec![node("docs", SPACE, "docs", NodeKind::Dir, 0, "")];
    let trash = vec![
        node("photos", SPACE, "photos", NodeKind::Dir, 0, "g1"),
        node("cat", "photos", "cat.jpg", NodeKind::File, 300, "g1"),
        node("trip", "photos", "trip", NodeKind::Dir, 0, "g1"),
        node("dog", "trip", "dog.jpg", NodeKind::File, 700, "g1"),
        node("late", "photos", "late.jpg", NodeKind::File, 9, "g2"),
        NodeState {
            current: Some(version("old-1", 12, 1_700)),
            ..node("old", "docs", "old.txt", NodeKind::File, 12, "g3")
        },
        node("lost", "purged", "lost.txt", NodeKind::File, 5, "g4"),
    ];

    let deleted: HashMap<String, Trashed> = [("photos", 5_000, "desk"), ("old", 4_000, "gone")]
        .map(|(node, at, device)| {
            let deletion = Trashed {
                node_id: node.into(),
                trashed_at_ms: at,
                device_id: device.into(),
            };
            (node.to_owned(), deletion)
        })
        .into();
    let names = HashMap::from([("desk".to_owned(), "Ada's Desk".to_owned())]);
    let found = deletions(&live, &trash, &deleted, SPACE);

    let summary: Vec<_> = found
        .iter()
        .map(|deleted| (deleted.names.join("/"), deleted.files, deleted.bytes))
        .collect();
    assert_eq!(
        summary,
        [
            ("docs/old.txt".to_owned(), 1, 12),
            ("photos".to_owned(), 2, 1000),
            ("photos/late.jpg".to_owned(), 1, 9),
            ("…/lost.txt".to_owned(), 1, 5),
        ]
    );
    let modified: Vec<Option<i64>> = (found.iter())
        .map(|deleted| deleted.entry(&names).modified_at_ms)
        .collect();
    assert_eq!(modified, [Some(1_700), None, Some(0), Some(0)]);
    let deletion: Vec<_> = (found.iter())
        .map(|deleted| {
            let entry = deleted.entry(&names);
            (
                entry.deleted_at_ms,
                entry.deleted_by_device_id,
                entry.deleted_by_device_name,
            )
        })
        .collect();
    assert_eq!(
        deletion,
        [
            (Some(4_000), Some("gone".into()), None),
            (Some(5_000), Some("desk".into()), Some("Ada's Desk".into())),
            (None, None, None),
            (None, None, None),
        ]
    );
    let linked = FolderStatus {
        recovery: None,
        path: Some("/home/ada/Koidrive".into()),
        ..serde_json::from_value(serde_json::json!({
            "space_id": SPACE, "space_name": "Home", "path": "", "state": "up_to_date",
            "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
            "local_entries": null, "questions": 0
        }))
        .unwrap()
    };
    assert_eq!(
        trash_report(&found, &linked, &names, 5_000 + 3 * 3_600_000),
        "     12 B  docs/old.txt  (deleted 3 h ago)\n   1.0 kB  photos/  (folder, 2 files, deleted 3 h ago on Ada's Desk)\n      9 B  photos/late.jpg\n      5 B  …/lost.txt\n"
    );
    assert_eq!(
        trash_report(&[], &linked, &HashMap::new(), 0),
        "Nothing from /home/ada/Koidrive is in the trash.\n"
    );
    let forged = FolderStatus {
        recovery: None,
        path: Some("/home/ada/Docs\u{1b}[31m\nFORGED".into()),
        ..linked
    };
    assert_eq!(
        trash_report(&[], &forged, &HashMap::new(), 0),
        "Nothing from /home/ada/Docs\\u{1b}[31m\\u{a}FORGED is in the trash.\n"
    );
    assert_eq!(
        restores_report(&[], &forged),
        "No restore was asked for in /home/ada/Docs\\u{1b}[31m\\u{a}FORGED.\n"
    );
}

#[test]
fn a_restore_picks_the_deletion_that_had_the_path() {
    let live = vec![
        node("docs", SPACE, "docs", NodeKind::Dir, 0, ""),
        node("note", "docs", "note.txt", NodeKind::File, 4, ""),
    ];
    let trash = vec![
        node("photos", SPACE, "photos", NodeKind::Dir, 0, "g1"),
        node("cat", "photos", "cat.jpg", NodeKind::File, 300, "g1"),
        node("old-b", "docs", "old.txt", NodeKind::File, 12, "g3"),
        node("old-a", "docs", "old.txt", NodeKind::File, 7, "g2"),
    ];
    let pick = |path: &str, node: Option<&str>| {
        trashed_at(&live, &trash, SPACE, &names(path), node)
            .map(|picked| picked.node_id.as_str())
            .map_err(|why| match why {
                NotPicked::Several(found) => {
                    format!(
                        "several: {:?}",
                        found.iter().map(|node| &node.node_id).collect::<Vec<_>>()
                    )
                }
                other => format!("{other:?}"),
            })
    };

    assert_eq!(pick("photos", None), Ok("photos"));
    assert_eq!(
        pick("photos/cat.jpg", None),
        Ok("cat"),
        "a file trashed with its folder"
    );
    assert_eq!(
        pick("docs/old.txt", None),
        Err(r#"several: ["old-a", "old-b"]"#.into())
    );
    assert_eq!(pick("docs/old.txt", Some("old-b")), Ok("old-b"));
    assert_eq!(
        pick("docs/old.txt", Some("cat")),
        Err(r#"NotThere("cat")"#.into())
    );
    assert_eq!(
        pick("docs/note.txt", None),
        Err("None { live: true }".into())
    );
    assert_eq!(
        pick("docs/new.txt", None),
        Err("None { live: false }".into())
    );

    let several = trashed_at(&live, &trash, SPACE, &names("docs/old.txt"), None).unwrap_err();
    assert_eq!(
        several.message("docs/old.txt", Path::new("/home/ada/Koidrive")),
        "2 deletions had the path docs/old.txt; pick one with --node: old-a (file, 7 B), old-b (file, 12 B)"
    );
}

fn linked() -> FolderStatus {
    FolderStatus {
        recovery: None,
        path: Some("/home/ada/Koidrive".into()),
        ..serde_json::from_value(serde_json::json!({
            "space_id": SPACE, "space_name": "Home", "path": "", "state": "up_to_date",
            "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
            "local_entries": null, "questions": 0
        }))
        .unwrap()
    }
}

#[test]
fn the_restores_report_says_what_holds_a_pending_restore_back() {
    let restores = [
        RestoreStatus {
            id: "r1".into(),
            path: None,
            asked_at_ms: 0,
            request: RestoreRequest::FromTrash {
                node_id: "cat".into(),
                parent_id: None,
                name: None,
            },
            state: RestoreState::Renamed {
                name: "cat (2).jpg".into(),
            },
        },
        RestoreStatus {
            id: "r2".into(),
            path: None,
            asked_at_ms: 0,
            request: RestoreRequest::Version {
                node_id: "note".into(),
                version_id: "note-0".into(),
                saved_at_ms: None,
            },
            state: RestoreState::Pending { waits_for: None },
        },
    ];
    let folder = FolderStatus {
        recovery: None,
        state: FolderState::Stopped,
        questions: 1,
        problem: Some("the root is missing".into()),
        ..linked()
    };

    assert_eq!(
        restores_report(&restores, &folder),
        concat!(
            "r1  deleted node cat\n",
            "  restored as cat (2).jpg, since its name was taken\n",
            "r2  version note-0 of node note\n",
            "  pending: waits for the folder to sync\n",
            "A pending restore goes out with a commit of the folder once the earlier changes of its node have synced.\n",
            "The folder is not syncing: run koi _daemon.\n",
            "The folder has a problem: the root is missing\n",
            "1 needs you: koi resolve lists them.\n",
        )
    );
    let two = FolderStatus {
        recovery: None,
        questions: 2,
        ..linked()
    };
    assert!(restores_report(&restores, &two).ends_with("2 need you: koi resolve lists them.\n"));
    assert_eq!(
        restores_report(&restores[..1], &folder),
        "r1  deleted node cat\n  restored as cat (2).jpg, since its name was taken\n"
    );
    assert_eq!(
        restores_report(&[], &folder),
        "No restore was asked for in /home/ada/Koidrive.\n"
    );
}

#[test]
fn the_versions_report_marks_the_current_one() {
    let kept = [version("note-1", 20, 3_000_000), version("note-0", 10, 0)];

    let report = versions_report("note.txt", &kept, Some("note-1"), 3_000_000 + 7_200_000);

    assert_eq!(
        report,
        "note.txt: 2 versions, newest first\ncurrent       20 B  2 h ago         note-1\n              10 B  never           note-0\n"
    );
}
