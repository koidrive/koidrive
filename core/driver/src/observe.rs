// SPDX-License-Identifier: Apache-2.0

//! Local observation: what the disk under a linked folder looks like now, as the engine's `Observed` input
//! describes it (design §1): created, modified, renamed and removed entries with their file identities, the
//! `Content` of every file that was created or modified, chunked with `core/chunk`, and the root check of design
//! §6.2 made for the batch.
//!
//! [`FsObserver`] runs the watchers of `core/fs`: inotify on Linux and FSEvents on macOS, which only choose what to
//! rescan, with a periodic full rescan when the platform watcher cannot cover the tree (design §5.1).
//!
//! The scans leave out excluded paths (design §5.10) and read the `.koiignore` files, whose rules each batch passes
//! on. A move across the boundary still reaches the engine: a move into an excluded path is reported as a rename, so
//! the engine can freeze the node at its synced position, and what a move out of one brings into the sync tree is
//! reported as created, each entry with its identity and every file with its content.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::ops::Bound;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use koidrive_chunk::ChunkerConfig;
use koidrive_engine::ignore::RuleFile;
use koidrive_fs::{
    Change, Entry, EntryKind, Error as FsError, FileId, Known, NativeWatcher, RescanWatcher,
    RootAt, RootProblem, Snapshot, Watcher, check_root,
};
use koidrive_proto::v1::{ChunkRef, Content};
use rustix::fs::{Mode, OFlags};

use crate::ignore::{Matcher, scan_rules};

/// How often the fallback watcher rescans the whole tree.
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);
/// The wait before a file whose content could not be read is read again; it doubles with each failure.
const RETRY_FIRST: Duration = Duration::from_millis(250);
/// The longest wait between two reads of such a file.
const RETRY_MAX: Duration = Duration::from_secs(30);
/// The most files whose content one observation reads. A created or modified file past it is reported without
/// content and read by a later observation, so a large tree reaches the engine in batches of bounded reading.
pub(crate) const READ_FILES: usize = 1_000;
/// The most bytes one observation reads, past its first file.
const READ_BYTES: u64 = 256 << 20;

/// One batch of observations.
#[derive(Debug)]
pub struct Observation {
    pub changes: Vec<Observed>,
    /// The root check of design §6.2, made after the scans of this batch. Removals in a batch whose check failed
    /// are not trusted.
    pub root: Result<(), RootProblem>,
    /// Folders the scans may not read. What is below them is unknown, not absent. Until a scan of the current watcher
    /// has settled every scope, the folders still pending are among them too, since no scan may have read their rules.
    pub unreadable: Vec<PathBuf>,
    /// Folders whose rules the scans established, at the paths the scans read them. A folder reported unreadable
    /// before has known rules again once it is here.
    pub read: Vec<PathBuf>,
    /// The identity of the root folder the observer's scans opened, from the latest poll that scanned. A batch
    /// whose root is not the folder the task locked describes another folder. `None` only before the first scan.
    pub scanned: Option<FileId>,
    /// Files that changed while they were chunked, or that could not be opened or read as the entry the scan saw.
    /// Their changes carry no content. The observer keeps reading them, waiting longer after each failure, and
    /// reports a `Modified` with the content once a read succeeds, unless a later change to the path replaces it
    /// (design §5.8).
    pub busy: Vec<PathBuf>,
    /// The folders whose `.koiignore` file the scans of this batch found changed, each with the rules it holds now.
    /// The engine takes them before the changes of the batch. The first batch of an observer follows its scan of the
    /// whole tree and lists every rules file there is, also when nothing else changed.
    pub rules: Vec<RulesChange>,
    /// Whether `rules` lists every rules file the scans read: the first batch of an observer, and the first after the
    /// platform watcher failed and a rescanning one took over. Rules known from before are stale then, since the new
    /// scans compare with no earlier rules and report no rules file as gone.
    pub all_rules: bool,
    pub mounts: BTreeSet<PathBuf>,
}

/// The rules of the `.koiignore` file in `dir`, a folder relative to the root and empty for the root, or `None` when
/// the folder has no rules file any more. A line the file cannot use is among [`RuleFile::invalid_lines`].
#[derive(Clone, Debug, PartialEq)]
pub struct RulesChange {
    pub dir: PathBuf,
    pub rules: Option<RuleFile>,
}

/// One change, in the terms of `core/fs` (paths relative to the root, previous-state paths first).
#[derive(Clone, Debug, PartialEq)]
pub enum Observed {
    /// `content` is set for a file that could be read whole. `linked` is set, here and on the other changes, for a
    /// file that another path also links, a hard link, whose identity therefore names no single path.
    Created {
        path: PathBuf,
        kind: EntryKind,
        id: FileId,
        linked: bool,
        content: Option<Content>,
    },
    Modified {
        path: PathBuf,
        id: FileId,
        linked: bool,
        content: Option<Content>,
    },
    Renamed {
        from: PathBuf,
        to: PathBuf,
        kind: EntryKind,
        id: FileId,
        linked: bool,
    },
    Removed {
        path: PathBuf,
        kind: EntryKind,
    },
    /// The folder at `path` keeps its entries under a new identity, `id`.
    Reidentified {
        path: PathBuf,
        id: FileId,
    },
}

/// What the observer is to believe about one scope, a path relative to the root, before it scans the scope again:
/// the entries L holds at and below it, by kind and identity. The scan then reports how the disk differs from L
/// there, and reads every file L holds there again, since L's content for a file does not prove what the disk holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Reset {
    pub scope: PathBuf,
    pub known: BTreeMap<PathBuf, Known>,
}

/// A source of observations for one linked folder.
pub trait Observer: Send {
    /// Waits up to about `timeout` and returns the next batch, or `None` when nothing changed.
    fn observe(&mut self, timeout: Duration) -> Result<Option<Observation>, FsError>;

    /// The disk as the returned observations leave it. Keep it to start the next observer from.
    fn snapshot(&self) -> &Snapshot;
}

/// Observes a folder through the watchers of `core/fs`.
pub struct FsObserver {
    root: PathBuf,
    space_id: String,
    linked: RootAt,
    watcher: Box<dyn Watcher + Send>,
    native: bool,
    matcher: Arc<dyn Matcher>,
    /// The root folder the latest scan opened.
    scanned: Option<FileId>,
    /// Whether the batch that follows the first scan of the whole tree is still to be returned.
    first: bool,
    /// Whether no poll of the current watcher has settled every scope yet. Until one has, a folder still pending may
    /// be one whose rules no scan of this watcher has read.
    settling: bool,
    /// Whether a reset still waits for the batch of a scan made after it.
    reset: bool,
    /// Files whose content could not be read, or that an observation left to a later one, with when to read them.
    unread: Unread,
    /// Whether the caller has stopped waiting for observations.
    stopped: Arc<dyn Fn() -> bool + Send + Sync>,
}

#[derive(Clone, Copy, Debug)]
struct Retry {
    failures: u32,
    due: Instant,
}

#[derive(Debug, Default)]
struct Unread {
    files: BTreeMap<PathBuf, Retry>,
    due: BTreeSet<(Instant, PathBuf)>,
}

impl Unread {
    fn insert(&mut self, path: PathBuf, retry: Retry) -> Option<Retry> {
        let old = self.files.insert(path.clone(), retry);
        if let Some(old) = old {
            self.due.remove(&(old.due, path.clone()));
        }
        self.due.insert((retry.due, path));
        old
    }

