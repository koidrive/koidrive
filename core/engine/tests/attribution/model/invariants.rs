use super::rules::Rules;
use super::{Action, Author, Broken, Dev, FIELDS, Field, Local, Op, Tok, World, slot};

impl World {
    pub(super) fn check(&self) -> Result<(), Broken> {
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

    pub(super) fn converged(&self) -> bool {
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

    pub(super) fn lost_writes(&self, rules: Rules) -> Option<Broken> {
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
        lost.or_else(|| {
            rules
                .checks_copies()
                .then(|| self.spurious_copy())
                .flatten()
        })
        .or_else(|| {
            if rules.old_server() {
                None
            } else {
                self.undone_delete(rules)
            }
        })
    }

    pub(super) fn spurious_copy(&self) -> Option<Broken> {
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
            !old_wire && delete.field == Field::Existence && delete.author.is_some()
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
    pub(super) fn attribution(&self, d: Dev, field: Field) -> String {
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

pub(super) fn label(action: Action, d: Dev) -> Option<&'static str> {
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
