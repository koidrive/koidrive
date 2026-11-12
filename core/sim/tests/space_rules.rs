// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap};

use koidrive_engine::ids::IdGen;
use koidrive_engine::names::conflict_name;
use koidrive_proto::v1::{
    ChunkRef, Commit, CommitResult, Content, CreateDir, CreateFile, Delete, Error, ListTrash,
    ListTree, ListVersions, Move, NodeState, NodeStatus, Op, PrepareUpload, Restore, Resume,
    RpcRequest, UpdateContent, UploadReceipt, chunk_plan, error, op, op_result, resync_required,
    rpc_request, rpc_response,
};
use koidrive_sim::space::{Role, Session, SpaceDo, node_states};
use sha2::{Digest, Sha256};

const NOW: i64 = 1_794_268_740_000;

struct Space {
    space: SpaceDo,
    ids: IdGen,
    sent: HashMap<String, u64>,
}

struct Client {
    session: Session,
    base_seq: u64,
}

impl Space {
    fn new() -> Self {
        Self {
            space: SpaceDo::new(1, NOW),
            ids: IdGen::new(2),
            sent: HashMap::new(),
        }
    }

    fn root(&self) -> String {
        self.space.space_id().to_string()
    }

    fn id(&mut self) -> String {
        self.ids.uuid_v7(NOW as u64)
    }

    fn client(&self, name: &str) -> Client {
        self.client_with(name, Role::Owner, 1)
    }

    fn client_with(&self, name: &str, role: Role, max_format: u32) -> Client {
        let session = Session {
            device_id: format!("dev-{name}"),
            user_id: "user".into(),
            device_name: name.into(),
            role,
            max_format,
        };
        Client {
            session,
            base_seq: 0,
        }
    }

    fn content(text: &str) -> Content {
        content_of(vec![chunk(text.as_bytes(), text.len() as u32)])
    }

    fn upload(&mut self, c: &Client, content: &Content) -> Vec<UploadReceipt> {
        let plan = self
            .space
            .prepare_upload(
                &c.session,
                NOW,
                &PrepareUpload {
                    chunks: content.chunks.clone(),
                },
            )
            .unwrap();
        plan.chunks
            .iter()
            .zip(&content.chunks)
            .filter(|(p, _)| p.action == chunk_plan::Action::Upload as i32)
            .map(|(_, chunk)| self.space.put_chunk(&chunk.hash, chunk.size, NOW))
            .collect()
    }

    fn commit_raw(
        &mut self,
        c: &Client,
        ops: Vec<Op>,
        base_seq: u64,
        receipts: Vec<UploadReceipt>,
        commit_id: Option<String>,
    ) -> Result<CommitResult, Error> {
        let commit_id = commit_id.unwrap_or_else(|| self.id());
        let next = self.space.device_seq(&c.session.device_id) + 1;
        let device_seq = *self.sent.entry(commit_id.clone()).or_insert(next);
        let commit = Commit {
            commit_id,
            base_seq,
            ops,
            receipts,
            history_id: self.space.history_id().into(),
            device_seq,
            ..Default::default()
        };
        self.space
            .commit(&c.session, NOW, &commit)
            .map(|done| done.result)
    }

    fn commit(
        &mut self,
        c: &mut Client,
        ops: Vec<Op>,
        receipts: Vec<UploadReceipt>,
    ) -> CommitResult {
        let result = self.commit_raw(c, ops, c.base_seq, receipts, None).unwrap();
        c.base_seq = result.seq;
        result
    }

    fn create_file(
        &mut self,
        c: &mut Client,
        parent: &str,
        name: &str,
        text: &str,
    ) -> (String, CommitResult) {
        let content = Self::content(text);
        let receipts = self.upload(c, &content);
        let id = self.id();
        let result = self.commit(c, vec![create_file(&id, parent, name, content)], receipts);
        (id, result)
    }

    fn live(&self, node_id: &str) -> bool {
        self.space
            .node(node_id)
            .is_some_and(|n| n.status == NodeStatus::Live as i32)
    }

    fn get(&self, node_id: &str) -> NodeState {
        self.space.node(node_id).unwrap()
    }

    fn live_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .space
            .nodes()
            .into_iter()
            .filter(|n| n.status == NodeStatus::Live as i32)
            .map(|n| n.name)
            .collect();
        names.sort();
        names
    }
}

fn chunk(seed: &[u8], size: u32) -> ChunkRef {
    ChunkRef {
        hash: Sha256::digest(seed).to_vec(),
        size,
        encoding: 0,
    }
}

fn content_of(chunks: Vec<ChunkRef>) -> Content {
    let list: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
        .collect();
    Content {
        size: chunks.iter().map(|c| u64::from(c.size)).sum(),
        mtime_ns: 1,
        executable: false,
        chunk_count: chunks.len() as u32,
        manifest_digest: Sha256::digest(list).to_vec(),
        chunker: "fastcdc2020-1m-4m-16m".into(),
        chunks,
    }
}

fn wrap(op: op::Op) -> Op {
    Op { op: Some(op) }
}

fn mkdir(id: &str, parent: &str, name: &str) -> Op {
    wrap(op::Op::CreateDir(CreateDir {
        node_id: id.into(),
        parent_id: parent.into(),
        name: name.into(),
        pending_for: String::new(),
    }))
}

fn create_file(id: &str, parent: &str, name: &str, content: Content) -> Op {
    wrap(op::Op::CreateFile(CreateFile {
        node_id: id.into(),
        parent_id: parent.into(),
        name: name.into(),
        content: Some(content),
    }))
}

fn update(id: &str, content: Content) -> Op {
    wrap(op::Op::UpdateContent(UpdateContent {
        node_id: id.into(),
        content: Some(content),
        resend: false,
    }))
}

fn mv(id: &str, parent: &str, name: &str) -> Op {
    wrap(op::Op::Move(Move {
        node_id: id.into(),
        new_parent_id: parent.into(),
        new_name: name.into(),
        from: None,
        pending_for: String::new(),
    }))
}

fn delete(id: &str) -> Op {
    wrap(op::Op::Delete(Delete {
        node_id: id.into(),
        requires: None,
    }))
}

fn restore(id: &str, version_id: &str) -> Op {
    wrap(op::Op::Restore(Restore {
        node_id: id.into(),
        version_id: version_id.into(),
        ..Default::default()
    }))
}

fn codes(result: &CommitResult) -> Vec<op_result::Code> {
    result.results.iter().map(|r| r.code()).collect()
}

