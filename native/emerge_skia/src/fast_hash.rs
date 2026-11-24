//! One Raven patch: a fast non-cryptographic hasher for the renderer's
//! per-frame keys (visible-frame fingerprint, paint-run payload keys, asset
//! and font generations). Those paths used std's SipHash `DefaultHasher`,
//! which showed up as ~5% of the render thread on the RK3566 during a sheet
//! drag. Nothing here needs DoS resistance: the inputs are our own scene.
//!
//! This is the Fx hash (rustc's), one multiply per word. Each step,
//! `h = (h.rotate_left(5) ^ word) * K` with odd `K`, is a bijection of `h`
//! for a fixed word and of the word for a fixed `h`, so two equal-length
//! inputs that differ in a single word never collide; that is the common
//! shape of a changed key (one moved offset, one new alpha).

use std::hash::Hasher;

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FastHasher {
    hash: u64,
}

impl FastHasher {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            self.add(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            self.add(u64::from(u32::from_le_bytes(
                bytes[..4].try_into().unwrap(),
            )));
            bytes = &bytes[4..];
        }
        for &byte in bytes {
            self.add(u64::from(byte));
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u16(&mut self, value: u16) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::FastHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of(value: impl Hash) -> u64 {
        let mut hasher = FastHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn single_word_changes_never_collide() {
        let base = [1.5f32, 2.0, 300.25, 0.5, 7.0].map(f32::to_bits);
        let base_hash = hash_of(base);
        for index in 0..base.len() {
            for delta in 1..2_000u32 {
                let mut changed = base;
                changed[index] = changed[index].wrapping_add(delta);
                assert_ne!(hash_of(changed), base_hash);
            }
        }
    }

    #[test]
    fn strings_and_lengths_are_distinguished() {
        assert_ne!(hash_of("ab"), hash_of("ba"));
        assert_ne!(hash_of(("a", "bc")), hash_of(("ab", "c")));
        assert_ne!(hash_of(vec![0u8; 3]), hash_of(vec![0u8; 4]));
        assert_eq!(hash_of("same text"), hash_of("same text"));
    }
}
