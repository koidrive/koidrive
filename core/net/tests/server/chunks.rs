// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use koidrive_net::{Api, Auth, Backoff, ChunkError, Event, Session, Transfer, UploadGate, matches};
use koidrive_proto::v1::{
    ChunkRef, CommitResult, Content, CreateFile, Op, Resume, UploadReceipt, change_record,
    error::Code, op, op_result,
};
use sha2::{Digest, Sha256};

use crate::support::proxy::{Corrupt, Proxy};
use crate::support::*;

fn chunk(bytes: &[u8]) -> ChunkRef {
    ChunkRef {
        hash: Sha256::digest(bytes).to_vec(),
        size: bytes.len() as u32,
        encoding: 0,
    }
}

fn content(chunks: &[ChunkRef]) -> Content {
    let mut manifest = Sha256::new();
    for chunk in chunks {
        manifest.update(&chunk.hash);
        manifest.update(chunk.size.to_le_bytes());
    }
    Content {
        size: chunks.iter().map(|c| u64::from(c.size)).sum(),
        mtime_ns: 1_700_000_000_000_000_000,
        executable: false,
        chunks: chunks.to_vec(),
        chunk_count: chunks.len() as u32,
        manifest_digest: manifest.finalize().to_vec(),
        chunker: "fastcdc2020-1m-4m-16m".into(),
    }
}

fn create_file(parent_id: &str, name: &str, content: Content) -> Op {
    let file = CreateFile {
        made_over: Vec::new(),
        source_version_id: String::new(),
        node_id: uuidv7(),
        parent_id: parent_id.into(),
        name: name.into(),
        content: Some(content),
    };
    Op {
        op: Some(op::Op::CreateFile(file)),
    }
}

fn new_transfer(api: &Api) -> Transfer {
    let mut transfer = Transfer::new(api);
    transfer.backoff = Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(200),
    };
    transfer
}

struct Source {
    bytes: HashMap<Vec<u8>, Bytes>,
    reads: Cell<usize>,
}