fn error_code(result: Result<CommitResult, Error>) -> error::Code {
    result.unwrap_err().code()
}

#[test]
fn two_devices_commit_and_see_each_others_changes() {
    let mut s = Space::new();
    let (mut a, mut b) = (s.client("Laptop A"), s.client("Desktop B"));
    let (root, docs) = (s.root(), s.id());
    let notes = Space::content("hello from A");
    let receipts = s.upload(&a, &notes);
    let file = s.id();
    let first = s.commit(
        &mut a,
        vec![
            mkdir(&docs, &root, "Docs"),
            create_file(&file, &docs, "a.txt", notes),
        ],
        receipts,
    );
    assert_eq!(first.seq, 1);
    assert_eq!(
        codes(&first),
        [op_result::Code::Applied, op_result::Code::Applied]
    );
    assert_eq!(first.changes.len(), 2);

    b.base_seq = 1;
    let (_, second) = s.create_file(&mut b, &docs, "b.txt", "hello from B");
    assert_eq!(second.seq, 2);
    assert_eq!(s.live_names(), ["", "Docs", "a.txt", "b.txt"]);
}

#[test]
fn returns_the_stored_result_when_a_commit_id_is_retried() {
    let mut s = Space::new();
    let a = s.client("A");
    let (root, commit_id) = (s.root(), s.id());
    let (once, twice) = (s.id(), s.id());
    let first = s
        .commit_raw(
            &a,
            vec![mkdir(&once, &root, "Once")],
            0,
            vec![],
            Some(commit_id.clone()),
        )
        .unwrap();
    let again = s
        .commit_raw(
            &a,
            vec![mkdir(&once, &root, "Once")],
            0,
            vec![],
            Some(commit_id.clone()),
        )
        .unwrap();
    assert_eq!(again, first);
    let other = s
        .commit_raw(
            &a,
            vec![mkdir(&twice, &root, "Twice")],
            0,
            vec![],
            Some(commit_id),
        )
        .unwrap_err();
    assert_eq!(error_code(Err(other)), error::Code::InvalidArgument);
    assert!(!s.live_names().contains(&"Twice".to_string()));
    assert_eq!(s.space.head_seq(), 1);
}

#[test]
fn rejects_a_base_ahead_of_the_server_and_commits_from_viewers_or_old_clients() {
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let x = s.id();
    assert_eq!(
        error_code(s.commit_raw(&a, vec![mkdir(&x, &root, "X")], 5, vec![], None)),
        error::Code::AheadOfServer
    );
    let viewer = s.client_with("Viewer", Role::Viewer, 1);
    assert_eq!(
        error_code(s.commit_raw(&viewer, vec![mkdir(&x, &root, "X")], 0, vec![], None)),
        error::Code::Forbidden
    );
    let old = s.client_with("Old", Role::Editor, 0);
    assert_eq!(
        error_code(s.commit_raw(&old, vec![mkdir(&x, &root, "X")], 0, vec![], None)),
        error::Code::ReadOnly
    );
    assert!(s.space.welcome(&old.session).read_only);
    assert!(!s.space.welcome(&a.session).read_only);
}

#[test]
fn welcome_reports_the_devices_highest_accepted_device_seq() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, b) = (s.client("A"), s.client("B"));
    assert_eq!(s.space.welcome(&a.session).device_seq, 0);
    for name in ["X", "Y"] {
        let id = s.id();
        s.commit(&mut a, vec![mkdir(&id, &root, name)], vec![]);
    }
    let refused = s.id();
    assert_eq!(
        error_code(s.commit_raw(&a, vec![mkdir(&refused, &root, "Z")], 9, vec![], None)),
        error::Code::AheadOfServer
    );
    assert_eq!(s.space.welcome(&a.session).device_seq, 2);
    assert_eq!(s.space.welcome(&b.session).device_seq, 0);
}

#[test]
fn requires_a_receipt_or_a_stored_object_for_every_pending_chunk() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let content = Space::content("chunk bytes");
    s.space
        .prepare_upload(
            &a.session,
            NOW,
            &PrepareUpload {
                chunks: content.chunks.clone(),
            },
        )
        .unwrap();
    let f = s.id();
    let missing = s
        .commit_raw(
            &a,
            vec![create_file(&f, &root, "f.bin", content.clone())],
            0,
            vec![],
            None,
        )
        .unwrap_err();
    assert_eq!(missing.code(), error::Code::MissingChunks);
    assert_eq!(missing.missing_chunks, [content.chunks[0].hash.clone()]);
    let forged = UploadReceipt {
        hash: content.chunks[0].hash.clone(),
        size: content.chunks[0].size,
        token: "forged".into(),
    };
    assert_eq!(
        error_code(s.commit_raw(
            &a,
            vec![create_file(&f, &root, "f.bin", content.clone())],
            0,
            vec![forged],
            None
        )),
        error::Code::MissingChunks
    );
    let receipts = s.upload(&a, &content);
    s.commit(
        &mut a,
        vec![create_file(&f, &root, "f.bin", content.clone())],
        receipts,
    );

    let plan = s
        .space
        .prepare_upload(
            &b.session,
            NOW,
            &PrepareUpload {
                chunks: content.chunks.clone(),
            },
        )
        .unwrap();
    assert_eq!(plan.chunks[0].action, chunk_plan::Action::Present as i32);
    b.base_seq = 1;
    let copy = s.id();
    let reuse = s.commit(
        &mut b,
        vec![create_file(&copy, &root, "copy.bin", content)],
        vec![],
    );
    assert_eq!(codes(&reuse), [op_result::Code::Applied]);
}

#[test]
fn rejects_malformed_content_and_ids() {
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let mut content = Space::content("abc");
    let receipts = s.upload(&a, &content);
    content.size = 4;
    let f = s.id();
    assert_eq!(
        error_code(s.commit_raw(
            &a,
            vec![create_file(&f, &root, "bad", content)],
            0,
            receipts,
            None
        )),
        error::Code::InvalidArgument
    );
    assert_eq!(
        error_code(s.commit_raw(&a, vec![mkdir("not-a-uuid", &root, "x")], 0, vec![], None)),
        error::Code::InvalidArgument
    );
    assert_eq!(
        error_code(s.commit_raw(&a, vec![Op { op: None }], 0, vec![], None)),
        error::Code::InvalidArgument
    );
    assert_eq!(
        error_code(s.commit_raw(&a, vec![], 0, vec![], None)),
        error::Code::InvalidArgument
    );
    let many = (0..1001).map(|_| delete(&root)).collect();
    assert_eq!(
        error_code(s.commit_raw(&a, many, 0, vec![], None)),
        error::Code::LimitExceeded
    );
    assert_eq!(s.space.head_seq(), 0);
}