    fn remove(&mut self, path: &Path) -> Option<Retry> {
        let old = self.files.remove(path)?;
        self.due.remove(&(old.due, path.to_path_buf()));
        Some(old)
    }

    fn get(&self, path: &Path) -> Option<&Retry> {
        self.files.get(path)
    }

    fn contains_key(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.files.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = (&PathBuf, &Retry)> {
        self.files.iter()
    }

    #[cfg(test)]
    fn keys(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.keys()
    }

    #[cfg(test)]
    fn values(&self) -> impl Iterator<Item = &Retry> {
        self.files.values()
    }

    fn below(&self, dir: &Path) -> impl Iterator<Item = &PathBuf> {
        (self
            .files
            .range::<Path, _>((Bound::Included(dir), Bound::Unbounded)))
        .take_while(move |(path, _)| path.starts_with(dir))
        .map(|(path, _)| path)
    }

    fn first_due(&self) -> Option<Instant> {
        self.due.first().map(|(due, _)| *due)
    }

    fn next_due(
        &self,
        now: Instant,
        after: Option<&(Instant, PathBuf)>,
    ) -> Option<(Instant, PathBuf)> {
        let from = after.map_or(Bound::Unbounded, Bound::Excluded);
        (self.due.range((from, Bound::Unbounded)))
            .next()
            .filter(|(due, _)| *due <= now)
            .cloned()
    }
}

/// The reads of one observation.
#[derive(Default)]
struct Reads {
    files: usize,
    bytes: u64,
    /// Files that changed while they were read, or could not be read.
    busy: Vec<PathBuf>,
    /// Files left to a later observation because this one read all it may.
    deferred: Vec<PathBuf>,
    /// Folders the changes of this observation removed or renamed into place. A file created under one may be a node
    /// that the batch takes out of L with the folder, which the engine keeps only for a created file with content, so
    /// such a file is read whatever the budget left.
    replaced: Vec<PathBuf>,
}

impl Reads {
    /// Counts a file of `size` bytes as read if [`READ_FILES`] and [`READ_BYTES`] leave room for it, or if it `must` be
    /// read. The first file always has room, so a file larger than the budget is read on its own.
    fn take(&mut self, size: u64, must: bool) -> bool {
        let room = must
            || self.files == 0
            || (self.files < READ_FILES && self.bytes.saturating_add(size) <= READ_BYTES);
        if room {
            self.files += 1;
            self.bytes = self.bytes.saturating_add(size);
        }
        room
    }
}

impl Retry {
    fn after(failures: u32, now: Instant) -> Self {
        let wait = RETRY_FIRST
            .saturating_mul(1 << failures.min(16))
            .min(RETRY_MAX);
        Retry {
            failures,
            due: now + wait,
        }
    }
}

impl FsObserver {
    /// Starts watching the folder of space `space_id` that `linked` records. The first observation compares the whole tree
    /// with `previous`. The scans leave out what `matcher`, the built-in exclusions and the `.koiignore` files of the
    /// folder exclude. Once `stopped` holds, the scan walking the tree is left at its next entry and the file being
    /// read at its next chunk, and `observe` returns `None` without the rest, so a caller that stops waits neither
    /// for a walk of the whole tree nor for every file of a scan to be read. The observer's snapshot then counts
    /// changes it never returned, so drop the observer.
    pub fn start(
        root: &Path,
        space_id: &str,
        linked: RootAt,
        previous: Snapshot,
        matcher: Arc<dyn Matcher>,
        stopped: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Self, FsError> {
        let stopped: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(stopped);
        let (watcher, native): (Box<dyn Watcher + Send>, bool) =
            match NativeWatcher::start_with_rules(root, previous.clone(), scan_rules(&matcher)) {
                Ok(watcher) => (Box::new(watcher.stopping_when(watching(&stopped))), true),
                Err(FsError::Watch(error)) => {
                    tracing::warn!(%error, "the platform watcher is unavailable; rescanning periodically");
                    let rules = scan_rules(&matcher);
                    let watcher =
                        RescanWatcher::with_rules(root, previous, RESCAN_INTERVAL, rules)?;
                    (Box::new(watcher.stopping_when(watching(&stopped))), false)
                }
                Err(error) => return Err(error),
            };
        Ok(FsObserver {
            root: std::fs::canonicalize(root)?,
            space_id: space_id.to_owned(),
            linked,
            watcher,
            native,
            matcher,
            scanned: None,
            first: true,
            settling: true,
            reset: false,
            unread: Unread::default(),
            stopped,
        })
    }

    /// The root folder the latest scan opened, `None` until a scan has opened it. A folder with nothing to report
    /// gives no [`Observation`], so this is how a caller learns that the first scan is done.
    pub fn scanned(&self) -> Option<FileId> {
        self.scanned
    }

    /// Resets the snapshot at each scope of `resets`, in order, to the entries L holds there, so the next observation
    /// reports how the disk differs from L in those scopes: what L lacks as created, what the disk lacks as removed,
    /// and every file both hold as modified, each file with the content read from the disk. Nothing outside the
    /// scopes is reported again unless it changed. The first observation whose scans settled every scope after a
    /// reset is returned also when it holds nothing, so the caller learns that the disk was read after the reset.
    pub fn reset(&mut self, resets: Vec<Reset>) {
        self.reset |= !resets.is_empty();
        for Reset { scope, known } in resets {
            let below: Vec<PathBuf> = self.unread.below(&scope).cloned().collect();
            for path in below {
                self.unread.remove(&path);
            }
            self.watcher.reset(&scope, known);
        }
    }

    /// Whether every reset so far is answered: a scan made after it settled each of its scopes, and the batch of that
    /// scan was returned. A reset stays unanswered while a scope of it is pending, and through a batch that scanned
    /// nothing.
    pub fn answered(&self) -> bool {
        !self.reset
    }

    /// Adds what `change` means for the engine to `out`. The scans leave out excluded paths, so every path here is
    /// one this device syncs, except the target of a move into an excluded path.
    fn observed(&self, change: Change, out: &mut Vec<Observed>, reads: &mut Reads) {
        let entry = |path: &Path| self.watcher.snapshot().entries.get(path).copied();
        match change {
            Change::Created { path, kind } => {
                let Some(entry) = entry(&path) else { return };
                let must = reads.replaced.iter().any(|dir| path.starts_with(dir));
                let content = (kind == EntryKind::File)
                    .then(|| self.content(&path, &entry, reads, must))
                    .flatten();
                out.push(Observed::Created {
                    linked: self.linked(&path, kind),
                    path,
                    kind,
                    id: entry.id,
                    content,
                });
            }
            Change::Modified { path } => {
                let Some(entry) = entry(&path) else { return };
                if entry.kind != EntryKind::File {
                    return;
                }
                let content = self.content(&path, &entry, reads, false);
                out.push(Observed::Modified {
                    content,
                    id: entry.id,
                    linked: self.linked(&path, entry.kind),
                    path,
                });
            }
            Change::Renamed { from, to, kind } => {
                let Some(entry) = entry(&to) else { return };
                out.push(Observed::Renamed {
                    from,
                    linked: self.linked(&to, kind),
                    to,
                    kind,
                    id: entry.id,
                });
            }
            Change::Ignored { from, to, kind, id } => {
                let linked = self.linked(&to, kind);
                out.push(Observed::Renamed {
                    from,
                    to,
                    kind,
                    id,
                    linked,
                })
            }
            Change::Removed { path, kind } => out.push(Observed::Removed { path, kind }),
            Change::Reidentified { path } => {
                let Some(entry) = entry(&path) else { return };
                out.push(Observed::Reidentified { path, id: entry.id });
            }
        }
    }

    fn supersede(&mut self, changes: &[Change]) {
        let mut moves: BTreeMap<&Path, Option<&Path>> = BTreeMap::new();
        for change in changes {
            match change {
                Change::Renamed { from, to, .. } => {
                    moves.insert(from, Some(to));
                }
                Change::Removed { path, .. } | Change::Ignored { from: path, .. } => {
                    moves.insert(path, None);
                }
                Change::Created { .. } | Change::Modified { .. } | Change::Reidentified { .. } => {}
            }
        }
        let below: BTreeSet<PathBuf> = (moves.keys())
            .flat_map(|from| self.unread.below(from).cloned())
            .collect();
        let left: Vec<(PathBuf, Retry)> = (below.into_iter())
            .filter_map(|path| Some((path.clone(), self.unread.remove(&path)?)))
            .collect();
        let mut placed = BTreeSet::new();
        for (path, retry) in left {
            let (from, to) = (path.ancestors())
                .find_map(|above| moves.get_key_value(above))
                .expect("below a moved path");
            let Some(to) = to else { continue };
            let rest = path.strip_prefix(from).expect("an ancestor of the path");
            let moved = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            };
            let kept = !placed.contains(&moved) && self.unread.contains_key(&moved) && moved > path;
            if !kept {
                self.unread.insert(moved.clone(), retry);
                placed.insert(moved);
            }
        }
        for change in changes {
            if let Change::Created { path, .. } | Change::Modified { path } = change {
                self.unread.remove(path);
            }
        }
    }

