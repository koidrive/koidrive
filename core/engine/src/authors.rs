// SPDX-License-Identifier: Apache-2.0

//! Who made each field of a node that L holds and the server may lack (docs/design/sync-engine/server-order.md
//! section 4.4).

/// A part of a node that changes on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Position,
    Content,
    Existence,
}

/// Who made a value L holds: this device's user, or another device whose value this device sends again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Author {
    Own,
    Other,
}

/// For each field whose value in L the server may lack, who made that value. A field is set when L takes the value
/// and never changes author while it stays set. It ends once the server holds L's value or L takes the server's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Unsent {
    pub position: Option<Author>,
    pub content: Option<Author>,
    pub existence: Option<Author>,
}

impl Unsent {
    pub fn get(&self, field: Field) -> Option<Author> {
        match field {
            Field::Position => self.position,
            Field::Content => self.content,
            Field::Existence => self.existence,
        }
    }

    pub fn set(&mut self, field: Field, author: Option<Author>) {
        match field {
            Field::Position => self.position = author,
            Field::Content => self.content = author,
            Field::Existence => self.existence = author,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether a field holds a change of this device's user, which beats a delete that did not see it.
    pub fn holds_own(&self) -> bool {
        [self.position, self.content, self.existence].contains(&Some(Author::Own))
    }

    /// Whether the field's value goes out as a resend: a `Move` with `from` or an `UpdateContent` with `resend`.
    pub fn resends(&self, field: Field) -> bool {
        self.get(field) == Some(Author::Other)
    }
}
