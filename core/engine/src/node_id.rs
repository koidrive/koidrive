// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, RwLock};

/// The id of a node. A lowercase UUID, the only form Koidrive and the server create, is held as its
/// 16 bytes; any other text is interned for the life of the process. Ids order as their text does.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NodeId(Repr);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Repr {
    Uuid([u8; 16]),
    Text([u8; 4]),
}

#[derive(Default)]
struct Texts {
    index: HashMap<&'static str, u32>,
    texts: Vec<&'static str>,
}

static TEXTS: LazyLock<RwLock<Texts>> = LazyLock::new(RwLock::default);

const UUID_LEN: usize = 36;

fn is_dash(at: usize) -> bool {
    matches!(at, 8 | 13 | 18 | 23)
}

fn uuid_bytes(text: &str) -> Option<[u8; 16]> {
    let text = text.as_bytes();
    if text.len() != UUID_LEN {
        return None;
    }
    let mut bytes = [0; 16];
    let mut digits = 0;
    for (at, &c) in text.iter().enumerate() {
        if is_dash(at) {
            if c != b'-' {
                return None;
            }
            continue;
        }
        let nibble = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return None,
        };
        bytes[digits / 2] |= nibble << (4 * (1 - digits % 2));
        digits += 1;
    }
    Some(bytes)
}

fn intern(text: &str) -> [u8; 4] {
    if let Some(&at) = TEXTS.read().unwrap().index.get(text) {
        return at.to_ne_bytes();
    }
    let mut texts = TEXTS.write().unwrap();
    if let Some(&at) = texts.index.get(text) {
        return at.to_ne_bytes();
    }
    let at = u32::try_from(texts.texts.len()).expect("fewer than 2^32 ids that are not UUIDs");
    let text: &'static str = Box::leak(text.into());
    texts.texts.push(text);
    texts.index.insert(text, at);
    at.to_ne_bytes()
}

fn interned(at: [u8; 4]) -> &'static str {
    TEXTS.read().unwrap().texts[u32::from_ne_bytes(at) as usize]
}

struct Uuid([u8; UUID_LEN]);

impl Uuid {
    fn new(bytes: &[u8; 16]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = [b'-'; UUID_LEN];
        let mut digits = (0..UUID_LEN).filter(|&at| !is_dash(at));
        for byte in bytes {
            for nibble in [byte >> 4, byte & 0xf] {
                text[digits.next().unwrap()] = HEX[usize::from(nibble)];
            }
        }
        Self(text)
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap()
    }
}

impl NodeId {
    fn with_str<T>(&self, use_text: impl FnOnce(&str) -> T) -> T {
        match self.0 {
            Repr::Uuid(bytes) => use_text(Uuid::new(&bytes).as_str()),
            Repr::Text(at) => use_text(interned(at)),
        }
    }
}

impl From<&str> for NodeId {
    fn from(text: &str) -> Self {
        Self(match uuid_bytes(text) {
            Some(bytes) => Repr::Uuid(bytes),
            None => Repr::Text(intern(text)),
        })
    }
}

impl From<String> for NodeId {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

impl From<&String> for NodeId {
    fn from(text: &String) -> Self {
        Self::from(text.as_str())
    }
}

impl From<&NodeId> for NodeId {
    fn from(id: &NodeId) -> Self {
        id.clone()
    }
}

impl From<&NodeId> for String {
    fn from(id: &NodeId) -> Self {
        id.to_string()
    }
}

impl From<NodeId> for String {
    fn from(id: NodeId) -> Self {
        id.to_string()
    }
}

impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.0, other.0) {
            (Repr::Uuid(a), Repr::Uuid(b)) => a.cmp(&b),
            _ => self.with_str(|a| other.with_str(|b| a.cmp(b))),
        }
    }
}

impl PartialOrd for NodeId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<str> for NodeId {
    fn eq(&self, other: &str) -> bool {
        self.with_str(|text| text == other)
    }
}

impl PartialEq<&str> for NodeId {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl PartialEq<String> for NodeId {
    fn eq(&self, other: &String) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<NodeId> for str {
    fn eq(&self, other: &NodeId) -> bool {
        other == self
    }
}

impl PartialEq<NodeId> for &str {
    fn eq(&self, other: &NodeId) -> bool {
        other == *self
    }
}

impl PartialEq<NodeId> for String {
    fn eq(&self, other: &NodeId) -> bool {
        other == self.as_str()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with_str(|text| f.write_str(text))
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with_str(|text| fmt::Debug::fmt(text, f))
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for NodeId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.with_str(|text| serializer.serialize_str(text))
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for NodeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_text_of_every_id() {
        for text in [
            "0190f3a0-1234-7fff-bfff-ffffffffffff",
            "00000000-0000-0000-0000-000000000000",
            "0190F3A0-1234-7FFF-BFFF-FFFFFFFFFFFF",
            "0190f3a0-1234-7fff-bfff-fffffffffff",
            "0190f3a0+1234-7fff-bfff-ffffffffffff",
            "root",
            "",
        ] {
            let id = NodeId::from(text);
            assert_eq!(id.to_string(), text);
            assert_eq!(format!("{id:?}"), format!("{text:?}"));
            assert_eq!(id, NodeId::from(text.to_owned()));
        }
    }

    #[test]
    fn orders_ids_as_their_text() {
        let texts = [
            "",
            "0",
            "0190f3a0-1234-7fff-bfff-ffffffffffff",
            "0190f3a0-1234-7fff-bfff-ffffffffffff0",
            "0190f3a0-1235-7000-8000-000000000000",
            "0190f3a0-1235-7000-8000-00000000000a",
            "0190F3A0-1234-7FFF-BFFF-FFFFFFFFFFFF",
            "a",
            "ab",
            "b",
            "f0000000-0000-7000-8000-000000000000",
            "root",
        ];
        for a in texts {
            for b in texts {
                let (x, y) = (NodeId::from(a), NodeId::from(b));
                assert_eq!(x.cmp(&y), a.cmp(b), "{a} against {b}");
                assert_eq!(x == y, a == b, "{a} against {b}");
            }
        }
    }
}