#[test]
fn r1_renames_an_incoming_node_whose_name_clashes_case_insensitively() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("Laptop A"), s.client("Desktop B"));
    s.create_file(&mut a, &root, "Notes.txt", "a");
    let (_, result) = s.create_file(&mut b, &root, "notes.TXT", "b");
    assert_eq!(result.results[0].code(), op_result::Code::AppliedRenamed);
    assert_eq!(
        result.results[0].name,
        conflict_name("notes.TXT", "Desktop B", NOW, 1)
    );
    assert_eq!(
        result.results[0].name,
        "notes (conflict from Desktop B 2026-11-09).TXT"
    );
    let (_, again) = s.create_file(&mut b, &root, "NOTES.txt", "c");
    assert_eq!(
        again.results[0].name,
        "NOTES (conflict from Desktop B 2026-11-09 2).txt"
    );
}

#[test]
fn r2_treats_canonically_equivalent_names_that_differ_only_by_case_as_one_name() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let first = s.id();
    s.commit(&mut a, vec![mkdir(&first, &root, "\u{0390}")], vec![]);
    let second = s.id();
    let result = s.commit(
        &mut b,
        vec![mkdir(&second, &root, "\u{03AA}\u{0301}")],
        vec![],
    );
    assert_eq!(result.results[0].code(), op_result::Code::MergedInto);
    assert_eq!(result.results[0].node_id, first);
}

#[test]
fn r2_merges_a_new_directory_into_an_existing_one_with_the_same_name() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (photos_a, photos_b, file) = (s.id(), s.id(), s.id());
    s.commit(&mut a, vec![mkdir(&photos_a, &root, "Photos")], vec![]);
    let content = Space::content("img");
    let receipts = s.upload(&b, &content);
    let result = s.commit(
        &mut b,
        vec![
            mkdir(&photos_b, &root, "photos"),
            create_file(&file, &photos_b, "1.jpg", content),
        ],
        receipts,
    );
    assert_eq!(result.results[0].code(), op_result::Code::MergedInto);
    assert_eq!(
        (
            result.results[0].node_id.as_str(),
            result.results[0].name.as_str()
        ),
        (photos_a.as_str(), "Photos")
    );
    assert_eq!(s.get(&file).parent_id, photos_a);
    assert!(s.space.node(&photos_b).is_none());
}

#[test]
fn r2_does_not_merge_a_directory_into_a_file() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    s.create_file(&mut a, &root, "Thing", "x");
    let dir = s.id();
    let result = s.commit(&mut a, vec![mkdir(&dir, &root, "thing")], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::AppliedRenamed);
}

#[test]
fn r3_keeps_the_first_edit_and_turns_a_concurrent_edit_into_a_conflict_copy() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("Laptop A"), s.client("Desktop B"));
    let (node, _) = s.create_file(&mut a, &root, "report.docx", "v1");
    b.base_seq = 1;
    let from_b = Space::content("v2 from B");
    let receipts = s.upload(&b, &from_b);
    s.commit(&mut b, vec![update(&node, from_b.clone())], receipts);

    let from_a = Space::content("v2 from A");
    let receipts = s.upload(&a, &from_a);
    let result = s
        .commit_raw(&a, vec![update(&node, from_a.clone())], 1, receipts, None)
        .unwrap();
    let copy = &result.results[0];
    assert_eq!(copy.code(), op_result::Code::ConflictCopy);
    assert_ne!(copy.node_id, node);
    assert_eq!(copy.name, conflict_name("report.docx", "Laptop A", NOW, 1));
    let digest = |id: &str| s.get(id).current.unwrap().content.unwrap().manifest_digest;
    assert_eq!(digest(&node), from_b.manifest_digest);
    assert_eq!(digest(&copy.node_id), from_a.manifest_digest);
    assert_eq!(s.get(&copy.node_id).parent_id, root);
}

#[test]
fn r3_does_not_treat_a_devices_own_earlier_commit_as_unseen() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (node, _) = s.create_file(&mut a, &root, "draft.md", "1");
    for text in ["2", "3"] {
        let content = Space::content(text);
        let receipts = s.upload(&a, &content);
        let result = s
            .commit_raw(&a, vec![update(&node, content)], 1, receipts, None)
            .unwrap();
        assert_eq!(codes(&result), [op_result::Code::Applied]);
    }
}

#[test]
fn r4_applies_an_edit_over_an_unseen_rename() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (node, _) = s.create_file(&mut a, &root, "old.txt", "1");
    b.base_seq = 1;
    s.commit(&mut b, vec![mv(&node, &root, "new.txt")], vec![]);
    let edit = Space::content("2");
    let receipts = s.upload(&a, &edit);
    let result = s
        .commit_raw(&a, vec![update(&node, edit.clone())], 1, receipts, None)
        .unwrap();
    assert_eq!(codes(&result), [op_result::Code::Applied]);
    let state = s.get(&node);
    assert_eq!(state.name, "new.txt");
    assert_eq!(
        state.current.unwrap().content.unwrap().manifest_digest,
        edit.manifest_digest
    );
}

#[test]
fn r5_and_r6_apply_moves_in_commit_order_and_reject_a_cycle() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (x, y) = (s.id(), s.id());
    s.commit(
        &mut a,
        vec![mkdir(&x, &root, "X"), mkdir(&y, &root, "Y")],
        vec![],
    );
    b.base_seq = 1;
    s.commit(&mut a, vec![mv(&x, &y, "X")], vec![]);
    let result = s.commit(&mut b, vec![mv(&y, &x, "Y")], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::Rejected);
    assert_eq!(result.results[0].reject_reason, "CYCLE");
    assert_eq!(s.get(&x).parent_id, y);
    assert_eq!(s.get(&y).parent_id, root);
}

