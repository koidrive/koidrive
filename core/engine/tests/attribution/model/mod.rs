// SPDX-License-Identifier: Apache-2.0

use std::hash::Hash;

use super::authors::{Author, Field, Unsent};

mod device;
mod invariants;
mod rules;
mod search;
mod server;
mod steps;

use rules::Fix;
pub use rules::Rules;
pub use search::{check, check_deep, settle, show};

type Seq = u8;
type Dev = usize;

const FIELDS: [Field; 2] = [Field::Position, Field::Content];
const UNKNOWN: Tok = Tok(u8::MAX);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Tok(u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Value {
    field: Field,
    author: Option<Dev>,
    pos: Tok,
    content: Tok,
    seen: [Tok; 2],
    held: [u32; 2],
    renamed_from: Option<Tok>,
    lost_position: Option<Tok>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Node {
    created: bool,
    created_rev: Seq,
    trashed: Option<Tok>,
    pos: Tok,
    content: Tok,
    meta_rev: Seq,
    meta_edit_rev: Seq,
    meta_editor: Option<Dev>,
    content_rev: Seq,
    edit_rev: Seq,
    editor: Option<Dev>,
    pos_rev: Seq,
    trash_base: Seq,
}

impl Node {
    fn get(&self, field: Field) -> Tok {
        match field {
            Field::Position => self.pos,
            _ => self.content,
        }
    }

    fn rev(&self, field: Field) -> Seq {
        match field {
            Field::Position => self.meta_rev,
            _ => self.content_rev,
        }
    }

    fn user_set(&self, field: Field) -> bool {
        match field {
            Field::Position => self.meta_edit_rev >= self.meta_rev,
            _ => self.edit_rev >= self.content_rev,
        }
    }

    fn by(&self, field: Field) -> Option<Dev> {
        match field {
            Field::Position => self.meta_editor,
            _ => self.editor,
        }
    }

    fn edit(&self, field: Field) -> Seq {
        match field {
            Field::Position => self.meta_edit_rev,
            _ => self.edit_rev,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Record {
    seq: Seq,
    token: u8,
    node: Node,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Server {
    log: Vec<Record>,
    copies: Vec<(Tok, Seq)>,
    tokens: u8,
    overwritten: Vec<Tok>,
    restored: Vec<Tok>,
    overruled: Vec<Tok>,
    authors: Vec<Option<Dev>>,
    before_loss: Option<Backup>,
}

type Backup = (Vec<Record>, Vec<(Tok, Seq)>);

impl Server {
    fn head(&self) -> Seq {
        self.log.len() as Seq
    }

    fn node(&self) -> Node {
        self.log
            .last()
            .expect("the log starts with the create")
            .node
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Local {
    Live { pos: Tok, content: Tok },
    Gone(Tok),
}

impl Local {
    fn get(&self, field: Field) -> Option<Tok> {
        match (self, field) {
            (Local::Live { pos, .. }, Field::Position) => Some(*pos),
            (Local::Live { content, .. }, Field::Content) => Some(*content),
            _ => None,
        }
    }

    fn set(&mut self, field: Field, value: Tok) {
        match (self, field) {
            (Local::Live { pos, .. }, Field::Position) => *pos = value,
            (Local::Live { content, .. }, Field::Content) => *content = value,
            _ => unreachable!("only a live node has fields"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Agreed {
    val: Tok,
    seq: Seq,
    own: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Synced {
    live: bool,
    exist_seq: Seq,
    deleted_here: bool,
    pos: Agreed,
    content: Agreed,
}

impl Synced {
    fn get(&self, field: Field) -> Agreed {
        match field {
            Field::Position => self.pos,
            _ => self.content,
        }
    }

    fn set(&mut self, field: Field, agreed: Agreed) {
        match field {
            Field::Position => self.pos = agreed,
            _ => self.content = agreed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Op {
    Delete {
        base: Seq,
        gone: Tok,
    },
    Move {
        to: Tok,
        from: Option<Tok>,
    },
    Update {
        content: Tok,
        resend: bool,
        base: Seq,
    },
    Copy {
        content: Tok,
    },
    Create {
        pos: Tok,
        content: Tok,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Code {
    Applied,
    Renamed(Tok),
    Superseded,
    Copied,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Answer {
    Ahead,
    HeadMoved,
    Done {
        seq: Seq,
        token: u8,
        codes: Vec<Code>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Phase {
    Sent,
    Answered(Answer),
    Arrived(Answer),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Flight {
    base: (Seq, u8),
    require_head: bool,
    ops: Vec<(Op, Author)>,
    by_user: Vec<bool>,
    phase: Phase,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Device {
    l: Local,
    s: Synced,
    r: Node,
    seen: Vec<u8>,
    history: [Vec<(Tok, Seq)>; 2],
    earlier: Vec<u8>,
    held: [u32; 2],
    unsent: Unsent,
    by_user: [bool; 2],
    flight: Option<Flight>,
    copies: Vec<(Tok, Option<Seq>)>,
    lost_position: Option<Tok>,
}

fn old_wire(d: Dev, rules: Rules) -> bool {
    rules.old_server() || (rules == Rules::OldDeleter && d == 1)
}

fn slot(field: Field) -> usize {
    match field {
        Field::Position => 0,
        _ => 1,
    }
}

impl Device {
    fn take(&mut self, field: Field, value: Tok) {
        self.l.set(field, value);
        self.by_user[slot(field)] = false;
    }

    fn create_at(&self) -> Option<Tok> {
        let pos = self.l.get(Field::Position)?;
        Some(match self.unsent.position {
            Some(Author::Own) => pos,
            _ => self.lost_position.unwrap_or(pos),
        })
    }

    fn take_refused_create(&mut self, sent: Tok, rules: Rules, own: impl Fn(Tok) -> bool) {
        let r = self.r;
        let lost = self.lost_position.take().is_some();
        let Local::Live { pos, content } = self.l else {
            return;
        };
        let takes = lost
            && rules.has(Fix::RefusedKeeps)
            && (r.trashed.is_some() || (rules.has(Fix::RefusedTakes) && content == r.content));
        if r.trashed.is_none() && !takes {
            return;
        }
        let sent = if takes { pos } else { sent };
        self.unsent.existence = None;
        self.s = Synced {
            live: true,
            exist_seq: r.meta_rev,
            deleted_here: false,
            pos: Agreed {
                val: sent,
                seq: r.meta_rev,
                own: own(sent),
            },
            content: Agreed {
                val: r.content,
                seq: r.content_rev,
                own: own(r.content),
            },
        };
        for (field, local, synced) in [
            (Field::Position, pos, sent),
            (Field::Content, content, r.content),
        ] {
            if local != synced && self.unsent.get(field).is_none() {
                self.unsent.set(field, Some(Author::Own));
            }
        }
    }

    fn head(&self) -> (Seq, u8) {
        (
            self.seen.len() as Seq,
            *self.seen.last().expect("every device saw the create"),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Budget {
    pub writes: [u8; 2],
    pub moves: [u8; 2],
    pub deletes: [u8; 2],
    pub trash_restores: [u8; 2],
    pub renames: u8,
    pub losses: u8,
    pub created_by: Option<Dev>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Write(Dev),
    Move(Dev),
    Delete(Dev),
    TrashRestore(Dev),
    Build(Dev, u8),
    Serve(Dev),
    ServeRenamed(Dev),
    Answer(Dev),
    Record(Dev),
    Lose(Seq),
    Unlose,
    Bootstrap(Dev),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct World {
    values: Vec<Value>,
    server: Server,
    devices: [Device; 2],
    budget: Budget,
}

type Broken = String;
