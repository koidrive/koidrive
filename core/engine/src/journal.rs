// SPDX-License-Identifier: Apache-2.0

//! The journal of RFC 0029 §5.1: each change this device's user made that the server may not hold in its current
//! form, with the intent the user saw, the history of the intents R held, and the seen list of each delete.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use koidrive_proto::v1 as proto;
use sha1::{Digest, Sha1};

use crate::ids::{IdGen, uuid_v7_bits};
use crate::names::same_name;
use crate::state::{ContentValue, MapChanges};
use crate::tracked::TrackedMap;
use crate::tree::NodeId;

/// The most intents the history keeps per node and field, and a seen list per node and field (RFC 0029 §5.1).
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

    /// A lowercase hyphenated UUID.
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
    pub fn legacy(node: &str, field: Field, key: &str) -> Self {
        let mut hash = Sha1::new();
        hash.update(LEGACY_NAMESPACE);
        hash.update(node.as_bytes());
        hash.update([0]);
        hash.update(field.as_str().as_bytes());
        hash.update([0]);
        hash.update(key.as_bytes());
        let digest = hash.finalize();
        let mut id = [0; 16];
        id.copy_from_slice(&digest[..16]);
        id[6] = (id[6] & 0x0f) | 0x50;
        id[8] = (id[8] & 0x3f) | 0x80;
        Self(id)
    }

    pub fn legacy_content(node: &str, version_id: &str) -> Self {
        Self::legacy(node, Field::Content, version_id)
    }

    pub fn legacy_place(node: &str, meta_rev: u64, parent: &str, name: &str) -> Self {
        Self::legacy(node, Field::Place, &format!("{meta_rev}\0{parent}\0{name}"))
    }

    pub fn legacy_live(node: &str, seq: u64) -> Self {
        Self::legacy(node, Field::Status, &format!("live:{seq}"))
    }

    pub fn legacy_trashed(node: &str, trash_group: &str) -> Self {
        Self::legacy(node, Field::Status, &format!("trashed:{trash_group}"))
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

/// The intent that set a field, as a `NodeState` reports it. `carried` is the wire's `replay` on a `NodeState`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldIntent {
    pub id: IntentId,
    pub author: String,
    pub carried: bool,
    pub applied_at_ms: i64,
}

impl FieldIntent {
    pub fn legacy(id: IntentId) -> Self {
        Self {
            id,
            author: String::new(),
            carried: false,
            applied_at_ms: 0,
        }
    }

    pub fn from_proto(intent: &proto::Intent) -> Option<Self> {
        Some(Self {
            id: IntentId::from_bytes(&intent.id)?,
            author: intent.author_device_id.clone(),
            carried: intent.replay,
            applied_at_ms: intent.applied_at_ms,
        })
    }

    pub fn to_proto(&self) -> proto::Intent {
        proto::Intent {
            id: self.id.0.to_vec(),
            author_device_id: self.author.clone(),
            replay: self.carried,
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
        }
    }

    pub fn to_proto(&self) -> proto::FieldIntents {
        proto::FieldIntents {
            place: self.place.as_ref().map(FieldIntent::to_proto),
            content: self.content.as_ref().map(FieldIntent::to_proto),
            status: self.status.as_ref().map(FieldIntent::to_proto),
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

/// `Carried` holds the author's device id, empty when the store could not show who made the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    Own,
    Carried(String),
}

/// An intent the commit in flight sends for a field whose row a later change of the user replaced.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flight {
    pub intent: IntentId,
    pub value: Value,
    pub base: Option<IntentId>,
    pub wire_base: WireBase,
    pub time_ms: i64,
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
            RowState::Sent => Some(Flight {
                intent: self.intent,
                value: self.value.clone(),
                base: self.base,
                wire_base: self.wire_base.clone(),
                time_ms: self.time_ms,
            }),
            RowState::Local { flight } => flight.clone(),
            RowState::Held { .. } => None,
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

/// One field of R that a delete's lists compare against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteField {
    pub node: NodeId,
    pub field: Field,
    pub intent: IntentId,
    pub value: Value,
}

/// `Delete.unseen` and `Delete.held` (RFC 0029 §5.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteView {
    pub unseen: Vec<IntentId>,
    pub held: Vec<IntentId>,
}

