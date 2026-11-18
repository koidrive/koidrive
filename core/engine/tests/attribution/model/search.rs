use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use super::invariants::{Report, label};
use super::rules::Rules;
use super::{
    Action, Agreed, Answer, Broken, Budget, Code, Dev, Field, Local, Node, Op, Phase, Record, Seq,
    Synced, Tok, UNKNOWN, Value, World,
};

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
                lost_position: value.lost_position.map(tok),
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
            "{action:?}\n  server {:?} copies {:?}\n  d0 l={:?} s={:?} unsent={:?}\n  d1 l={:?} s={:?} unsent={:?}",
            world.server.node(),
            world.server.copies,
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