impl Source {
    fn new(parts: &[&'static [u8]]) -> Self {
        Source {
            bytes: parts
                .iter()
                .map(|p| (chunk(p).hash, Bytes::from_static(p)))
                .collect(),
            reads: Cell::new(0),
        }
    }

    async fn read(&self, chunk: &ChunkRef) -> io::Result<Bytes> {
        self.reads.set(self.reads.get() + 1);
        self.bytes
            .get(&chunk.hash)
            .cloned()
            .ok_or_else(|| io::Error::other("unknown chunk"))
    }
}

async fn upload(
    transfer: &Transfer,
    session: &Session,
    source: &Source,
    chunks: &[ChunkRef],
) -> Result<Vec<UploadReceipt>, ChunkError> {
    transfer
        .upload(session, chunks, |c: &ChunkRef| {
            let c = c.clone();
            async move { source.read(&c).await }
        })
        .await
}

async fn download(
    transfer: &Transfer,
    session: &Session,
    chunks: &[ChunkRef],
) -> Result<Vec<Bytes>, ChunkError> {
    let mut received = Vec::new();
    transfer
        .download(session, chunks, |c: &ChunkRef, bytes: Bytes| {
            assert!(matches(c, &bytes), "only verified bytes reach the caller");
            received.push((c.clone(), bytes));
            std::future::ready(Ok(()))
        })
        .await?;
    received.sort_by_key(|(c, _)| chunks.iter().position(|wanted| wanted.hash == c.hash));
    Ok(received.into_iter().map(|(_, bytes)| bytes).collect())
}

fn applied(result: &CommitResult) -> bool {
    result
        .results
        .iter()
        .all(|r| r.code == op_result::Code::Applied as i32)
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn upload_commit_and_download_round_trip() {
    let space = new_space().await;
    let writer_auth = auth(owner().await);
    let (writer, mut writer_events) = start(writer_auth.clone(), &space);
    let history = connected(&mut writer_events).await.history_id;
    let parts: [&'static [u8]; 3] = [b"first chunk", b"second chunk", b"first chunk"];
    let chunks: Vec<ChunkRef> = parts.iter().map(|p| chunk(p)).collect();
    let file = create_file(&space.space_id, "notes.txt", content(&chunks));

    let mut sent = commit(&history, 0, 1, vec![file]);
    let missing = writer.commit(sent.clone()).await.unwrap_err();
    assert_eq!(missing.code(), Some(Code::MissingChunks));
    let koidrive_net::CallError::Server(error) = missing else {
        unreachable!()
    };
    assert_eq!(
        error.missing_chunks().len(),
        2,
        "each distinct chunk is listed once"
    );

    let transfer = new_transfer(writer_auth.api());
    let source = Source::new(&parts);
    let receipts = upload(&transfer, &writer, &source, &chunks).await.unwrap();
    assert_eq!(
        (receipts.len(), source.reads.get()),
        (2, 2),
        "one upload and one receipt per distinct chunk"
    );
    sent.receipts = receipts;
    let result = writer.commit(sent).await.unwrap();
    assert!(
        applied(&result),
        "the same commit_id and slot, with the receipts added (RFC 0002 section 11)"
    );

    let again = Source::new(&parts);
    assert!(
        upload(&transfer, &writer, &again, &chunks)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        again.reads.get(),
        0,
        "chunks the server holds are not read again"
    );

    let reader_auth = auth(new_device("reader").await);
    let (reader, mut reader_events) = start(reader_auth.clone(), &space);
    connected(&mut reader_events).await;
    reader.subscribe(Resume {
        history_id: history.clone(),
        since_seq: 0,
        ..Default::default()
    });
    let records = until(&mut reader_events, |e| match e {
        Event::Changes(records) => Some(records.clone()),
        _ => None,
    })
    .await;
    let Some(change_record::Change::Node(node)) = &records[0].changes[0].change else {
        panic!("a node state")
    };
    let received = node
        .current
        .as_ref()
        .unwrap()
        .content
        .as_ref()
        .unwrap()
        .chunks
        .clone();
    assert_eq!(received, chunks);
    let bytes = download(&new_transfer(reader_auth.api()), &reader, &received)
        .await
        .unwrap();
    assert_eq!(
        bytes,
        [
            Bytes::from_static(b"first chunk"),
            Bytes::from_static(b"second chunk")
        ]
    );
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn forty_chunks_uploaded_four_at_once_commit_and_download_intact() {
    let space = new_space().await;
    let writer_auth = auth(owner().await);
    let (writer, mut events) = start(writer_auth.clone(), &space);
    let history = connected(&mut events).await.history_id;
    let parts: Vec<Bytes> = (0..40u8).map(|i| Bytes::from(vec![i; 64 << 10])).collect();
    let chunks: Vec<ChunkRef> = parts.iter().map(|p| chunk(p)).collect();
    let source = Source {
        bytes: parts.iter().map(|p| (chunk(p).hash, p.clone())).collect(),
        reads: Cell::new(0),
    };
    let mut transfer = new_transfer(writer_auth.api());
    transfer.gate = UploadGate::new(4);

    let receipts = upload(&transfer, &writer, &source, &chunks).await.unwrap();
    assert_eq!(
        (receipts.len(), source.reads.get()),
        (40, 40),
        "one read and one receipt per chunk"
    );
    let mut sent = commit(
        &history,
        0,
        1,
        vec![create_file(&space.space_id, "forty", content(&chunks))],
    );
    sent.receipts = receipts;
    assert!(applied(&writer.commit(sent).await.unwrap()));

    assert_eq!(download(&transfer, &writer, &chunks).await.unwrap(), parts);
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn corrupted_chunks_are_refused_in_both_directions() {
    let space = new_space().await;
    let proxy = Proxy::start(&server_url()).await;
    let through_proxy = Arc::new(Auth::new(
        Api::local_development(proxy.url.clone()).unwrap(),
        owner().await,
    ));
    let (session, mut events) = start(through_proxy.clone(), &space);
    let history = connected(&mut events).await.history_id;
    let transfer = new_transfer(through_proxy.api());
    let parts: [&'static [u8]; 1] = [b"bytes that must arrive intact"];
    let chunks = vec![chunk(parts[0])];

    proxy.corrupt(Corrupt {
        uploads: 1,
        downloads: 0,
    });
    let receipts = upload(&transfer, &session, &Source::new(&parts), &chunks)
        .await
        .unwrap();
    assert_eq!(
        proxy.corrupt_left().uploads,
        0,
        "the first upload was damaged on the way"
    );
    assert_eq!(
        receipts.len(),
        1,
        "the relay refused the damaged body and accepted the retry"
    );
    let mut sent = commit(
        &history,
        0,
        1,
        vec![create_file(&space.space_id, "intact", content(&chunks))],
    );
    sent.receipts = receipts;
    assert!(applied(&session.commit(sent).await.unwrap()));

    proxy.corrupt(Corrupt {
        uploads: 0,
        downloads: 1,
    });
    let bytes = download(&transfer, &session, &chunks).await.unwrap();
    assert_eq!(
        proxy.corrupt_left().downloads,
        0,
        "the first download was damaged on the way"
    );
    assert_eq!(bytes, [Bytes::from_static(parts[0])]);

    proxy.corrupt(Corrupt {
        uploads: 0,
        downloads: 1000,
    });
    let outcome = download(&transfer, &session, &chunks).await;
    assert!(
        matches!(outcome, Err(ChunkError::Corrupt { .. })),
        "{outcome:?}"
    );
    assert_eq!(
        proxy.corrupt_left().downloads,
        1000 - transfer.attempts,
        "every attempt was damaged and refused"
    );

    let changed = Source::new(&[b"the file changed after chunking"]);
    let stale = vec![ChunkRef {
        hash: chunk(b"the file changed after chunking").hash,
        size: 5,
        encoding: 0,
    }];
    let outcome = upload(&transfer, &session, &changed, &stale).await;
    assert!(
        matches!(outcome, Err(ChunkError::SourceChanged { .. })),
        "{outcome:?}"
    );
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn commits_are_bound_by_the_1_mib_server_message() {
    let space = new_space().await;
    let writer_auth = auth(owner().await);
    let (writer, mut writer_events) = start(writer_auth.clone(), &space);
    let history = connected(&mut writer_events).await.history_id;
    let parts: [&'static [u8]; 1] = [b"x"];
    let one = chunk(parts[0]);
    let transfer = new_transfer(writer_auth.api());
    let receipts = upload(
        &transfer,
        &writer,
        &Source::new(&parts),
        std::slice::from_ref(&one),
    )
    .await
    .unwrap();

    let file = |name: &str, repeats: usize| {
        create_file(&space.space_id, name, content(&vec![one.clone(); repeats]))
    };
    let mut too_large = commit(&history, 0, 1, vec![file("a", 16_000), file("b", 16_000)]);
    too_large.receipts = receipts.clone();
    let refused = writer.commit(too_large).await.unwrap_err();
    assert_eq!(refused.code(), Some(Code::LimitExceeded), "{refused:?}");

    let mut largest = commit(&history, 0, 1, vec![file("c", 13_000), file("d", 13_000)]);
    largest.receipts = receipts;
    let result = writer.commit(largest).await.unwrap();
    assert!(applied(&result));

    let (reader, mut reader_events) = start(auth(new_device("reader").await), &space);
    connected(&mut reader_events).await;
    reader.subscribe(Resume {
        history_id: history.clone(),
        since_seq: 0,
        ..Default::default()
    });
    let records = until(&mut reader_events, |e| match e {
        Event::Changes(records) => Some(records.clone()),
        _ => None,
    })
    .await;
    let size = koidrive_proto::v1::ServerMessage {
        msg: Some(koidrive_proto::v1::server_message::Msg::Changes(
            koidrive_proto::v1::Changes {
                commits: records.clone(),
            },
        )),
    };
    let size = prost::Message::encoded_len(&size);
    assert!(
        size > 900 * 1024 && size <= koidrive_net::MAX_SERVER_MESSAGE,
        "a replay of {size} bytes, just under 1 MiB"
    );
    assert_eq!(records[0].seq, 1);
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn space_usage_counts_a_committed_chunk_once() {
    let space = new_space().await;
    let owner_auth = auth(owner().await);
    let api = owner_auth.api();
    let empty = api.space_usage(&owner_auth, &space.space_id).await.unwrap();
    assert_eq!((empty.live_files, empty.live_bytes), (0, 0));

    let (writer, mut events) = start(owner_auth.clone(), &space);
    let history = connected(&mut events).await.history_id;
    let parts: [&'static [u8]; 3] = [b"first chunk", b"second chunk", b"first chunk"];
    let chunks: Vec<ChunkRef> = parts.iter().map(|p| chunk(p)).collect();
    let mut sent = commit(
        &history,
        0,
        1,
        vec![create_file(&space.space_id, "notes.txt", content(&chunks))],
    );
    let source = Source::new(&parts);
    sent.receipts = upload(&new_transfer(api), &writer, &source, &chunks)
        .await
        .unwrap();
    assert!(applied(&writer.commit(sent).await.unwrap()));

    let usage = api.space_usage(&owner_auth, &space.space_id).await.unwrap();
    assert_eq!(
        (usage.live_files, usage.live_bytes),
        (1, (b"first chunk".len() + b"second chunk".len()) as u64),
        "the repeated chunk counts once"
    );
    assert_eq!(
        (
            usage.trashed_files,
            usage.old_versions,
            usage.unreferenced_bytes
        ),
        (0, 0, 0)
    );
}
