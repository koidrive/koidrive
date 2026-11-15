// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use koidrive_engine::authors::{Author, Field, Unsent};

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
            && rules.refused_keeps_lost_position()
            && (r.trashed.is_some()
                || (rules.refused_live_takes_lost_position() && content == r.content));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rules {
    Spec,
    MoveResultEndsContent,
    MoveResultMarksContentResent,
    LandedResendIsOwn,
    ResendRecordEndsOwn,
    NodeAuthor,
    TrashedRecordProves,
    ClaimAtSeq,
    LossForgetsDeletedFields,
    SupersededDeleteGivesUp,
    DeleteSeesSynced,
    ResendRevivesSeenValue,
    DeleteDropsResends,
    StaleTrashProves,
    DeleteNeedsSyncedLive,
    SeqDelete,
    SupersededDropsOwn,
    TrashSettlesDelete,
    DeleteSeesOnlyLocal,
    LossKeepsStaleSeq,
    OldServer,
    OldDeleter,
    DeletedAdoptsRecords,
    RevivalMarksPosition,
    BoundOnly,
    ListedRemovalsInRecovery,
    RemovalTakesListedShape,
    HighestHeld,
    DeleteSeesOnlyView,
    CopiesOwnBase,
    BoundOnlyResendsSeen,
    TrashCoversView,
    LostCreateResends,
    NoLostPosition,
    AbsentDeleteResent,
    ResentCreateFrom,
    RefusedKeepsLostPosition,
    RefusedTakesLostPosition,
    CreateFixes,
    AllFixes,
    HeldDeleteResent,
    HeldFixes,
}

impl Rules {
    fn old_server(self) -> bool {
        matches!(self, Rules::OldServer | Rules::BoundOnlyResendsSeen)
    }

    fn create_from(self) -> bool {
        matches!(
            self,
            Rules::ResentCreateFrom | Rules::CreateFixes | Rules::AllFixes | Rules::HeldFixes
        )
    }

    fn refused_keeps_lost_position(self) -> bool {
        matches!(
            self,
            Rules::RefusedKeepsLostPosition
                | Rules::RefusedTakesLostPosition
                | Rules::CreateFixes
                | Rules::AllFixes
                | Rules::HeldFixes
        )
    }

    fn refused_live_takes_lost_position(self) -> bool {
        matches!(
            self,
            Rules::RefusedTakesLostPosition
                | Rules::CreateFixes
                | Rules::AllFixes
                | Rules::HeldFixes
        )
    }

    fn absent_delete_resent(self) -> bool {
        matches!(
            self,
            Rules::AbsentDeleteResent
                | Rules::AllFixes
                | Rules::HeldDeleteResent
                | Rules::HeldFixes
        )
    }

    fn delete_needs_held(self) -> bool {
        matches!(self, Rules::HeldDeleteResent | Rules::HeldFixes)
    }