    /// Reads again the unread files that are due, the longest due first and as many as `reads` has room for, and
    /// reports each one that can now be read. An observation retries before it reads its own changes, so a file the
    /// budget left out gets the first turn of a later observation, also when it is larger than the budget. A read that
    /// fails once the caller has stopped ends the retry and leaves that file and the rest due as they were.
    fn retry(&mut self, now: Instant, out: &mut Vec<Observed>, reads: &mut Reads) {
        let mut at = None;
        while reads.files < READ_FILES
            && let Some(next) = self.unread.next_due(now, at.as_ref())
        {
            let path = next.1.clone();
            at = Some(next);
            let entry = self.watcher.snapshot().entries.get(&path).copied();
            let Some(entry) = entry.filter(|entry| entry.kind == EntryKind::File) else {
                self.unread.remove(&path);
                continue;
            };
            if !reads.take(entry.size, false) {
                continue;
            }
            match read_content(&self.root, &path, &entry, &*self.stopped) {
                Ok(Some(content)) => {
                    self.unread.remove(&path);
                    out.push(Observed::Modified {
                        linked: self.linked(&path, entry.kind),
                        path,
                        id: entry.id,
                        content: Some(content),
                    });
                }
                Err(_) if (self.stopped)() => break,
                _ => {
                    let failures = self.unread.get(&path).expect("an unread file").failures + 1;
                    self.unread.insert(path, Retry::after(failures, now));
                }
            }
        }
    }

    /// True for a file with more than one link. A file that cannot be read is taken to have one.
    fn linked(&self, path: &Path, kind: EntryKind) -> bool {
        kind == EntryKind::File
            && std::fs::symlink_metadata(self.root.join(path)).is_ok_and(|meta| meta.nlink() > 1)
    }

    /// Chunks the file at `path` if `reads` has room for it or it `must` be read, it is still the entry the scan saw and
    /// it does not change while it is read.
    fn content(
        &self,
        path: &Path,
        entry: &Entry,
        reads: &mut Reads,
        must: bool,
    ) -> Option<Content> {
        if !reads.take(entry.size, must) {
            reads.deferred.push(path.to_path_buf());
            return None;
        }
        match read_content(&self.root, path, entry, &*self.stopped) {
            Ok(Some(content)) => Some(content),
            Ok(None) => {
                reads.busy.push(path.to_path_buf());
                None
            }
            Err(error) => {
                tracing::debug!(path = %koidrive_fs::quoted(path), %error, "a file could not be chunked");
                reads.busy.push(path.to_path_buf());
                None
            }
        }
    }
}

impl Observer for FsObserver {
    fn observe(&mut self, timeout: Duration) -> Result<Option<Observation>, FsError> {
        let timeout = match self.unread.first_due() {
            Some(due) => timeout.min(due.saturating_duration_since(Instant::now())),
            None => timeout,
        };
        let poll = match self.watcher.poll(timeout) {
            Err(FsError::Watch(error)) if self.native => {
                tracing::warn!(%error, "the platform watcher failed; rescanning periodically");
                let snapshot = self.watcher.snapshot().clone();
                let rules = scan_rules(&self.matcher);
                let watcher =
                    RescanWatcher::with_rules(&self.root, snapshot, RESCAN_INTERVAL, rules)?;
                self.watcher = Box::new(watcher.stopping_when(watching(&self.stopped)));
                self.native = false;
                self.first = true;
                self.settling = true;
                return Ok(None);
            }
            Err(FsError::Stopped) => return Ok(None),
            poll => poll?,
        };
        if poll.root.is_some() {
            self.scanned = poll.root;
        }
        let now = Instant::now();
        let mut changes = Vec::new();
        let mut reads = Reads {
            replaced: (poll.changes.iter())
                .filter_map(|change| match change {
                    Change::Removed {
                        path,
                        kind: EntryKind::Dir,
                    }
                    | Change::Renamed {
                        to: path,
                        kind: EntryKind::Dir,
                        ..
                    } => Some(path.clone()),
                    _ => None,
                })
                .collect(),
            ..Reads::default()
        };
        let observed = !poll.changes.is_empty();
        self.supersede(&poll.changes);
        let mut retried = Vec::new();
        self.retry(now, &mut retried, &mut reads);
        for change in poll.changes {
            if (self.stopped)() {
                return Ok(None);
            }
            self.observed(change, &mut changes, &mut reads);
        }
        changes.extend(retried);
        if (self.stopped)() {
            return Ok(None);
        }
        for path in &reads.busy {
            self.unread.insert(path.clone(), Retry::after(0, now));
        }
        for path in reads.deferred {
            self.unread.insert(
                path,
                Retry {
                    failures: 0,
                    due: now,
                },
            );
        }
        let rules: Vec<RulesChange> = (poll.rules.into_iter())
            .map(|(dir, bytes)| RulesChange {
                dir,
                rules: bytes.as_deref().map(RuleFile::parse),
            })
            .collect();
        let first = self.first && poll.root.is_some();
        let answers = self.reset && poll.root.is_some() && poll.pending.is_empty();
        let mut unreadable = poll.unreadable;
        if self.settling && poll.root.is_some() {
            let snapshot = self.watcher.snapshot();
            unreadable.extend(
                (poll.pending.iter())
                    .filter(|scope| {
                        scope.as_os_str().is_empty()
                            || (snapshot.entries.get(*scope))
                                .is_some_and(|entry| entry.kind == EntryKind::Dir)
                    })
                    .cloned(),
            );
            self.settling = !poll.pending.is_empty();
        }
        if !first
            && !answers
            && !observed
            && changes.is_empty()
            && unreadable.is_empty()
            && poll.read.is_empty()
            && rules.is_empty()
        {
            return Ok(None);
        }
        self.first &= !first;
        self.reset &= !answers;
        let root = check_root(&self.root, &self.space_id, &self.linked).map(|_| ());
        Ok(Some(Observation {
            changes,
            root,
            unreadable,
            read: poll.read,
            scanned: self.scanned,
            busy: reads.busy,
            rules,
            all_rules: first,
            mounts: (self.watcher.snapshot().entries.iter())
                .filter(|(_, entry)| entry.mount_point)
                .map(|(path, _)| path.clone())
                .collect(),
        }))
    }