/// The view a delete sends: R's intents the seen list neither holds nor matches by value are unseen, and the seen
/// intents R's fields do not hold are held.
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

/// A `held` row while R holds its `held_as` with the row's value and time: R gives the rest of the row.
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
    /// The row with the value, time and version id R gives it.
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
    /// The state of the generator that mints this user's intents, once seeded.
    pub ids: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JournalChanges {
    pub rows: MapChanges<RowKey, Row>,
    pub history: MapChanges<RowKey, Vec<HistoryEntry>>,
    pub seen: MapChanges<NodeId, Vec<Seen>>,
    pub listed: Option<bool>,
    pub ids: Option<Option<u64>>,
}

impl JournalChanges {
    pub fn between(before: &JournalState, after: &JournalState) -> Self {
        Self {
            rows: changes(&before.rows, &after.rows),
            history: changes(&before.history, &after.history),
            seen: changes(&before.seen, &after.seen),
            listed: (before.listed != after.listed).then_some(after.listed),
            ids: (before.ids != after.ids).then_some(after.ids),
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
#[derive(Clone, Debug, Default)]
pub struct Journal {
    rows: TrackedMap<RowKey, Row>,
    held: BTreeMap<RowKey, HeldRow>,
    held_changed: BTreeSet<RowKey>,
    history: TrackedMap<RowKey, Vec<HistoryEntry>>,
    seen: TrackedMap<NodeId, Vec<Seen>>,
    listed: bool,
    listed_before: bool,
    ids: Option<IdGen>,
    ids_before: Option<u64>,
    now_ms: u64,
}

const JOURNAL_STREAM: u64 = 0x6a09_e667_f3bc_c908;

impl PartialEq for Journal {
    fn eq(&self, other: &Self) -> bool {
        *self.rows == *other.rows
            && self.held == other.held
            && *self.history == *other.history
            && *self.seen == *other.seen
            && self.listed == other.listed
            && self.ids.as_ref().map(|ids| ids.state) == other.ids.as_ref().map(|ids| ids.state)
    }
}

impl Journal {
    pub fn restore(state: JournalState) -> Self {
        Self {
            rows: state.rows.into(),
            history: state.history.into(),
            seen: state.seen.into(),
            listed: state.listed,
            listed_before: state.listed,
            ids: state.ids.map(IdGen::new),
            ids_before: state.ids,
            ..Self::default()
        }
    }

    /// What the store keeps, each compact row in full as `full` gives it.
    pub fn state(&self, full: impl Fn(&RowKey, &HeldRow) -> Row) -> JournalState {
        let mut rows = (*self.rows).clone();
        rows.extend((self.held.iter()).map(|(key, held)| (key.clone(), full(key, held))));
        JournalState {
            rows,
            history: (*self.history).clone(),
            seen: (*self.seen).clone(),
            listed: self.listed,
            ids: self.ids.as_ref().map(|ids| ids.state),
        }
    }

    /// What changed since the last take, each compact row in full as `full` gives it.
    pub fn take_changes(&mut self, full: impl Fn(&RowKey, &HeldRow) -> Row) -> JournalChanges {
        let mut keys: BTreeSet<RowKey> = std::mem::take(&mut self.held_changed);
        keys.extend(self.rows.take_changes().into_iter().map(|(key, _)| key));
        let rows = (keys.into_iter())
            .map(|key| {
                let row = (self.rows.get(&key).cloned())
                    .or_else(|| self.held.get(&key).map(|held| full(&key, held)));
                (key, row)
            })
            .collect();
        let listed = (self.listed != self.listed_before).then_some(self.listed);
        self.listed_before = self.listed;
        let state = self.ids.as_ref().map(|ids| ids.state);
        let ids = (state != self.ids_before).then_some(state);
        self.ids_before = state;
        JournalChanges {
            rows,
            history: self.history.take_changes(),
            seen: self.seen.take_changes(),
            listed,
            ids,
        }
    }

    /// Gives a journal that has no generator yet one of its own, derived from the state of another generator
    /// (the engine's) so that the two streams differ.
    pub fn seed(&mut self, from: u64) {
        if self.ids.is_none() {
            let mut ids = IdGen::new(from ^ JOURNAL_STREAM);
            ids.next_u64();
            self.ids = Some(ids);
        }
    }

    pub fn set_clock(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    pub fn mint(&mut self) -> IntentId {
        let now_ms = self.now_ms;
        IntentId::mint(
            self.ids.get_or_insert_with(|| IdGen::new(JOURNAL_STREAM)),
            now_ms,
        )
    }

    pub fn listed(&self) -> bool {
        self.listed
    }

    pub fn set_listed(&mut self, listed: bool) {
        self.listed = listed;
    }

    /// The field's row unless it is compact; Trees gives every row whole.
    pub(crate) fn row(&self, node: &str, field: Field) -> Option<&Row> {
        self.rows.get(&(node.to_owned(), field))
    }

    pub(crate) fn rows(&self) -> impl Iterator<Item = (&RowKey, &Row)> {
        self.rows.iter()
    }

    pub fn compact_row(&self, node: &str, field: Field) -> Option<&HeldRow> {
        self.held.get(&(node.to_owned(), field))
    }

    pub(crate) fn compact_rows(&self) -> impl Iterator<Item = (&RowKey, &HeldRow)> {
        self.held.iter()
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
        self.held.insert(key, held);
        true
    }

    /// Puts a compact row back in full, before R stops giving it.
    pub(crate) fn write_out(&mut self, key: RowKey, row: Row) {
        self.held.remove(&key);
        self.rows.insert_quietly(key, row);
    }

    pub fn history(&self, node: &str, field: Field) -> &[HistoryEntry] {
        (self.history.get(&(node.to_owned(), field))).map_or(&[], Vec::as_slice)
    }

    pub fn seen(&self, node: &str) -> &[Seen] {
        self.seen.get(node).map_or(&[], Vec::as_slice)
    }

    /// Writes the row of a change the user made, which replaces the field's row (RFC 0029 §5.1). `seen` is the
    /// intent the field held for the user when no row covers it. A value the commit in flight sends for the field
    /// takes that intent back.
    pub fn user_change(
        &mut self,
        node: &str,
        value: Value,
        wire_base: WireBase,
        seen: Option<IntentId>,
    ) -> IntentId {
        let key = (node.to_owned(), value.field());
        let old = self.rows.get(&key);
        let flight = old.and_then(Row::in_flight);
        let base = match old {
            None => (self.held.get(&key)).map_or(seen, |held| Some(held.held_as)),
            Some(old) => match &old.state {
                RowState::Local { .. } => old.base,
                RowState::Sent => Some(old.intent),
                RowState::Held { held_as, .. } => Some(*held_as),
            },
        };
        let row = match flight {
            Some(flight) if flight.value.same(&value) => Row {
                intent: flight.intent,
                author: Author::Own,
                base: flight.base,
                value,
                wire_base: flight.wire_base,
                state: RowState::Sent,
                time_ms: flight.time_ms,
                replay: false,
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
        if self.held.remove(&key).is_some() {
            self.held_changed.insert(key.clone());
        }
        self.rows.insert(key, row);
        intent
    }

    /// Writes the status row of a user's delete with its seen list, at most [`HISTORY_KEPT`] entries per node and
    /// field, the newest kept.
    pub fn user_delete(
        &mut self,
        node: &str,
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

    /// Ends the status row of a node the user created or restored again.
    pub fn user_revived(&mut self, node: &str) {
        if (self.row(node, Field::Status))
            .is_some_and(|row| row.value == Value::Status { live: false })
        {
            self.end(node, Field::Status);
        }
    }

    /// Writes a row a store upgrade finds, unless the field has one.
    pub fn insert_if_absent(&mut self, node: &str, row: Row) -> bool {
        let key = (node.to_owned(), row.value.field());
        if self.rows.contains_key(&key) || self.held.contains_key(&key) {
            return false;
        }
        self.rows.insert(key, row);
        true
    }

    pub fn replace(&mut self, node: &str, row: Row) {
        let key = (node.to_owned(), row.value.field());
        if self.held.remove(&key).is_some() {
            self.held_changed.insert(key.clone());
        }
        self.rows.insert(key, row);
    }

    pub fn set_seen(&mut self, node: &str, seen: Vec<Seen>) {
        self.seen.insert(node.to_owned(), seen);
    }

    pub fn end_unsent_carried(&mut self, node: &str) {
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

    pub fn end(&mut self, node: &str, field: Field) -> Option<Row> {
        if field == Field::Status {
            self.seen.remove(node);
        }
        let key = (node.to_owned(), field);
        if self.held.remove(&key).is_some() {
            self.held_changed.insert(key.clone());
        }
        self.rows.remove(&key)
    }

    /// Puts the field's row in the commit being built: a `local` row becomes `sent` at `time_ms`, R's head time.
    pub fn send(&mut self, node: &str, field: Field, time_ms: i64) -> Option<&Row> {
        let row = self.rows.get_mut(&(node.to_owned(), field))?;
        if matches!(row.state, RowState::Local { .. }) {
            row.state = RowState::Sent;
            row.time_ms = time_ms;
        }
        Some(row)
    }

    /// Writes an own `sent` row for a change the commit being built sends that no row recorded.
    pub fn send_unrecorded(
        &mut self,
        node: &str,
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
        &self.rows[&key]
    }

    /// Takes a `sent` row back to `local`, so the next commit sends it again.
    pub fn unsend(&mut self, node: &str, field: Field) {
        if let Some(row) = self.rows.get_mut(&(node.to_owned(), field))
            && row.state == RowState::Sent
        {
            row.state = RowState::Local { flight: None };
        }
    }

    /// A result applied the op that sent `sent` for the field, and the server holds `held` (RFC 0029 §5.2).
    pub fn applied(
        &mut self,
        node: &str,
        field: Field,
        sent: IntentId,
        held: &FieldIntent,
        version_id: Option<String>,
    ) {
        if let Some(row) = self.rows.get_mut(&(node.to_owned(), field)) {
            match &mut row.state {
                RowState::Sent if row.intent == sent => {
                    row.state = RowState::Held {
                        held_as: held.id,
                        version_id,
                    };
                    row.time_ms = held.applied_at_ms;
                }
                RowState::Local { flight } if flight.as_ref().is_some_and(|f| f.intent == sent) => {
                    *flight = None;
                }
                _ => {}
            }
        }
        self.rebase(sent, held.id);
        self.push_history(node, field, held.id, held.applied_at_ms, true);
    }

    /// A result answered the op that sent `sent` `SUPERSEDED`: the row of that intent ends, and rows made over it
    /// take the intent the field reported.
    pub fn superseded(
        &mut self,
        node: &str,
        field: Field,
        sent: IntentId,
        reported: Option<IntentId>,
    ) {
        match self.rows.get_mut(&(node.to_owned(), field)) {
            Some(row) if row.intent == sent => {
                self.end(node, field);
            }
            Some(Row {
                state: RowState::Local { flight },
                ..
            }) if flight.as_ref().is_some_and(|f| f.intent == sent) => *flight = None,
            _ => {}
        }
        if let Some(reported) = reported {
            self.rebase(sent, reported);
        }
    }

    /// A result showed `target` holding the intent `sent` the commit sent for the field of `node`, as the copy of a
    /// `CONFLICT_COPY` does: the row moves to `target`, which holds it.
    pub fn held_elsewhere(
        &mut self,
        node: &str,
        field: Field,
        sent: IntentId,
        target: &str,
        version_id: Option<String>,
        at_ms: i64,
    ) {
        let key = (node.to_owned(), field);
        let moved = match self.rows.get_mut(&key) {
            Some(row) if row.intent == sent => self.rows.remove(&key),
            Some(Row {
                state: RowState::Local { flight },
                ..
            }) => flight.take_if(|f| f.intent == sent).map(|flight| Row {
                intent: flight.intent,
                author: Author::Own,
                base: flight.base,
                value: flight.value,
                wire_base: flight.wire_base,
                state: RowState::Sent,
                time_ms: flight.time_ms,
                replay: false,
            }),
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

    /// The intent the commit in flight sent for the field.
    pub fn sent_intent(&self, node: &str, field: Field) -> Option<IntentId> {
        self.row(node, field)?
            .in_flight()
            .map(|flight| flight.intent)
    }

    /// A result merged `node` into `target`: its place and status rows end, and its content row and the place rows
    /// of its children move to the target.
    pub fn merged_into(&mut self, node: &str, target: &str) {
        self.end(node, Field::Place);
        self.end(node, Field::Status);
        if let Some(row) = self.rows.remove(&(node.to_owned(), Field::Content)) {
            self.rows.insert((target.to_owned(), Field::Content), row);
        }
        let children: Vec<RowKey> = (self.rows.iter())
            .filter(|(_, row)| matches!(&row.value, Value::Place(place) if place.parent == node))
            .map(|(key, _)| key.clone())
            .collect();
        for key in children {
            if let Some(Row {
                value: Value::Place(place),
                ..
            }) = self.rows.get_mut(&key)
            {
                place.parent = target.to_owned();
            }
        }
    }

    /// Every row whose base is `from` takes `to` as its base.
    pub fn rebase(&mut self, from: IntentId, to: IntentId) {
        if from == to {
            return;
        }
        let keys: Vec<RowKey> = (self.rows.iter())
            .filter(|(_, row)| {
                row.base == Some(from)
                    || matches!(&row.state, RowState::Local { flight: Some(f) } if f.base == Some(from))
            })
            .map(|(key, _)| key.clone())
            .collect();
        let held: Vec<RowKey> = (self.held.iter())
            .filter(|(_, held)| held.base == Some(from))
            .map(|(key, _)| key.clone())
            .collect();
        for key in held {
            if let Some(held) = self.held.get_mut(&key) {
                held.base = Some(to);
            }
            self.held_changed.insert(key);
        }
        for key in keys {
            let Some(row) = self.rows.get_mut(&key) else {
                continue;
            };
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
        }
    }

    /// R holds `intent` for the field: the history takes it, and a `held` row whose `held_as` the history places
    /// before it ends.
    pub fn remote_holds(&mut self, node: &str, field: Field, intent: &FieldIntent, in_local: bool) {
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

    /// L took the value of `intent`, which R holds.
    pub fn local_holds(&mut self, node: &str, field: Field, intent: IntentId) {
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

    /// Whether the history holds both intents and placed `later` after `earlier`.
    pub fn places_after(
        &self,
        node: &str,
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
        node: &str,
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

    /// Drops the newest entry of the field's history when it is `implied`, which R's intent gives.
    pub(crate) fn trim_history(&mut self, node: &str, field: Field, implied: &HistoryEntry) {
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

    pub fn forget_history(&mut self, node: &str) {
        for field in Field::ALL {
            self.history.remove(&(node.to_owned(), field));
        }
    }

    /// The server lost a commit of this device: every row a commit sent is a replay from now on (RFC 0029 §5.4).
    pub fn lose(&mut self) {
        let keys: Vec<RowKey> = (self.rows.iter())
            .filter(|(_, row)| !row.replay && !matches!(row.state, RowState::Local { .. }))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Some(row) = self.rows.get_mut(&key) {
                row.replay = true;
            }
        }
    }

    /// Replaces each id the map holds wherever the journal names it: rows' intents, `held_as` and bases, the
    /// history and the seen lists.
    pub fn replace_ids(&mut self, ids: &BTreeMap<IntentId, IntentId>) {
        if ids.is_empty() {
            return;
        }
        let swap = |id: &mut IntentId| {
            if let Some(new) = ids.get(id) {
                *id = *new;
            }
        };
        let keys: Vec<RowKey> = self.rows.keys().cloned().collect();
        for key in keys {
            let mut row = self.rows[&key].clone();
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
            if row != self.rows[&key] {
                self.rows.insert(key, row);
            }
        }
        for (key, held) in self.held.iter_mut() {
            let before = held.clone();
            swap(&mut held.intent);
            swap(&mut held.held_as);
            if let Some(base) = &mut held.base {
                swap(base);
            }
            if *held != before {
                self.held_changed.insert(key.clone());
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

#[cfg(test)]
mod tests;
