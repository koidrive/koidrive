// SPDX-License-Identifier: Apache-2.0

//! Collections that remember what changed since their changes were last taken, so that a
//! checkpoint writes the rows of what changed and nothing else. Each one reads like the collection
//! it wraps and mutates only through methods that record the key they touch, with the value it
//! held at the last take. Taking the changes returns the keys whose value differs from that one,
//! so a change undone before the take is no change. Equality, `Debug` and `Default` see the
//! contents only, and a value built `From` its contents has no changes.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Deref;

/// A [`BTreeSet`] that records the keys whose membership changed.
#[derive(Clone)]
pub struct TrackedSet<K> {
    set: BTreeSet<K>,
    before: BTreeMap<K, bool>,
}

impl<K: Ord + Clone> TrackedSet<K> {
    fn touch(before: &mut BTreeMap<K, bool>, key: &K, held: bool) {
        if !before.contains_key(key) {
            before.insert(key.clone(), held);
        }
    }

    pub fn insert(&mut self, key: K) -> bool {
        if self.set.contains(&key) {
            return false;
        }
        Self::touch(&mut self.before, &key, false);
        self.set.insert(key)
    }

    pub fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let Some(key) = self.set.take(key) else {
            return false;
        };
        self.before.entry(key).or_insert(true);
        true
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        let before = &mut self.before;
        self.set.retain(|key| {
            let kept = keep(key);
            if !kept {
                Self::touch(before, key, true);
            }
            kept
        });
    }

    pub fn clear(&mut self) {
        for key in std::mem::take(&mut self.set) {
            self.before.entry(key).or_insert(true);
        }
    }

    /// Every key whose membership changed since the last take, with whether the set holds it now.
    pub fn take_changes(&mut self) -> Vec<(K, bool)> {
        (std::mem::take(&mut self.before).into_iter())
            .filter_map(|(key, held)| {
                let holds = self.set.contains(&key);
                (holds != held).then_some((key, holds))
            })
            .collect()
    }
}

impl<K: Ord + Clone> Extend<K> for TrackedSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, keys: I) {
        for key in keys {
            self.insert(key);
        }
    }
}

impl<K> Deref for TrackedSet<K> {
    type Target = BTreeSet<K>;

    fn deref(&self) -> &BTreeSet<K> {
        &self.set
    }
}

impl<K> From<BTreeSet<K>> for TrackedSet<K> {
    fn from(set: BTreeSet<K>) -> Self {
        Self {
            set,
            before: BTreeMap::new(),
        }
    }
}

impl<K> Default for TrackedSet<K> {
    fn default() -> Self {
        BTreeSet::new().into()
    }
}

impl<K: PartialEq> PartialEq for TrackedSet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.set == other.set
    }
}

impl<K: fmt::Debug> fmt::Debug for TrackedSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.set.fmt(f)
    }
}

/// A [`BTreeMap`] that records the keys whose value changed.
#[derive(Clone)]
pub struct TrackedMap<K, V> {
    map: BTreeMap<K, V>,
    before: BTreeMap<K, Option<V>>,
}

impl<K: Ord + Clone, V: Clone + PartialEq> TrackedMap<K, V> {
    fn touch(&mut self, key: &K) {
        if !self.before.contains_key(key) {
            let held = self.map.get(key).cloned();
            self.before.insert(key.clone(), held);
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if self.map.get(&key) == Some(&value) {
            return Some(value);
        }
        self.touch(&key);
        self.map.insert(key, value)
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (key, value) = self.map.remove_entry(key)?;
        self.before
            .entry(key)
            .or_insert_with(|| Some(value.clone()));
        Some(value)
    }

    /// The value of `key` to change in place. It is a change once it differs from the value the
    /// key held at the last take.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (held, _) = self.map.get_key_value(key)?;
        let held = held.clone();
        self.touch(&held);
        self.map.get_mut(key)
    }

