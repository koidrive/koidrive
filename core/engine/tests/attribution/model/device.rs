use super::rules::{Fix, Rules};
use super::{
    Agreed, Answer, Author, Broken, Code, Dev, FIELDS, Field, Local, Node, Op, Phase, Seq, Synced,
    Tok, UNKNOWN, Unsent, World, old_wire, slot,
};

impl World {
    pub(super) fn prove(&mut self, d: Dev, rules: Rules) {
        let covered = self.covers(d, rules);
        let dev = &mut self.devices[d];
        let r = dev.r;
        if r.trashed.is_some() && rules.main_rules() {
            let settled = dev.flight.is_none() && covered;
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

    pub(super) fn settle(&mut self, d: Dev, rules: Rules) -> Result<(), Broken> {
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
                (Op::Delete { .. }, Code::Superseded) if rules.main_rules() => {}
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
        let held = self.devices[d].held[slot(Field::Content)];
        let ancestors: Vec<u32> = (0..self.values.len())
            .map(|index| {
                let author = self.values[index].author;
                self.ancestors(Tok(index as u8), |at| {
                    held & (1 << at.0) != 0 || self.values[at.0 as usize].author == author
                })
            })
            .collect();
        let authors: Vec<Option<Dev>> = self.values.iter().map(|value| value.author).collect();
        let saw = |edit: Tok, value: Tok| {
            rules.edits_see() && ancestors[edit.0 as usize] & (1 << value.0) != 0
        };
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
            let gone = self.values[gone.0 as usize];
            let unheld = gone.lost_position == Some(r.pos)
                || FIELDS.iter().any(|&field| {
                    r.user_set(field)
                        && !edited_here[slot(field)]
                        && gone.held[slot(field)] & (1 << r.get(field).0) == 0
                });
            if r.trashed.is_none()
                && rules.has(Fix::AbsentResent)
                && !(rules.has(Fix::Held) && unheld)
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
                    if content != r.content && saw(content, r.content) {
                        let made_here = authors[content.0 as usize] == Some(d);
                        dev.unsent.content.get_or_insert(if made_here {
                            Author::Own
                        } else {
                            Author::Other
                        });
                    } else if content != r.content {
                        if !saw(r.content, content) {
                            dev.copies.push((content, None));
                        }
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
                    if rules.has(Fix::Held) {
                        let gone = self.values[gone.0 as usize];
                        let lost = field == Field::Position && gone.lost_position == Some(r.pos);
                        return lost
                            || r.user_set(field)
                                && !edited_here[slot(field)]
                                && gone.held[slot(field)] & (1 << r.get(field).0) == 0;
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
                            if rules.main_rules() && {
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
                        Some(_) if field == Field::Content && saw(remote, local) => {
                            dev.take(field, remote);
                            dev.s.set(field, agreed);
                            dev.unsent.set(field, None);
                        }
                        Some(Author::Own)
                            if field == Field::Content
                                && (saw(local, remote)
                                    || (rules.edits_see()
                                        && authors[remote.0 as usize] == Some(d)
                                        && dev.held[slot(field)] & (1 << remote.0) != 0)) =>
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

    pub(super) fn bootstrap(&mut self, d: Dev, rules: Rules) -> Result<(), Broken> {
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
}
