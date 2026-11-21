// SPDX-License-Identifier: Apache-2.0

//! The journal of RFC 0029 §5.1: each change this device's user made that the server may not hold in its current
//! form, with the intent the user saw, the history of the intents R held, and the seen list of each delete.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use koidrive_proto::v1 as proto;
use sha1::{Digest, Sha1};

use crate::ids::{IdGen, uuid_v7_bits};
use crate::names::same_name;
use crate::state::{ContentValue, MapChanges};
use crate::tracked::TrackedMap;
use crate::tree::NodeId;
use rows::Rows;
use seen::SeenLists;

pub const HISTORY_KEPT: usize = 64;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    Place,
    Content,
    Status,
}

impl Field {
    pub const ALL: [Field; 3] = [Field::Place, Field::Content, Field::Status];

    pub fn as_str(self) -> &'static str {
        match self {
            Field::Place => "place",
            Field::Content => "content",
            Field::Status => "status",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Field::ALL.into_iter().find(|field| field.as_str() == text)
    }
}

/// The 16-byte id of an intent, as the wire carries it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IntentId([u8; 16]);

const LEGACY_NAMESPACE: [u8; 16] = [
    0x70, 0x14, 0x71, 0xd0, 0xf5, 0xea, 0x47, 0xeb, 0x93, 0x6a, 0x48, 0x5a, 0x91, 0xd0, 0x89, 0x1e,
];

impl IntentId {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        <[u8; 16]>::try_from(bytes).ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn parse(text: &str) -> Option<Self> {
        let hex: String = text.chars().filter(|c| *c != '-').collect();
        (text.len() == 36 && hex.len() == 32)
            .then(|| u128::from_str_radix(&hex, 16).ok())
            .flatten()
            .map(|value| Self(value.to_be_bytes()))
    }

    pub fn mint(ids: &mut IdGen, unix_ms: u64) -> Self {
        Self(uuid_v7_bits(unix_ms, ids.next_u128()).to_be_bytes())
    }

    /// The UUIDv5 of RFC 0029 §7.2.
    pub fn legacy(node: &str, field: Field, key: &[&str]) -> Self {
        let mut hash = Sha1::new();
        hash.update(LEGACY_NAMESPACE);
        hash.update(node.as_bytes());
        hash.update([0]);
        hash.update(field.as_str().as_bytes());
        for part in key {
            hash.update([0]);
            hash.update(part.as_bytes());
        }
        let digest = hash.finalize();
        let mut id = [0; 16];
        id.copy_from_slice(&digest[..16]);
        id[6] = (id[6] & 0x0f) | 0x50;
        id[8] = (id[8] & 0x3f) | 0x80;
        Self(id)
    }

    pub fn legacy_content(node: &str, version_id: &str) -> Self {
        Self::legacy(node, Field::Content, &[version_id])
    }

    pub fn legacy_place(node: &str, meta_rev: u64, parent: &str, name: &str) -> Self {
        Self::legacy(node, Field::Place, &[&meta_rev.to_string(), parent, name])
    }

    pub fn legacy_live(node: &str, seq: u64) -> Self {
        Self::legacy(node, Field::Status, &[&format!("live:{seq}")])
    }

    pub fn legacy_trashed(node: &str, trash_group: &str) -> Self {
        Self::legacy(node, Field::Status, &[&format!("trashed:{trash_group}")])
    }
}

impl fmt::Display for IntentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = format!("{:032x}", u128::from_be_bytes(self.0));
        let parts = [
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..],
        ];
        f.write_str(&parts.join("-"))
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for IntentId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for IntentId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        IntentId::parse(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("intent id {text:?}")))
    }
}

impl fmt::Debug for IntentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldIntent {
    pub id: IntentId,
    pub author: Arc<str>,
    pub replay: bool,
    pub applied_at_ms: i64,
}

impl FieldIntent {
    pub fn legacy(id: IntentId) -> Self {
        Self {
            id,
            author: "".into(),
            replay: false,
            applied_at_ms: 0,
        }
    }

    pub fn from_proto(intent: &proto::Intent) -> Option<Self> {
        Some(Self {
            id: IntentId::from_bytes(&intent.id)?,
            author: intent.author_device_id.as_str().into(),
            replay: intent.replay,
            applied_at_ms: intent.applied_at_ms,
        })
    }

