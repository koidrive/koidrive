// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use koidrive_engine::tree::NodeId;
use koidrive_proto::v1::{Commit, op};
use tokio::sync::watch;

use crate::lifecycle::Lifecycle;
use crate::status::{Progress, Totals};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Direction {
    Upload,
    Download,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Item {
    pub direction: Direction,
    pub node: NodeId,
    pub digest: Vec<u8>,
}

struct Entry {
    size: u64,
    done: u64,
    credited: HashSet<Vec<u8>>,
    path: Option<String>,
}

struct Sink {
    status: Arc<Lifecycle>,
    progressed: Arc<watch::Sender<()>>,
    sum: Arc<Mutex<Sum>>,
    space_id: String,
}

#[derive(Default)]
pub(crate) struct Sum {
    live: BTreeMap<String, Progress>,
    moved: u64,
}

impl Sum {
    pub fn totals(&self) -> Option<Totals> {
        if self.live.is_empty() {
            return None;
        }
        let mut totals = Totals {
            bytes_done: self.moved,
            bytes_total: self.moved,
            ..Totals::default()
        };
        for progress in self.live.values() {
            totals.files_to_upload += progress.files_to_upload;
            totals.files_to_download += progress.files_to_download;
            totals.bytes_done += progress.bytes_done;
            totals.bytes_total += progress.bytes_total;
        }
        Some(totals)
    }

    fn set(&mut self, space_id: &str, progress: Option<Progress>, moved: u64) -> bool {
        self.moved += moved;
        let previous = match progress.clone() {
            Some(progress) => self.live.insert(space_id.to_owned(), progress),
            None => self.live.remove(space_id),
        };
        if self.live.is_empty() {
            self.moved = 0;
        }
        moved != 0 || previous != progress
    }
}

#[derive(Default)]
pub(crate) struct Tracker {
    entries: BTreeMap<Item, Entry>,
    uploads: u64,
    downloads: u64,
    done: u64,
    total: u64,
    finished: u64,
    current: Option<Item>,
    sink: Option<Sink>,
}

impl Tracker {
    pub fn publish_to(
        &mut self,
        status: Arc<Lifecycle>,
        progressed: Arc<watch::Sender<()>>,
        sum: Arc<Mutex<Sum>>,
        space_id: String,
    ) {
        self.sink = Some(Sink {
            status,
            progressed,
            sum,
            space_id,
        });
        self.publish(0);
    }

    pub fn progress(&self) -> Option<Progress> {
        if self.entries.is_empty() {
            return None;
        }
        Some(Progress {
            files_to_upload: self.uploads,
            files_to_download: self.downloads,
            bytes_done: self.finished + self.done,
            bytes_total: self.finished + self.total,
            current: (self.current.as_ref())
                .and_then(|item| self.entries.get(item))
                .and_then(|entry| entry.path.clone()),
        })
    }

    pub fn track(&mut self, wanted: BTreeMap<Item, u64>, synced: impl Fn(&Item) -> bool) -> bool {
        let shown = !self.entries.is_empty();
        let gone: Vec<Item> = (self.entries.keys())
            .filter(|item| !wanted.contains_key(*item))
            .cloned()
            .collect();
        for item in gone {
            let entry = self.remove(&item);
            self.finished += match synced(&item) {
                true => entry.size,
                false => entry.done,
            };
        }
        for (item, size) in wanted {
            if self.entries.contains_key(&item) {
                continue;
            }
            match item.direction {
                Direction::Upload => self.uploads += 1,
                Direction::Download => self.downloads += 1,
            }
            self.total += size;
            let entry = Entry {
                size,
                done: 0,
                credited: HashSet::new(),
                path: None,
            };
            self.entries.insert(item, entry);
        }
        let moved = match self.entries.is_empty() {
            true => std::mem::take(&mut self.finished),
            false => 0,
        };
        self.publish(moved);
        shown != !self.entries.is_empty()
    }

    pub fn clear(&mut self) -> bool {
        self.track(BTreeMap::new(), |_| false)
    }

    fn remove(&mut self, item: &Item) -> Entry {
        let entry = self.entries.remove(item).expect("the entry is tracked");
        match item.direction {
            Direction::Upload => self.uploads -= 1,
            Direction::Download => self.downloads -= 1,
        }
        self.done -= entry.done;
        self.total -= entry.size;
        if self.current.as_ref() == Some(item) {
            self.current = None;
        }
        entry
    }

    fn credit(&mut self, item: &Item, chunk: &[u8], bytes: u64, path: Option<&String>) {
        let Some(entry) = self.entries.get_mut(item) else {
            return;
        };
        if !entry.credited.insert(chunk.to_vec()) {
            return;
        }
        if let Some(path) = path {
            entry.path = Some(path.clone());
        }
        let done = entry.done.saturating_add(bytes).min(entry.size);
        self.done += done - entry.done;
        entry.done = done;
        self.current = Some(item.clone());
        self.publish(0);
    }

    fn publish(&self, moved: u64) {
        let Some(sink) = &self.sink else { return };
        let progress = self.progress();
        let changed = (sink.sum.lock().unwrap()).set(&sink.space_id, progress.clone(), moved);
        sink.status.lock().unwrap().progress = progress;
        if changed {
            sink.progressed.send_replace(());
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Credit {
    tracker: Option<Arc<Mutex<Tracker>>>,
    paths: HashMap<NodeId, String>,
    chunks: HashMap<Vec<u8>, Vec<(Item, u64)>>,
}

impl Credit {
    pub fn upload(
        tracker: &Arc<Mutex<Tracker>>,
        commit: &Commit,
        path_of: impl Fn(&str) -> Option<PathBuf>,
    ) -> Credit {
        let mut credit = Credit::download(tracker, []);
        for op in &commit.ops {
            let (node, content) = match &op.op {
                Some(op::Op::CreateFile(create)) => (&create.node_id, &create.content),
                Some(op::Op::UpdateContent(update)) => (&update.node_id, &update.content),
                _ => continue,
            };
            let Some(content) = content else { continue };
            if let Some(path) = path_of(node) {
                credit
                    .paths
                    .insert(node.clone(), path.to_string_lossy().into_owned());
            }
            let item = Item {
                direction: Direction::Upload,
                node: node.clone(),
                digest: content.manifest_digest.clone(),
            };
            for chunk in &content.chunks {
                let holders = credit.chunks.entry(chunk.hash.clone()).or_default();
                match holders.iter_mut().find(|(held, _)| *held == item) {
                    Some((_, bytes)) => *bytes += u64::from(chunk.size),
                    None => holders.push((item.clone(), u64::from(chunk.size))),
                }
            }
        }
        credit
    }

    pub fn download<'a>(
        tracker: &Arc<Mutex<Tracker>>,
        paths: impl IntoIterator<Item = (&'a str, PathBuf)>,
    ) -> Credit {
        Credit {
            tracker: Some(tracker.clone()),
            paths: (paths.into_iter())
                .map(|(node, path)| (node.to_owned(), path.to_string_lossy().into_owned()))
                .collect(),
            chunks: HashMap::new(),
        }
    }

    pub fn chunk(&self, hash: &[u8]) {
        let Some(tracker) = &self.tracker else { return };
        let mut tracker = tracker.lock().unwrap();
        for (item, bytes) in self.chunks.get(hash).into_iter().flatten() {
            tracker.credit(item, hash, *bytes, self.paths.get(&item.node));
        }
    }

    pub fn item(&self, item: &Item, hash: &[u8], bytes: u64) {
        let Some(tracker) = &self.tracker else { return };
        let path = self.paths.get(&item.node);
        tracker.lock().unwrap().credit(item, hash, bytes, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::FolderStatus;

    struct Folder {
        _dir: tempfile::TempDir,
        status: Arc<Lifecycle>,
        tracker: Tracker,
    }

    fn folder(space: &str, sum: &Arc<Mutex<Sum>>) -> Folder {
        let dir = tempfile::tempdir().unwrap();
        let status: FolderStatus = serde_json::from_value(serde_json::json!({
            "space_id": space, "space_name": space, "path": "/folder", "state": "catching_up",
            "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
            "local_entries": null, "questions": 0, "problem": null,
        }))
        .unwrap();
        let status = Arc::new(Lifecycle::open(
            &dir.path().join("store.sqlite"),
            status,
            koidrive_telemetry::Recorder::off(),
        ));
        let mut tracker = Tracker::default();
        let progressed = Arc::new(watch::Sender::new(()));
        tracker.publish_to(status.clone(), progressed, sum.clone(), space.into());
        Folder {
            _dir: dir,
            status,
            tracker,
        }
    }

    fn item(node: &str) -> Item {
        Item {
            direction: Direction::Upload,
            node: node.into(),
            digest: node.as_bytes().to_vec(),
        }
    }

    impl Folder {
        fn want(&mut self, files: &[(&str, u64)], synced: &[&str]) {
            let wanted = (files.iter())
                .map(|(node, size)| (item(node), *size))
                .collect();
            (self.tracker).track(wanted, |item| synced.contains(&item.node.as_str()));
        }

        fn credit(&mut self, node: &str, chunk: &str, bytes: u64) {
            (self.tracker).credit(&item(node), chunk.as_bytes(), bytes, None);
        }
    }

    fn shown(sum: &Arc<Mutex<Sum>>) -> Option<(u64, u64)> {
        let totals = sum.lock().unwrap().totals();
        totals.map(|totals| (totals.bytes_done, totals.bytes_total))
    }

    #[test]
    fn a_folder_that_finishes_keeps_its_whole_content_in_the_sum_while_another_syncs() {
        let sum = Arc::default();
        let (mut a, mut b) = (folder("a", &sum), folder("b", &sum));
        a.want(&[("a", 100)], &[]);
        a.credit("a", "1", 90);
        b.want(&[("b", 100)], &[]);
        b.credit("b", "1", 1);
        assert_eq!(shown(&sum), Some((91, 200)));
        a.want(&[], &["a"]);
        assert_eq!(shown(&sum), Some((101, 200)));
        b.credit("b", "2", 59);
        assert_eq!(shown(&sum), Some((160, 200)));
        b.want(&[], &["b"]);
        assert_eq!(shown(&sum), None);
    }

    #[test]
    fn a_file_abandoned_part_way_keeps_only_the_bytes_it_moved() {
        let sum = Arc::default();
        let (mut a, mut b) = (folder("a", &sum), folder("b", &sum));
        a.want(&[("a", 20_971_520)], &[]);
        a.credit("a", "1", 8_267_018);
        b.want(&[("b", 100)], &[]);
        b.credit("b", "1", 1);
        assert_eq!(shown(&sum), Some((8_267_019, 20_971_620)));
        a.want(&[], &[]);
        assert_eq!(shown(&sum), Some((8_267_019, 8_267_118)));
    }

    #[test]
    fn a_restarted_folder_keeps_its_credit_and_counts_a_chunk_sent_again_once() {
        let sum = Arc::default();
        let (mut a, mut b) = (folder("a", &sum), folder("b", &sum));
        a.want(&[("a", 100)], &[]);
        a.credit("a", "1", 30);
        b.want(&[("b", 10)], &[]);
        b.credit("b", "1", 1);
        assert_eq!(shown(&sum), Some((31, 110)));
        let progressed = Arc::new(watch::Sender::new(()));
        (a.tracker).publish_to(a.status.clone(), progressed, sum.clone(), "a".into());
        a.want(&[("a", 100)], &[]);
        a.credit("a", "1", 30);
        assert_eq!(shown(&sum), Some((31, 110)));
        a.credit("a", "2", 20);
        assert_eq!(shown(&sum), Some((51, 110)));
    }

    #[test]
    fn an_unlinked_folder_keeps_the_bytes_it_moved() {
        let sum = Arc::default();
        let (mut a, mut b) = (folder("a", &sum), folder("b", &sum));
        a.want(&[("a", 100)], &[]);
        a.credit("a", "1", 30);
        b.want(&[("b", 10)], &[]);
        b.credit("b", "1", 1);
        a.tracker.clear();
        assert_eq!(shown(&sum), Some((31, 40)));
    }

    #[test]
    fn new_work_grows_the_total_and_keeps_the_bytes_done() {
        let sum = Arc::default();
        let mut a = folder("a", &sum);
        a.want(&[("a", 100)], &[]);
        a.credit("a", "1", 50);
        assert_eq!(shown(&sum), Some((50, 100)));
        a.want(&[("a", 100), ("late", 50)], &[]);
        assert_eq!(shown(&sum), Some((50, 150)));
    }

    #[test]
    fn a_file_edited_while_it_syncs_keeps_the_bytes_its_old_content_moved() {
        let sum = Arc::default();
        let mut a = folder("a", &sum);
        a.want(&[("old", 100)], &[]);
        a.credit("old", "1", 40);
        a.want(&[("new", 80)], &[]);
        assert_eq!(shown(&sum), Some((40, 120)));
    }

    #[test]
    fn the_sum_counts_from_zero_once_no_folder_has_a_file_left() {
        let sum = Arc::default();
        let (mut a, mut b) = (folder("a", &sum), folder("b", &sum));
        a.want(&[("a", 100)], &[]);
        b.want(&[("b", 10)], &[]);
        a.want(&[], &["a"]);
        b.want(&[], &["b"]);
        assert_eq!(shown(&sum), None);
        b.want(&[("b2", 10)], &[]);
        assert_eq!(shown(&sum), Some((0, 10)));
    }
}
