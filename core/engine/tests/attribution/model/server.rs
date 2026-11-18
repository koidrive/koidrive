use super::rules::{Fix, Rules};
use super::{
    Answer, Broken, Code, Dev, Field, Local, Node, Op, Record, Seq, Tok, Value, World, slot,
};

impl World {
    pub(super) fn append(&mut self, node: Node, by: Option<Dev>) -> Seq {
        let seq = self.server.head() + 1;
        let token = self.server.tokens;
        self.server.tokens += 1;
        self.server.log.push(Record { seq, token, node });
        self.server.authors.push(by);
        seq
    }

    pub(super) fn serve(&mut self, d: Dev, renamed: bool, rules: Rules) -> Result<Answer, Broken> {
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
        let logged = (self.server.log.iter())
            .filter(|record| record.node.created)
            .fold(0u32, |logged, record| logged | 1 << record.node.content.0);
        let ancestors: Vec<u32> = (0..self.values.len())
            .map(|index| {
                let author = self.values[index].author;
                self.ancestors(Tok(index as u8), |at| {
                    logged & (1 << at.0) != 0 || self.values[at.0 as usize].author == author
                })
            })
            .collect();
        let authors: Vec<Option<Dev>> = self.values.iter().map(|value| value.author).collect();
        let saw_content = |edit: Tok, value: Tok| {
            rules.edits_see() && ancestors[edit.0 as usize] & (1 << value.0) != 0
        };
        let replaces_own = |edit: Tok, value: Tok| {
            rules.edits_see()
                && authors[edit.0 as usize].is_some()
                && authors[edit.0 as usize] == authors[value.0 as usize]
                && helds[edit.0 as usize][slot(Field::Content)] & (1 << value.0) != 0
        };
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
                        !rules.has(Fix::CreateFrom)
                            || self.values[value.0 as usize].author == Some(d)
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
                    if node.trashed.is_some() || saw_content(node.content, content) {
                        Code::Superseded
                    } else if node.content != content
                        && node.content_rev > base
                        && !saw_content(content, node.content)
                    {
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
                        if !rules.main_rules() {
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
                    } else if saw_content(node.content, content) {
                        Code::Superseded
                    } else if node.content_rev > base
                        && !saw_content(content, node.content)
                        && !replaces_own(content, node.content)
                    {
                        self.server.copies.push((content, seq));
                        Code::Copied
                    } else {
                        if replaces_own(content, node.content)
                            && !saw_content(content, node.content)
                        {
                            self.server.overwritten.push(node.content);
                        }
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

    pub(super) fn covers(&self, d: Dev, rules: Rules) -> bool {
        let dev = &self.devices[d];
        rules == Rules::TrashSettlesDelete
            || dev
                .r
                .trashed
                .is_some_and(|trash| self.trash_covers(d, trash, rules))
    }

    pub(super) fn trash_covers(&self, d: Dev, trash: Tok, rules: Rules) -> bool {
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
}