    fn snapshot(&self) -> &Snapshot {
        self.watcher.snapshot()
    }
}

/// `stopped` as a watcher takes it.
fn watching(stopped: &Arc<dyn Fn() -> bool + Send + Sync>) -> impl Fn() -> bool + Send + 'static {
    let stopped = stopped.clone();
    move || stopped()
}

/// The content of the file at `path` under `root`, or `None` when it is not the file the scan saw or changed while
/// it was read. Fails with [`io::ErrorKind::Interrupted`] at the first chunk after `stopped` holds.
pub(crate) fn read_content(
    root: &Path,
    path: &Path,
    entry: &Entry,
    stopped: &dyn Fn() -> bool,
) -> io::Result<Option<Content>> {
    let file = open_beneath(root, path)?;
    let before = file.metadata()?;
    let same_file = before.is_file() && koidrive_fs::id_of(file.as_fd())? == entry.id;
    if !same_file || !same_version(&before, entry) {
        return Ok(None);
    }
    let config = ChunkerConfig::V0;
    let chunks = koidrive_chunk::chunks(io::Read::take(&file, entry.size), config)
        .map(|chunk| match stopped() {
            true => Err(io::ErrorKind::Interrupted.into()),
            false => chunk.map(|chunk| chunk.chunk_ref()),
        })
        .collect::<io::Result<Vec<ChunkRef>>>()?;
    let after = file.metadata()?;
    if !same_version(&after, entry) {
        return Ok(None);
    }
    Ok(Some(koidrive_chunk::content(
        chunks,
        mtime_ns(&after) as i64,
        entry.executable,
        &config,
    )))
}

fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
}

fn same_version(meta: &std::fs::Metadata, entry: &Entry) -> bool {
    meta.size() == entry.size && mtime_ns(meta) == entry.mtime_ns
}

/// Opens the file at the relative `path` under `root` for reading, one folder at a time, following no symbolic link.
pub(crate) fn open_beneath(root: &Path, path: &Path) -> io::Result<File> {
    #[cfg(test)]
    tests::before_open(path)?;
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    open_under(rustix::fs::open(root, dir_flags, Mode::empty())?, path)
}

