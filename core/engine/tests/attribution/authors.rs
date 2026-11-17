// SPDX-License-Identifier: Apache-2.0

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Position,
    Content,
    Existence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Author {
    Own,
    Other,
}

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

    pub fn holds_own(&self) -> bool {
        [self.position, self.content, self.existence].contains(&Some(Author::Own))
    }

    pub fn resends(&self, field: Field) -> bool {
        self.get(field) == Some(Author::Other)
    }
}