#[test]
fn r7_keeps_nodes_with_unseen_changes_and_their_ancestors_when_deleting() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (dir, sub) = (s.id(), s.id());
    s.commit(
        &mut a,
        vec![mkdir(&dir, &root, "Project"), mkdir(&sub, &dir, "src")],
        vec![],
    );
    let (edited, _) = s.create_file(&mut a, &sub, "main.rs", "fn main() {}");
    let (untouched, _) = s.create_file(&mut a, &dir, "README", "readme");
    b.base_seq = a.base_seq;
    let edit = Space::content("fn main() { edited }");
    let receipts = s.upload(&a, &edit);
    s.commit(&mut a, vec![update(&edited, edit)], receipts);

    let result = s.commit(&mut b, vec![delete(&dir)], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::Superseded);
    assert!([&dir, &sub, &edited].iter().all(|id| s.live(id)));
    assert!(!s.live(&untouched));
    let trash = s.space.list_trash(&ListTrash::default());
    assert_eq!(
        trash
            .nodes
            .iter()
            .map(|n| n.node_id.clone())
            .collect::<Vec<_>>(),
        [untouched]
    );
}

#[test]
fn r7_trashes_a_whole_subtree_under_one_trash_group() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let dir = s.id();
    s.commit(&mut a, vec![mkdir(&dir, &root, "Old")], vec![]);
    let (file, _) = s.create_file(&mut a, &dir, "x", "x");
    let commit_id = s.id();
    let result = s
        .commit_raw(
            &a,
            vec![delete(&dir)],
            a.base_seq,
            vec![],
            Some(commit_id.clone()),
        )
        .unwrap();
    assert_eq!(codes(&result), [op_result::Code::Applied]);
    let mut ids: Vec<&str> = node_states(&result.changes)
        .map(|n| n.node_id.as_str())
        .collect();
    ids.sort();
    let mut expected = [dir.as_str(), file.as_str()];
    expected.sort();
    assert_eq!(ids, expected);
    assert!(node_states(&result.changes).all(
        |n| n.trash_group == format!("{commit_id}:0") && n.status == NodeStatus::Trashed as i32
    ));
    let again = s.commit(&mut a, vec![delete(&dir)], vec![]);
    assert_eq!(codes(&again), [op_result::Code::Applied]);
    assert!(again.changes.is_empty());
}

#[test]
fn deletes_in_one_commit_take_trash_groups_of_its_id() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (first, second) = (s.id(), s.id());
    s.commit(
        &mut a,
        vec![
            mkdir(&first, &root, "First"),
            mkdir(&second, &root, "Second"),
        ],
        vec![],
    );
    let commit_id = s.id();
    let result = s
        .commit_raw(
            &a,
            vec![delete(&first), delete(&second)],
            a.base_seq,
            vec![],
            Some(commit_id.clone()),
        )
        .unwrap();
    let groups: BTreeMap<&str, String> = node_states(&result.changes)
        .map(|n| (n.node_id.as_str(), n.trash_group.clone()))
        .collect();
    assert_eq!(
        groups,
        BTreeMap::from([
            (first.as_str(), format!("{commit_id}:0")),
            (second.as_str(), format!("{commit_id}:1")),
        ])
    );
}

#[test]
fn a_delete_at_a_lost_seq_takes_another_trash_group() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let dir = s.id();
    s.commit(&mut a, vec![mkdir(&dir, &root, "Dir")], vec![]);
    let backup = s.space.clone();
    let lost = s
        .commit_raw(&a, vec![delete(&dir)], a.base_seq, vec![], None)
        .unwrap();
    s.space.restore(&backup);
    let again = s
        .commit_raw(&a, vec![delete(&dir)], a.base_seq, vec![], None)
        .unwrap();
    assert_eq!(again.seq, lost.seq);
    let group = |result: &CommitResult| {
        node_states(&result.changes)
            .next()
            .unwrap()
            .trash_group
            .clone()
    };
    assert_ne!(group(&again), group(&lost));
}

#[test]
fn r8_an_unseen_edit_revives_a_deleted_file_and_creating_inside_a_trashed_folder_revives_the_folder()
 {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let dir = s.id();
    s.commit(&mut a, vec![mkdir(&dir, &root, "Shared")], vec![]);
    let (file, _) = s.create_file(&mut a, &dir, "notes.txt", "1");
    b.base_seq = a.base_seq;
    s.commit(&mut a, vec![delete(&dir)], vec![]);

    let edit = Space::content("edited on B");
    let receipts = s.upload(&b, &edit);
    let revived = s.commit(&mut b, vec![update(&file, edit)], receipts);
    assert_eq!(codes(&revived), [op_result::Code::Applied]);
    assert!(s.live(&file) && s.live(&dir));

    let base = b.base_seq;
    s.commit_raw(&a, vec![delete(&dir)], base, vec![], None)
        .unwrap();
    let created = s.id();
    s.commit_raw(
        &b,
        vec![mkdir(&created, &dir, "new")],
        base + 1,
        vec![],
        None,
    )
    .unwrap();
    assert!(s.live(&dir) && s.live(&created));
    assert!(!s.live(&file));
}

#[test]
fn r9_and_r10_restore_from_the_trash_and_restore_an_old_version() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let dir = s.id();
    s.commit(&mut a, vec![mkdir(&dir, &root, "Album")], vec![]);
    let (file, created) = s.create_file(&mut a, &dir, "pic.png", "first");
    let first_version = node_states(&created.changes)
        .find(|n| n.node_id == file)
        .unwrap()
        .current
        .clone()
        .unwrap()
        .version_id;
    let second = Space::content("second");
    let receipts = s.upload(&a, &second);
    s.commit(&mut a, vec![update(&file, second)], receipts);

    s.commit(&mut a, vec![delete(&dir)], vec![]);
    let restored = s.commit(&mut a, vec![restore(&dir, "")], vec![]);
    assert_eq!(codes(&restored), [op_result::Code::Applied]);
    assert!(s.live(&dir) && s.live(&file));

    let back = s.commit(&mut a, vec![restore(&file, &first_version)], vec![]);
    assert_eq!(codes(&back), [op_result::Code::Applied]);
    let current = s.get(&file).current.unwrap();
    assert_ne!(current.version_id, first_version);
    assert_eq!(
        current.content.unwrap().manifest_digest,
        Space::content("first").manifest_digest
    );

    let versions = s
        .space
        .list_versions(&ListVersions {
            node_id: file.clone(),
            limit: 2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(versions.versions.len(), 2);
    assert_ne!(versions.next_cursor, "");
    let rest = s
        .space
        .list_versions(&ListVersions {
            node_id: file,
            limit: 2,
            cursor: versions.next_cursor,
        })
        .unwrap();
    assert_eq!(rest.versions.len(), 1);
    assert_eq!(rest.next_cursor, "");
}

#[test]
fn r11_rejects_invalid_ops_without_failing_the_commit() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (file, _) = s.create_file(&mut a, &root, "file", "x");
    let (under, missing, dot, dir, update_dir) = (s.id(), s.id(), s.id(), s.id(), s.id());
    let result = s.commit(
        &mut a,
        vec![
            mv(&root, &root, "root"),
            delete(&root),
            mkdir(&under, &file, "under a file"),
            delete(&missing),
            mkdir(&file, &root, "duplicate id"),
            mkdir(&dot, &root, ".."),
            mkdir(&dir, &root, "fine"),
            update(&dir, Space::content("x")),
            mkdir(&update_dir, &root, "fine too"),
        ],
        vec![],
    );
    let reasons: Vec<&str> = result
        .results
        .iter()
        .map(|r| r.reject_reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        [
            "ROOT",
            "ROOT",
            "PARENT_NOT_DIR",
            "NOT_FOUND",
            "ID_EXISTS",
            "INVALID_NAME",
            "",
            "NOT_FILE",
            ""
        ]
    );
    assert_eq!(result.results[6].code(), op_result::Code::Applied);
}