/// Opens the file at the relative `path` under the open folder `root` for reading, one folder at a time, following
/// no symbolic link.
fn open_under(root: impl AsFd, path: &Path) -> io::Result<File> {
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir: OwnedFd = rustix::fs::openat(&root, ".", dir_flags, Mode::empty())?;
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            break;
        };
        if parts.peek().is_none() {
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            return Ok(File::from(rustix::fs::openat(
                &dir,
                name,
                flags,
                Mode::empty(),
            )?));
        }
        dir = rustix::fs::openat(&dir, name, dir_flags | OFlags::NOFOLLOW, Mode::empty())?;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is not a relative file path", koidrive_fs::quoted(path)),
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::time::Instant;

    use koidrive_fs::{Marker, Poll, write_marker};

    use super::*;
    use crate::ignore::Unfiltered;

    const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";

    thread_local! {
        /// The file whose next open on this thread fails with EINTR, as an open the OS interrupts does.
        static INTERRUPTED_OPEN: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(super) fn before_open(path: &Path) -> io::Result<()> {
        match INTERRUPTED_OPEN.with_borrow_mut(|next| next.take_if(|next| next == path)) {
            Some(_) => Err(rustix::io::Errno::INTR.into()),
            None => Ok(()),
        }
    }

    fn observer(root: &Path) -> FsObserver {
        let dev = RootAt {
            dev: fs::metadata(root).unwrap().dev(),
            volume: None,
        };
        FsObserver::start(
            root,
            SPACE,
            dev,
            Snapshot::default(),
            Arc::new(Unfiltered),
            || false,
        )
        .unwrap()
    }

    #[test]
    fn an_observer_stopped_during_a_walk_returns_no_observation_and_no_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = marked();
        for file in 0..20 {
            fs::write(dir.path().join(format!("{file}.txt")), "x").unwrap();
        }
        let left = AtomicUsize::new(5);
        let stopped = move || {
            (left.try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            }))
            .is_err()
        };
        let dev = RootAt {
            dev: fs::metadata(dir.path()).unwrap().dev(),
            volume: None,
        };
        let started = FsObserver::start(
            dir.path(),
            SPACE,
            dev,
            Snapshot::default(),
            Arc::new(Unfiltered),
            stopped,
        );
        let mut observer = started.unwrap();
        assert!(matches!(
            observer.observe(Duration::from_secs(10)),
            Ok(None)
        ));
        assert_eq!(observer.snapshot(), &Snapshot::default());
    }

    fn marked() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_marker(
            dir.path(),
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
            },
        )
        .unwrap();
        dir
    }

    /// Observes until `done` accepts the changes seen so far.
    fn observe_until(
        observer: &mut FsObserver,
        done: impl Fn(&[Observed]) -> bool,
    ) -> (Vec<Observed>, Observation) {
        let mut seen = Vec::new();
        loop {
            if let Some(observation) = observer.observe(Duration::from_millis(500)).unwrap() {
                seen.extend(observation.changes.iter().cloned());
                if done(&seen) {
                    return (seen, observation);
                }
            }
        }
    }

    #[test]
    fn the_first_observation_reports_every_entry_with_file_contents() {
        let dir = marked();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/a.txt"), b"hello").unwrap();
        fs::write(dir.path().join("empty"), b"").unwrap();
        let mut observer = observer(dir.path());
        let (seen, last) = observe_until(&mut observer, |seen| seen.len() >= 3);
        assert!(last.root.is_ok(), "{:?}", last.root);

        let created = |name: &str| {
            seen.iter()
                .find_map(|change| match change {
                    Observed::Created {
                        path,
                        kind,
                        content,
                        ..
                    } if path == Path::new(name) => Some((*kind, content.clone())),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} not created in {seen:?}"))
        };
        assert_eq!(created("docs"), (EntryKind::Dir, None));
        let (kind, content) = created("docs/a.txt");
        assert_eq!(kind, EntryKind::File);
        let content = content.unwrap();
        assert_eq!(content.size, 5);
        assert_eq!(content.chunks.len(), 1);
        assert_eq!(
            content.chunks[0].hash,
            koidrive_chunk::chunk_hash(b"hello").to_vec()
        );
        assert_eq!(created("empty").1.unwrap().chunks.len(), 0);
        assert!(
            seen.iter().all(|change| !matches!(change, Observed::Created { path, .. } if path.starts_with(".koidrive"))),
            "{seen:?}"
        );
    }

    #[test]
    fn renames_keep_the_identity_and_edits_carry_new_content() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| !seen.is_empty());
        let id = u128::from(fs::metadata(dir.path().join("a.txt")).unwrap().ino());

        fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Renamed { .. }))
        });
        assert!(
            seen.iter().any(|change| matches!(
                change,
                Observed::Renamed { from, to, id: renamed, .. }
                    if from == Path::new("a.txt") && to == Path::new("b.txt") && renamed.ino == id
            )),
            "{seen:?}"
        );

        fs::write(dir.path().join("b.txt"), b"two two").unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
        });
        let content = seen
            .iter()
            .find_map(|change| match change {
                Observed::Modified { content, .. } => content.clone(),
                _ => None,
            })
            .unwrap();
        assert_eq!(content.size, 7);
    }

    #[test]
    fn a_batch_without_its_marker_fails_the_root_check() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| !seen.is_empty());

        fs::remove_dir_all(dir.path().join(".koidrive")).unwrap();
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        let (_, last) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Removed { .. }))
        });
        assert!(
            matches!(last.root, Err(RootProblem::NoMarker)),
            "{:?}",
            last.root
        );
    }

    #[test]
    fn a_file_replaced_after_the_scan_is_not_chunked() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let mut snapshot = Snapshot::default();
        snapshot.rescan(dir.path(), [PathBuf::new()]).unwrap();
        let entry = snapshot.entries[Path::new("a.txt")];
        assert!(
            read_content(dir.path(), Path::new("a.txt"), &entry, &|| false)
                .unwrap()
                .is_some()
        );

        fs::remove_file(dir.path().join("a.txt")).unwrap();
        fs::write(dir.path().join("a.txt"), b"other bytes").unwrap();
        assert_eq!(
            read_content(dir.path(), Path::new("a.txt"), &entry, &|| false).unwrap(),
            None
        );
    }

    #[test]
    fn a_scan_of_more_files_than_one_observation_reads_reports_the_rest_later() {
        let dir = marked();
        for i in 0..=READ_FILES {
            fs::write(dir.path().join(format!("{i:04}.txt")), i.to_string()).unwrap();
        }
        let mut observer = observer(dir.path());
        let (first, _) = observe_until(&mut observer, |seen| !seen.is_empty());
        let unread: Vec<&Path> = (first.iter())
            .filter_map(|change| match change {
                Observed::Created {
                    path,
                    content: None,
                    ..
                } => Some(path.as_path()),
                _ => None,
            })
            .collect();
        assert_eq!(first.len(), READ_FILES + 1, "every file is reported");
        assert_eq!(
            unread,
            [Path::new("1000.txt")],
            "one file is past the budget"
        );

        let (later, _) = observe_until(&mut observer, |seen| !seen.is_empty());
        assert!(
            matches!(
                later.as_slice(),
                [Observed::Modified { path, content: Some(content), .. }]
                    if path == Path::new("1000.txt") && content.size == 4
            ),
            "{later:?}"
        );
        assert!(observer.unread.is_empty());
    }

    #[test]
    fn a_file_larger_than_the_budget_is_read_before_the_changes_of_a_later_observation() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"a").unwrap();
        File::create(dir.path().join("z.bin"))
            .unwrap()
            .set_len(READ_BYTES + 1)
            .unwrap();
        let mut observer = observer(dir.path());
        let (first, _) = observe_until(&mut observer, |seen| !seen.is_empty());
        assert!(
            first.iter().any(|change| matches!(
                change,
                Observed::Created { path, content: None, .. } if path == Path::new("z.bin")
            )),
            "{first:?}"
        );
        observer.watcher = Box::new(
            RescanWatcher::new(dir.path(), observer.snapshot().clone(), Duration::ZERO).unwrap(),
        );
        fs::write(dir.path().join("a.txt"), b"aa").unwrap();
        let next = observer.observe(Duration::ZERO).unwrap().unwrap().changes;
        assert!(
            next.iter().any(|change| matches!(
                change,
                Observed::Modified { path, content: Some(_), .. } if path == Path::new("z.bin")
            )),
            "{next:?}"
        );
        assert!(
            next.iter().any(|change| matches!(
                change,
                Observed::Modified { path, content: None, .. } if path == Path::new("a.txt")
            )),
            "{next:?}"
        );
    }

    fn work_to_observe_with_waiting(waiting: usize) -> u64 {
        let dir = marked();
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |_| true);
        let later = Instant::now() + Duration::from_secs(3600);
        for file in 0..waiting {
            let retry = Retry {
                failures: 0,
                due: later,
            };
            observer.unread.insert(format!("{file}").into(), retry);
        }
        let before = crate::counting::allocations();
        for _ in 0..20 {
            observer.observe(Duration::ZERO).unwrap();
        }
        crate::counting::allocations() - before
    }

    #[test]
    fn an_observation_takes_no_more_work_while_more_files_wait_to_be_read_again() {
        let few = work_to_observe_with_waiting(READ_FILES);
        let many = work_to_observe_with_waiting(10 * READ_FILES);
        assert!(
            many < 2 * few.max(1_000),
            "20 observations made {few} allocations while {} files waited and {many} while {} did",
            READ_FILES,
            10 * READ_FILES
        );
    }

    #[test]
    fn the_unread_file_due_longest_is_read_first() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"a").unwrap();
        fs::write(dir.path().join("b.txt"), b"b").unwrap();
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| seen.len() >= 2);
        let now = Instant::now();
        observer.unread.insert(
            "a.txt".into(),
            Retry {
                failures: 0,
                due: now,
            },
        );
        observer.unread.insert(
            "b.txt".into(),
            Retry {
                failures: 3,
                due: now - Duration::from_secs(1),
            },
        );
        let mut reads = Reads {
            files: READ_FILES - 1,
            ..Reads::default()
        };
        let mut out = Vec::new();
        observer.retry(now, &mut out, &mut reads);
        assert!(
            matches!(out.as_slice(), [Observed::Modified { path, .. }] if path == Path::new("b.txt")),
            "{out:?}"
        );
        assert!(observer.unread.contains_key(Path::new("a.txt")));
    }

    #[test]
    fn a_retry_the_caller_stopped_reads_no_further_and_keeps_every_file_due() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = marked();
        for file in 0..20 {
            fs::write(dir.path().join(format!("{file:04}.txt")), b"x").unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = {
            let stop = stop.clone();
            move || stop.load(Ordering::SeqCst)
        };
        let dev = RootAt {
            dev: fs::metadata(dir.path()).unwrap().dev(),
            volume: None,
        };
        let started = FsObserver::start(
            dir.path(),
            SPACE,
            dev,
            Snapshot::default(),
            Arc::new(Unfiltered),
            stopped,
        );
        let mut observer = started.unwrap();
        observe_until(&mut observer, |seen| seen.len() >= 20);
        let now = Instant::now();
        for file in 0..20 {
            observer.unread.insert(
                format!("{file:04}.txt").into(),
                Retry {
                    failures: 0,
                    due: now,
                },
            );
        }
        stop.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        let mut reads = Reads::default();
        observer.retry(now, &mut out, &mut reads);
        assert_eq!(reads.files, 1);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(observer.unread.len(), 20);
        assert!(
            (observer.unread.values()).all(|retry| retry.failures == 0 && retry.due == now),
            "{:?}",
            observer.unread
        );
    }

    #[test]
    fn an_open_the_os_interrupts_is_a_failed_read_and_the_retry_goes_on() {
        let dir = marked();
        for file in 0..20 {
            fs::write(dir.path().join(format!("{file:04}.txt")), b"x").unwrap();
        }
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| seen.len() >= 20);
        let now = Instant::now();
        for file in 0..20 {
            observer.unread.insert(
                format!("{file:04}.txt").into(),
                Retry {
                    failures: 0,
                    due: now,
                },
            );
        }
        INTERRUPTED_OPEN.set(Some("0000.txt".into()));
        let mut out = Vec::new();
        let mut reads = Reads::default();
        observer.retry(now, &mut out, &mut reads);
        assert_eq!(reads.files, 20);
        assert_eq!(out.len(), 19, "{out:?}");
        assert!(
            (out.iter()).all(|change| matches!(change,
                Observed::Modified { path, content: Some(_), .. } if path != Path::new("0000.txt"))),
            "{out:?}"
        );
        let unread: Vec<(&Path, u32)> = (observer.unread.iter())
            .map(|(path, retry)| (path.as_path(), retry.failures))
            .collect();
        assert_eq!(unread, [(Path::new("0000.txt"), 1)]);
    }

    #[test]
    fn an_observation_reads_no_more_bytes_than_the_budget_past_its_first_file() {
        let mut reads = Reads::default();
        assert!(
            reads.take(READ_BYTES * 2, false),
            "a file larger than the budget is read on its own"
        );
        assert!(!reads.take(0, false));
        let mut reads = Reads::default();
        assert!(reads.take(READ_BYTES / 2, false));
        assert!(reads.take(READ_BYTES / 2, false));
        assert!(!reads.take(1, false));
        assert!(
            reads.take(1, true),
            "a file that must be read is read past the budget"
        );
    }

    #[test]
    fn symbolic_links_are_not_followed() {
        let dir = marked();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(open_beneath(dir.path(), Path::new("link/secret")).is_err());
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("file")).unwrap();
        assert!(open_beneath(dir.path(), Path::new("file")).is_err());
        assert!(open_beneath(dir.path(), Path::new("../x")).is_err());
    }
    #[test]
    fn a_file_that_could_not_be_read_is_read_again_once_it_can_be() {
        use std::os::unix::fs::PermissionsExt;

        let dir = marked();
        let file = dir.path().join("locked");
        fs::write(&file, b"content").unwrap();
        fs::write(dir.path().join("gone"), b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(dir.path().join("gone"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        let mut observer = observer(dir.path());
        let (seen, _) = observe_until(&mut observer, |seen| seen.len() >= 2);
        assert!(
            seen.iter()
                .all(|change| matches!(change, Observed::Created { content: None, .. })),
            "{seen:?}"
        );
        assert_eq!(observer.unread.len(), 2);

        fs::remove_file(dir.path().join("gone")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
                && seen
                    .iter()
                    .any(|change| matches!(change, Observed::Removed { .. }))
        });
        let content = seen
            .iter()
            .find_map(|change| match change {
                Observed::Modified { path, content, .. } if path == Path::new("locked") => {
                    content.clone()
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no content in {seen:?}"));
        assert_eq!(content.size, 7);
        assert!(observer.unread.is_empty());
    }

    fn unread_file_read_at(dirs: &[&str], locked: &str, moves: &[(&str, &str)], read_at: &str) {
        use std::os::unix::fs::PermissionsExt;

        let dir = marked();
        for folder in dirs {
            fs::create_dir_all(dir.path().join(folder)).unwrap();
            fs::write(dir.path().join(folder).join("other"), b"readable").unwrap();
        }
        let file = dir.path().join(locked);
        fs::write(&file, b"content").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| seen.len() > dirs.len() * 2);
        assert_eq!(
            observer.unread.keys().collect::<Vec<_>>(),
            [Path::new(locked)]
        );

        for (from, to) in moves {
            match to {
                &"" => fs::remove_dir_all(dir.path().join(from)).unwrap(),
                to => fs::rename(dir.path().join(from), dir.path().join(to)).unwrap(),
            }
        }
        observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Renamed { .. }))
        });
        assert_eq!(
            observer.unread.keys().collect::<Vec<_>>(),
            [Path::new(read_at)]
        );
        fs::set_permissions(dir.path().join(read_at), fs::Permissions::from_mode(0o600)).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
        });
        let read: Vec<_> = (seen.iter())
            .filter_map(|change| match change {
                Observed::Modified { path, content, .. } => {
                    Some((path.clone(), content.as_ref().map(|c| c.size)))
                }
                _ => None,
            })
            .collect();
        assert_eq!(read, [(PathBuf::from(read_at), Some(7))]);
    }

    #[test]
    fn an_unread_file_in_a_folder_swapped_through_a_third_name_is_read_at_its_new_path() {
        unread_file_read_at(
            &["a", "b"],
            "a/locked",
            &[("a", "t"), ("b", "a"), ("t", "b")],
            "b/locked",
        );
    }

    #[test]
    fn an_unread_file_renamed_inside_a_renamed_folder_is_read_at_its_new_path() {
        unread_file_read_at(
            &["a"],
            "a/locked",
            &[("a", "b"), ("b/locked", "b/moved")],
            "b/moved",
        );
    }

    #[test]
    fn an_unread_file_in_a_folder_renamed_only_in_case_is_read_at_its_new_path() {
        unread_file_read_at(&["Docs"], "Docs/locked", &[("Docs", "docs")], "docs/locked");
    }

    #[test]
    fn an_unread_file_in_a_folder_whose_case_rotated_through_a_third_name_is_read_at_its_new_path()
    {
        unread_file_read_at(
            &["A", "b"],
            "A/locked",
            &[("A", "t"), ("b", "B"), ("t", "a")],
            "a/locked",
        );
    }

    #[test]
    fn an_unread_file_in_three_rotated_folders_is_read_at_its_new_path() {
        unread_file_read_at(
            &["a", "b", "c"],
            "a/locked",
            &[("a", "t"), ("c", "a"), ("b", "c"), ("t", "b")],
            "b/locked",
        );
    }

    #[test]
    fn an_unread_file_whose_folder_moved_before_a_new_file_took_its_path_is_read_at_its_new_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = marked();
        fs::create_dir(dir.path().join("a")).unwrap();
        let file = dir.path().join("a/locked");
        fs::write(&file, b"content").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| seen.len() >= 2);

        fs::rename(dir.path().join("a"), dir.path().join("b")).unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/locked"), b"new").unwrap();
        observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Renamed { .. }))
        });
        assert_eq!(
            observer.unread.keys().collect::<Vec<_>>(),
            [Path::new("b/locked")]
        );
        fs::set_permissions(
            dir.path().join("b/locked"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
        });
        let read: Vec<_> = (seen.iter())
            .filter_map(|change| match change {
                Observed::Modified { path, content, .. } => {
                    Some((path.clone(), content.as_ref().map(|c| c.size)))
                }
                _ => None,
            })
            .collect();
        assert_eq!(read, [(PathBuf::from("b/locked"), Some(7))]);
    }

    #[test]
    fn an_unread_file_unlocked_as_it_and_its_folder_are_renamed_is_read_at_its_new_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = marked();
        fs::create_dir(dir.path().join("a")).unwrap();
        let file = dir.path().join("a/locked");
        fs::write(&file, b"content").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| seen.len() >= 2);

        fs::rename(dir.path().join("a"), dir.path().join("b")).unwrap();
        fs::rename(dir.path().join("b/locked"), dir.path().join("b/moved")).unwrap();
        fs::set_permissions(
            dir.path().join("b/moved"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
        });
        let read: Vec<_> = (seen.iter())
            .filter_map(|change| match change {
                Observed::Modified { path, content, .. } => {
                    Some((path.clone(), content.as_ref().map(|c| c.size)))
                }
                _ => None,
            })
            .collect();
        assert_eq!(read, [(PathBuf::from("b/moved"), Some(7))]);
    }

    #[test]
    fn an_unread_file_moved_out_of_a_removed_folder_is_read_at_its_new_path() {
        unread_file_read_at(
            &["a", "c"],
            "a/locked",
            &[("a/locked", "c/locked"), ("a", "")],
            "c/locked",
        );
    }

    #[test]
    fn an_unread_file_that_is_renamed_is_read_at_its_new_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = marked();
        let file = dir.path().join("locked");
        fs::write(&file, b"content").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| !seen.is_empty());
        assert_eq!(observer.unread.len(), 1);

        fs::rename(&file, dir.path().join("moved")).unwrap();
        observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Renamed { .. }))
        });
        fs::set_permissions(dir.path().join("moved"), fs::Permissions::from_mode(0o600)).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
        });
        let path = seen
            .iter()
            .find_map(|change| match change {
                Observed::Modified { path, .. } => Some(path.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(path.as_os_str(), "moved");
    }

    #[test]
    fn moves_across_an_exclusion_reach_the_engine_both_ways() {
        struct IgnoredFolder;
        impl Matcher for IgnoredFolder {
            fn is_excluded(&self, path: &Path, _dir: bool) -> bool {
                path.starts_with("ignored")
            }
        }
        let dir = marked();
        let root = dir.path();
        fs::create_dir_all(root.join("ignored/folder")).unwrap();
        fs::write(root.join("ignored/folder/inner"), b"inner").unwrap();
        fs::write(root.join("tracked"), b"one").unwrap();
        let tracked = u128::from(fs::metadata(root.join("tracked")).unwrap().ino());
        let dev = RootAt {
            dev: fs::metadata(root).unwrap().dev(),
            volume: None,
        };
        let mut observer = FsObserver::start(
            root,
            SPACE,
            dev,
            Snapshot::default(),
            Arc::new(IgnoredFolder),
            || false,
        )
        .unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| !seen.is_empty());
        assert_eq!(seen.len(), 1, "{seen:?}");

        fs::rename(root.join("tracked"), root.join("ignored/tracked")).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| !seen.is_empty());
        let [Observed::Renamed { from, to, id, .. }] = &seen[..] else {
            panic!("a move into an excluded path is one rename: {seen:?}");
        };
        assert_eq!(
            (from.as_path(), to.as_path()),
            (Path::new("tracked"), Path::new("ignored/tracked"))
        );
        assert_eq!(id.ino, tracked);

        fs::write(root.join("ignored/tracked"), b"edited").unwrap();
        fs::rename(root.join("ignored/tracked"), root.join("back")).unwrap();
        fs::rename(root.join("ignored/folder"), root.join("folder")).unwrap();
        let created = |seen: &[Observed], name: &str| {
            seen.iter().find_map(|change| match change {
                Observed::Created {
                    path, id, content, ..
                } if path == Path::new(name) => Some((*id, content.clone())),
                _ => None,
            })
        };
        let (seen, _) = observe_until(&mut observer, |seen| {
            ["back", "folder", "folder/inner"]
                .iter()
                .all(|name| created(seen, name).is_some())
        });
        let (id, content) = created(&seen, "back").unwrap();
        assert_eq!(id.ino, tracked, "the entry that returns keeps its identity");
        assert_eq!(
            content.unwrap().size,
            6,
            "with the edit made while it was excluded"
        );
        assert_eq!(created(&seen, "folder/inner").unwrap().1.unwrap().size, 5);
        assert!(
            seen.iter()
                .all(|change| !matches!(change, Observed::Removed { .. })),
            "{seen:?}"
        );
    }

    fn paths(seen: &[Observed]) -> Vec<&Path> {
        let mut paths: Vec<&Path> = seen
            .iter()
            .map(|change| match change {
                Observed::Created { path, .. }
                | Observed::Modified { path, .. }
                | Observed::Removed { path, .. }
                | Observed::Reidentified { path, .. } => path.as_path(),
                Observed::Renamed { to, .. } => to.as_path(),
            })
            .collect();
        paths.sort();
        paths
    }

    /// A watcher that answers each poll from a script, and fails as the platform watcher can once the script ends.
    struct Scripted {
        snapshot: Snapshot,
        polls: Vec<Poll>,
    }

    impl Watcher for Scripted {
        fn poll(&mut self, _timeout: Duration) -> Result<Poll, FsError> {
            if self.polls.is_empty() {
                return Err(FsError::Watch("the platform watcher failed".into()));
            }
            Ok(self.polls.remove(0))
        }

        fn snapshot(&self) -> &Snapshot {
            &self.snapshot
        }

        fn reset(&mut self, _scope: &Path, _known: BTreeMap<PathBuf, Known>) {}
    }

    /// The first scan of an observer saw `proj`, a folder it knew, disappear and could not settle it, so it may not
    /// have read the folder's rules file. The folder is reported unreadable until a scan settles it. Once every scope
    /// has settled, a folder that is pending again is not: the rules a scan read for it stay in force.
    #[test]
    fn a_folder_the_first_scan_left_pending_is_unreadable_until_a_scan_settles_it() {
        let dir = marked();
        let root = dir.path();
        fs::create_dir(root.join("proj")).unwrap();
        let mut known = observer(root);
        known.observe(Duration::from_secs(10)).unwrap();
        let snapshot = known.snapshot().clone();
        let scanned = known.scanned();
        let dev = RootAt {
            dev: fs::metadata(root).unwrap().dev(),
            volume: None,
        };
        let mut observer = FsObserver::start(
            root,
            SPACE,
            dev,
            snapshot.clone(),
            Arc::new(Unfiltered),
            || false,
        )
        .unwrap();
        let proj = PathBuf::from("proj");
        observer.watcher = Box::new(Scripted {
            snapshot,
            polls: vec![
                Poll {
                    pending: vec![proj.clone()],
                    root: scanned,
                    read: vec![PathBuf::new()],
                    ..Poll::default()
                },
                Poll {
                    root: scanned,
                    read: vec![proj.clone()],
                    ..Poll::default()
                },
                Poll {
                    pending: vec![proj.clone()],
                    root: scanned,
                    ..Poll::default()
                },
            ],
        });
        let first =
            (observer.observe(Duration::ZERO).unwrap()).expect("the batch of the first scan");
        assert_eq!(first.unreadable, std::slice::from_ref(&proj));
        let settled =
            (observer.observe(Duration::ZERO).unwrap()).expect("the scan that read the folder");
        assert!(settled.unreadable.is_empty());
        assert_eq!(settled.read, [proj]);
        assert!(observer.observe(Duration::ZERO).unwrap().is_none());
    }

    /// The platform watcher fails after the user deleted a rules file it had not reported. The rescanning watcher that
    /// takes over compares with no rules, so it cannot report the rules file as gone. Its first batch says that it
    /// lists every rules file there is, so the engine forgets the others.
    #[test]
    fn the_first_batch_after_the_platform_watcher_fails_lists_every_rules_file() {
        let dir = marked();
        let root = dir.path();
        fs::create_dir(root.join("proj")).unwrap();
        fs::write(root.join("proj/.koiignore"), b"*.log\n").unwrap();
        fs::write(root.join("proj/build.log"), b"log").unwrap();
        let mut observer = observer(root);
        let first = (observer.observe(Duration::from_secs(10)).unwrap())
            .expect("the batch of the first scan");
        assert!(first.all_rules);
        assert!(observer.native, "the platform watcher is running");
        observer.watcher = Box::new(Scripted {
            snapshot: observer.watcher.snapshot().clone(),
            polls: Vec::new(),
        });
        fs::remove_file(root.join("proj/.koiignore")).unwrap();
        assert!(observer.observe(Duration::ZERO).unwrap().is_none());
        let after = (observer.observe(Duration::from_secs(10)).unwrap())
            .expect("the batch of the rescanning watcher's first scan");
        assert!(after.all_rules);
        assert_eq!(after.rules, []);
        assert_eq!(
            paths(&after.changes),
            [Path::new("proj/.koiignore"), Path::new("proj/build.log")]
        );
        assert!(
            !observer
                .observe(Duration::ZERO)
                .unwrap()
                .is_some_and(|next| next.all_rules)
        );
    }

    #[test]
    fn the_first_observation_lists_the_rules_files_and_leaves_out_what_they_exclude() {
        let dir = marked();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join(".koiignore"), b"*.log\nbroken[\n").unwrap();
        fs::write(root.join("sub/.koiignore"), b"/secret\n").unwrap();
        for name in [
            "a.log",
            "b.txt",
            "sub/c.log",
            "sub/secret",
            "sub/d.txt",
            ".DS_Store",
        ] {
            fs::write(root.join(name), b"x").unwrap();
        }
        let observation = observer(root)
            .observe(Duration::from_secs(10))
            .unwrap()
            .expect("the batch of the first scan");
        let expected = [".koiignore", "b.txt", "sub", "sub/.koiignore", "sub/d.txt"];
        assert_eq!(paths(&observation.changes), expected.map(Path::new));
        let dirs: Vec<&Path> = (observation.rules.iter())
            .map(|change| change.dir.as_path())
            .collect();
        assert_eq!(dirs, [Path::new(""), Path::new("sub")]);
        let root_rules = observation.rules[0].rules.as_ref().unwrap();
        assert_eq!(
            root_rules.invalid_lines().len(),
            1,
            "the line no pattern can use"
        );
    }

    #[test]
    fn the_first_observation_is_returned_when_the_folder_is_as_it_was() {
        let dir = marked();
        let mut observer = observer(dir.path());
        let observation = observer
            .observe(Duration::from_secs(10))
            .unwrap()
            .expect("the batch of the first scan, with nothing in it");
        assert!(observation.changes.is_empty() && observation.rules.is_empty());
        assert!(
            observer
                .observe(Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_rule_that_starts_to_match_removes_nothing_and_one_that_stops_brings_the_entry_back() {
        let dir = marked();
        let root = dir.path();
        fs::create_dir(root.join("build")).unwrap();
        fs::write(root.join("build/out.bin"), b"out").unwrap();
        fs::write(root.join("notes.txt"), b"notes").unwrap();
        let mut observer = observer(root);
        let (seen, _) = observe_until(&mut observer, |seen| seen.len() == 3);
        assert_eq!(
            paths(&seen),
            ["build", "build/out.bin", "notes.txt"].map(Path::new)
        );

        fs::write(root.join(".koiignore"), b"/build/\n").unwrap();
        let mut rules = Vec::new();
        let mut seen = Vec::new();
        while rules.is_empty() {
            let observation = observer.observe(Duration::from_secs(10)).unwrap().unwrap();
            seen.extend(observation.changes);
            rules.extend(observation.rules);
        }
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].dir, Path::new(""));
        assert!(rules[0].rules.is_some());
        assert!(
            seen.iter()
                .all(|change| !matches!(change, Observed::Removed { .. })),
            "becoming ignored is not a removal: {seen:?}"
        );
        assert!(!observer.snapshot().entries.contains_key(Path::new("build")));

        fs::remove_file(root.join(".koiignore")).unwrap();
        let (seen, observation) = observe_until(&mut observer, |seen| {
            paths(seen).contains(&Path::new("build/out.bin"))
        });
        assert!(paths(&seen).contains(&Path::new("build")), "{seen:?}");
        assert_eq!(observation.rules.len(), 1);
        assert!(
            observation.rules[0].rules.is_none(),
            "the folder has no rules file now"
        );
    }

    #[test]
    fn an_observer_started_again_finds_a_file_that_was_moved_into_an_ignored_folder() {
        let dir = marked();
        let root = dir.path();
        fs::write(root.join(".koiignore"), b"/build/\n").unwrap();
        fs::create_dir(root.join("build")).unwrap();
        fs::write(root.join("notes.txt"), b"notes").unwrap();
        let mut stopped = observer(root);
        stopped.observe(Duration::from_secs(10)).unwrap().unwrap();
        let previous = stopped.snapshot().clone();
        drop(stopped);
        fs::rename(root.join("notes.txt"), root.join("build/notes.txt")).unwrap();

        let dev = RootAt {
            dev: fs::metadata(root).unwrap().dev(),
            volume: None,
        };
        let mut observer =
            FsObserver::start(root, SPACE, dev, previous, Arc::new(Unfiltered), || false).unwrap();
        let observation = observer.observe(Duration::from_secs(10)).unwrap().unwrap();
        let [Observed::Renamed { from, to, .. }] = &observation.changes[..] else {
            panic!("one move, no removal: {:?}", observation.changes);
        };
        assert_eq!(
            (from.as_path(), to.as_path()),
            (Path::new("notes.txt"), Path::new("build/notes.txt"))
        );
        assert_eq!(
            observation.rules.len(),
            1,
            "a new observer lists every rules file"
        );
    }

    #[test]
    fn the_device_include_list_narrows_what_is_observed() {
        let dir = marked();
        let root = dir.path();
        for name in ["Work", "Photos"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::write(root.join(name).join("file"), b"x").unwrap();
        }
        fs::write(root.join("top.txt"), b"x").unwrap();
        let sync = crate::SyncRules {
            include: vec!["Work/".into()],
            exclude: vec![],
        };
        let dev = RootAt {
            dev: fs::metadata(root).unwrap().dev(),
            volume: None,
        };
        let matcher = crate::ignore::matcher(&sync).unwrap();
        let mut observer =
            FsObserver::start(root, SPACE, dev, Snapshot::default(), matcher, || false).unwrap();
        let observation = observer.observe(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(
            paths(&observation.changes),
            ["Work", "Work/file"].map(Path::new)
        );
    }
}