    /// The value of `key` to change in place, as [`TrackedMap::get_mut`] gives it, inserted with
    /// `value` first when the map does not hold the key.
    pub fn get_or_insert_with(&mut self, key: K, value: impl FnOnce() -> V) -> &mut V {
        self.touch(&key);
        self.map.entry(key).or_insert_with(value)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        for (key, value) in &self.map {
            if !self.before.contains_key(key) {
                self.before.insert(key.clone(), Some(value.clone()));
            }
        }
        self.map.values_mut()
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let before = &mut self.before;
        self.map.retain(|key, value| {
            let kept = keep(key, value);
            if !kept && !before.contains_key(key) {
                before.insert(key.clone(), Some(value.clone()));
            }
            kept
        });
    }

    pub fn pop_first(&mut self) -> Option<(K, V)> {
        let (key, value) = self.map.pop_first()?;
        if !self.before.contains_key(&key) {
            self.before.insert(key.clone(), Some(value.clone()));
        }
        Some((key, value))
    }

    /// Removes every entry and returns them.
    pub fn take_all(&mut self) -> BTreeMap<K, V> {
        let map = std::mem::take(&mut self.map);
        for (key, value) in &map {
            if !self.before.contains_key(key) {
                self.before.insert(key.clone(), Some(value.clone()));
            }
        }
        map
    }

    pub fn clear(&mut self) {
        self.take_all();
    }

    /// Every key whose value changed since the last take, with its value now, or `None` when the
    /// map no longer holds it.
    pub fn take_changes(&mut self) -> Vec<(K, Option<V>)> {
        (std::mem::take(&mut self.before).into_iter())
            .filter_map(|(key, held)| {
                let value = self.map.get(&key);
                (value != held.as_ref()).then(|| (key, value.cloned()))
            })
            .collect()
    }
}

impl<K: Ord + Clone, V: Clone + PartialEq> Extend<(K, V)> for TrackedMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, entries: I) {
        for (key, value) in entries {
            self.insert(key, value);
        }
    }
}

impl<K, V> Deref for TrackedMap<K, V> {
    type Target = BTreeMap<K, V>;

    fn deref(&self) -> &BTreeMap<K, V> {
        &self.map
    }
}

impl<K, V> From<BTreeMap<K, V>> for TrackedMap<K, V> {
    fn from(map: BTreeMap<K, V>) -> Self {
        Self {
            map,
            before: BTreeMap::new(),
        }
    }
}

impl<K, V> Default for TrackedMap<K, V> {
    fn default() -> Self {
        BTreeMap::new().into()
    }
}

impl<K: PartialEq, V: PartialEq> PartialEq for TrackedMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for TrackedMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.map.fmt(f)
    }
}

/// A value the store keeps whole, which records whether it changed.
#[derive(Clone)]
pub struct Marked<T> {
    value: T,
    before: Option<T>,
}

impl<T: Clone + PartialEq> Marked<T> {
    /// The value to change in place. It is a change once it differs from the value at the last
    /// take.
    pub fn get_mut(&mut self) -> &mut T {
        if self.before.is_none() {
            self.before = Some(self.value.clone());
        }
        &mut self.value
    }

    pub fn set(&mut self, value: T) {
        if value == self.value {
            return;
        }
        let held = std::mem::replace(&mut self.value, value);
        self.before.get_or_insert(held);
    }

    /// The value, when it differs from the value at the last take.
    pub fn take_changed(&mut self) -> Option<&T> {
        let held = self.before.take()?;
        (held != self.value).then_some(&self.value)
    }
}

impl<T: Clone + PartialEq> Marked<Option<T>> {
    /// Takes the value out when `predicate` holds for it.
    pub fn take_if(&mut self, predicate: impl FnOnce(&T) -> bool) -> Option<T> {
        if !self.value.as_ref().is_some_and(predicate) {
            return None;
        }
        let taken = self.value.take();
        self.before.get_or_insert_with(|| taken.clone());
        taken
    }
}

impl<T> Deref for Marked<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> From<T> for Marked<T> {
    fn from(value: T) -> Self {
        Self {
            value,
            before: None,
        }
    }
}

impl<T: Default> Default for Marked<T> {
    fn default() -> Self {
        T::default().into()
    }
}

impl<T: PartialEq> PartialEq for Marked<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: fmt::Debug> fmt::Debug for Marked<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}