    fn bound_only(self) -> bool {
        matches!(
            self,
            Rules::OldServer | Rules::BoundOnly | Rules::BoundOnlyResendsSeen
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct World {
    values: Vec<Value>,
    server: Server,
    devices: [Device; 2],
    budget: Budget,
}

type Broken = String;

impl World {
    fn new(budget: Budget) -> Self {
        let creator = budget.created_by;
        let at: Seq = if creator.is_some() { 2 } else { 1 };
        let node = Node {
            created: true,
            created_rev: at,
            trashed: None,
            pos: Tok(0),
            content: Tok(1),
            meta_rev: at,
            meta_edit_rev: at,
            meta_editor: creator,
            content_rev: at,
            edit_rev: at,
            editor: creator,
            pos_rev: at,
            trash_base: 0,
        };
        let absent = Node {
            created: false,
            created_rev: 0,
            pos: UNKNOWN,
            content: UNKNOWN,
            meta_rev: 0,
            meta_edit_rev: 0,
            meta_editor: None,
            content_rev: 0,
            edit_rev: 0,
            editor: None,
            pos_rev: 0,
            ..node
        };
        let initial = |field| Value {
            field,
            author: creator,
            pos: Tok(0),
            content: Tok(1),
            seen: [Tok(0), Tok(1)],
            held: [0; 2],
            renamed_from: None,
        };
        let device = |d: Dev| {
            let agreed = |val| Agreed {
                val,
                seq: at,
                own: creator == Some(d),
            };
            Device {
                l: Local::Live {
                    pos: Tok(0),
                    content: Tok(1),
                },
                s: Synced {
                    live: true,
                    exist_seq: at,
                    deleted_here: false,
                    pos: agreed(Tok(0)),
                    content: agreed(Tok(1)),
                },
                r: node,
                seen: (0..at).collect(),
                history: [Vec::new(), Vec::new()],
                earlier: Vec::new(),
                held: [1 << 0, 1 << 1],
                unsent: Unsent::default(),
                by_user: [creator == Some(d); 2],
                flight: None,
                copies: Vec::new(),
                lost_position: None,
            }
        };
        let records = [absent, node].into_iter().skip(2 - at as usize);
        World {
            values: vec![initial(Field::Position), initial(Field::Content)],
            server: Server {
                log: (records.enumerate())
                    .map(|(token, node)| Record {
                        seq: token as Seq + 1,
                        token: token as u8,
                        node,
                    })
                    .collect(),
                copies: Vec::new(),
                tokens: at,
                overwritten: Vec::new(),
                restored: Vec::new(),
                overruled: Vec::new(),
                authors: [None, creator].into_iter().take(at as usize).collect(),
                before_loss: None,
            },
            devices: [device(0), device(1)],
            budget,
        }
    }

    fn caught_up(&self, d: Dev) -> bool {
        let dev = &self.devices[d];
        dev.seen.len() == self.server.log.len()
            && dev.head().1 == self.server.log.last().unwrap().token
    }

    fn lost(&self, d: Dev) -> bool {
        let dev = &self.devices[d];
        let kept = |seq: Seq, token: u8| {
            (self.server.log.get(seq as usize - 1)).is_some_and(|record| record.token == token)
        };
        let result = match dev.flight.as_ref().map(|flight| &flight.phase) {
            Some(Phase::Arrived(Answer::Done { seq, token, .. })) => Some((*seq, *token)),
            _ => None,
        };
        !kept(dev.head().0, dev.head().1) || result.is_some_and(|(seq, token)| !kept(seq, token))
    }

    fn saw(&self, gone: Tok, rules: Rules) -> Option<[Tok; 2]> {
        let value = self.values[gone.0 as usize];
        let old = rules.old_server() || value.author.is_some_and(|d| old_wire(d, rules));
        (!old).then_some(value.seen)
    }

    fn edited_here(&self, d: Dev, node: &Node, seen: &[u8], rules: Rules) -> [bool; 2] {
        FIELDS.map(|field| {
            if !old_wire(d, rules) {
                return node.by(field) == Some(d);
            }
            let seq = node.edit(field) as usize;
            seq > 0
                && seen
                    .get(seq - 1)
                    .is_some_and(|&token| self.server.authors[token as usize] == Some(d))
        })
    }

    fn author(&self, value: Tok) -> Option<Dev> {
        self.values[value.0 as usize].author
    }

    fn mint(&mut self, value: Value) -> Tok {
        assert!(self.values.len() < 32, "a held set is a u32 of tokens");
        self.values.push(value);
        Tok(self.values.len() as u8 - 1)
    }

    pub fn actions(&self) -> Vec<Action> {
        let mut out = Vec::new();
        let budget = self.budget;
        for d in 0..2 {
            let dev = &self.devices[d];
            if let Local::Live { .. } = dev.l {
                if budget.writes[d] > 0 {
                    out.push(Action::Write(d));
                }
                if budget.moves[d] > 0 {
                    out.push(Action::Move(d));
                }
                if budget.deletes[d] > 0 {
                    out.push(Action::Delete(d));
                }
            }
            if budget.trash_restores[d] > 0 && self.server.node().trashed.is_some() {
                out.push(Action::TrashRestore(d));
            }
            if dev.flight.is_none() && self.caught_up(d) {
                for k in 1..=self.plan(d).len() {
                    out.push(Action::Build(d, k as u8));
                }
            }
            match dev.flight.as_ref().map(|flight| &flight.phase) {
                Some(Phase::Sent) => {
                    out.push(Action::Serve(d));
                    let moves = dev.flight.as_ref().unwrap().ops.iter();
                    if budget.renames > 0
                        && moves
                            .into_iter()
                            .any(|(op, _)| matches!(op, Op::Move { .. }))
                    {
                        out.push(Action::ServeRenamed(d));
                    }
                }
                Some(Phase::Answered(_)) => out.push(Action::Answer(d)),
                _ => {}
            }
            if dev.seen.len() < self.server.log.len()
                && self.server.log[dev.seen.len() - 1].token == dev.head().1
            {
                out.push(Action::Record(d));
            }
            let waiting = matches!(
                dev.flight.as_ref().map(|flight| &flight.phase),
                Some(Phase::Sent | Phase::Answered(_))
            );
            if self.lost(d) && !waiting {
                out.push(Action::Bootstrap(d));
            }
        }
        if budget.losses > 0 {
            for held in 1..self.server.head() {
                out.push(Action::Lose(held));
            }
            if self.server.before_loss.is_some() {
                out.push(Action::Unlose);
            }
        }
        out
    }

    fn plan(&self, d: Dev) -> Vec<(Op, Author)> {
        let dev = &self.devices[d];
        let mut ops = Vec::new();
        let absent = !dev.r.created;
        let refused = dev.r.trashed.is_some() && !dev.s.live;
        let creating = absent || refused;
        if let (true, Local::Live { content, .. }, Some(author), Some(at)) =
            (creating, dev.l, dev.unsent.existence, dev.create_at())
        {
            let author = if dev.unsent.holds_own() {
                Author::Own
            } else {
                author
            };
            ops.push((Op::Create { pos: at, content }, author));
        }
        match dev.l {
            _ if absent || (refused && !ops.is_empty()) => {}
            Local::Gone(gone) => {
                if dev.unsent.existence == Some(Author::Own) {
                    if let Some(author) = dev.unsent.position {
                        ops.push((
                            Op::Move {
                                to: dev.s.pos.val,
                                from: Some(dev.r.pos),
                            },
                            author,
                        ));
                    }
                    if let Some(author) = dev.unsent.content {
                        ops.push((
                            Op::Update {
                                content: dev.s.content.val,
                                resend: true,
                                base: dev.head().0,
                            },
                            author,
                        ));
                    }
                    ops.push((
                        Op::Delete {
                            base: dev.head().0,
                            gone,
                        },
                        Author::Own,
                    ));
                }
            }
            Local::Live { pos, content } => {
                if let Some(author) = dev.unsent.position {
                    let from = dev.unsent.resends(Field::Position).then_some(dev.r.pos);
                    ops.push((Op::Move { to: pos, from }, author));
                }
                if let Some(author) = dev.unsent.content {
                    let resend = dev.unsent.resends(Field::Content);
                    ops.push((
                        Op::Update {
                            content,
                            resend,
                            base: dev.head().0,
                        },
                        author,
                    ));
                }
            }
        }
        for (content, landed) in &dev.copies {
            if landed.is_none() {
                ops.push((Op::Copy { content: *content }, Author::Own));
            }
        }
        ops
    }

    pub fn step(&self, action: Action, rules: Rules) -> Result<World, Broken> {
        let mut w = self.clone();
        match action {
            Action::Write(d) | Action::Move(d) => {
                let field = if let Action::Write(_) = action {
                    Field::Content
                } else {
                    Field::Position
                };
                let Local::Live { pos, content } = w.devices[d].l else {
                    unreachable!()
                };
                if field == Field::Content {
                    w.budget.writes[d] -= 1;
                } else {
                    w.budget.moves[d] -= 1;
                }
                let seen = [pos, content];
                let value = w.mint(Value {
                    field,
                    author: Some(d),
                    pos,
                    content,
                    seen,
                    held: [0; 2],
                    renamed_from: None,
                });
                let dev = &mut w.devices[d];
                dev.l.set(field, value);
                dev.by_user[slot(field)] = true;
                dev.unsent.set(field, Some(Author::Own));
                if field == Field::Position {
                    dev.lost_position = None;
                }
                if rules == Rules::NodeAuthor {
                    for other in FIELDS {
                        if dev.unsent.get(other).is_some() {
                            dev.unsent.set(other, Some(Author::Own));
                        }
                    }
                }
            }
            Action::Delete(d) => {
                w.budget.deletes[d] -= 1;
                let Local::Live { pos, content } = w.devices[d].l else {
                    unreachable!()
                };
                let seen = [pos, content];
                let dev = &w.devices[d];
                for field in FIELDS {
                    let remote = dev.r.get(field);
                    let overrules = dev.r.created
                        && dev.unsent.get(field) == Some(Author::Own)
                        && dev.l.get(field) != Some(remote);
                    if overrules
                        && w.author(remote).is_some()
                        && rules != Rules::DeleteSeesOnlyLocal
                    {
                        w.server.overruled.push(remote);
                    }
                }
                let held = w.devices[d].held;
                let gone = w.mint(Value {
                    field: Field::Existence,
                    author: Some(d),
                    pos,
                    content,
                    seen,
                    held,
                    renamed_from: None,
                });
                let dev = &mut w.devices[d];
                if rules != Rules::DeleteSeesSynced {
                    for (field, local) in [(Field::Position, pos), (Field::Content, content)] {
                        let synced = dev.s.get(field);
                        dev.s.set(
                            field,
                            Agreed {
                                val: local,
                                own: dev.unsent.get(field) == Some(Author::Own)
                                    || (synced.own && synced.val == local),
                                ..synced
                            },
                        );
                    }
                }
                dev.l = Local::Gone(gone);
                dev.lost_position = None;
                let resent = |author: Option<Author>| {
                    author.filter(|&author| {
                        author == Author::Other && rules != Rules::DeleteDropsResends
                    })
                };
                dev.unsent = Unsent {
                    existence: Some(Author::Own),
                    position: resent(dev.unsent.position),
                    content: resent(dev.unsent.content),
                };
                if !dev.r.created && dev.flight.is_none() {
                    dev.unsent = Unsent::default();
                    dev.s.live = false;
                    dev.s.deleted_here = true;
                    dev.s.exist_seq = 0;
                } else if !dev.s.live && dev.flight.is_none() && w.covers(d, rules) {
                    w.devices[d].unsent = Unsent::default();
                }
            }
            Action::TrashRestore(d) => {
                w.budget.trash_restores[d] -= 1;
                let seq = w.server.head() + 1;
                let mut node = w.server.node();
                w.server.restored.extend(node.trashed.take());
                node.meta_rev = seq;
                if rules == Rules::RevivalMarksPosition {
                    node.meta_edit_rev = seq;
                    node.meta_editor = Some(d);
                }
                w.append(node, None);
            }
            Action::Build(d, k) => {
                let ops: Vec<(Op, Author)> = w.plan(d).into_iter().take(k as usize).collect();
                let dev = &w.devices[d];
                let by_user: Vec<bool> = (ops.iter())
                    .map(|(op, author)| match op {
                        Op::Delete { gone, .. } => w.author(*gone) == Some(d),
                        Op::Move { .. } => dev.by_user[0],
                        Op::Update { .. } => dev.by_user[1],
                        Op::Copy { .. } => true,
                        Op::Create { .. } => *author == Author::Own,
                    })
                    .collect();
                for ((op, author), by_user) in ops.iter().zip(&by_user) {
                    if (*author == Author::Own) != *by_user {
                        return Err(format!(
                            "device {d} sends {op:?} as {author:?}, but its user's own value: {by_user}"
                        ));
                    }
                }
                let require_head = ops.iter().any(|(_, author)| *author == Author::Other);
                let base = dev.head();
                w.devices[d].flight = Some(Flight {
                    base,
                    require_head,
                    ops,
                    by_user,
                    phase: Phase::Sent,
                });
            }
            Action::Serve(d) | Action::ServeRenamed(d) => {
                if let Action::ServeRenamed(_) = action {
                    w.budget.renames -= 1;
                }
                let answer = w.serve(d, matches!(action, Action::ServeRenamed(_)), rules)?;
                w.devices[d].flight.as_mut().unwrap().phase = Phase::Answered(answer);
            }
            Action::Answer(d) => {
                let flight = w.devices[d].flight.as_mut().unwrap();
                let Phase::Answered(answer) = flight.phase.clone() else {
                    unreachable!()
                };
                flight.phase = Phase::Arrived(answer);
                w.settle(d, rules)?;
            }
            Action::Record(d) => {
                let record = w.server.log[w.devices[d].seen.len()];
                let dev = &mut w.devices[d];
                for field in FIELDS
                    .into_iter()
                    .filter(|_| rules.bound_only() && dev.r.created)
                {
                    let replaced = (dev.r.get(field), dev.r.rev(field));
                    let history = &mut dev.history[slot(field)];
                    if replaced.0 != record.node.get(field) && !history.contains(&replaced) {
                        history.push(replaced);
                    }
                }
                dev.r = record.node;
                dev.seen.push(record.token);
                w.prove(d, rules);
                w.settle(d, rules)?;
            }
            Action::Lose(held) => {
                w.budget.losses -= 1;
                w.server.before_loss =
                    (w.budget.losses > 0).then(|| (w.server.log.clone(), w.server.copies.clone()));
                w.server.log.truncate(held as usize);
                w.server.copies.retain(|(_, seq)| *seq <= held);
            }
            Action::Unlose => {
                w.budget.losses -= 1;
                (w.server.log, w.server.copies) = w.server.before_loss.take().unwrap();
                for d in 0..2 {
                    w.settle(d, rules)?;
                }
            }
            Action::Bootstrap(d) => w.bootstrap(d, rules)?,
        }
        for dev in &mut w.devices {
            if let Local::Live { pos, content } = dev.l {
                dev.held[0] |= 1 << pos.0;
                dev.held[1] |= 1 << content.0;
            }
        }
        w.check()?;
        Ok(w)
    }

    fn append(&mut self, node: Node, by: Option<Dev>) -> Seq {
        let seq = self.server.head() + 1;
        let token = self.server.tokens;
        self.server.tokens += 1;
        self.server.log.push(Record { seq, token, node });
        self.server.authors.push(by);
        seq
    }

    fn serve(&mut self, d: Dev, renamed: bool, rules: Rules) -> Result<Answer, Broken> {
        let flight = self.devices[d].flight.clone().unwrap();
        let head = self.server.head();
        let (base, token) = flight.base;
        if base > head || self.server.log[base as usize - 1].token != token {
            return Ok(Answer::Ahead);
        }
        if flight.require_head && head != base {
            return Ok(Answer::HeadMoved);
        }
        let seq = head + 1;
        let mut node = self.server.node();
        let mut codes = Vec::new();
        let saws: Vec<Option<[Tok; 2]>> = (0..self.values.len())
            .map(|index| self.saw(Tok(index as u8), rules))
            .collect();
        let saw = |gone: Tok| saws[gone.0 as usize];
        let helds: Vec<[u32; 2]> = self.values.iter().map(|value| value.held).collect();
        let saw_value = |gone: Tok, field: Field, value: Tok| {
            saw(gone).is_some_and(|seen| {
                seen[slot(field)] == value
                    || (rules != Rules::DeleteSeesOnlyView
                        && helds[gone.0 as usize][slot(field)] & (1 << value.0) != 0)
            })
        };
        let seen_by_delete = |node: &Node, value: Tok, field: Field| {
            !matches!(rules, Rules::SeqDelete | Rules::ResendRevivesSeenValue)
                && node
                    .trashed
                    .is_some_and(|trash| saw_value(trash, field, value))
        };
        for ((op, _), &by_user) in flight.ops.iter().zip(&flight.by_user) {
            let revives = |value: Tok| {
                if by_user {
                    return Ok(());
                }
                Err(format!(
                    "device {d} revives a deleted node with {value:?}, which its user did not make"
                ))
            };
            let code = match *op {
                Op::Create { pos, content } if !node.created => {
                    let claims = |value: Tok| {
                        !rules.create_from() || self.values[value.0 as usize].author == Some(d)
                    };
                    node = Node {
                        created: true,
                        created_rev: seq,
                        trashed: None,
                        pos,
                        content,
                        meta_rev: seq,
                        meta_edit_rev: if claims(pos) { seq } else { 0 },
                        meta_editor: claims(pos).then_some(d),
                        content_rev: seq,
                        edit_rev: if claims(content) { seq } else { 0 },
                        editor: claims(content).then_some(d),
                        pos_rev: seq,
                        trash_base: 0,
                    };
                    Code::Applied
                }
                Op::Create { .. } => Code::Rejected,
                Op::Delete { .. } | Op::Move { .. } | Op::Update { .. } if !node.created => {
                    Code::Rejected
                }
                Op::Delete { base, gone } => {
                    let unseen = |field: Field| {
                        let differs =
                            rules == Rules::SeqDelete || !saw_value(gone, field, node.get(field));
                        differs && node.edit(field) > base && node.by(field) != Some(d)
                    };
                    if let Some(trash) = node.trashed {
                        if rules != Rules::TrashSettlesDelete
                            && saw(gone).is_some()
                            && saw(trash) != saw(gone)
                        {
                            node.trashed = Some(gone);
                            node.trash_base = base;
                            node.meta_rev = seq;
                        }
                        Code::Applied
                    } else if unseen(Field::Content) || unseen(Field::Position) {
                        Code::Superseded
                    } else {
                        node.trashed = Some(gone);
                        node.trash_base = base;
                        node.meta_rev = seq;
                        node.meta_edit_rev = seq;
                        node.meta_editor = Some(d);
                        Code::Applied
                    }
                }
                Op::Move {
                    to,
                    from: Some(from),
                } => {
                    if node.trashed.is_none() && node.pos == from {
                        node.pos = to;
                        node.pos_rev = seq;
                        node.meta_rev = seq;
                        self.rename(&mut node, to, renamed)
                    } else {
                        Code::Superseded
                    }
                }
                Op::Move { to, from: None } if seen_by_delete(&node, to, Field::Position) => {
                    Code::Superseded
                }
                Op::Move { to, from: None } => {
                    let revived = node.trashed.take().is_some();
                    if revived {
                        revives(to)?;
                        node.meta_rev = seq;
                        node.meta_edit_rev = seq;
                        node.meta_editor = Some(d);
                    }
                    if node.pos == to {
                        if node.meta_edit_rev < node.meta_rev {
                            node.meta_edit_rev = if rules == Rules::ClaimAtSeq || rules.old_server()
                            {
                                seq
                            } else {
                                node.meta_rev
                            };
                            node.meta_editor = Some(d);
                        }
                        Code::Applied
                    } else {
                        let before = node.pos;
                        let value = self.values[to.0 as usize];
                        if self.author(before).is_some() && value.pos != before {
                            self.server.overwritten.push(before);
                        }
                        node.pos = to;
                        node.pos_rev = seq;
                        node.meta_rev = seq;
                        node.meta_edit_rev = seq;
                        node.meta_editor = Some(d);
                        self.rename(&mut node, to, renamed)
                    }
                }
                Op::Update {
                    content,
                    resend: true,
                    base,
                } => {
                    if node.trashed.is_some() {
                        Code::Superseded
                    } else if node.content != content && node.content_rev > base {
                        self.server.copies.push((content, seq));
                        Code::Copied
                    } else {
                        if node.content != content {
                            node.content = content;
                            node.content_rev = seq;
                        }
                        Code::Applied
                    }
                }
                Op::Update {
                    content,
                    resend: false,
                    ..
                } if seen_by_delete(&node, content, Field::Content) => Code::Superseded,
                Op::Update {
                    content,
                    resend: false,
                    base,
                } => {
                    if node.trashed.is_some() {
                        revives(content)?;
                        node.trashed = None;
                        node.meta_rev = seq;
                        if rules == Rules::RevivalMarksPosition {
                            node.meta_edit_rev = seq;
                            node.meta_editor = Some(d);
                        }
                        node.content = content;
                        node.content_rev = seq;
                        node.edit_rev = seq;
                        node.editor = Some(d);
                        Code::Applied
                    } else if node.content == content {
                        if node.edit_rev < node.content_rev {
                            node.edit_rev = if rules == Rules::ClaimAtSeq || rules.old_server() {
                                seq
                            } else {
                                node.content_rev
                            };
                            node.editor = Some(d);
                        }
                        Code::Applied
                    } else if node.content_rev > base {
                        self.server.copies.push((content, seq));
                        Code::Copied
                    } else {
                        node.content = content;
                        node.content_rev = seq;
                        node.edit_rev = seq;
                        node.editor = Some(d);
                        Code::Applied
                    }
                }
                Op::Copy { content } => {
                    self.server.copies.push((content, seq));
                    Code::Applied
                }
            };
            codes.push(code);
        }
        let token = self.server.tokens;
        self.append(node, Some(d));
        Ok(Answer::Done { seq, token, codes })
    }

    fn rename(&mut self, node: &mut Node, to: Tok, renamed: bool) -> Code {
        if !renamed {
            return Code::Applied;
        }
        let value = self.values[to.0 as usize];
        let name = self.mint(Value {
            renamed_from: Some(to),
            ..value
        });
        node.pos = name;
        Code::Renamed(name)
    }

    fn covers(&self, d: Dev, rules: Rules) -> bool {
        let dev = &self.devices[d];
        rules == Rules::TrashSettlesDelete
            || dev
                .r
                .trashed
                .is_some_and(|trash| self.trash_covers(d, trash, rules))
    }

    fn trash_covers(&self, d: Dev, trash: Tok, rules: Rules) -> bool {
        let dev = &self.devices[d];
        let view = [dev.s.pos.val, dev.s.content.val];
        let held = match dev.l {
            Local::Gone(delete) if rules != Rules::TrashCoversView => {
                self.values[delete.0 as usize].held
            }
            _ => [0; 2],
        };
        let trash_held = self.values[trash.0 as usize].held;
        self.saw(trash, rules) == Some(view) && (0..2).all(|at| held[at] & !trash_held[at] == 0)
    }

    fn prove(&mut self, d: Dev, rules: Rules) {
        let covered = self.covers(d, rules);
        let dev = &mut self.devices[d];
        let r = dev.r;
        if r.trashed.is_some() && rules != Rules::TrashedRecordProves {
            let settled = (dev.flight.is_none() || rules == Rules::StaleTrashProves) && covered;
            if let (Local::Gone(_), Some(author), true) = (dev.l, dev.unsent.existence, settled) {
                dev.s.live = false;
                dev.s.exist_seq = dev.seen.len() as Seq;
                dev.s.deleted_here = author == Author::Own;
                dev.unsent = Unsent::default();
            }
            return;
        }
        for field in FIELDS {
            let (Some(author), Some(local)) = (dev.unsent.get(field), dev.l.get(field)) else {
                continue;
            };
            let proves =
                author == Author::Other || r.user_set(field) || rules == Rules::ResendRecordEndsOwn;
            if local == r.get(field) && proves {
                let seq = dev.seen.len() as Seq;
                dev.s.set(
                    field,
                    Agreed {
                        val: local,
                        seq,
                        own: author == Author::Own,
                    },
                );
                dev.unsent.set(field, None);
            }
        }
    }

    fn settle(&mut self, d: Dev, rules: Rules) -> Result<(), Broken> {
        if self.lost(d) {
            return Ok(());
        }
        let dev = &mut self.devices[d];
        if let Some(flight) = &dev.flight {
            match &flight.phase {
                Phase::Arrived(Answer::Done { seq, .. }) if dev.head().0 >= *seq => {
                    self.acknowledge(d, rules)
                }
                Phase::Arrived(Answer::Ahead | Answer::HeadMoved) => dev.flight = None,
                _ => {}
            }
        }
        if self.devices[d].flight.is_none() {
            self.classify(d, rules)?;
        }
        Ok(())
    }

    fn acknowledge(&mut self, d: Dev, rules: Rules) {
        let authors: Vec<Option<Dev>> = self.values.iter().map(|value| value.author).collect();
        let made_here = |value: Tok| authors[value.0 as usize] == Some(d);
        let landed_own = |value: Tok| rules == Rules::LandedResendIsOwn || made_here(value);
        let dev = &mut self.devices[d];
        let flight = dev.flight.take().unwrap();
        let Phase::Arrived(Answer::Done { seq, codes, .. }) = flight.phase else {
            unreachable!()
        };
        for ((op, _), code) in (flight.ops.iter().zip(&codes)).filter(|_| rules.bound_only()) {
            let (field, value) = match (*op, *code) {
                (Op::Move { .. }, Code::Renamed(name)) => (Field::Position, name),
                (Op::Move { to, .. }, Code::Applied) => (Field::Position, to),
                (Op::Update { content, .. }, Code::Applied) => (Field::Content, content),
                _ => continue,
            };
            let replaced = (dev.r.get(field), dev.r.rev(field));
            let history = &mut dev.history[slot(field)];
            if replaced.0 != value && !history.contains(&replaced) {
                history.push(replaced);
            }
        }
        for ((op, author), code) in flight.ops.into_iter().zip(codes) {
            let own = author == Author::Own || rules == Rules::LandedResendIsOwn;
            match (op, code) {
                (Op::Create { pos, content }, Code::Applied) => {
                    let lost = dev.lost_position.take().is_some();
                    let held = dev.l.get(Field::Position);
                    let Some(at) = (if lost { held } else { Some(pos) }) else {
                        dev.unsent = Unsent::default();
                        continue;
                    };
                    dev.s = Synced {
                        live: true,
                        exist_seq: seq,
                        deleted_here: false,
                        pos: Agreed {
                            val: at,
                            seq,
                            own: landed_own(at),
                        },
                        content: Agreed {
                            val: content,
                            seq,
                            own: landed_own(content),
                        },
                    };
                    if let Local::Live { .. } = dev.l {
                        dev.unsent.existence = None;
                        if dev.unsent.position.is_none() && held != Some(at) {
                            dev.take(Field::Position, at);
                        }
                        for (field, sent) in [(Field::Position, at), (Field::Content, content)] {
                            if dev.l.get(field) == Some(sent) {
                                dev.unsent.set(field, None);
                            }
                        }
                    }
                }
                (Op::Create { pos, .. }, _) => dev.take_refused_create(pos, rules, made_here),
                (_, Code::Rejected) => {}
                (Op::Delete { .. }, Code::Superseded)
                    if rules != Rules::SupersededDeleteGivesUp => {}
                (Op::Move { to, .. }, Code::Applied | Code::Renamed(_))
                    if matches!(dev.l, Local::Gone(_)) =>
                {
                    let at = if let Code::Renamed(name) = code {
                        name
                    } else {
                        to
                    };
                    if rules != Rules::DeletedAdoptsRecords && dev.s.pos.val == to {
                        dev.s.pos = Agreed { val: at, seq, own };
                    }
                    dev.unsent.position = None;
                }
                (Op::Update { content, .. }, Code::Applied) if matches!(dev.l, Local::Gone(_)) => {
                    if rules != Rules::DeletedAdoptsRecords && dev.s.content.val == content {
                        dev.s.content = Agreed {
                            val: content,
                            seq,
                            own,
                        };
                    }
                    dev.unsent.content = None;
                }
                (Op::Move { .. }, _) if matches!(dev.l, Local::Gone(_)) => {
                    dev.unsent.position = None
                }
                (Op::Update { .. }, _) if matches!(dev.l, Local::Gone(_)) => {
                    dev.unsent.content = None
                }
                (Op::Delete { .. }, code) => {
                    dev.s.live = false;
                    dev.s.exist_seq = seq;
                    dev.s.deleted_here = code == Code::Applied;
                    if let Local::Gone(_) = dev.l {
                        dev.unsent.existence = None;
                    }
                }
                (Op::Move { to, .. }, Code::Applied | Code::Renamed(_)) => {
                    let at = if let Code::Renamed(name) = code {
                        name
                    } else {
                        to
                    };
                    dev.s.pos = Agreed { val: at, seq, own };
                    if dev.l.get(Field::Position) == Some(to) {
                        dev.l.set(Field::Position, at);
                        dev.unsent.position = None;
                    }
                    let content = dev.l.get(Field::Content);
                    if let (Rules::MoveResultEndsContent, Some(val)) = (rules, content) {
                        dev.s.content = Agreed {
                            val,
                            seq,
                            own: true,
                        };
                        dev.unsent.content = None;
                    }
                    if rules == Rules::MoveResultMarksContentResent
                        && author == Author::Other
                        && content.is_some_and(|content| content != dev.s.content.val)
                    {
                        dev.unsent.content = Some(Author::Other);
                    }
                }
                (Op::Move { to, .. }, _) => {
                    if dev.l.get(Field::Position) == Some(to) {
                        dev.s.pos = Agreed {
                            val: to,
                            seq,
                            own: author == Author::Own && rules != Rules::SupersededDropsOwn,
                        };
                        dev.unsent.position = None;
                    }
                }
                (Op::Update { content, .. }, Code::Applied) => {
                    dev.s.content = Agreed {
                        val: content,
                        seq,
                        own,
                    };
                    if dev.l.get(Field::Content) == Some(content) {
                        dev.unsent.content = None;
                    }
                }
                (Op::Update { content, .. }, Code::Copied) => {
                    dev.copies.push((content, Some(seq)));
                    if dev.l.get(Field::Content) == Some(content) {
                        let content = dev.r.content;
                        dev.take(Field::Content, content);
                        dev.s.content = Agreed {
                            val: dev.r.content,
                            seq: dev.r.content_rev,
                            own: false,
                        };
                        dev.unsent.content = None;
                    }
                }
                (Op::Update { content, .. }, _) => {
                    if dev.l.get(Field::Content) == Some(content) {
                        dev.s.content = Agreed {
                            val: content,
                            seq,
                            own: author == Author::Own && rules != Rules::SupersededDropsOwn,
                        };
                        dev.unsent.content = None;
                    }
                }
                (Op::Copy { content }, _) => {
                    for copy in &mut dev.copies {
                        if copy.0 == content && copy.1.is_none() {
                            copy.1 = Some(seq);
                        }
                    }
                }
            }
        }
    }

    fn take_remote(&mut self, d: Dev) -> Result<(), Broken> {
        let dev = &mut self.devices[d];
        if dev.l.get(Field::Position).is_some() && dev.unsent.holds_own() {
            return Err(format!(
                "device {d} downloads the node over its user's change {:?}",
                dev.unsent
            ));
        }
        let r = dev.r;
        dev.l = Local::Live {
            pos: r.pos,
            content: r.content,
        };
        for field in FIELDS {
            if dev.s.get(field).val != r.get(field) {
                dev.s.set(
                    field,
                    Agreed {
                        val: r.get(field),
                        seq: r.rev(field),
                        own: false,
                    },
                );
            }
            dev.by_user[slot(field)] = dev.s.get(field).own;
        }
        dev.s.live = true;
        dev.s.exist_seq = r.meta_rev;
        dev.s.deleted_here = false;
        dev.unsent = Unsent::default();
        dev.lost_position = None;
        Ok(())
    }

    fn classify(&mut self, d: Dev, rules: Rules) -> Result<(), Broken> {
        let covered = self.covers(d, rules);
        let edited_here = self.edited_here(d, &self.devices[d].r, &self.devices[d].seen, rules);
        let dev = &mut self.devices[d];
        let r = dev.r;
        if !r.created {
            if let Local::Gone(_) = dev.l {
                if dev.unsent.existence == Some(Author::Own) {
                    dev.s.deleted_here = true;
                    dev.s.exist_seq = 0;
                }
                dev.s.live = false;
                dev.unsent = Unsent::default();
                dev.lost_position = None;
            }
            return Ok(());
        }
        if let Local::Gone(gone) = dev.l
            && !dev.s.live
            && dev.s.deleted_here
            && dev.s.exist_seq == 0
        {
            dev.s.exist_seq = r.meta_rev;
            let held = self.values[gone.0 as usize].held;
            let unheld = FIELDS.iter().any(|&field| {
                r.user_set(field)
                    && !edited_here[slot(field)]
                    && held[slot(field)] & (1 << r.get(field).0) == 0
            });
            if r.trashed.is_none()
                && rules.absent_delete_resent()
                && !(rules.delete_needs_held() && unheld)
            {
                dev.s.live = true;
                dev.unsent.existence = Some(Author::Own);
            }
        }
        if let (Local::Live { content, .. }, Some(pending)) = (dev.l, dev.unsent.existence) {
            match r.trashed {
                Some(delete) if pending == Author::Other && !dev.unsent.holds_own() => {
                    dev.l = Local::Gone(delete);
                    dev.s.live = false;
                    dev.s.exist_seq = r.meta_rev;
                    dev.s.deleted_here = false;
                    dev.unsent = Unsent::default();
                    dev.lost_position = None;
                    return Ok(());
                }
                Some(_) => return Ok(()),
                None => {
                    dev.unsent.existence = None;
                    dev.lost_position = None;
                    if content != r.content {
                        dev.copies.push((content, None));
                        dev.take(Field::Content, r.content);
                        dev.unsent.content = None;
                    }
                    if dev.unsent.position.is_none() {
                        dev.take(Field::Position, r.pos);
                    }
                    for field in FIELDS {
                        dev.s.set(
                            field,
                            Agreed {
                                val: r.get(field),
                                seq: r.rev(field),
                                own: false,
                            },
                        );
                    }
                    dev.s.live = true;
                    dev.s.exist_seq = r.meta_rev;
                    dev.s.deleted_here = false;
                }
            }
        }
        match (dev.l, r.trashed) {
            (Local::Live { .. }, Some(delete)) => {
                if !dev.unsent.holds_own() {
                    dev.l = Local::Gone(delete);
                    dev.s.live = false;
                    dev.s.exist_seq = r.meta_rev;
                    dev.s.deleted_here = false;
                    dev.unsent = Unsent::default();
                }
            }
            (Local::Gone(_), Some(_)) if dev.unsent.existence == Some(Author::Own) && !covered => {
                dev.unsent = Unsent {
                    existence: Some(Author::Own),
                    ..Unsent::default()
                };
            }
            (Local::Gone(_), Some(delete)) => {
                if dev.s.live {
                    dev.s.live = false;
                    dev.s.exist_seq = r.meta_rev;
                    let pending = dev.unsent.existence == Some(Author::Own)
                        && rules != Rules::TrashSettlesDelete;
                    dev.s.deleted_here =
                        pending || self.values[delete.0 as usize].author == Some(d);
                }
                self.devices[d].unsent = Unsent::default();
            }
            (Local::Gone(gone), None) => {
                let deleting = (dev.s.live || rules != Rules::DeleteNeedsSyncedLive)
                    && dev.unsent.existence == Some(Author::Own);
                if !deleting {
                    return self.take_remote(d);
                }
                if !dev.s.live {
                    dev.s.live = true;
                    dev.s.exist_seq = r.meta_rev;
                    dev.s.deleted_here = false;
                }
                let old = rules == Rules::LossForgetsDeletedFields;
                let unseen = FIELDS.iter().any(|&field| {
                    let synced = dev.s.get(field);
                    if rules.delete_needs_held() {
                        let held = self.values[gone.0 as usize].held[slot(field)];
                        return r.user_set(field)
                            && !edited_here[slot(field)]
                            && held & (1 << r.get(field).0) == 0;
                    }
                    r.get(field) != synced.val
                        && r.user_set(field)
                        && !edited_here[slot(field)]
                        && (old || r.edit(field) > synced.seq)
                        && (rules == Rules::DeleteSeesOnlyView
                            || old_wire(d, rules)
                            || dev.held[slot(field)] & (1 << r.get(field).0) == 0)
                });
                if unseen {
                    return self.take_remote(d);
                }
                for field in FIELDS {
                    let adopts = old
                        || (rules == Rules::DeletedAdoptsRecords
                            && r.user_set(field)
                            && r.edit(field) > dev.s.get(field).seq);
                    if adopts && r.get(field) != dev.s.get(field).val {
                        let own = !old && r.user_set(field) && edited_here[slot(field)];
                        dev.s.set(
                            field,
                            Agreed {
                                val: r.get(field),
                                seq: r.rev(field),
                                own,
                            },
                        );
                    }
                }
            }
            (Local::Live { .. }, None) => {
                if !dev.s.live {
                    dev.s.live = true;
                    dev.s.exist_seq = r.meta_rev;
                    dev.s.deleted_here = false;
                }
                for field in FIELDS {
                    let (remote, synced, local) = (
                        r.get(field),
                        dev.s.get(field).val,
                        dev.l.get(field).unwrap(),
                    );
                    if remote == synced {
                        continue;
                    }
                    let agreed = Agreed {
                        val: remote,
                        seq: r.rev(field),
                        own: false,
                    };
                    match dev.unsent.get(field) {
                        None => {
                            if local != synced {
                                return Err(format!(
                                    "device {d} holds {field:?} {local:?} apart from S with no author"
                                ));
                            }
                            dev.take(field, remote);
                            dev.s.set(field, agreed);
                        }
                        Some(Author::Other) => {
                            dev.take(field, remote);
                            dev.s.set(field, agreed);
                            dev.unsent.set(field, None);
                        }
                        Some(Author::Own) if local == remote => {
                            dev.s.set(
                                field,
                                Agreed {
                                    own: true,
                                    ..agreed
                                },
                            );
                            if r.user_set(field) {
                                dev.unsent.set(field, None);
                            }
                        }
                        Some(Author::Own)
                            if rules != Rules::CopiesOwnBase && {
                                let mut at = local;
                                let mut based = false;
                                for _ in 0..self.values.len() {
                                    let value = self.values[at.0 as usize];
                                    if value.author != Some(d) || value.renamed_from.is_some() {
                                        break;
                                    }
                                    at = match field {
                                        Field::Position => value.pos,
                                        _ => value.content,
                                    };
                                    if at == remote {
                                        based = true;
                                        break;
                                    }
                                }
                                based
                            } =>
                        {
                            dev.s.set(field, agreed)
                        }
                        Some(Author::Own) if field == Field::Content => {
                            dev.copies.push((local, None));
                            dev.take(field, remote);
                            dev.s.set(field, agreed);
                            dev.unsent.set(field, None);
                        }
                        Some(Author::Own) => dev.s.set(field, agreed),
                    }
                }
            }
        }
        Ok(())
    }

    fn bootstrap(&mut self, d: Dev, rules: Rules) -> Result<(), Broken> {
        let log: Vec<u8> = self.server.log.iter().map(|record| record.token).collect();
        let listing = self.server.node();
        if !listing.created {
            return self.bootstrap_absent(d, rules, log, listing);
        }
        let trash_covers = listing
            .trashed
            .is_some_and(|trash| self.trash_covers(d, trash, rules));
        let edited_here = self.edited_here(d, &listing, &self.devices[d].seen, rules);
        let dev = &self.devices[d];
        let own = |token: u8| self.server.authors[token as usize] == Some(d);
        let pairs = |seen: &[u8]| -> Vec<bool> {
            (0..log.len())
                .map(|at| seen.get(at) == Some(&log[at]))
                .collect()
        };
        let held_pairs = pairs(&dev.seen);
        let bound = (0..log.len())
            .rev()
            .find(|&at| held_pairs[at] && own(log[at]))
            .map_or(0, |at| at + 1);
        let earlier_pairs = pairs(&dev.earlier);
        let highest = (0..log.len())
            .rev()
            .find(|&at| held_pairs[at] || earlier_pairs[at])
            .map_or(0, |at| at + 1);
        let kept_at: Vec<bool> = (0..log.len())
            .map(|at| match rules {
                _ if rules.bound_only() => at < bound,
                Rules::HighestHeld => at < highest,
                _ => held_pairs[at],
            })
            .collect();
        let kept = |seq: Seq| seq == 0 || kept_at.get(seq as usize - 1) == Some(&true);
        let below = |seq: Seq| (0..=seq).rev().find(|&at| kept(at)).unwrap_or(0);
        let bound = bound as Seq;
        let placed_here =
            (dev.seen.get(listing.meta_rev as usize - 1)).is_some_and(|&token| own(token));
        let removed_at = (!dev.s.live && dev.s.deleted_here).then_some(dev.s.exist_seq);
        let at = |field: Field, record: Option<Seq>, value: Tok| {
            (rules.bound_only())
                .then(|| {
                    (dev.history[slot(field)].iter()).position(|&(held, rev)| {
                        held == value && record.is_none_or(|record| record == rev)
                    })
                })
                .flatten()
        };
        let listed = |field: Field| at(field, Some(listing.rev(field)), listing.get(field));
        let known = FIELDS.map(|field| kept(listing.rev(field)) || listed(field).is_some());
        let earlier = FIELDS.map(|field| {
            listed(field).is_some_and(|listed| {
                at(field, None, dev.s.get(field).val).is_none_or(|synced| listed < synced)
            })
        });
        let known = |field: Field| known[slot(field)];
        let dev = &mut self.devices[d];
        dev.r = listing;
        if rules == Rules::HighestHeld {
            dev.earlier = std::mem::take(&mut dev.seen);
        }
        dev.seen = log;
        dev.flight = None;
        for copy in &mut dev.copies {
            if copy.1.is_some_and(|seq| !kept(seq)) {
                copy.1 = None;
            }
        }
        let mut lost_own = false;
        for field in FIELDS {
            let old = dev.s.get(field);
            let held_as_own = listing.user_set(field) && edited_here[slot(field)];
            if matches!(dev.l, Local::Gone(_)) && rules != Rules::LossForgetsDeletedFields {
                dev.s.set(
                    field,
                    Agreed {
                        seq: if earlier[slot(field)] {
                            below(old.seq).max(listing.rev(field))
                        } else {
                            below(old.seq)
                        },
                        ..old
                    },
                );
                let resend = dev.unsent.existence == Some(Author::Own)
                    && !kept(old.seq)
                    && listing.get(field) != old.val
                    && (!rules.bound_only() || kept(listing.rev(field)) || earlier[slot(field)]);
                if resend && rules != Rules::DeleteDropsResends {
                    dev.unsent.set(
                        field,
                        Some(if old.own { Author::Own } else { Author::Other }),
                    );
                }
                continue;
            }
            if kept(old.seq) {
                continue;
            }
            if listing.get(field) == old.val && (!old.own || held_as_own) {
                if rules != Rules::LossKeepsStaleSeq {
                    dev.s.set(
                        field,
                        Agreed {
                            seq: listing.rev(field),
                            ..old
                        },
                    );
                }
                continue;
            }
            let agreed = if known(field) {
                listing.get(field)
            } else {
                UNKNOWN
            };
            dev.s.set(
                field,
                Agreed {
                    val: agreed,
                    seq: listing.rev(field),
                    own: false,
                },
            );
            if dev.l.get(field) == Some(old.val) && dev.unsent.get(field).is_none() {
                let newer = old.own
                    || !rules.bound_only()
                    || rules == Rules::BoundOnlyResendsSeen
                    || !known(field)
                    || kept(listing.rev(field))
                    || earlier[slot(field)];
                if !newer {
                    dev.take(field, agreed);
                    continue;
                }
                dev.unsent.set(
                    field,
                    Some(if old.own { Author::Own } else { Author::Other }),
                );
                lost_own |= old.own;
            }
        }
        if rules == Rules::NodeAuthor && (lost_own || dev.unsent.holds_own()) {
            for field in FIELDS {
                if dev.unsent.get(field).is_some() {
                    dev.unsent.set(field, Some(Author::Own));
                }
            }
        }
        let listed = listing.trashed.is_none();
        let covered = rules == Rules::TrashSettlesDelete || trash_covers;
        let lost_delete = !dev.s.live && (listed || !covered) && !kept(dev.s.exist_seq);
        if lost_delete && matches!(dev.l, Local::Gone(_)) && dev.unsent.existence.is_none() {
            dev.unsent.existence = Some(if dev.s.deleted_here {
                Author::Own
            } else {
                Author::Other
            });
        }
        if lost_delete || (dev.s.live && !listed) {
            dev.s.live = listed;
            dev.s.exist_seq = listing.meta_rev;
            dev.s.deleted_here = !listed
                && covered
                && dev.unsent.existence == Some(Author::Own)
                && rules != Rules::TrashSettlesDelete;
        }
        if let Some(removed_at) = removed_at {
            let edited_since = listing.content_rev.max(listing.edit_rev) > removed_at;
            let credited = placed_here
                && (listing.meta_rev <= bound || rules == Rules::ListedRemovalsInRecovery);
            if listed && matches!(dev.l, Local::Gone(_)) && !edited_since && credited {
                dev.s.live = true;
                dev.s.exist_seq = listing.meta_rev;
                dev.s.deleted_here = false;
                dev.unsent.existence.get_or_insert(Author::Own);
                let shape = matches!(
                    rules,
                    Rules::ListedRemovalsInRecovery | Rules::RemovalTakesListedShape
                );
                for field in FIELDS.into_iter().filter(|_| shape) {
                    dev.s.set(
                        field,
                        Agreed {
                            val: listing.get(field),
                            seq: listing.rev(field),
                            own: false,
                        },
                    );
                }
            }
        }
        self.classify(d, rules)
    }

    fn bootstrap_absent(
        &mut self,
        d: Dev,
        rules: Rules,
        log: Vec<u8>,
        listing: Node,
    ) -> Result<(), Broken> {
        let dev = &self.devices[d];
        let pre = dev.r;
        let lost_own = FIELDS.map(|field| {
            dev.unsent.get(field) == Some(Author::Own)
                || (dev.by_user[slot(field)]
                    && dev.l.get(field).is_some_and(|local| {
                        let value = self.values[local.0 as usize];
                        let before = match field {
                            Field::Position => value.pos,
                            _ => value.content,
                        };
                        before != local
                    }))
        });
        let changed = lost_own.contains(&true);
        let content_here = pre.created
            && (dev.seen.get(pre.content_rev as usize - 1))
                .is_some_and(|&token| self.server.authors[token as usize] == Some(d));
        let untaken = (pre.trashed).filter(|&trash| self.author(trash) != Some(d));
        let dev = &mut self.devices[d];
        dev.r = listing;
        dev.seen = log;
        dev.flight = None;
        for copy in &mut dev.copies {
            if copy.1.is_some_and(|seq| seq > 1) {
                copy.1 = None;
            }
        }
        dev.s.live = false;
        dev.s.exist_seq = 1;
        dev.s.deleted_here = false;
        dev.s.pos.seq = 1;
        dev.s.content.seq = 1;
        let Local::Live { pos, .. } = dev.l else {
            if let Local::Gone(gone) = dev.l
                && self.values[gone.0 as usize].author == Some(d)
            {
                let dev = &mut self.devices[d];
                dev.s.deleted_here = true;
                dev.s.exist_seq = 0;
            }
            let dev = &mut self.devices[d];
            dev.unsent = Unsent::default();
            dev.lost_position = None;
            return Ok(());
        };
        if let (Some(trash), false) = (untaken, changed) {
            dev.l = Local::Gone(trash);
            dev.unsent = Unsent::default();
            dev.lost_position = None;
            return Ok(());
        }
        let resends = !changed && (rules == Rules::LostCreateResends || !content_here);
        if pre.created {
            dev.lost_position = (rules != Rules::NoLostPosition
                && dev.unsent.position.is_none()
                && pre.trashed.is_none()
                && pre.pos != pos)
                .then_some(pre.pos);
        }
        let own = |lost: bool| lost.then_some(Author::Own);
        dev.unsent = Unsent {
            existence: Some(if resends { Author::Other } else { Author::Own }),
            position: own(lost_own[0]),
            content: own(lost_own[1]),
        };
        Ok(())
    }

    fn check(&self) -> Result<(), Broken> {
        for (d, dev) in self.devices.iter().enumerate() {
            match dev.l {
                Local::Live { .. } => {
                    for field in FIELDS {
                        let local = dev.l.get(field).unwrap();
                        match dev.unsent.get(field) {
                            Some(author) if (author == Author::Own) != dev.by_user[slot(field)] => {
                                return Err(format!(
                                    "device {d} holds {field:?} {local:?} as {author:?}, but its user's own value: {}",
                                    dev.by_user[slot(field)]
                                ));
                            }
                            None if dev.s.live && dev.s.get(field).val != local => {
                                return Err(format!(
                                    "device {d} holds {field:?} {local:?} apart from S with no author"
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                Local::Gone(gone) => {
                    if dev.s.live && dev.unsent.existence.is_none() && dev.flight.is_none() {
                        return Err(format!(
                            "device {d} holds a delete apart from S with no author"
                        ));
                    }
                    if dev.unsent.existence == Some(Author::Own) && self.author(gone) != Some(d) {
                        return Err(format!(
                            "device {d} holds another device's delete as its own"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn converged(&self) -> bool {
        let node = self.server.node();
        (0..2).all(|d| {
            let dev = &self.devices[d];
            dev.flight.is_none()
                && self.caught_up(d)
                && dev.unsent.is_empty()
                && dev.copies.iter().all(|(_, landed)| landed.is_some())
                && match dev.l {
                    Local::Live { pos, content } => {
                        node.trashed.is_none()
                            && (pos, content) == (node.pos, node.content)
                            && dev.s.live
                            && (dev.s.pos.val, dev.s.content.val) == (pos, content)
                    }
                    Local::Gone(_) => (node.trashed.is_some() || !node.created) && !dev.s.live,
                }
        })
    }

    fn survived(&self, value: Tok) -> bool {
        let node = self.server.node();
        let info = self.values[value.0 as usize];
        let held = node.created && node.trashed.is_none() && node.get(info.field) == value;
        let copied = info.field == Field::Content
            && self.server.copies.iter().any(|(copy, _)| *copy == value);
        let overwritten =
            self.server.overwritten.contains(&value) || self.server.overruled.contains(&value);
        let later = (self.values.iter().enumerate()).any(|(index, later)| {
            let tok = Tok(index as u8);
            let pred = match info.field {
                Field::Position => later.pos,
                _ => later.content,
            };
            let continues = later.author.is_some()
                && later.renamed_from.is_none()
                && pred == value
                && tok != value;
            let deleted_seen = later.field == Field::Existence
                && (later.seen[slot(info.field)] == value
                    || later.held[slot(info.field)] & (1 << value.0) != 0);
            (later.renamed_from == Some(value) && self.survived(tok))
                || (continues && later.field == Field::Existence)
                || deleted_seen
                || (continues && later.field == info.field && self.survived(tok))
        });
        held || copied || overwritten || later
    }

    fn lost_writes(&self, rules: Rules) -> Option<Broken> {
        let lost = (self.values.iter().enumerate())
            .filter(|(_, value)| {
                value.author.is_some()
                    && value.field != Field::Existence
                    && value.renamed_from.is_none()
            })
            .find(|(index, _)| !self.survived(Tok(*index as u8)))
            .map(|(index, value)| {
                format!(
                    "device {:?}'s {:?} {:?} is lost",
                    value.author.unwrap(),
                    value.field,
                    Tok(index as u8)
                )
            });
        static COPIES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let copies = *COPIES.get_or_init(|| std::env::var("MODEL_COPIES").is_ok());
        lost.or_else(|| copies.then(|| self.spurious_copy()).flatten())
            .or_else(|| {
                if rules.old_server() {
                    None
                } else {
                    self.undone_delete(rules)
                }
            })
    }

    fn spurious_copy(&self) -> Option<Broken> {
        let follows = |later: Tok, earlier: Tok| {
            let mut at = later;
            for _ in 0..self.values.len() {
                let value = self.values[at.0 as usize];
                if value.author.is_none() {
                    return false;
                }
                at = value.content;
                if at == earlier {
                    return true;
                }
            }
            false
        };
        let (copy, _) = self.server.copies.iter().find(|&&(copy, _)| {
            let author = self.values[copy.0 as usize].author;
            !(0..self.values.len()).any(|index| {
                let other = Tok(index as u8);
                let value = self.values[index];
                value.field == Field::Content
                    && value.author.is_some()
                    && value.author != author
                    && value.renamed_from.is_none()
                    && !follows(other, copy)
                    && !follows(copy, other)
            })
        })?;
        Some(format!(
            "the conflict copy of {copy:?} has no concurrent change by another user"
        ))
    }

    fn follows(&self, later: Tok, earlier: Tok) -> bool {
        let field = self.values[later.0 as usize].field;
        let mut at = later;
        for _ in 0..self.values.len() {
            let value = self.values[at.0 as usize];
            if value.author.is_none() || value.renamed_from.is_some() {
                return false;
            }
            at = match field {
                Field::Position => value.pos,
                _ => value.content,
            };
            if at == earlier {
                return true;
            }
        }
        false
    }

    fn undone_delete(&self, rules: Rules) -> Option<Broken> {
        let node = self.server.node();
        if node.trashed.is_some() || !node.created {
            return None;
        }
        let tokens = (0..self.values.len()).map(|index| Tok(index as u8));
        let mut deletes = tokens.clone().filter(|&gone| {
            let delete = self.values[gone.0 as usize];
            let old_wire = rules == Rules::OldDeleter && delete.author == Some(1);
            !old_wire
                && delete.field == Field::Existence
                && delete.author.is_some()
                && !self
                    .server
                    .log
                    .iter()
                    .any(|record| record.node.trashed == Some(gone))
        });
        deletes
            .find(|&gone| {
                let delete = self.values[gone.0 as usize];
                !self.server.restored.contains(&gone)
                    && !tokens.clone().any(|tok| {
                        let value = self.values[tok.0 as usize];
                        value.author.is_some()
                            && value.author != delete.author
                            && value.field != Field::Existence
                            && delete.seen[slot(value.field)] != tok
                            && !self.follows(delete.seen[slot(value.field)], tok)
                            && self.survived(tok)
                    })
            })
            .map(|gone| format!("the delete {gone:?} came undone with no other user's change"))
    }
}

pub struct Report {
    pub states: usize,
    pub converged: usize,
    pub spurious: usize,
    pub broken: Option<(Broken, Vec<Action>)>,
    pub capped: bool,
    pub steps: std::collections::BTreeSet<(Field, String, &'static str, String)>,
}

impl World {
    fn attribution(&self, d: Dev, field: Field) -> String {
        let dev = &self.devices[d];
        let carried = (dev.flight.iter().flat_map(|flight| &flight.ops)).any(|(op, _)| match op {
            Op::Delete { .. } => field == Field::Existence,
            Op::Move { .. } => field == Field::Position,
            Op::Update { .. } => field == Field::Content,
            Op::Copy { .. } => false,
            Op::Create { .. } => field == Field::Existence,
        });
        let author = match dev.unsent.get(field) {
            None => "agreed",
            Some(Author::Own) => "own unsent",
            Some(Author::Other) => "resent",
        };
        if carried {
            format!("{author}, op in flight")
        } else {
            author.to_owned()
        }
    }
}

fn label(action: Action, d: Dev) -> Option<&'static str> {
    Some(match action {
        Action::Write(at) | Action::Move(at) if at == d => "user edit or move",
        Action::Delete(at) if at == d => "user delete",
        Action::Build(at, _) if at == d => "op sent",
        Action::Answer(at) if at == d => "result arrives",
        Action::Record(at) if at == d => "record arrives",
        Action::Bootstrap(at) if at == d => "restore found",
        _ => return None,
    })
}

impl World {
    fn canonical_hash(&self, swap: bool) -> u64 {
        let dev = |d: Option<Dev>| d.map(|d| if swap { 1 - d } else { d });
        let at = |d: Dev| if swap { 1 - d } else { d };
        let count = self.values.len();
        let mut order = [0u8; 32];
        for (index, slot) in order.iter_mut().enumerate().take(count) {
            *slot = index as u8;
        }
        order[..count].sort_by_key(|&old| {
            let value = &self.values[old as usize];
            (value.renamed_from.is_some(), dev(value.author))
        });
        let mut toks = [0u8; 32];
        for (new, &old) in order[..count].iter().enumerate() {
            toks[old as usize] = new as u8;
        }
        let tok = |t: Tok| {
            if t == UNKNOWN {
                t
            } else {
                Tok(toks[t.0 as usize])
            }
        };
        let set = |held: u32| {
            (0..count)
                .filter(|&old| held & (1 << old) != 0)
                .fold(0u32, |acc, old| acc | (1 << toks[old]))
        };
        let mut commits = [u8::MAX; 256];
        let mut sources = [0u8; 256];
        let mut noted = 0usize;
        let mut note = |token: u8| {
            if commits[token as usize] == u8::MAX {
                commits[token as usize] = noted as u8;
                sources[noted] = token;
                noted += 1;
            }
        };
        let backup = self.server.before_loss.iter().flat_map(|(log, _)| log);
        for record in self.server.log.iter().chain(backup) {
            note(record.token);
        }
        for d in 0..2 {
            let device = &self.devices[at(d)];
            device
                .seen
                .iter()
                .chain(&device.earlier)
                .for_each(|&token| note(token));
            if let Some(flight) = &device.flight {
                note(flight.base.1);
                if let Phase::Answered(Answer::Done { token, .. })
                | Phase::Arrived(Answer::Done { token, .. }) = &flight.phase
                {
                    note(*token);
                }
            }
        }
        let commit = |token: u8| commits[token as usize];
        let node = |n: &Node| Node {
            trashed: n.trashed.map(tok),
            pos: tok(n.pos),
            content: tok(n.content),
            meta_editor: dev(n.meta_editor),
            editor: dev(n.editor),
            ..*n
        };
        let toks_set = |list: &[Tok]| list.iter().fold(0u32, |acc, &t| acc | (1 << tok(t).0));
        let mut h = std::hash::DefaultHasher::new();
        let budget = &self.budget;
        for pair in [
            budget.writes,
            budget.moves,
            budget.deletes,
            budget.trash_restores,
        ] {
            (pair[at(0)], pair[at(1)]).hash(&mut h);
        }
        (budget.renames, budget.losses, dev(budget.created_by), count).hash(&mut h);
        for &old in &order[..count] {
            let value = &self.values[old as usize];
            Value {
                author: dev(value.author),
                pos: tok(value.pos),
                content: tok(value.content),
                seen: value.seen.map(tok),
                held: value.held.map(set),
                renamed_from: value.renamed_from.map(tok),
                ..*value
            }
            .hash(&mut h);
        }
        let log = |log: &[Record], h: &mut std::hash::DefaultHasher| {
            log.len().hash(h);
            for record in log {
                (record.seq, commit(record.token), node(&record.node)).hash(h);
            }
        };
        let copies = |copies: &[(Tok, Seq)], h: &mut std::hash::DefaultHasher| {
            let mut sorted: Vec<(Tok, Seq)> =
                copies.iter().map(|&(copy, seq)| (tok(copy), seq)).collect();
            sorted.sort();
            sorted.hash(h);
        };
        let server = &self.server;
        log(&server.log, &mut h);
        copies(&server.copies, &mut h);
        (
            toks_set(&server.overwritten),
            toks_set(&server.restored),
            toks_set(&server.overruled),
            noted,
        )
            .hash(&mut h);
        for &old in &sources[..noted] {
            dev(server.authors[old as usize]).hash(&mut h);
        }
        server.before_loss.is_some().hash(&mut h);
        if let Some((backup, kept)) = &server.before_loss {
            log(backup, &mut h);
            copies(kept, &mut h);
        }
        for d in 0..2 {
            let device = &self.devices[at(d)];
            match device.l {
                Local::Live { pos, content } => Local::Live {
                    pos: tok(pos),
                    content: tok(content),
                },
                Local::Gone(gone) => Local::Gone(tok(gone)),
            }
            .hash(&mut h);
            Synced {
                pos: Agreed {
                    val: tok(device.s.pos.val),
                    ..device.s.pos
                },
                content: Agreed {
                    val: tok(device.s.content.val),
                    ..device.s.content
                },
                ..device.s
            }
            .hash(&mut h);
            node(&device.r).hash(&mut h);
            for tokens in [&device.seen, &device.earlier] {
                tokens.len().hash(&mut h);
                tokens.iter().for_each(|&token| commit(token).hash(&mut h));
            }
            for history in &device.history {
                history.len().hash(&mut h);
                for &(value, seq) in history {
                    (tok(value), seq).hash(&mut h);
                }
            }
            (
                device.held.map(set),
                device.unsent,
                device.by_user,
                device.flight.is_some(),
            )
                .hash(&mut h);
            if let Some(flight) = &device.flight {
                (flight.base.0, commit(flight.base.1), flight.require_head).hash(&mut h);
                flight.ops.len().hash(&mut h);
                for &(op, author) in &flight.ops {
                    let op = match op {
                        Op::Delete { base, gone } => Op::Delete {
                            base,
                            gone: tok(gone),
                        },
                        Op::Move { to, from } => Op::Move {
                            to: tok(to),
                            from: from.map(tok),
                        },
                        Op::Update {
                            content,
                            resend,
                            base,
                        } => Op::Update {
                            content: tok(content),
                            resend,
                            base,
                        },
                        Op::Copy { content } => Op::Copy {
                            content: tok(content),
                        },
                        Op::Create { pos, content } => Op::Create {
                            pos: tok(pos),
                            content: tok(content),
                        },
                    };
                    (op, author).hash(&mut h);
                }
                flight.by_user.hash(&mut h);
                let answer = match &flight.phase {
                    Phase::Sent => {
                        0u8.hash(&mut h);
                        None
                    }
                    Phase::Answered(answer) => {
                        1u8.hash(&mut h);
                        Some(answer)
                    }
                    Phase::Arrived(answer) => {
                        2u8.hash(&mut h);
                        Some(answer)
                    }
                };
                match answer {
                    None => {}
                    Some(Answer::Ahead) => 0u8.hash(&mut h),
                    Some(Answer::HeadMoved) => 1u8.hash(&mut h),
                    Some(Answer::Done { seq, token, codes }) => {
                        (2u8, *seq, commit(*token), codes.len()).hash(&mut h);
                        for &code in codes {
                            match code {
                                Code::Renamed(name) => Code::Renamed(tok(name)),
                                code => code,
                            }
                            .hash(&mut h);
                        }
                    }
                }
            }
            device.copies.len().hash(&mut h);
            for &(copy, landed) in &device.copies {
                (tok(copy), landed).hash(&mut h);
            }
            device.lost_position.map(tok).hash(&mut h);
        }
        h.finish()
    }
}

fn fingerprint(world: &World, rules: Rules) -> u64 {
    let canonical = world.canonical_hash(false);
    if rules == Rules::OldDeleter {
        return canonical;
    }
    let least = canonical.min(world.canonical_hash(true));
    let mixed = (least ^ (least >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

pub fn check(budget: Budget, rules: Rules) -> Report {
    let start = World::new(budget);
    let mut seen: HashMap<u64, u32> = HashMap::new();
    let mut parents: Vec<(u32, Option<Action>)> = vec![(0, None)];
    let mut queue = VecDeque::new();
    seen.insert(fingerprint(&start, rules), 0);
    queue.push_back((start, 0u32));
    let mut converged = 0;
    let mut spurious = 0;
    let mut steps = std::collections::BTreeSet::new();
    let trace = |parents: &Vec<(u32, Option<Action>)>, mut at: u32, last: Option<Action>| {
        let mut actions: Vec<Action> = last.into_iter().collect();
        while let (parent, Some(action)) = parents[at as usize] {
            actions.push(action);
            at = parent;
        }
        actions.reverse();
        actions
    };
    while let Some((world, index)) = queue.pop_front() {
        let actions = world.actions();
        let settled = world.converged();
        if settled {
            converged += 1;
            spurious += usize::from(world.spurious_copy().is_some());
        }
        if settled || actions.is_empty() {
            let broken = if settled {
                world.lost_writes(rules)
            } else {
                Some("the devices never converge".to_owned())
            };
            if let Some(broken) = broken {
                return Report {
                    states: seen.len(),
                    converged,
                    spurious,
                    broken: Some((broken, trace(&parents, index, None))),
                    capped: false,
                    steps,
                };
            }
        }
        for action in actions {
            match world.step(action, rules) {
                Err(broken) => {
                    return Report {
                        states: seen.len(),
                        converged,
                        spurious,
                        broken: Some((broken, trace(&parents, index, Some(action)))),
                        capped: false,
                        steps,
                    };
                }
                Ok(next) => {
                    for d in 0..2 {
                        let Some(label) = label(action, d) else {
                            continue;
                        };
                        for field in [Field::Position, Field::Content, Field::Existence] {
                            let (before, after) =
                                (world.attribution(d, field), next.attribution(d, field));
                            if before != after {
                                let why = format!("{field:?}: {before} --{label}--> {after}");
                                if std::env::var("MODEL_WHY").is_ok_and(|wanted| wanted == why)
                                    && !steps.contains(&(
                                        field,
                                        before.clone(),
                                        label,
                                        after.clone(),
                                    ))
                                {
                                    println!("{why}: {:?}", trace(&parents, index, Some(action)));
                                }
                                steps.insert((field, before, label, after));
                            }
                        }
                    }
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        seen.entry(fingerprint(&next, rules))
                    {
                        let at = parents.len() as u32;
                        parents.push((index, Some(action)));
                        entry.insert(at);
                        queue.push_back((next, at));
                    }
                }
            }
        }
    }
    Report {
        states: seen.len(),
        converged,
        spurious,
        broken: None,
        capped: false,
        steps,
    }
}

struct Fingerprints {
    slots: Vec<u64>,
    len: usize,
    fixed: bool,
}

impl Fingerprints {
    fn new() -> Self {
        let fixed = std::env::var("MODEL_TABLE_SLOTS").ok();
        let slots = fixed
            .as_ref()
            .map_or(1 << 20, |slots| slots.parse().unwrap());
        Fingerprints {
            slots: vec![0; slots],
            len: 0,
            fixed: fixed.is_some(),
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn full(&self) -> bool {
        self.fixed && self.len * 20 >= self.slots.len() * 19
    }

    fn insert(&mut self, fingerprint: u64) -> bool {
        if !self.fixed && self.len * 4 >= self.slots.len() * 3 {
            let grown = vec![0; self.slots.len() * 2];
            let old = std::mem::replace(&mut self.slots, grown);
            self.len = 0;
            for fingerprint in old.into_iter().filter(|&slot| slot != 0) {
                self.insert(fingerprint);
            }
        }
        let fingerprint = fingerprint.max(1);
        let size = self.slots.len();
        let mut at = ((u128::from(fingerprint) * size as u128) >> 64) as usize;
        loop {
            match self.slots[at] {
                0 => {
                    self.slots[at] = fingerprint;
                    self.len += 1;
                    if self.len.is_multiple_of(100_000_000) {
                        println!("{} states", self.len);
                    }
                    return true;
                }
                slot if slot == fingerprint => return false,
                _ => at = (at + 1) % size,
            }
        }
    }
}

struct Frame {
    world: World,
    print: u64,
    todo: Vec<Action>,
    deferred: Vec<Action>,
}

impl World {
    fn ample(&self, actions: Vec<Action>) -> (Vec<Action>, Vec<Action>) {
        let owner = |action: &Action| match *action {
            Action::Write(d)
            | Action::Move(d)
            | Action::Delete(d)
            | Action::Build(d, _)
            | Action::Serve(d)
            | Action::ServeRenamed(d)
            | Action::Answer(d)
            | Action::Record(d)
            | Action::Bootstrap(d) => Some(d),
            Action::TrashRestore(_) | Action::Lose(_) | Action::Unlose => None,
        };
        let local = |action: &Action| {
            matches!(
                action,
                Action::Write(_)
                    | Action::Move(_)
                    | Action::Delete(_)
                    | Action::Answer(_)
                    | Action::Record(_)
            )
        };
        if self.budget.losses == 0 {
            for d in 0..2 {
                let (mine, rest): (Vec<Action>, Vec<Action>) =
                    actions.iter().partition(|action| owner(action) == Some(d));
                if mine.iter().all(local)
                    && mine
                        .iter()
                        .any(|action| matches!(action, Action::Record(_)))
                {
                    return (mine, rest);
                }
            }
        }
        (actions, Vec::new())
    }
}

pub fn check_deep(budget: Budget, rules: Rules) -> Report {
    let start = World::new(budget);
    let mut seen = Fingerprints::new();
    let print = fingerprint(&start, rules);
    seen.insert(print);
    let mut path: Vec<Action> = Vec::new();
    let (todo, deferred) = start.ample(start.actions());
    let mut stack = vec![Frame {
        world: start.clone(),
        print,
        todo,
        deferred,
    }];
    let mut converged = 0;
    let mut spurious = 0;
    let steps = std::collections::BTreeSet::new();
    let cap: usize =
        std::env::var("MODEL_MAX_STATES").map_or(usize::MAX, |cap| cap.parse().unwrap());
    let report = |seen: &Fingerprints, (converged, spurious), broken, steps| Report {
        states: seen.len(),
        converged,
        spurious,
        broken,
        capped: seen.len() >= cap || seen.full(),
        steps,
    };
    if start.converged() {
        converged += 1;
    }
    while let Some(frame) = stack.last_mut() {
        let Some(action) = frame.todo.pop() else {
            stack.pop();
            path.pop();
            continue;
        };
        let next = match frame.world.step(action, rules) {
            Ok(next) => next,
            Err(broken) => {
                path.push(action);
                return report(
                    &seen,
                    (converged, spurious),
                    Some((broken, path.clone())),
                    steps,
                );
            }
        };
        let print = fingerprint(&next, rules);
        if !seen.insert(print) {
            if stack.iter().any(|frame| frame.print == print) {
                let frame = stack.last_mut().unwrap();
                let deferred = std::mem::take(&mut frame.deferred);
                frame.todo.extend(deferred);
            }
            continue;
        }
        if seen.len() >= cap || seen.full() {
            return report(&seen, (converged, spurious), None, steps);
        }
        path.push(action);
        let actions = next.actions();
        let settled = next.converged();
        if settled {
            converged += 1;
            spurious += usize::from(next.spurious_copy().is_some());
        }
        if settled || actions.is_empty() {
            let broken = if settled {
                next.lost_writes(rules)
            } else {
                Some("the devices never converge".to_owned())
            };
            if let Some(broken) = broken {
                return report(
                    &seen,
                    (converged, spurious),
                    Some((broken, path.clone())),
                    steps,
                );
            }
        }
        let (todo, deferred) = next.ample(actions);
        stack.push(Frame {
            world: next,
            print,
            todo,
            deferred,
        });
    }
    report(&seen, (converged, spurious), None, steps)
}

pub fn show(budget: Budget, rules: Rules, actions: &[Action]) {
    let mut world = World::new(budget);
    for action in actions {
        match world.step(*action, rules) {
            Ok(next) => world = next,
            Err(broken) => {
                println!("{action:?}: {broken}");
                return;
            }
        }
        println!(
            "{action:?}\n  server {:?}\n  d0 l={:?} s={:?} unsent={:?}\n  d1 l={:?} s={:?} unsent={:?}",
            world.server.node(),
            world.devices[0].l,
            world.devices[0].s,
            world.devices[0].unsent,
            world.devices[1].l,
            world.devices[1].s,
            world.devices[1].unsent
        );
    }
}

pub fn settle(budget: Budget, rules: Rules, actions: &[Action]) -> Result<Vec<Action>, Broken> {
    let mut world = World::new(budget);
    for action in actions {
        if !world.actions().contains(action) {
            return Err(format!(
                "{action:?} cannot happen, only {:?}",
                world.actions()
            ));
        }
        world = world.step(*action, rules)?;
    }
    let mut tail = Vec::new();
    while !world.converged() {
        let available = world.actions();
        let next = (0..2)
            .flat_map(|d| {
                [
                    Action::Record(d),
                    Action::Answer(d),
                    Action::Serve(d),
                    Action::Bootstrap(d),
                    Action::Build(d, 2),
                    Action::Build(d, 1),
                ]
            })
            .find(|action| available.contains(action) && tail.len() < 64);
        let Some(action) = next else {
            return Err(format!("the devices never converge after {tail:?}"));
        };
        world = world.step(action, rules)?;
        tail.push(action);
    }
    world.lost_writes(rules).map_or(Ok(tail), Err)
}
