// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::state::{ContentProvenance, ContentVersion, MapChanges};
use crate::tree::{NodeId, RemoteNode, Tree, read_entries};

const IMPLIED: &str = "R holds every node whose provenance it implies";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Implied {
    own: bool,
    synced: bool,
}

impl Implied {
    fn of(record: &ContentProvenance, remote: &RemoteNode) -> Option<Self> {
        let implied = Self {
            own: record.local.as_ref()?.own,
            synced: record.synced.is_some(),
        };
        (implied.record(remote) == *record).then_some(implied)
    }

    fn record(self, remote: &RemoteNode) -> ContentProvenance {
        let version = ContentVersion::remote(remote, self.own);
        ContentProvenance {
            synced: version.clone().filter(|_| self.synced),
            local: version,
            ..ContentProvenance::default()
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Provenances {
    held: BTreeMap<NodeId, ContentProvenance>,
    implied: BTreeMap<NodeId, Implied>,
    before: BTreeMap<NodeId, Option<ContentProvenance>>,
    #[cfg(any(test, feature = "check-provenance"))]
    full: BTreeMap<NodeId, ContentProvenance>,
    #[cfg(any(test, feature = "check-provenance"))]
    taken: BTreeMap<NodeId, ContentProvenance>,
}

impl Provenances {
    pub(crate) fn restored(
        records: BTreeMap<NodeId, ContentProvenance>,
        r: &Tree<RemoteNode>,
    ) -> Self {
        let mut provenances = Self {
            #[cfg(any(test, feature = "check-provenance"))]
            full: records.clone(),
            #[cfg(any(test, feature = "check-provenance"))]
            taken: records.clone(),
            ..Self::default()
        };
        for (id, record) in records {
            if let Some(implied) = r.get(&id).and_then(|remote| Implied::of(&record, remote)) {
                provenances.implied.insert(id, implied);
            } else {
                provenances.held.insert(id, record);
            }
        }
        provenances
    }

    pub(crate) fn get(
        &self,
        id: &NodeId,
        r: &Tree<RemoteNode>,
    ) -> Option<Cow<'_, ContentProvenance>> {
        read_entries(1);
        let record = self.view(id, r);
        #[cfg(any(test, feature = "check-provenance"))]
        assert_eq!(
            record.as_deref(),
            self.full.get(id),
            "the provenance of {id:?}"
        );
        record
    }

    #[cfg(test)]
    pub(crate) fn holds(&self, id: &NodeId) -> bool {
        self.held.contains_key(id)
    }

    fn view(&self, id: &NodeId, r: &Tree<RemoteNode>) -> Option<Cow<'_, ContentProvenance>> {
        if let Some(record) = self.held.get(id) {
            return Some(Cow::Borrowed(record));
        }
        let implied = self.implied.get(id)?;
        Some(Cow::Owned(implied.record(r.get(id).expect(IMPLIED))))
    }

    pub(crate) fn update(
        &mut self,
        id: &NodeId,
        r: &Tree<RemoteNode>,
        insert: bool,
        mut change: impl FnMut(&mut ContentProvenance),
    ) {
        if !insert && !self.held.contains_key(id) && !self.implied.contains_key(id) {
            #[cfg(any(test, feature = "check-provenance"))]
            assert!(!self.full.contains_key(id), "the provenance of {id:?}");
            return;
        }
        self.touch(id, r);
        self.materialize(id, r);
        #[cfg(any(test, feature = "check-provenance"))]
        change(self.full.entry(id.clone()).or_default());
        change(self.held.entry(id.clone()).or_default());
        self.imply(id, r);
    }

    pub(crate) fn insert(&mut self, id: NodeId, r: &Tree<RemoteNode>, record: ContentProvenance) {
        #[cfg(any(test, feature = "check-provenance"))]
        self.full.insert(id.clone(), record.clone());
        self.touch(&id, r);
        self.implied.remove(&id);
        self.held.insert(id.clone(), record);
        self.imply(&id, r);
    }

    pub(crate) fn prune(&mut self, id: &NodeId, r: &Tree<RemoteNode>) {
        let empty = |record: &ContentProvenance| {
            record.synced.is_none()
                && record.local.is_none()
                && record.edit_bases.is_empty()
                && record.unversioned_bases.is_empty()
        };
        #[cfg(any(test, feature = "check-provenance"))]
        if self.full.get(id).is_some_and(empty) {
            self.full.remove(id);
        }
        if self.held.get(id).is_some_and(empty) {
            self.touch(id, r);
            self.held.remove(id);
        }
    }

    pub(crate) fn remote_changing(
        &mut self,
        id: &NodeId,
        r: &Tree<RemoteNode>,
        new: Option<&RemoteNode>,
    ) {
        let Some(&implied) = self.implied.get(id) else {
            return;
        };
        let old = r.get(id).expect(IMPLIED);
        if new.is_none_or(|new| implied.record(new) != implied.record(old)) {
            self.materialize(id, r);
        }
    }

    fn materialize(&mut self, id: &NodeId, r: &Tree<RemoteNode>) {
        let Some(implied) = self.implied.remove(id) else {
            return;
        };
        let record = implied.record(r.get(id).expect(IMPLIED));
        #[cfg(any(test, feature = "check-provenance"))]
        assert_eq!(Some(&record), self.full.get(id), "the provenance of {id:?}");
        self.held.insert(id.clone(), record);
    }

    fn imply(&mut self, id: &NodeId, r: &Tree<RemoteNode>) {
        let implied = (self.held.get(id).zip(r.get(id)))
            .and_then(|(record, remote)| Implied::of(record, remote));
        if let Some(implied) = implied {
            self.held.remove(id);
            self.implied.insert(id.clone(), implied);
        }
    }

    fn touch(&mut self, id: &NodeId, r: &Tree<RemoteNode>) {
        if !self.before.contains_key(id) {
            let record = self.view(id, r).map(Cow::into_owned);
            self.before.insert(id.clone(), record);
        }
    }

    pub(crate) fn state(&self, r: &Tree<RemoteNode>) -> BTreeMap<NodeId, ContentProvenance> {
        read_entries(self.held.len() + self.implied.len());
        let mut records = self.held.clone();
        records.extend(
            self.implied
                .iter()
                .map(|(id, implied)| (id.clone(), implied.record(r.get(id).expect(IMPLIED)))),
        );
        #[cfg(any(test, feature = "check-provenance"))]
        assert_eq!(records, self.full, "the provenance records");
        records
    }

    pub(crate) fn take_changes(
        &mut self,
        r: &Tree<RemoteNode>,
    ) -> MapChanges<NodeId, ContentProvenance> {
        read_entries(self.before.len());
        let changes: MapChanges<_, _> = (std::mem::take(&mut self.before).into_iter())
            .filter_map(|(id, held)| {
                let record = self.view(&id, r).map(Cow::into_owned);
                (record != held).then_some((id, record))
            })
            .collect();
        #[cfg(any(test, feature = "check-provenance"))]
        {
            let expected = crate::state::map_changes(&self.taken, &self.full);
            assert_eq!(changes, expected, "the provenance changes");
            self.taken = self.full.clone();
        }
        changes
    }
}