    pub fn to_proto(&self) -> proto::Intent {
        proto::Intent {
            id: self.id.0.to_vec(),
            author_device_id: self.author.to_string(),
            replay: self.replay,
            over: Vec::new(),
            applied_at_ms: self.applied_at_ms,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeIntents {
    pub place: Option<FieldIntent>,
    pub content: Option<FieldIntent>,
    pub status: Option<FieldIntent>,
    pub trash_over: Option<IntentId>,
    pub trash_held: Vec<IntentId>,
}

impl NodeIntents {
    pub fn get(&self, field: Field) -> Option<&FieldIntent> {
        match field {
            Field::Place => self.place.as_ref(),
            Field::Content => self.content.as_ref(),
            Field::Status => self.status.as_ref(),
        }
    }

    pub fn set(&mut self, field: Field, intent: Option<FieldIntent>) {
        match field {
            Field::Place => self.place = intent,
            Field::Content => self.content = intent,
            Field::Status => self.status = intent,
        }
    }

    pub fn from_proto(intents: &proto::FieldIntents) -> Self {
        let read =
            |intent: &Option<proto::Intent>| intent.as_ref().and_then(FieldIntent::from_proto);
        Self {
            place: read(&intents.place),
            content: read(&intents.content),
            status: read(&intents.status),
            trash_over: (intents.status.as_ref())
                .and_then(|status| IntentId::from_bytes(&status.over)),
            trash_held: Vec::new(),
        }
    }

    pub fn to_proto(&self) -> proto::FieldIntents {
        proto::FieldIntents {
            place: self.place.as_ref().map(FieldIntent::to_proto),
            content: self.content.as_ref().map(FieldIntent::to_proto),
            status: self.status.as_ref().map(|status| proto::Intent {
                over: (self.trash_over).map_or_else(Vec::new, |over| over.0.to_vec()),
                ..status.to_proto()
            }),
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub parent: NodeId,
    pub name: String,
    pub mark: Option<String>,
}

/// A field's value. Places compare names by their NFC form, contents by [`ContentValue`] and never by version id.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Place(Place),
    Content {
        version_id: Option<String>,
        value: ContentValue,
    },
    Status {
        live: bool,
    },
}

impl Value {
    pub fn field(&self) -> Field {
        match self {
            Value::Place(_) => Field::Place,
            Value::Content { .. } => Field::Content,
            Value::Status { .. } => Field::Status,
        }
    }

    pub fn same(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Place(a), Value::Place(b)) => {
                a.parent == b.parent && same_name(&a.name, &b.name) && a.mark == b.mark
            }
            (Value::Content { value: a, .. }, Value::Content { value: b, .. }) => a == b,
            (Value::Status { live: a }, Value::Status { live: b }) => a == b,
            _ => false,
        }
    }
}

/// What an op sends as its base: RFC 0026's `made_over` and `source_version_id` for content, the place a move
/// leaves for a place.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WireBase {
    #[default]
    None,
    Place(Place),
    Content {
        made_over: Vec<String>,
        source_version_id: Option<String>,
    },
}

/// `Carried` holds the author's device id when the store could show who made the value.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Author {
    #[default]
    Own,
    Carried(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    WithIntents,
    WithoutIntents,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpsWithoutIntent(pub usize);

/// An intent the commit in flight sends for a field whose row a later change of the user replaced.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flight {
    pub intent: IntentId,
    pub value: Value,
    pub base: Option<IntentId>,
    pub wire_base: WireBase,
    pub time_ms: i64,
    #[cfg_attr(feature = "serde", serde(default))]
    pub author: Author,
    #[cfg_attr(feature = "serde", serde(default))]
    pub replay: bool,
}

impl Flight {
    fn sent(self) -> Row {
        Row {
            intent: self.intent,
            author: self.author,
            base: self.base,
            value: self.value,
            wire_base: self.wire_base,
            state: RowState::Sent,
            time_ms: self.time_ms,
            replay: self.replay,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    Local {
        flight: Option<Flight>,
    },
    Sent,
    Held {
        held_as: IntentId,
        version_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub intent: IntentId,
    pub author: Author,
    pub base: Option<IntentId>,
    pub value: Value,
    pub wire_base: WireBase,
    pub state: RowState,
    pub time_ms: i64,
    pub replay: bool,
}

impl Row {
    pub fn is_sent(&self) -> bool {
        self.state == RowState::Sent
    }

    fn in_flight(&self) -> Option<Flight> {
        match &self.state {
            RowState::Sent => Some(self.as_flight()),
            RowState::Local { flight } => flight.clone(),
            RowState::Held { .. } => None,
        }
    }

    fn as_flight(&self) -> Flight {
        Flight {
            intent: self.intent,
            value: self.value.clone(),
            base: self.base,
            wire_base: self.wire_base.clone(),
            time_ms: self.time_ms,
            author: self.author.clone(),
            replay: self.replay,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub intent: IntentId,
    pub applied_at_ms: i64,
    pub in_local: bool,
}

/// One entry of a delete's seen list: an intent L held for the deleted node or a node below it. The value is known
/// for what L held when the user deleted, and unknown for an older intent the history kept.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub node: NodeId,
    pub field: Field,
    pub intent: IntentId,
    pub value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteField {
    pub node: NodeId,
    pub field: Field,
    pub intent: IntentId,
    pub value: Value,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteView {
    pub unseen: Vec<IntentId>,
    pub held: Vec<IntentId>,
}

pub fn delete_view(seen: &[Seen], remote: &[RemoteField]) -> DeleteView {
    let seen_ids: BTreeSet<IntentId> = seen.iter().map(|entry| entry.intent).collect();
    let unseen = (remote.iter())
        .filter(|field| {
            !seen_ids.contains(&field.intent)
                && !seen.iter().any(|entry| {
                    entry.node == field.node
                        && entry.field == field.field
                        && entry
                            .value
                            .as_ref()
                            .is_some_and(|value| value.same(&field.value))
                })
        })
        .map(|field| field.intent);
    let remote_ids: BTreeSet<IntentId> = remote.iter().map(|field| field.intent).collect();
    let mut held_once = BTreeSet::new();
    let held = (seen.iter())
        .map(|entry| entry.intent)
        .filter(|intent| !remote_ids.contains(intent) && held_once.insert(*intent));
    DeleteView {
        unseen: unseen.collect::<BTreeSet<_>>().into_iter().collect(),
        held: held.collect(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRow {
    pub intent: IntentId,
    pub author: Author,
    pub base: Option<IntentId>,
    pub held_as: IntentId,
    pub wire_base: Option<Box<WireBase>>,
    pub replay: bool,
    pub versioned: bool,
}

impl HeldRow {
    pub fn full(&self, value: Value, time_ms: i64, version_id: Option<String>) -> Row {
        let value = match value {
            Value::Content { value, .. } if !self.versioned => Value::Content {
                version_id: None,
                value,
            },
            value => value,
        };
        Row {
            intent: self.intent,
            author: self.author.clone(),
            base: self.base,
            value,
            wire_base: (self.wire_base.as_deref())
                .cloned()
                .unwrap_or(WireBase::None),
            state: RowState::Held {
                held_as: self.held_as,
                version_id,
            },
            time_ms,
            replay: self.replay,
        }
    }

    pub(crate) fn of(row: &Row) -> Option<Self> {
        let RowState::Held { held_as, .. } = row.state else {
            return None;
        };
        Some(Self {
            intent: row.intent,
            author: row.author.clone(),
            base: row.base,
            held_as,
            wire_base: (row.wire_base != WireBase::None).then(|| Box::new(row.wire_base.clone())),
            replay: row.replay,
            versioned: matches!(
                row.value,
                Value::Content {
                    version_id: Some(_),
                    ..
                }
            ),
        })
    }
}

pub type RowKey = (NodeId, Field);

/// What the store keeps of the journal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JournalState {
    pub rows: BTreeMap<RowKey, Row>,
    pub history: BTreeMap<RowKey, Vec<HistoryEntry>>,
    pub seen: BTreeMap<NodeId, Vec<Seen>>,
    /// R's intents came from a listing of a server that announced `field_intents`, so no id R holds was computed.
    pub listed: bool,
    /// The nodes R lacks.
    pub absent: BTreeMap<NodeId, Absent>,
}

/// A node R lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Absent {
    /// The journal time of the first listing, record or result that showed R without the node.
    pub since_ms: i64,
    /// The server answered a create of the node `SUPERSEDED` because it purged it, so no create of it is sent again
    /// (RFC 0029 section 5.2).
    pub purged: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JournalChanges {
    pub rows: MapChanges<RowKey, Row>,
    pub history: MapChanges<RowKey, Vec<HistoryEntry>>,
    pub seen: MapChanges<NodeId, Vec<Seen>>,
    pub listed: Option<bool>,
    pub absent: MapChanges<NodeId, Absent>,
}

impl JournalChanges {
    pub fn between(before: &JournalState, after: &JournalState) -> Self {
        Self {
            rows: changes(&before.rows, &after.rows),
            history: changes(&before.history, &after.history),
            seen: changes(&before.seen, &after.seen),
            listed: (before.listed != after.listed).then_some(after.listed),
            absent: changes(&before.absent, &after.absent),
        }
    }
}

fn changes<K: Ord + Clone, V: Clone + PartialEq>(
    before: &BTreeMap<K, V>,
    after: &BTreeMap<K, V>,
) -> MapChanges<K, V> {
    let keys: BTreeSet<&K> = before.keys().chain(after.keys()).collect();
    (keys.into_iter())
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|key| (key.clone(), after.get(key).cloned()))
        .collect()
}

/// The journal, the history and the seen lists, with the clock and generator that mint this user's intents.
#[derive(Clone, Debug)]
pub struct Journal {
    rows: Rows,
    held_changed: BTreeSet<RowKey>,
    history: TrackedMap<RowKey, Vec<HistoryEntry>>,
    seen: SeenLists,
    listed: bool,
    listed_before: bool,
    ids: IdGen,
    absent: TrackedMap<NodeId, Absent>,
    lapsing: BTreeSet<(i64, NodeId)>,
    now_ms: u64,
}

const ABSENT_FOR_MS: i64 = 30 * 24 * 60 * 60 * 1000;

const JOURNAL_STREAM: u64 = 0x6a09_e667_f3bc_c908;

impl PartialEq for Journal {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows
            && self.history == other.history
            && *self.seen == *other.seen
            && self.listed == other.listed
            && self.absent == other.absent
    }
}

impl Journal {
    pub fn new(engine: &IdGen) -> Self {
        let mut ids = IdGen::new(engine.state ^ JOURNAL_STREAM);
        ids.next_u64();
        Self {
            rows: Rows::default(),
            held_changed: BTreeSet::new(),
            history: TrackedMap::default(),
            seen: SeenLists::default(),
            listed: false,
            listed_before: false,
            ids,
            absent: TrackedMap::default(),
            lapsing: BTreeSet::new(),
            now_ms: 0,
        }
    }

    pub fn restore(&mut self, state: JournalState) {
        *self = Self {
            rows: Rows::restore(state.rows),
            held_changed: BTreeSet::new(),
            history: state.history.into(),
            seen: state.seen.into(),
            listed: state.listed,
            listed_before: state.listed,
            ids: self.ids.clone(),
            lapsing: (state.absent.iter())
                .map(|(node, absent)| (absent.since_ms, node.clone()))
                .collect(),
            absent: state.absent.into(),
            now_ms: self.now_ms,
        };
    }

    pub fn state(&self, full: impl Fn(&RowKey, &HeldRow) -> Row) -> JournalState {
        let mut rows: BTreeMap<RowKey, Row> = (self.rows.iter())
            .map(|(key, row)| (key.clone(), row.clone()))
            .collect();
        rows.extend((self.rows.iter_compact()).map(|(key, held)| {
            let row = full(&key, &held);
            (key, row)
        }));
        JournalState {
            rows,
            history: self.history.to_map(),
            seen: self.seen.to_map(),
            listed: self.listed,
            absent: self.absent.to_map(),
        }
    }

    pub fn take_changes(&mut self, full: impl Fn(&RowKey, &HeldRow) -> Row) -> JournalChanges {
        let mut keys: BTreeSet<RowKey> = std::mem::take(&mut self.held_changed);
        keys.extend(self.rows.take_changes().into_iter().map(|(key, _)| key));
        let rows = (keys.into_iter())
            .map(|key| {
                let row = (self.rows.get(&key).cloned())
                    .or_else(|| self.rows.get_compact(&key).map(|held| full(&key, &held)));
                (key, row)
            })
            .collect();
        let listed = (self.listed != self.listed_before).then_some(self.listed);
        self.listed_before = self.listed;
        JournalChanges {
            rows,
            history: self.history.take_changes(),
            seen: self.seen.take_changes(),
            listed,
            absent: self.absent.take_changes(),
        }
    }

    /// Sets the time the journal mints and ends rows at, and ends what RFC 0029 §5.1 ends for a node R has lacked
    /// for 30 days.
    pub fn set_clock(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        let lapsed_by = now_ms as i64 - ABSENT_FOR_MS;
        let lapsed: Vec<NodeId> = (self.lapsing.iter())
            .take_while(|(since_ms, _)| {
                #[cfg(test)]
                tests::CLOCK_VISITS.with(|visits| visits.set(visits.get() + 1));
                *since_ms <= lapsed_by
            })
            .map(|(_, node)| node.clone())
            .collect();
        for node in lapsed {
            self.end(&node, Field::Status);
            for field in [Field::Place, Field::Content] {
                let key = (node.clone(), field);
                let held = self.rows.get_compact(&key).is_some()
                    || (self.rows.get(&key))
                        .is_some_and(|row| matches!(row.state, RowState::Held { .. }));
                if held {
                    self.end(&node, field);
                }
            }
            let ended = (Field::ALL.iter()).all(|field| {
                self.row(&node, *field).is_none() && self.compact_row(&node, *field).is_none()
            });
            if ended {
                for field in Field::ALL {
                    self.history.remove(&(node.clone(), field));
                }
                self.drop_absent(&node);
            }
        }
    }

    pub fn nodes(&self) -> BTreeSet<NodeId> {
        (self.rows.iter().map(|(key, _)| key.0.clone()))
            .chain(self.rows.iter_compact().map(|((node, _), _)| node))
            .chain(self.history.keys().map(|key| key.0.clone()))
            .chain(self.seen.keys().cloned())
            .collect()
    }

    pub fn r_lacks(&mut self, node: &NodeId) {
        if self.absent.get(node).is_none() {
            let since_ms = self.now_ms as i64;
            self.put_absent(
                node,
                Absent {
                    since_ms,
                    purged: false,
                },
            );
        }
    }

    pub fn superseded_as_purged(&mut self, node: &NodeId) {
        let since_ms = (self.absent.get(node)).map_or(self.now_ms as i64, |absent| absent.since_ms);
        self.put_absent(
            node,
            Absent {
                since_ms,
                purged: true,
            },
        );
    }

    fn put_absent(&mut self, node: &NodeId, absent: Absent) {
        self.drop_absent(node);
        self.lapsing.insert((absent.since_ms, node.to_owned()));
        self.absent.insert(node.to_owned(), absent);
    }

    fn drop_absent(&mut self, node: &NodeId) {
        if let Some(absent) = self.absent.remove(node) {
            self.lapsing.remove(&(absent.since_ms, node.to_owned()));
        }
    }

    /// Whether the server answered a create of `node` `SUPERSEDED` because it purged it, and R has not held the node since.
    pub fn purged(&self, node: &NodeId) -> bool {
        self.absent.get(node).is_some_and(|absent| absent.purged)
    }

    pub fn r_holds(&mut self, node: &NodeId) {
        self.drop_absent(node);
    }

    pub fn mint(&mut self) -> IntentId {
        IntentId::mint(&mut self.ids, self.now_ms)
    }

    pub fn listed(&self) -> bool {
        self.listed
    }

    pub fn set_listed(&mut self, listed: bool) {
        self.listed = listed;
    }

    pub(crate) fn row(&self, node: &NodeId, field: Field) -> Option<&Row> {
        self.rows.get(&(node.to_owned(), field))
    }

    pub(crate) fn rows(&self) -> impl Iterator<Item = (&RowKey, &Row)> {
        self.rows.iter()
    }

    pub fn compact_row(&self, node: &NodeId, field: Field) -> Option<HeldRow> {
        self.rows.get_compact(&(node.to_owned(), field))
    }

    pub(crate) fn compact_rows(&self) -> impl Iterator<Item = (RowKey, HeldRow)> {
        self.rows.iter_compact()
    }

    pub(crate) fn held_rows(&self) -> impl Iterator<Item = (&RowKey, &Row)> {
        (self.rows.iter()).filter(|(_, row)| matches!(row.state, RowState::Held { .. }))
    }

    pub(crate) fn compact(&mut self, key: RowKey) -> bool {
        let Some(held) = self.rows.get(&key).and_then(HeldRow::of) else {
            return false;
        };
        if let Some((_, true)) = self.rows.hand_off(&key) {
            self.held_changed.insert(key.clone());
        }
        self.rows.insert_compact(key, held);
        true
    }

    pub(crate) fn write_out(&mut self, key: RowKey, row: Row) {
        self.rows.remove_compact(&key);
        self.rows.insert_quietly(key, row);
    }

    pub fn history(&self, node: &NodeId, field: Field) -> &[HistoryEntry] {
        (self.history.get(&(node.to_owned(), field))).map_or(&[], Vec::as_slice)
    }

    pub fn placed_under(&self, parent: &NodeId) -> impl Iterator<Item = NodeId> {
        self.rows.under(parent).into_iter().map(|(node, _)| node)
    }

    pub fn seen(&self, node: &NodeId) -> &[Seen] {
        self.seen.get(node).map_or(&[], Vec::as_slice)
    }

    pub fn user_change(
        &mut self,
        node: &NodeId,
        value: Value,
        wire_base: WireBase,
        seen: Option<IntentId>,
    ) -> IntentId {
        let key = (node.to_owned(), value.field());
        let old = self.rows.get(&key);
        let flight = old.and_then(Row::in_flight);
        let base = match old {
            None => (self.rows.get_compact(&key)).map_or(seen, |held| Some(held.held_as)),
            Some(old) => match &old.state {
                RowState::Local { .. } => old.base,
                RowState::Sent => Some(old.intent),
                RowState::Held { held_as, .. } => Some(*held_as),
            },
        };
        let row = match flight {
            Some(flight) if flight.value.same(&value) => Row {
                value,
                ..flight.sent()
            },
            flight => Row {
                intent: self.mint(),
                author: Author::Own,
                base,
                value,
                wire_base,
                state: RowState::Local { flight },
                time_ms: 0,
                replay: false,
            },
        };
        let intent = row.intent;
        if self.rows.remove_compact(&key).is_some() {
            self.held_changed.insert(key.clone());
        }
        self.rows.insert(key, row);
        intent
    }

    pub fn user_delete(
        &mut self,
        node: &NodeId,
        seen_list: Vec<Seen>,
        seen: Option<IntentId>,
    ) -> IntentId {
        let mut counts: BTreeMap<RowKey, usize> = BTreeMap::new();
        let mut kept: Vec<Seen> = (seen_list.into_iter().rev())
            .filter(|entry| {
                let count = counts.entry((entry.node.clone(), entry.field)).or_default();
                *count += 1;
                *count <= HISTORY_KEPT
            })
            .collect();
        kept.reverse();
        self.seen.insert(node.to_owned(), kept);
        self.user_change(node, Value::Status { live: false }, WireBase::None, seen)
    }

    pub fn user_revived(&mut self, node: &NodeId) {
        if (self.row(node, Field::Status))
            .is_some_and(|row| row.value == Value::Status { live: false })
        {
            self.end(node, Field::Status);
        }
    }

    pub fn insert_if_absent(&mut self, node: &NodeId, row: Row) -> bool {
        let key = (node.to_owned(), row.value.field());
        if self.rows.get(&key).is_some() || self.rows.get_compact(&key).is_some() {
            return false;
        }
        self.rows.insert(key, row);
        true
    }

    pub fn insert_sent_if_absent(&mut self, node: &NodeId, sent: Row) {
        let key = (node.to_owned(), sent.value.field());
        if self.rows.get_compact(&key).is_some() {
            return;
        }
        let Some(row) = self.rows.get(&key) else {
            self.rows.insert(key, sent);
            return;
        };
        if row.state != (RowState::Local { flight: None }) {
            return;
        }
        let state = match row.value.same(&sent.value) {
            true => RowState::Sent,
            false => RowState::Local {
                flight: Some(sent.as_flight()),
            },
        };
        self.rows.update(&key, |row| row.state = state);
    }

    pub fn replace(&mut self, node: &NodeId, row: Row) {
        let key = (node.to_owned(), row.value.field());
        if self.rows.remove_compact(&key).is_some() {
            self.held_changed.insert(key.clone());
        }
        self.rows.insert(key, row);
    }

    pub fn set_seen(&mut self, node: &NodeId, seen: Vec<Seen>) {
        self.seen.insert(node.to_owned(), seen);
    }

    pub fn end_unsent_carried(&mut self, node: &NodeId) {
        for field in Field::ALL {
            let unsent = (self.rows.get(&(node.to_owned(), field))).is_some_and(|row| {
                matches!(row.author, Author::Carried(_))
                    && matches!(row.state, RowState::Local { .. })
            });
            if unsent {
                self.end(node, field);
            }
        }
    }

    pub fn end(&mut self, node: &NodeId, field: Field) -> Option<Row> {
        if field == Field::Status {
            self.seen.remove(node);
        }
        let key = (node.to_owned(), field);
        if self.rows.remove_compact(&key).is_some() {
            self.held_changed.insert(key.clone());
        }
        self.rows.remove(&key)
    }

    pub fn send(&mut self, node: &NodeId, field: Field, head_time_ms: i64) -> Option<&Row> {
        let key = (node.to_owned(), field);
        self.rows.update(&key, |row| {
            if matches!(row.state, RowState::Local { .. }) {
                row.state = RowState::Sent;
                if !row.replay {
                    row.time_ms = head_time_ms;
                }
            }
        })?;
        self.rows.get(&key)
    }

    pub fn send_unrecorded(
        &mut self,
        node: &NodeId,
        value: Value,
        wire_base: WireBase,
        base: Option<IntentId>,
        time_ms: i64,
    ) -> &Row {
        let key = (node.to_owned(), value.field());
        let row = Row {
            intent: self.mint(),
            author: Author::Own,
            base,
            value,
            wire_base,
            state: RowState::Sent,
            time_ms,
            replay: false,
        };
        self.rows.insert(key.clone(), row);
        self.rows.get(&key).expect("the row was just written")
    }

    pub fn unsend(&mut self, node: &NodeId, field: Field) {
        self.rows.update(&(node.to_owned(), field), |row| {
            if row.state == RowState::Sent {
                row.state = RowState::Local { flight: None };
            }
        });
    }

    pub fn applied(
        &mut self,
        node: &NodeId,
        field: Field,
        sent: IntentId,
        held: &FieldIntent,
        version_id: Option<String>,
        in_local: bool,
    ) {
        self.rows
            .update(&(node.to_owned(), field), |row| match &mut row.state {
                RowState::Sent if row.intent == sent => {
                    row.state = RowState::Held {
                        held_as: held.id,
                        version_id,
                    };
                    row.time_ms = held.applied_at_ms;
                }
                RowState::Local { flight } => {
                    flight.take_if(|f| f.intent == sent);
                }
                _ => {}
            });
        self.rebase(sent, held.id);
        if sent != held.id {
            let keys: Vec<NodeId> = (self.seen.holding(sent))
                .difference(self.seen.holding(held.id))
                .cloned()
                .collect();
            for key in keys {
                let mut entries = Vec::new();
                for entry in &self.seen[&key] {
                    entries.push(entry.clone());
                    if entry.intent == sent {
                        entries.push(Seen {
                            intent: held.id,
                            value: None,
                            ..entry.clone()
                        });
                    }
                }
                self.seen.insert(key, entries);
            }
        }
        self.push_history(node, field, held.id, held.applied_at_ms, in_local);
    }

    pub fn superseded(
        &mut self,
        node: &NodeId,
        field: Field,
        sent: IntentId,
        reported: Option<IntentId>,
    ) {
        self.sent_ends(node, field, sent);
        if let Some(reported) = reported {
            self.rebase(sent, reported);
        }
    }

    pub fn sent_ends(&mut self, node: &NodeId, field: Field, sent: IntentId) {
        let sent_row = self.rows.update(&(node.to_owned(), field), |row| {
            if let RowState::Local { flight } = &mut row.state {
                flight.take_if(|f| f.intent == sent);
            }
            row.intent == sent
        });
        if sent_row == Some(true) {
            self.end(node, field);
        }
    }

    pub fn end_held_as(&mut self, node: &NodeId, field: Field, held_as: IntentId) {
        let held = (self.row(node, field)).is_some_and(
            |row| matches!(row.state, RowState::Held { held_as: at, .. } if at == held_as),
        );
        if held {
            self.end(node, field);
        }
    }

    pub fn held_elsewhere(
        &mut self,
        node: &NodeId,
        field: Field,
        sent: IntentId,
        target: &NodeId,
        version_id: Option<String>,
        at_ms: i64,
    ) {
        let key = (node.to_owned(), field);
        let moved = match self.rows.get(&key) {
            Some(row) if row.intent == sent => self.rows.remove(&key),
            Some(_) => (self.rows.update(&key, |row| match &mut row.state {
                RowState::Local { flight } => flight.take_if(|f| f.intent == sent),
                _ => None,
            }))
            .flatten()
            .map(Flight::sent),
            _ => None,
        };
        if let Some(mut row) = moved {
            row.state = RowState::Held {
                held_as: sent,
                version_id,
            };
            row.time_ms = at_ms;
            self.rows.insert((target.to_owned(), field), row);
        }
        self.push_history(target, field, sent, at_ms, true);
    }

    pub fn sent_intent(&self, node: &NodeId, field: Field) -> Option<IntentId> {
        self.row(node, field)?
            .in_flight()
            .map(|flight| flight.intent)
    }

    pub fn merged_into(&mut self, node: &NodeId, target: &NodeId, children_moved: bool) {
        self.end(node, Field::Place);
        self.end(node, Field::Status);
        if let Some(row) = self.rows.remove(&(node.to_owned(), Field::Content)) {
            self.rows.insert((target.to_owned(), Field::Content), row);
        }
        if !children_moved {
            return;
        }
        for key in self.rows.under(node) {
            self.rows.update(&key, |row| {
                if let Value::Place(place) = &mut row.value {
                    place.parent = target.to_owned();
                }
            });
        }
    }

    pub fn rebase(&mut self, from: IntentId, to: IntentId) {
        if from == to {
            return;
        }
        for key in self.rows.compact_over(from) {
            self.rows.update_compact(&key, |held| held.base = Some(to));
            self.held_changed.insert(key);
        }
        for key in self.rows.full_over(from) {
            self.rows.update(&key, |row| {
                if row.base == Some(from) {
                    row.base = Some(to);
                }
                if let RowState::Local {
                    flight: Some(flight),
                } = &mut row.state
                    && flight.base == Some(from)
                {
                    flight.base = Some(to);
                }
            });
        }
    }

    pub fn remote_holds(
        &mut self,
        node: &NodeId,
        field: Field,
        intent: &FieldIntent,
        in_local: bool,
    ) {
        self.push_history(node, field, intent.id, intent.applied_at_ms, in_local);
        let held_as = match (self.row(node, field), self.compact_row(node, field)) {
            (Some(row), _) => match row.state {
                RowState::Held { held_as, .. } => Some(held_as),
                _ => None,
            },
            (None, held) => held.map(|held| held.held_as),
        };
        let ended =
            held_as.is_some_and(|held_as| self.places_after(node, field, intent.id, held_as));
        if ended {
            self.end(node, field);
        }
    }

    pub fn local_holds(&mut self, node: &NodeId, field: Field, intent: IntentId) {
        let key = (node.to_owned(), field);
        let held = (self.history.get(&key)).is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.intent == intent && !entry.in_local)
        });
        if held
            && let Some(entry) = (self.history.get_mut(&key))
                .and_then(|entries| entries.iter_mut().find(|entry| entry.intent == intent))
        {
            entry.in_local = true;
        }
    }

    pub fn places_after(
        &self,
        node: &NodeId,
        field: Field,
        later: IntentId,
        earlier: IntentId,
    ) -> bool {
        let history = self.history(node, field);
        let at = |intent| {
            history
                .iter()
                .position(|entry: &HistoryEntry| entry.intent == intent)
        };
        at(later)
            .zip(at(earlier))
            .is_some_and(|(later, earlier)| later > earlier)
    }

    fn push_history(
        &mut self,
        node: &NodeId,
        field: Field,
        intent: IntentId,
        applied_at_ms: i64,
        in_local: bool,
    ) {
        let key = (node.to_owned(), field);
        let entries = self.history.get(&key).map_or(&[][..], Vec::as_slice);
        if let Some(entry) = entries.iter().find(|entry| entry.intent == intent) {
            let gains =
                (in_local && !entry.in_local) || (entry.applied_at_ms == 0 && applied_at_ms != 0);
            if gains
                && let Some(entry) = (self.history.get_mut(&key))
                    .and_then(|entries| entries.iter_mut().find(|entry| entry.intent == intent))
            {
                entry.in_local |= in_local;
                if entry.applied_at_ms == 0 {
                    entry.applied_at_ms = applied_at_ms;
                }
            }
            return;
        }
        let entries = self.history.get_or_insert_with(key, Vec::new);
        entries.push(HistoryEntry {
            intent,
            applied_at_ms,
            in_local,
        });
        if entries.len() > HISTORY_KEPT {
            entries.remove(0);
        }
    }

    pub(crate) fn remote_held_before(&mut self, node: &NodeId, field: Field, older: &[IntentId]) {
        let key = (node.to_owned(), field);
        let entries = self.history.get_or_insert_with(key, Vec::new);
        let missing: Vec<HistoryEntry> = (older.iter())
            .filter(|intent| !entries.iter().any(|entry| entry.intent == **intent))
            .map(|&intent| HistoryEntry {
                intent,
                applied_at_ms: 0,
                in_local: false,
            })
            .collect();
        entries.splice(0..0, missing);
        let over = entries.len().saturating_sub(HISTORY_KEPT);
        entries.drain(..over);
    }

    pub(crate) fn trim_history(&mut self, node: &NodeId, field: Field, implied: &HistoryEntry) {
        let key = (node.to_owned(), field);
        if self.history.get(&key).and_then(|entries| entries.last()) != Some(implied) {
            return;
        }
        match self.history[&key].len() {
            1 => {
                self.history.remove(&key);
            }
            _ => {
                if let Some(entries) = self.history.get_mut(&key) {
                    entries.pop();
                }
            }
        }
    }

    pub fn forget_history(&mut self, node: &NodeId) {
        for field in Field::ALL {
            self.history.remove(&(node.to_owned(), field));
        }
    }

    pub fn lose(&mut self) {
        let keys: Vec<RowKey> = (self.rows.iter())
            .filter(|(_, row)| !row.replay && !matches!(row.state, RowState::Local { .. }))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.rows.update(&key, |row| row.replay = true);
        }
    }

    pub fn replace_ids(&mut self, ids: &BTreeMap<IntentId, IntentId>) {
        if ids.is_empty() {
            return;
        }
        let swap = |id: &mut IntentId| {
            if let Some(new) = ids.get(id) {
                *id = *new;
            }
        };
        let rows: Vec<(RowKey, Row)> = (self.rows.iter())
            .map(|(key, row)| (key.clone(), row.clone()))
            .collect();
        for (key, before) in rows {
            let mut row = before.clone();
            swap(&mut row.intent);
            if let Some(base) = &mut row.base {
                swap(base);
            }
            match &mut row.state {
                RowState::Held { held_as, .. } => swap(held_as),
                RowState::Local {
                    flight: Some(flight),
                } => {
                    swap(&mut flight.intent);
                    if let Some(base) = &mut flight.base {
                        swap(base);
                    }
                }
                RowState::Local { flight: None } | RowState::Sent => {}
            }
            if row != before {
                self.rows.insert(key, row);
            }
        }
        let keys: Vec<RowKey> = (self.rows.iter_compact()).map(|(key, _)| key).collect();
        for key in keys {
            let changed = self.rows.update_compact(&key, |held| {
                let before = held.clone();
                swap(&mut held.intent);
                swap(&mut held.held_as);
                if let Some(base) = &mut held.base {
                    swap(base);
                }
                *held != before
            });
            if changed == Some(true) {
                self.held_changed.insert(key);
            }
        }
        let keys: Vec<RowKey> = self.history.keys().cloned().collect();
        for key in keys {
            if self.history[&key]
                .iter()
                .any(|entry| ids.contains_key(&entry.intent))
            {
                let mut entries = self.history[&key].clone();
                entries.iter_mut().for_each(|entry| swap(&mut entry.intent));
                let mut kept = BTreeSet::new();
                entries.retain(|entry| kept.insert(entry.intent));
                self.history.insert(key, entries);
            }
        }
        let keys: Vec<NodeId> = self.seen.keys().cloned().collect();
        for key in keys {
            if self.seen[&key]
                .iter()
                .any(|entry| ids.contains_key(&entry.intent))
            {
                let mut entries = self.seen[&key].clone();
                entries.iter_mut().for_each(|entry| swap(&mut entry.intent));
                self.seen.insert(key, entries);
            }
        }
    }
}

mod rows;
mod seen;
#[cfg(test)]
mod tests;
