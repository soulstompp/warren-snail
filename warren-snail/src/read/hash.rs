//! A hash for keys made of a few integers or short text, as a round's joins and groups hold them: a
//! multiply and a rotate a word, where the standard library's hash runs rounds made to resist chosen
//! keys. A round's keys come from its own file.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

#[derive(Clone, Copy, Default)]
pub struct WordHasher(u64);

const K: u64 = 0x517c_c1b7_2722_0a95;

impl WordHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(K);
    }
}

impl Hasher for WordHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.add(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last));
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(n as u64);
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(n as u64);
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[inline]
    fn write_i64(&mut self, n: i64) {
        self.add(n as u64);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// A map hashed a word at a time.
pub type WordMap<K, V> = HashMap<K, V, BuildHasherDefault<WordHasher>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn hash<T: Hash + ?Sized>(t: &T) -> u64 {
        BuildHasherDefault::<WordHasher>::default().hash_one(t)
    }

    #[test]
    fn keys_that_differ_hash_apart_and_equal_keys_alike() {
        let keys: Vec<Vec<i64>> = (0..200)
            .flat_map(|b| (0..50).map(move |r| vec![b, r]))
            .collect();
        let mut hashes: Vec<u64> = keys.iter().map(hash).collect();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), keys.len());
        assert_eq!(hash(&vec![3i64, 4]), hash(&[3i64, 4][..]));
        assert_ne!(hash("AWS hourly"), hash("AWS Backup"));
        assert_ne!(hash("ab"), hash("ac"));
        let mut m: WordMap<Vec<i64>, u32> = WordMap::default();
        m.insert(vec![1, 2], 7);
        assert_eq!(m.get(&[1i64, 2][..]), Some(&7));
    }
}
