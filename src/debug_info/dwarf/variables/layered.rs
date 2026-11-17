use std::sync::Arc;

use foldhash::HashMap;

use super::types::Bits;
use crate::debug_info::dwarf::{DieKey, DieMap};

pub(super) struct Rows<T> {
    base: Arc<Vec<T>>,
    own: Vec<T>,
}

impl<T> Default for Rows<T> {
    fn default() -> Self {
        Self {
            base: Arc::default(),
            own: Vec::new(),
        }
    }
}

impl<T> Rows<T> {
    pub(super) fn len(&self) -> usize {
        self.base.len() + self.own.len()
    }

    pub(super) fn get(&self, index: usize) -> Option<&T> {
        index
            .checked_sub(self.base.len())
            .map_or_else(|| self.base.get(index), |own| self.own.get(own))
    }

    pub(super) fn push(&mut self, row: T) {
        self.own.push(row);
    }

    pub(super) fn resize_with(&mut self, len: usize, fill: impl FnMut() -> T) {
        if let Some(own) = len.checked_sub(self.base.len())
            && own > self.own.len()
        {
            self.own.resize_with(own, fill);
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.base.iter().chain(&self.own)
    }

    pub(super) fn base_len(&self) -> usize {
        self.base.len()
    }

    pub(super) fn into_own(self) -> Vec<T> {
        self.own
    }

    pub(super) fn clear_own(&mut self) {
        self.own.clear();
    }

    pub(super) fn freeze(&mut self)
    where
        T: Clone,
    {
        let rows = std::mem::take(self.flat());
        self.base = Arc::new(rows);
    }

    pub(super) fn split(&self) -> Self {
        debug_assert!(self.own.is_empty(), "only a frozen table splits");
        Self {
            base: Arc::clone(&self.base),
            own: Vec::new(),
        }
    }
}

impl<T: Clone> Rows<T> {
    pub(super) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        match index.checked_sub(self.base.len()) {
            None => Arc::make_mut(&mut self.base).get_mut(index),
            Some(own) => self.own.get_mut(own),
        }
    }

    pub(super) fn flat(&mut self) -> &mut Vec<T> {
        if !self.base.is_empty() {
            let base = std::mem::take(&mut self.base);
            let mut base = Arc::try_unwrap(base).unwrap_or_else(|shared| (*shared).clone());
            if base.len() >= self.own.len() {
                base.append(&mut self.own);
                self.own = base;
            } else {
                self.own.splice(0..0, base);
            }
        }
        &mut self.own
    }

    pub(super) fn into_flat(mut self) -> Vec<T> {
        std::mem::take(self.flat())
    }
}

impl<T> std::ops::Index<usize> for Rows<T> {
    type Output = T;

    #[expect(
        clippy::indexing_slicing,
        reason = "like a `Vec`'s, this index is for rows its caller added; `Self::get` is for others"
    )]
    fn index(&self, index: usize) -> &T {
        index
            .checked_sub(self.base.len())
            .map_or_else(|| &self.base[index], |own| &self.own[own])
    }
}

impl<T: Clone> std::ops::IndexMut<usize> for Rows<T> {
    #[expect(
        clippy::indexing_slicing,
        reason = "like a `Vec`'s, this index is for rows its caller added; `Self::get_mut` is for \
                  others"
    )]
    fn index_mut(&mut self, index: usize) -> &mut T {
        match index.checked_sub(self.base.len()) {
            None => &mut Arc::make_mut(&mut self.base)[index],
            Some(own) => &mut self.own[own],
        }
    }
}

#[derive(Default)]
pub(super) struct Marks {
    base: Arc<Bits>,
    own: Bits,
}

impl Marks {
    pub(super) fn contains(&self, value: usize) -> bool {
        self.own.contains(value) || self.base.contains(value)
    }

    pub(super) fn insert(&mut self, value: usize) {
        self.own.insert(value);
    }

    pub(super) fn into_own(self) -> Bits {
        self.own
    }

    pub(super) fn clear_own(&mut self) {
        self.own = Bits::default();
    }

    pub(super) fn freeze(&mut self) {
        let mut own = std::mem::take(&mut self.own);
        if !self.base.is_empty() {
            let mut base = (*self.base).clone();
            base.union(&own);
            own = base;
        }

        self.base = Arc::new(own);
    }

    pub(super) fn split(&self) -> Self {
        debug_assert!(self.own.is_empty(), "only a frozen set splits");
        Self {
            base: Arc::clone(&self.base),
            own: Bits::default(),
        }
    }
}

pub(super) trait Table: Default + Clone {
    type Key;
    type Value;

    fn get(&self, key: &Self::Key) -> Option<&Self::Value>;

    fn insert(&mut self, key: Self::Key, value: Self::Value) -> Option<Self::Value>;

    fn extend_with(&mut self, other: Self);

    fn is_empty(&self) -> bool;
}

impl<K: Clone + Eq + std::hash::Hash, V: Clone> Table for HashMap<K, V> {
    type Key = K;
    type Value = V;

    fn get(&self, key: &K) -> Option<&V> {
        Self::get(self, key)
    }

    fn insert(&mut self, key: K, value: V) -> Option<V> {
        Self::insert(self, key, value)
    }

    fn extend_with(&mut self, other: Self) {
        self.extend(other);
    }

    fn is_empty(&self) -> bool {
        Self::is_empty(self)
    }
}

impl<V: Clone> Table for DieMap<V> {
    type Key = DieKey;
    type Value = V;

    fn get(&self, key: &DieKey) -> Option<&V> {
        Self::get(self, key)
    }

    fn insert(&mut self, key: DieKey, value: V) -> Option<V> {
        Self::insert(self, key, value)
    }

    fn extend_with(&mut self, other: Self) {
        Self::extend(self, other);
    }

    fn is_empty(&self) -> bool {
        Self::is_empty(self)
    }
}

#[derive(Default)]
pub(super) struct Layered<M> {
    base: Arc<M>,
    own: M,
}

impl<M: Table> Layered<M> {
    pub(super) fn get(&self, key: &M::Key) -> Option<&M::Value> {
        self.own.get(key).or_else(|| self.base.get(key))
    }

    pub(super) fn insert(&mut self, key: M::Key, value: M::Value) {
        self.own.insert(key, value);
    }

    pub(super) fn into_own(self) -> M {
        self.own
    }

    pub(super) fn clear_own(&mut self) {
        self.own = M::default();
    }

    pub(super) fn flat(&mut self) -> &mut M {
        if !self.base.is_empty() {
            let base = std::mem::take(&mut self.base);
            let mut all = Arc::try_unwrap(base).unwrap_or_else(|shared| (*shared).clone());
            all.extend_with(std::mem::take(&mut self.own));
            self.own = all;
        }
        &mut self.own
    }

    pub(super) fn into_flat(mut self) -> M {
        std::mem::take(self.flat())
    }

    pub(super) fn freeze(&mut self) {
        let all = std::mem::take(self.flat());
        self.base = Arc::new(all);
    }

    pub(super) fn split(&self) -> Self {
        debug_assert!(self.own.is_empty(), "only a frozen map splits");
        Self {
            base: Arc::clone(&self.base),
            own: M::default(),
        }
    }
}