#[test]
fn treats_names_that_differ_only_by_unicode_case_folding_as_the_same() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (x, y, z) = (s.id(), s.id(), s.id());
    s.commit(&mut a, vec![mkdir(&x, &root, "Straße")], vec![]);
    let result = s.commit(&mut a, vec![mkdir(&y, &root, "STRASSE")], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::MergedInto);
    let nfd = s.commit(&mut a, vec![mkdir(&z, &root, "Cafe\u{301}")], vec![]);
    assert_eq!(nfd.results[0].name, "Café");
}

#[test]
fn a_commit_that_fails_while_applying_leaves_no_trace() {
    let mut s = Space::new();
    let mut a = s.client("A");
    let mut parent = s.root();
    let chain: Vec<String> = (0..257).map(|_| s.id()).collect();
    let ops = chain
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let op = mkdir(id, &parent, &format!("d{i}"));
            parent = id.clone();
            op
        })
        .collect();
    let deep = s.commit_raw(&a, ops, 0, vec![], None).unwrap_err();
    assert_eq!(deep.code(), error::Code::LimitExceeded);
    assert_eq!(s.space.head_seq(), 0);
    assert_eq!(s.live_names(), [""]);
    let ok = s.id();
    let root = s.root();
    assert_eq!(
        s.commit(&mut a, vec![mkdir(&ok, &root, "ok")], vec![]).seq,
        1
    );
}

#[test]
fn resumes_from_the_log_or_asks_for_a_resync() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    for name in ["a", "b", "c"] {
        let id = s.id();
        s.commit(&mut a, vec![mkdir(&id, &root, name)], vec![]);
    }
    let history_id = s.space.history_id().to_string();
    let since = |since_seq| Resume {
        history_id: history_id.clone(),
        since_seq,
        ..Default::default()
    };
    let backlog = s.space.resume(&since(1)).unwrap();
    assert_eq!(backlog.iter().map(|r| r.seq).collect::<Vec<_>>(), [2, 3]);
    let reason = |r: Resume| s.space.resume(&r).unwrap_err().reason();
    assert_eq!(reason(since(4)), resync_required::Reason::AheadOfServer);
    assert_eq!(
        reason(Resume {
            history_id: "other".into(),
            ..Default::default()
        }),
        resync_required::Reason::HistoryChanged
    );

    s.space.truncate_log(2);
    assert_eq!(s.space.log_floor_seq(), 2);
    assert_eq!(
        s.space.resume(&since(1)).unwrap_err().reason(),
        resync_required::Reason::LogTruncated
    );
    assert_eq!(
        s.space
            .resume(&since(2))
            .unwrap()
            .iter()
            .map(|r| r.seq)
            .collect::<Vec<_>>(),
        [3]
    );
}

#[test]
fn lists_the_live_tree_in_pages() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let ops = (0..25)
        .map(|i| {
            let id = s.id();
            mkdir(&id, &root, &format!("d{i}"))
        })
        .collect();
    s.commit(&mut a, ops, vec![]);
    let mut seen = Vec::new();
    let mut cursor = String::new();
    loop {
        let page = s.space.list_tree(&ListTree {
            cursor: cursor.clone(),
            limit: 10,
        });
        assert_eq!(page.head_seq, 1);
        seen.extend(page.nodes.into_iter().map(|n| n.node_id));
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), 26);
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn answers_rpcs_and_returns_the_record_only_for_a_new_commit() {
    use koidrive_proto::v1::{RpcRequest, rpc_request, rpc_response};
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let (commit_id, dir) = (s.id(), s.id());
    let commit = Commit {
        commit_id,
        base_seq: 0,
        ops: vec![mkdir(&dir, &root, "D")],
        history_id: s.space.history_id().into(),
        device_seq: 1,
        ..Default::default()
    };
    let request = RpcRequest {
        req_id: 7,
        call: Some(rpc_request::Call::Commit(commit)),
    };
    let (response, record) = s.space.rpc(&a.session, NOW, &request);
    assert_eq!(response.req_id, 7);
    assert!(matches!(response.result, Some(rpc_response::Result::Commit(ref r)) if r.seq == 1));
    assert_eq!(record.map(|r| r.seq), Some(1));
    let (again, record) = s.space.rpc(&a.session, NOW, &request);
    assert_eq!(again, response);
    assert!(record.is_none());

    let viewer = s.client_with("V", Role::Viewer, 1);
    let (denied, _) = s.space.rpc(
        &viewer.session,
        NOW,
        &RpcRequest {
            req_id: 8,
            call: Some(rpc_request::Call::PrepareUpload(PrepareUpload::default())),
        },
    );
    assert!(
        matches!(denied.result, Some(rpc_response::Result::Error(ref e)) if e.code() == error::Code::Forbidden)
    );
    let (empty, _) = s.space.rpc(
        &a.session,
        NOW,
        &RpcRequest {
            req_id: 9,
            call: None,
        },
    );
    assert!(
        matches!(empty.result, Some(rpc_response::Result::Error(ref e)) if e.code() == error::Code::InvalidArgument)
    );
}

