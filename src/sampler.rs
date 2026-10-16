//! Bounded, distinct-row sampling.
//!
//! We want up to `limit` example rows that are distinct by their unique id and
//! not biased towards the first rows of the file. Instead of a classic
//! reservoir (which needs a growing membership set), we keep the `limit` rows
//! with the smallest 64-bit hash of their id in a max-heap. This is:
//!
//! * **bounded** — O(limit) memory regardless of file size;
//! * **distinct** — ids currently in the sample are tracked in a small set;
//! * **deterministic** — the same file always yields the same sample;
//! * **stream-friendly** — works identically across parallel segments.

use std::collections::BinaryHeap;
use std::collections::HashSet;

struct HeapEntry<T> {
    priority: u64,
    id: Box<str>,
    payload: T,
}

impl<T> PartialEq for HeapEntry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}
impl<T> Eq for HeapEntry<T> {}
impl<T> PartialOrd for HeapEntry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for HeapEntry<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap on priority so `peek` is the worst (largest hash) sample.
        self.priority.cmp(&other.priority)
    }
}

pub struct Sampler<T> {
    limit: usize,
    heap: BinaryHeap<HeapEntry<T>>,
    ids: HashSet<Box<str>>,
}

impl<T> Sampler<T> {
    pub fn new(limit: usize) -> Self {
        Sampler {
            limit,
            heap: BinaryHeap::new(),
            ids: HashSet::new(),
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.limit == 0
    }

    /// Offer a row. Rows whose id is already sampled are ignored.
    pub fn offer(&mut self, id: &str, payload: T) {
        self.offer_with(id, || payload);
    }

    /// Like [`offer`], but builds the payload only if the row is actually
    /// retained, so expensive example formatting is skipped for the vast
    /// majority of rows.
    pub fn offer_with<F>(&mut self, id: &str, make: F)
    where
        F: FnOnce() -> T,
    {
        if self.limit == 0 {
            return;
        }
        if self.ids.contains(id) {
            return;
        }

        let priority = fnv1a(id.as_bytes());

        if self.heap.len() >= self.limit {
            if let Some(worst) = self.heap.peek() {
                if priority >= worst.priority {
                    return;
                }
            }
        }

        self.ids.insert(id.into());
        self.heap.push(HeapEntry {
            priority,
            id: id.into(),
            payload: make(),
        });

        while self.heap.len() > self.limit {
            if let Some(entry) = self.heap.pop() {
                self.ids.remove(entry.id.as_ref());
            }
        }
    }

    #[allow(dead_code)]
    pub fn into_payloads(self) -> Vec<T> {
        let mut entries: Vec<HeapEntry<T>> = self.heap.into_iter().collect();
        entries.sort_by_key(|e| e.priority);
        entries.into_iter().map(|e| e.payload).collect()
    }

    /// Borrow the sampled payloads (cloned) without consuming the sampler.
    pub fn payloads(&self) -> Vec<T>
    where
        T: Clone,
    {
        let mut entries: Vec<&HeapEntry<T>> = self.heap.iter().collect();
        entries.sort_by_key(|e| e.priority);
        entries.into_iter().map(|e| e.payload.clone()).collect()
    }

    /// Merge another sampler's contents (used to combine parallel segments).
    pub fn merge(&mut self, other: Sampler<T>) {
        if other.limit > self.limit {
            self.limit = other.limit;
        }
        for entry in other.heap.into_iter() {
            self.offer(&entry.id, entry.payload);
        }
    }
}

/// FNV-1a, fast and stable across runs and platforms.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_distinct_and_bounded() {
        let mut sampler = Sampler::new(5);
        for i in 0..1000 {
            sampler.offer(&format!("id-{i}"), i);
        }
        let out = sampler.into_payloads();
        assert_eq!(out.len(), 5);
        let unique: HashSet<_> = out.iter().collect();
        assert_eq!(unique.len(), 5);
    }

    #[test]
    fn duplicate_ids_are_ignored() {
        let mut sampler = Sampler::new(10);
        for _ in 0..100 {
            sampler.offer("same", 1);
        }
        assert_eq!(sampler.into_payloads().len(), 1);
    }

    #[test]
    fn offer_with_builds_payloads_lazily() {
        // With a limit of 1 only new record minima are retained, so the
        // payload builder runs about `ln(n)` times instead of once per row.
        let mut sampler = Sampler::new(1);
        let mut built = 0usize;
        for i in 0..1000 {
            sampler.offer_with(&format!("id-{i}"), || {
                built += 1;
                i
            });
        }
        assert!(built < 100, "built {built}");
        assert_eq!(sampler.into_payloads().len(), 1);
    }
}
