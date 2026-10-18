//! Bloom filter: a compact set that can answer "definitely not here" or
//! "maybe here". The LSM tree keeps one per SSTable so most lookups for a key
//! the SSTable does not hold never read its pages.
//!
//! Each key sets `HASHES` bits chosen by double hashing one 64-bit hash.
//! With `BITS_PER_KEY` = 10 the false positive rate is about 1%.

const BITS_PER_KEY: usize = 10;
const HASHES: u64 = 7;

pub struct Bloom {
    bits: Vec<u64>,
}

impl Bloom {
    /// Builds a filter holding the keys with these [`hash`]es.
    pub fn new(hashes: &[u64]) -> Self {
        let words = (hashes.len() * BITS_PER_KEY).div_ceil(64).max(1);
        let mut bloom = Bloom { bits: vec![0; words] };
        for &hash in hashes {
            for bit in bloom.probes(hash) {
                bloom.bits[bit / 64] |= 1 << (bit % 64);
            }
        }
        bloom
    }

    /// False means the key was certainly not added; true means it may have been.
    pub fn may_contain(&self, key: &[u8]) -> bool {
        self.probes(hash(key)).all(|bit| self.bits[bit / 64] & (1 << (bit % 64)) != 0)
    }

    fn probes(&self, hash: u64) -> impl Iterator<Item = usize> + use<> {
        let bits = (self.bits.len() * 64) as u64;
        let (h1, h2) = (hash, hash.rotate_left(32) | 1);
        (0..HASHES).map(move |i| (h1.wrapping_add(i.wrapping_mul(h2)) % bits) as usize)
    }
}

/// FNV-1a followed by a final mix, so similar keys spread over all 64 bits.
pub fn hash(key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in key {
        h = (h ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    // SplitMix64 finalizer.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u32) -> Vec<u8> {
        format!("key-{i}").into_bytes()
    }

    #[test]
    fn has_no_false_negatives_and_few_false_positives() {
        let hashes: Vec<u64> = (0..1000).map(|i| hash(&key(i))).collect();
        let bloom = Bloom::new(&hashes);
        assert!((0..1000).all(|i| bloom.may_contain(&key(i))));

        let false_positives = (1000..11_000).filter(|&i| bloom.may_contain(&key(i))).count();
        assert!(false_positives < 300, "{false_positives} of 10000");
    }

    #[test]
    fn empty_filter_rejects_everything() {
        let bloom = Bloom::new(&[]);
        assert!(!bloom.may_contain(b"anything"));
    }
}