#[test]
fn serves_downloads_only_for_committed_chunks() {
    use koidrive_proto::v1::{GetDownloadTargets, GetVersionChunks};
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let content = Space::content("bytes");
    let receipts = s.upload(&a, &content);
    let hashes = vec![content.chunks[0].hash.clone()];
    let pending = s
        .space
        .download_targets(
            NOW,
            &GetDownloadTargets {
                hashes: hashes.clone(),
            },
        )
        .unwrap_err();
    assert_eq!(pending.code(), error::Code::MissingChunks);
    let f = s.id();
    let created = s.commit(
        &mut a,
        vec![create_file(&f, &root, "f", content.clone())],
        receipts,
    );
    let targets = s
        .space
        .download_targets(NOW, &GetDownloadTargets { hashes })
        .unwrap();
    assert_eq!(targets.targets.len(), 1);
    assert_eq!(targets.targets[0].target.as_ref().unwrap().method, "GET");

    let version_id = node_states(&created.changes)
        .next()
        .unwrap()
        .current
        .clone()
        .unwrap()
        .version_id;
    let page = s
        .space
        .version_chunks(&GetVersionChunks {
            version_id: version_id.clone(),
            page: 0,
        })
        .unwrap();
    assert_eq!((page.chunks, page.page_count), (content.chunks, 1));
    assert_eq!(
        s.space
            .version_chunks(&GetVersionChunks {
                version_id,
                page: 1
            })
            .unwrap_err()
            .code(),
        error::Code::InvalidArgument
    );
}

fn restore_to(id: &str, parent: &str, name: &str) -> Op {
    wrap(op::Op::Restore(Restore {
        node_id: id.into(),
        target_parent_id: parent.into(),
        name: name.into(),
        ..Default::default()
    }))
}

fn reject_reasons(result: &CommitResult) -> Vec<&str> {
    result
        .results
        .iter()
        .map(|r| r.reject_reason.as_str())
        .collect()
}

#[test]
fn rejects_one_chunk_hash_listed_with_two_sizes() {
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let (three, two) = (chunk(b"h", 3), chunk(b"h", 2));
    let prepared = s.space.prepare_upload(
        &a.session,
        NOW,
        &PrepareUpload {
            chunks: vec![three.clone(), two.clone()],
        },
    );
    assert_eq!(prepared.unwrap_err().code(), error::Code::InvalidArgument);

    let receipts = s.upload(&a, &content_of(vec![three.clone()]));
    let within = content_of(vec![three.clone(), two.clone()]);
    let id = s.id();
    let one_manifest = s.commit_raw(
        &a,
        vec![create_file(&id, &root, "within", within)],
        0,
        receipts.clone(),
        None,
    );
    assert_eq!(error_code(one_manifest), error::Code::InvalidArgument);
    let (x, y) = (s.id(), s.id());
    let across_ops = s.commit_raw(
        &a,
        vec![
            create_file(&x, &root, "x", content_of(vec![three])),
            create_file(&y, &root, "y", content_of(vec![two])),
        ],
        0,
        receipts,
        None,
    );
    assert_eq!(error_code(across_ops), error::Code::InvalidArgument);
    assert_eq!(s.space.head_seq(), 0);
}

#[test]
fn moving_or_restoring_a_subtree_cannot_exceed_the_depth_limit() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let mut chain = Vec::new();
    let mut ops = Vec::new();
    let mut parent = root.clone();
    for depth in 1..=254 {
        let id = s.id();
        ops.push(mkdir(&id, &parent, &format!("d{depth}")));
        parent = id.clone();
        chain.push(id);
    }
    let (top, mid, leaf) = (s.id(), s.id(), s.id());
    ops.extend([
        mkdir(&top, &root, "top"),
        mkdir(&mid, &top, "mid"),
        mkdir(&leaf, &mid, "leaf"),
    ]);
    s.commit(&mut a, ops, vec![]);
    let (deepest, shallower) = (&chain[253], &chain[251]);
    let head = s.space.head_seq();

    let moved = s.commit_raw(&a, vec![mv(&top, deepest, "top")], head, vec![], None);
    assert_eq!(error_code(moved), error::Code::LimitExceeded);
    assert_eq!(s.get(&top).parent_id, root);

    s.commit(&mut a, vec![delete(&leaf)], vec![]);
    let head = s.space.head_seq();
    let with_trashed_leaf = s.commit_raw(&a, vec![mv(&top, deepest, "top")], head, vec![], None);
    assert_eq!(error_code(with_trashed_leaf), error::Code::LimitExceeded);

    s.commit(&mut a, vec![delete(&top)], vec![]);
    let head = s.space.head_seq();
    let restored = s.commit_raw(&a, vec![restore_to(&top, deepest, "")], head, vec![], None);
    assert_eq!(error_code(restored), error::Code::LimitExceeded);
    assert_eq!(s.space.head_seq(), head);
    assert!(!s.live(&top) && !s.live(&mid));

    let fits = s.commit(&mut a, vec![restore_to(&top, shallower, "")], vec![]);
    assert_eq!(codes(&fits), [op_result::Code::Applied]);
    assert!(s.live(&top) && s.live(&mid) && !s.live(&leaf));
}

#[test]
fn version_pages_split_versions_of_one_commit_by_version_id() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (one, two, three) = (
        Space::content("one"),
        Space::content("two"),
        Space::content("three"),
    );
    let mut receipts = s.upload(&a, &one);
    receipts.extend(s.upload(&a, &two));
    receipts.extend(s.upload(&a, &three));
    let file = s.id();
    s.commit(
        &mut a,
        vec![
            create_file(&file, &root, "f", one),
            update(&file, two),
            update(&file, three),
        ],
        receipts,
    );
    let list = |cursor: String, limit: u32| {
        s.space.list_versions(&ListVersions {
            node_id: file.clone(),
            limit,
            cursor,
        })
    };
    let all: Vec<String> = list(String::new(), 10)
        .unwrap()
        .versions
        .into_iter()
        .map(|v| v.version_id)
        .collect();
    assert_eq!(all.len(), 3);
    let mut sorted = all.clone();
    sorted.sort();
    sorted.reverse();
    assert_eq!(all, sorted, "newest first by (seq, version_id)");

    let (mut paged, mut cursor) = (Vec::new(), String::new());
    loop {
        let page = list(cursor, 1).unwrap();
        paged.extend(page.versions.into_iter().map(|v| v.version_id));
        if page.next_cursor.is_empty() {
            break;
        }
        assert_eq!(page.next_cursor, format!("1/{}", paged.last().unwrap()));
        cursor = page.next_cursor;
    }
    assert_eq!(paged, all);
    assert_eq!(
        list("one/x".into(), 1).unwrap_err().code(),
        error::Code::InvalidArgument
    );
}

