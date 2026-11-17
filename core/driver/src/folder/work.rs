// SPDX-License-Identifier: Apache-2.0

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use koidrive_net::{Backoff, CallError, ChunkError, Session, Transfer};
use koidrive_proto::v1::{ChunkRef, UploadReceipt};

use crate::disk::{Pending, Staged, Stall, Stop, Upload};
use crate::failures;
use crate::policy::{self, Disposition};

use super::{Listed, Listing, TREE_PAGE};

/// Work that is running, or nothing.
pub(super) type Slot<'a, T> = Option<Pin<Box<dyn Future<Output = T> + Send + 'a>>>;

/// Tokio does not let another worker take a task from a busy worker's LIFO slot
/// (<https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html#method.disable_lifo_slot>).
pub(super) fn in_place<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|runtime| runtime.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(work),
        _ => work(),
    }
}

pub(super) fn spawned<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> Slot<'static, T> {
    struct Spawned<T>(tokio::task::JoinHandle<T>);
    impl<T> Future for Spawned<T> {
        type Output = T;
        fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<T> {
            Pin::new(&mut self.0).poll(cx).map(|done| match done {
                Ok(out) => out,
                Err(error) => std::panic::resume_unwind(error.into_panic()),
            })
        }
    }
    impl<T> Drop for Spawned<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    Some(Box::pin(Spawned(tokio::spawn(work))))
}

/// Waits for the work in `slot` and empties it. Never returns for an empty slot.
pub(super) async fn finished<T>(slot: &mut Slot<'_, T>) -> T {
    match slot {
        Some(work) => {
            let out = work.as_mut().await;
            *slot = None;
            out
        }
        None => std::future::pending().await,
    }
}

/// Reads the rest of the tree with `ListTree` from `listing`, after `wait` when there is one, page by page (spec
/// v0 section 7.5), then the trash with `ListTrash` when the listing asks for it (RFC 0002 §6). A page refused in a
/// way the server asks to retry ends it with the wait, so the folder can show it.
pub(super) async fn list_tree(
    session: Arc<Session>,
    space_id: String,
    mut listing: Listing,
    wait: Option<Duration>,
) -> Result<Listed, Stop> {
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    while listing.pages.is_empty() || !listing.cursor.is_empty() {
        match session.list_tree(listing.cursor.clone(), TREE_PAGE).await {
            Ok(page) => {
                listing.failures = 0;
                listing.cursor.clone_from(&page.next_cursor);
                listing.pages.push(page);
            }
            Err(error) => return later(&space_id, "tree", listing, &error),
        }
    }
    while listing.trash_pages.is_empty() || !listing.trash_cursor.is_empty() {
        match session
            .list_trash(listing.trash_cursor.clone(), TREE_PAGE)
            .await
        {
            Ok(page) => {
                listing.failures = 0;
                listing.trash_cursor.clone_from(&page.next_cursor);
                listing.trash_pages.push(page);
            }
            Err(error) => return later(&space_id, "trash", listing, &error),
        }
    }
    Ok(Listed::Done {
        pages: listing.pages,
        trash: listing.trash_pages,
    })
}

/// How a listing ends after a page of the space's `what`, tree or trash, failed with `error`: later with a wait, or
/// with a stop when the server refuses it for good.
pub(super) fn later(
    space_id: &str,
    what: &str,
    mut listing: Listing,
    error: &CallError,
) -> Result<Listed, Stop> {
    failures::call_failed(error);
    let why = match error {
        CallError::Server(refusal) => policy::describe(refusal),
        other => other.to_string(),
    };
    let wait = match policy::after_call_error(error) {
        Disposition::RetryAfter(wait) => wait,
        Disposition::Backoff => Backoff::default().delay(listing.failures),
        Disposition::Halt => {
            return Err(Stop::after_call(
                error,
                format!("reading the space's {what} failed: {why}"),
            ));
        }
    };
    tracing::info!(space_id, %error, ?wait, "reading the {what} again later");
    listing.failures += 1;
    Ok(Listed::Later {
        listing,
        wait,
        why: format!("reading the space's {what} failed: {why}"),
        kind: policy::wait_kind(match error {
            CallError::Server(refusal) => Some(refusal),
            _ => None,
        }),
    })
}

/// Stages `pending`, after `wait` when there is one, and gives it back with how the staging ended.
pub(super) async fn stage_effect(
    session: Arc<Session>,
    transfer: Transfer,
    pending: Pending,
    wait: Option<Duration>,
) -> (Pending, Result<Staged, Stall>) {
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    let staged = pending.stage(&(&*session, &transfer)).await;
    (pending, staged)
}

/// Uploads the chunks of `upload` the server lacks, and adds the bytes of each to [`crate::volume`] once it is stored.
pub(super) async fn upload_chunks(
    session: Arc<Session>,
    transfer: Transfer,
    upload: Upload,
) -> Result<Vec<UploadReceipt>, ChunkError> {
    let bytes = upload.timing_bytes.clone();
    upload_counted(&session, &transfer, upload, |count| {
        crate::volume::uploaded(count);
        crate::timing::add_bytes(&bytes, count);
    })
    .await
}

/// Uploads the chunks of `upload` the server lacks, each read on a thread that may block, and passes the size of each
/// to `stored` once it is stored.
pub(super) async fn upload_counted(
    session: &Session,
    transfer: &Transfer,
    upload: Upload,
    mut stored: impl FnMut(u64),
) -> Result<Vec<UploadReceipt>, ChunkError> {
    let upload = Arc::new(upload);
    let read = |chunk: &ChunkRef| {
        let (upload, chunk) = (upload.clone(), chunk.clone());
        async move {
            tokio::task::spawn_blocking(move || upload.read(&chunk))
                .await
                .map_err(io::Error::other)?
        }
    };
    let credit = upload.credit.clone();
    let stored = |chunk: &ChunkRef| {
        stored(u64::from(chunk.size));
        credit.moved(u64::from(chunk.size));
        credit.chunk(&chunk.hash);
    };
    let present = |chunk: &ChunkRef| credit.chunk(&chunk.hash);
    (transfer.upload_observed(session, &upload.chunks, read, stored, present)).await
}
