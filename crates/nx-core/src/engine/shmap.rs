//! Sharded copy-on-write map: `clone()` = 64 Arc bumps; a write clones only the shards it touches.
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::Arc;

pub const SHARDS: usize = 64;

/// Fx-style hasher (keys are small ids or short strings).
#[derive(Default, Clone, Copy)]
pub struct Fx(u64);
impl Hasher for Fx {
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0.rotate_left(5) ^ x as u64).wrapping_mul(0x517cc1b727220a95);
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x517cc1b727220a95);
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517cc1b727220a95);
    }
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64)
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
pub type FxBuild = BuildHasherDefault<Fx>;
type Shard<K, V> = HashMap<K, V, FxBuild>;

pub struct ShardedMap<K, V> {
    shards: Vec<Arc<Shard<K, V>>>,
    len: usize,
}

impl<K, V> Clone for ShardedMap<K, V> {
    fn clone(&self) -> Self {
        ShardedMap { shards: self.shards.clone(), len: self.len }
    }
}

impl<K: Hash + Eq + Clone, V: Clone> Default for ShardedMap<K, V> {
    fn default() -> Self {
        ShardedMap { shards: (0..SHARDS).map(|_| Arc::new(Shard::default())).collect(), len: 0 }
    }
}

impl<K: Hash + Eq + Clone, V: Clone> ShardedMap<K, V> {
    fn shard_of<Q: Hash + ?Sized>(k: &Q) -> usize {
        let mut h = Fx::default();
        k.hash(&mut h);
        ((h.finish() >> 40) as usize) % SHARDS
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.shards[Self::shard_of(k)].get(k)
    }
    pub fn insert(&mut self, k: K, v: V) {
        let i = Self::shard_of(&k);
        if Arc::make_mut(&mut self.shards[i]).insert(k, v).is_none() {
            self.len += 1;
        }
    }
    pub fn remove(&mut self, k: &K) {
        let i = Self::shard_of(k);
        if self.shards[i].contains_key(k) {
            Arc::make_mut(&mut self.shards[i]).remove(k);
            self.len -= 1;
        }
    }
    /// Mutate the value (default-created if absent); an emptied value per `is_empty` is removed.
    pub fn modify(&mut self, k: K, f: impl FnOnce(&mut V), empty: impl Fn(&V) -> bool, mk: impl FnOnce() -> V) {
        let i = Self::shard_of(&k);
        let sh = Arc::make_mut(&mut self.shards[i]);
        match sh.get_mut(&k) {
            Some(v) => {
                f(v);
                if empty(v) {
                    sh.remove(&k);
                    self.len -= 1;
                }
            }
            None => {
                let mut v = mk();
                f(&mut v);
                if !empty(&v) {
                    sh.insert(k, v);
                    self.len += 1;
                }
            }
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.shards.iter().flat_map(|s| s.iter())
    }
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.shards.iter().flat_map(|s| s.keys())
    }
}

/// Multimap helpers: `Vec<T>` values with add/remove of one item.
impl<K: Hash + Eq + Clone, T: Clone + PartialEq> ShardedMap<K, Vec<T>> {
    pub fn add(&mut self, k: K, t: T) {
        self.modify(k, |v| v.push(t), |v| v.is_empty(), Vec::new);
    }
    pub fn del(&mut self, k: K, t: &T) {
        self.modify(k, |v| v.retain(|x| x != t), |v| v.is_empty(), Vec::new);
    }
}