#[test]
fn version_cursors_follow_the_servers_number_parsing() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (one, two) = (Space::content("one"), Space::content("two"));
    let mut receipts = s.upload(&a, &one);
    receipts.extend(s.upload(&a, &two));
    let file = s.id();
    s.commit(
        &mut a,
        vec![create_file(&file, &root, "f", one), update(&file, two)],
        receipts,
    );
    let request = |cursor: &str| ListVersions {
        node_id: file.clone(),
        limit: 10,
        cursor: cursor.into(),
    };
    for (cursor, count) in [
        ("9007199254740991/x", 2),
        ("2", 2),
        ("1e0/x", 2),
        ("1.0/x", 2),
        ("+1/x", 2),
        (" 1\t/x", 2),
        ("0x2/", 2),
        ("/x", 0),
        ("-1/x", 0),
        ("1/", 0),
    ] {
        let page = s.space.list_versions(&request(cursor));
        assert_eq!(page.unwrap().versions.len(), count, "cursor {cursor:?}");
    }
    for cursor in [
        "9007199254740992/x",
        "-9007199254740992/x",
        "1.5/x",
        "1e400/x",
        "Infinity/x",
        "0x/x",
        "-0x1/x",
        "1_0/x",
        "seven/x",
        "\u{2603}/x",
        "\u{2603}",
        "0\u{2603}/x",
        "1\u{2603}/x",
        "0x\u{2603}/x",
    ] {
        assert_eq!(
            s.space.list_versions(&request(cursor)).unwrap_err().code(),
            error::Code::InvalidArgument,
            "cursor {cursor:?}"
        );
        let (response, _) = s.space.rpc(
            &a.session,
            NOW,
            &RpcRequest {
                req_id: 7,
                call: Some(rpc_request::Call::ListVersions(request(cursor))),
            },
        );
        let Some(rpc_response::Result::Error(failure)) = response.result else {
            panic!("cursor {cursor:?} was accepted over RPC");
        };
        assert_eq!(failure.code(), error::Code::InvalidArgument);
    }
}

#[test]
fn a_no_op_move_changes_nothing_so_a_concurrent_delete_applies() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (file, created) = s.create_file(&mut a, &root, "notes.txt", "a");
    b.base_seq = created.seq;
    let before = s.get(&file);

    let same = s.commit(&mut a, vec![mv(&file, &root, "notes.txt")], vec![]);
    assert_eq!(codes(&same), [op_result::Code::Applied]);
    assert_eq!(same.results[0].name, "notes.txt");
    assert!(same.changes.is_empty());
    assert_eq!(s.get(&file), before);

    let deleted = s.commit(&mut b, vec![delete(&file)], vec![]);
    assert_eq!(codes(&deleted), [op_result::Code::Applied]);
    assert!(!s.live(&file));

    let (other, _) = s.create_file(&mut a, &root, "case.txt", "c");
    let renamed = s.commit(&mut a, vec![mv(&other, &root, "Case.txt")], vec![]);
    assert_eq!(renamed.changes.len(), 1);
    assert_eq!(s.get(&other).meta_rev, renamed.seq);
}

#[test]
fn the_root_reserves_koidrive_in_every_case_for_every_op() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (docs, moved, trashed) = (s.id(), s.id(), s.id());
    s.commit(
        &mut a,
        vec![
            mkdir(&docs, &root, "docs"),
            mkdir(&moved, &docs, "moved"),
            mkdir(&trashed, &docs, "trashed"),
        ],
        vec![],
    );
    s.commit(&mut a, vec![delete(&trashed)], vec![]);
    let before = s.space.nodes();
    let (dir, file, alias) = (s.id(), s.id(), s.id());
    let content = Space::content("x");
    let receipts = s.upload(&a, &content);
    let result = s.commit(
        &mut a,
        vec![
            mkdir(&dir, &root, ".Koidrive"),
            create_file(&file, &root, ".KOIDRIVE", content),
            mv(&moved, &root, ".koidrive"),
            mv(&trashed, &root, ".kOiDrIvE"),
            restore_to(&trashed, &root, ".koidrive"),
            mkdir(&alias, &root, ".\u{212A}oidrive"),
        ],
        receipts,
    );
    assert_eq!(reject_reasons(&result), ["INVALID_NAME"; 6]);
    assert!(result.changes.is_empty());
    assert_eq!(s.space.nodes(), before);
    assert!(!s.live(&trashed));

    let below = s.id();
    let allowed = s.commit(&mut a, vec![mkdir(&below, &docs, ".koidrive")], vec![]);
    assert_eq!(codes(&allowed), [op_result::Code::Applied]);
}

#[test]
fn a_commit_whose_log_row_is_too_large_is_refused_whole() {
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let chunks: Vec<ChunkRef> = (0u32..16_384).map(|i| chunk(&i.to_le_bytes(), 1)).collect();
    let content = content_of(chunks.clone());
    let mut receipts = Vec::new();
    for part in chunks.chunks(1000) {
        receipts.extend(s.upload(&a, &content_of(part.to_vec())));
    }
    let files: Vec<String> = (0..7).map(|_| s.id()).collect();
    let ops = |n: usize| -> Vec<Op> {
        files[..n]
            .iter()
            .enumerate()
            .map(|(i, id)| create_file(id, &root, &format!("f{i}"), content.clone()))
            .collect()
    };
    let commit_id = s.id();
    for _ in 0..2 {
        let too_large = s.commit_raw(&a, ops(7), 0, receipts.clone(), Some(commit_id.clone()));
        assert_eq!(error_code(too_large), error::Code::LimitExceeded);
        assert_eq!(s.space.head_seq(), 0);
        assert!(s.space.node(&files[0]).is_none());
    }
    let two = s.commit_raw(&a, ops(2), 0, receipts.clone(), None);
    assert_eq!(error_code(two), error::Code::LimitExceeded);
    let one = s.commit_raw(&a, ops(1), 0, receipts, None).unwrap();
    assert_eq!(one.seq, 1);
}

