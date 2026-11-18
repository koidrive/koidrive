use super::rules::{Fix, Rules};
use super::{
    Action, Agreed, Answer, Author, Broken, Budget, Dev, Device, FIELDS, Field, Flight, Local,
    Node, Op, Phase, Record, Seq, Server, Synced, Tok, UNKNOWN, Unsent, Value, World, old_wire,
    slot,
};

impl World {
    pub(super) fn new(budget: Budget) -> Self {
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
            lost_position: None,
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

    pub(super) fn caught_up(&self, d: Dev) -> bool {
        let dev = &self.devices[d];
        dev.seen.len() == self.server.log.len()
            && dev.head().1 == self.server.log.last().unwrap().token
    }

    pub(super) fn lost(&self, d: Dev) -> bool {
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

    pub(super) fn saw(&self, gone: Tok, rules: Rules) -> Option<[Tok; 2]> {
        let value = self.values[gone.0 as usize];
        let old = rules.old_server() || value.author.is_some_and(|d| old_wire(d, rules));
        (!old).then_some(value.seen)
    }

    pub(super) fn edited_here(&self, d: Dev, node: &Node, seen: &[u8], rules: Rules) -> [bool; 2] {
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

    fn carried(&self, edit: Tok) -> u32 {
        let author = self.values[edit.0 as usize].author;
        let mut carried = 0;
        let mut at = edit;
        for _ in 0..self.values.len() {
            let value = self.values[at.0 as usize];
            if value.author.is_none() || value.field != Field::Content || value.content == at {
                break;
            }
            carried |= 1 << value.content.0;
            if self.values[value.content.0 as usize].author != author {
                break;
            }
            at = value.content;
        }
        carried
    }

    pub(super) fn ancestors(&self, edit: Tok, known: impl Fn(Tok) -> bool) -> u32 {
        let mut ancestors = 0u32;
        let mut pending = vec![edit];
        while let Some(at) = pending.pop() {
            let carried = self.carried(at) & !ancestors;
            ancestors |= carried;
            pending.extend(
                (0..self.values.len() as u8)
                    .map(Tok)
                    .filter(|&tok| carried & (1 << tok.0) != 0 && known(tok)),
            );
        }
        ancestors
    }

    pub(super) fn author(&self, value: Tok) -> Option<Dev> {
        self.values[value.0 as usize].author
    }

    pub(super) fn mint(&mut self, value: Value) -> Tok {
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
                let held = w.devices[d].held;
                let value = w.mint(Value {
                    field,
                    author: Some(d),
                    pos,
                    content,
                    seen,
                    held,
                    renamed_from: None,
                    lost_position: None,
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
                    lost_position: (w.devices[d].flight.as_ref())
                        .filter(|_| rules.has(Fix::Carried))
                        .and_then(|flight| {
                            flight.ops.iter().find_map(|(op, _)| match op {
                                Op::Create { pos, .. } => Some(*pos),
                                _ => None,
                            })
                        })
                        .or(w.devices[d].lost_position)
                        .filter(|&lost| {
                            rules.has(Fix::LostPosition) && held[0] & (1 << lost.0) == 0
                        }),
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
                let trashes: Vec<Tok> = (w.server.log.iter().rev())
                    .map_while(|record| record.node.trashed)
                    .filter(|gone| !w.server.restored.contains(gone))
                    .collect();
                w.server.restored.extend(trashes);
                let mut node = w.server.node();
                node.trashed = None;
                node.meta_rev = seq;
                if !rules.main_rules() {
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
}
