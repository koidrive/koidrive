// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use koidrive_engine::ids::IdGen;
use koidrive_engine::journal;
use koidrive_proto::v1::{FieldIntents, Intent, Op, op};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct IntentId([u8; 16]);

impl IntentId {
    pub(crate) fn from_wire(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    pub(crate) fn mint(ids: &mut IdGen, now_ms: i64) -> Self {
        let uuid = ids.uuid_v7(now_ms as u64).replace('-', "");
        let mut id = [0; 16];
        for (i, byte) in id.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&uuid[2 * i..2 * i + 2], 16).expect("a UUID is hex");
        }
        Self(id)
    }

    pub(crate) fn wire(self) -> Vec<u8> {
        self.0.to_vec()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Place,
    Content,
    Status,
}

pub(crate) const FIELDS: [Field; 3] = [Field::Place, Field::Content, Field::Status];

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::Place => "place",
            Field::Content => "content",
            Field::Status => "status",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FieldIntent {
    pub id: IntentId,
    pub author: String,
    pub carried: bool,
    pub applied_at_ms: i64,
    pub seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NodeIntents([Option<FieldIntent>; 3]);

impl NodeIntents {
    pub(crate) fn get(&self, field: Field) -> Option<&FieldIntent> {
        self.0[field as usize].as_ref()
    }

    pub(crate) fn set(&mut self, field: Field, intent: FieldIntent) {
        self.0[field as usize] = Some(intent);
    }

    pub(crate) fn id(&self, field: Field) -> Option<IntentId> {
        self.get(field).map(|intent| intent.id)
    }

    pub(crate) fn holds(&self, id: IntentId) -> bool {
        FIELDS.iter().any(|&field| self.id(field) == Some(id))
    }

    pub(crate) fn wire(&self, replaced_status: Option<IntentId>) -> Option<FieldIntents> {
        if self.0.iter().all(Option::is_none) {
            return None;
        }
        let wire = |field: Field| {
            self.get(field).map(|intent| Intent {
                id: intent.id.wire(),
                author_device_id: intent.author.clone(),
                replay: intent.carried,
                over: match (field, replaced_status) {
                    (Field::Status, Some(over)) => over.wire(),
                    _ => Vec::new(),
                },
                applied_at_ms: intent.applied_at_ms,
            })
        };
        Some(FieldIntents {
            place: wire(Field::Place),
            content: wire(Field::Content),
            status: wire(Field::Status),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Tombstone {
    pub parent_id: Option<String>,
    pub name: String,
    pub intents: NodeIntents,
    pub held: BTreeSet<IntentId>,
}

impl Tombstone {
    pub(crate) fn knows(&self, id: IntentId) -> bool {
        self.intents.holds(id) || self.held.contains(&id)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Incoming {
    pub intent: FieldIntent,
    pub replay: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OpIntents {
    pub client: bool,
    fields: [Incoming; 3],
    pub over: Option<IntentId>,
}

impl OpIntents {
    pub(crate) fn field(&self, field: Field) -> &Incoming {
        &self.fields[field as usize]
    }
}

pub(crate) struct Sender<'a> {
    pub device_id: &'a str,
    pub seq: u64,
    pub now_ms: i64,
}

fn minted_carried(op: &Op) -> bool {
    match &op.op {
        Some(op::Op::UpdateContent(o)) => o.resend || !o.source_version_id.is_empty(),
        Some(op::Op::CreateFile(o)) => !o.source_version_id.is_empty(),
        Some(op::Op::Move(o)) => o.from.is_some(),
        _ => false,
    }
}

pub(crate) fn resolve(op: &Op, sender: &Sender, ids: &mut IdGen) -> OpIntents {
    let carried = |author: &str, replay: bool| author != sender.device_id || replay;
    let Some(main) = &op.intent else {
        let author = if minted_carried(op) {
            ""
        } else {
            sender.device_id
        };
        let minted = Incoming {
            intent: FieldIntent {
                id: IntentId::mint(ids, sender.now_ms),
                author: author.to_owned(),
                carried: carried(author, false),
                applied_at_ms: sender.now_ms,
                seq: sender.seq,
            },
            replay: false,
        };
        return OpIntents {
            client: false,
            fields: [minted.clone(), minted.clone(), minted],
            over: None,
        };
    };
    let incoming = |intent: &Intent| Incoming {
        intent: FieldIntent {
            id: IntentId::from_wire(&intent.id).expect("checked before applying"),
            author: intent.author_device_id.clone(),
            carried: carried(&intent.author_device_id, intent.replay),
            applied_at_ms: if intent.replay {
                intent.applied_at_ms.clamp(0, sender.now_ms.max(0))
            } else {
                sender.now_ms
            },
            seq: sender.seq,
        },
        replay: intent.replay,
    };
    let field = |given: Option<&Intent>| match given.filter(|intent| !intent.id.is_empty()) {
        Some(intent) => incoming(intent),
        None => incoming(main),
    };
    let given = op.fields.as_ref();
    OpIntents {
        client: true,
        fields: [
            field(given.and_then(|f| f.place.as_ref())),
            field(given.and_then(|f| f.content.as_ref())),
            field(given.and_then(|f| f.status.as_ref())),
        ],
        over: IntentId::from_wire(&main.over),
    }
}

fn valid_intent(intent: &Intent) -> bool {
    intent.id.len() == 16 && (intent.over.is_empty() || intent.over.len() == 16)
}

pub(crate) fn intents_valid(op: &Op) -> bool {
    if op
        .intent
        .as_ref()
        .is_some_and(|intent| !valid_intent(intent))
    {
        return false;
    }
    if let Some(fields) = &op.fields {
        if !matches!(op.op, Some(op::Op::CreateDir(_) | op::Op::CreateFile(_)))
            || op.intent.is_none()
        {
            return false;
        }
        if ![&fields.place, &fields.content, &fields.status]
            .into_iter()
            .flatten()
            .all(|intent| intent.id.is_empty() || valid_intent(intent))
        {
            return false;
        }
    }
    match &op.op {
        Some(op::Op::Delete(delete)) => delete
            .unseen
            .iter()
            .chain(&delete.held)
            .all(|id| id.len() == 16),
        _ => true,
    }
}

/// The legacy intent id of RFC 0029 section 7.2, as the engine computes it. `field` is `place`,
/// `content` or `status`.
pub fn legacy_intent_id(node_id: &str, field: &str, key: &str) -> [u8; 16] {
    let field = journal::Field::parse(field).expect("a field of RFC 0029");
    *journal::IntentId::legacy(node_id, field, &[key]).as_bytes()
}

pub(crate) fn legacy_intents(
    node_id: &str,
    meta_rev: u64,
    parent_id: Option<&str>,
    name: &str,
    live_since: u64,
) -> NodeIntents {
    let legacy = |field: Field, key: String, seq: u64| FieldIntent {
        id: IntentId(legacy_intent_id(node_id, field.name(), &key)),
        author: String::new(),
        carried: false,
        applied_at_ms: 0,
        seq,
    };
    let mut intents = NodeIntents::default();
    intents.set(
        Field::Place,
        legacy(
            Field::Place,
            format!("{meta_rev}\0{}\0{name}", parent_id.unwrap_or_default()),
            meta_rev,
        ),
    );
    intents.set(
        Field::Status,
        legacy(Field::Status, format!("live:{live_since}"), live_since),
    );
    intents
}