#[test]
fn a_receipt_holds_only_for_its_space_and_size_and_a_stored_object_outlasts_it() {
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let content = Space::content("receipted");
    let (hash, size) = (&content.chunks[0].hash, content.chunks[0].size);
    s.space
        .prepare_upload(
            &a.session,
            NOW,
            &PrepareUpload {
                chunks: content.chunks.clone(),
            },
        )
        .unwrap();
    let mut other = SpaceDo::new(9, NOW);
    let foreign = other.put_chunk(hash, size, NOW);
    let wrong_size = s.space.put_chunk(hash, size + 1, NOW);
    let attempt = |s: &mut Space, receipts: Vec<UploadReceipt>, now_ms: i64| {
        let commit = Commit {
            commit_id: s.id(),
            base_seq: 0,
            ops: vec![create_file(&s.id(), &root, "f", content.clone())],
            receipts,
            history_id: s.space.history_id().into(),
            device_seq: s.space.device_seq(&a.session.device_id) + 1,
            ..Default::default()
        };
        s.space
            .commit(&a.session, now_ms, &commit)
            .map(|c| c.result)
    };
    for receipts in [vec![foreign], vec![wrong_size]] {
        assert_eq!(
            error_code(attempt(&mut s, receipts, NOW)),
            error::Code::MissingChunks
        );
    }
    let receipts = vec![s.space.put_chunk(hash, size, NOW)];
    assert!(attempt(&mut s, receipts, NOW + 3_600_001).is_ok());
}

#[test]
fn a_base_naming_a_commit_the_server_lost_is_ahead_of_the_server() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let first = s.id();
    s.commit(&mut a, vec![mkdir(&first, &root, "first")], vec![]);
    let backup = s.space.clone();
    let lost_id = s.id();
    let lost = s
        .commit_raw(
            &a,
            vec![mkdir(&lost_id, &root, "lost")],
            1,
            vec![],
            Some(lost_id.clone()),
        )
        .unwrap();
    assert_eq!(lost.seq, 2);
    s.space.restore(&backup);
    b.base_seq = 1;
    let kept_id = s.id();
    let kept = s
        .commit_raw(
            &b,
            vec![mkdir(&kept_id, &root, "kept")],
            1,
            vec![],
            Some(kept_id.clone()),
        )
        .unwrap();
    assert_eq!(kept.seq, 2);
    let history_id = s.space.history_id().to_string();
    let from = |commit_id: &str, committed_at_ms: i64| Resume {
        history_id: history_id.clone(),
        since_seq: 2,
        since_commit_id: commit_id.into(),
        since_committed_at_ms: committed_at_ms,
    };
    assert_eq!(
        s.space
            .resume(&from(&lost_id, lost.committed_at_ms))
            .unwrap_err()
            .reason(),
        resync_required::Reason::AheadOfServer
    );
    assert_eq!(
        s.space
            .resume(&from(&kept_id, kept.committed_at_ms))
            .unwrap(),
        []
    );
    let on = |s: &mut Space, commit_id: &str, committed_at_ms: i64| {
        let id = s.id();
        let commit = Commit {
            commit_id: s.id(),
            base_seq: 2,
            base_commit_id: commit_id.into(),
            base_committed_at_ms: committed_at_ms,
            ops: vec![mkdir(&id, &root, &id)],
            history_id: s.space.history_id().into(),
            device_seq: s.space.device_seq(&a.session.device_id) + 1,
            ..Default::default()
        };
        s.space
            .commit(&a.session, NOW, &commit)
            .map(|done| done.result)
    };
    assert_eq!(
        error_code(on(&mut s, &lost_id, lost.committed_at_ms)),
        error::Code::AheadOfServer
    );
    assert_eq!(s.space.head_seq(), 2);
    assert_eq!(on(&mut s, &kept_id, kept.committed_at_ms).unwrap().seq, 3);
}

#[test]
fn a_retry_that_changes_only_its_base_token_gets_the_stored_result() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let one = s.id();
    s.commit(&mut a, vec![mkdir(&one, &root, "one")], vec![]);
    let (base_commit_id, base_committed_at_ms) = s.space.token_at(1).unwrap();
    let two = s.id();
    let commit = Commit {
        commit_id: s.id(),
        history_id: s.space.history_id().into(),
        device_seq: 2,
        base_seq: 1,
        base_commit_id,
        base_committed_at_ms,
        ops: vec![mkdir(&two, &root, "two")],
        ..Default::default()
    };
    let first = s.space.commit(&a.session, NOW, &commit).unwrap().result;
    let other_time = Commit {
        base_committed_at_ms: base_committed_at_ms + 1,
        ..commit.clone()
    };
    let other_id = Commit {
        base_commit_id: s.id(),
        ..commit.clone()
    };
    for retry in [commit, other_time, other_id] {
        assert_eq!(
            s.space.commit(&a.session, NOW + 1, &retry).unwrap().result,
            first
        );
    }
}

#[test]
fn a_token_below_the_floor_claims_nothing() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    for name in ["one", "two", "three"] {
        let id = s.id();
        s.commit(&mut a, vec![mkdir(&id, &root, name)], vec![]);
    }
    let tokens: Vec<_> = (0..=2).map(|seq| s.space.token_at(seq).unwrap()).collect();
    s.space.truncate_log(2);
    let history_id = s.space.history_id().to_string();
    let wrong = |(id, at): &(String, i64)| (id.clone(), at + 1);
    let resume = |seq: u64, (id, at): (String, i64)| Resume {
        history_id: history_id.clone(),
        since_seq: seq,
        since_commit_id: id,
        since_committed_at_ms: at,
    };
    assert_eq!(
        s.space.resume(&resume(2, tokens[2].clone())).unwrap().len(),
        1
    );
    assert_eq!(
        s.space
            .resume(&resume(2, wrong(&tokens[2])))
            .unwrap_err()
            .reason(),
        resync_required::Reason::AheadOfServer
    );
    let based = |s: &mut Space, seq: u64, (id, at): (String, i64)| {
        let node = s.id();
        let commit = Commit {
            commit_id: s.id(),
            history_id: s.space.history_id().into(),
            device_seq: s.space.device_seq(&a.session.device_id) + 1,
            base_seq: seq,
            base_commit_id: id,
            base_committed_at_ms: at,
            ops: vec![mkdir(&node, &root, &node)],
            ..Default::default()
        };
        s.space
            .commit(&a.session, NOW, &commit)
            .map(|done| done.result)
    };
    assert_eq!(
        error_code(based(&mut s, 2, wrong(&tokens[2]))),
        error::Code::AheadOfServer
    );
    assert_eq!(based(&mut s, 1, tokens[1].clone()).unwrap().seq, 4);
    assert_eq!(based(&mut s, 2, tokens[2].clone()).unwrap().seq, 5);
}
