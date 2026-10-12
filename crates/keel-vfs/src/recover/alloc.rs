//! Which clusters a volume's allocation map marks used (NTFS `$Bitmap`, the exFAT
//! allocation bitmap, a FAT's non-zero entries), with counts over ranges in O(1).

use super::Confidence;

pub(crate) struct Alloc {
    words: Vec<u64>,
    /// `ranks[i]`: set bits in `words[..i]`.
    ranks: Vec<u64>,
    /// Clusters the map covers, from `first`.
    bits: u64,
    /// The cluster number of bit 0.
    first: u64,
}

impl Alloc {
    /// Bit `i` of `bytes` (least significant first) is cluster `first + i`; at most
    /// `max_bits` bits are taken.
    pub fn from_bitmap(bytes: &[u8], first: u64, max_bits: u64) -> Self {
        let bits = (bytes.len() as u64 * 8).min(max_bits);
        let mut words: Vec<u64> = bytes
            .chunks(8)
            .map(|c| {
                let mut w = [0u8; 8];
                w[..c.len()].copy_from_slice(c);
                u64::from_le_bytes(w)
            })
            .collect();
        words.truncate(bits.div_ceil(64) as usize);
        if bits % 64 != 0 {
            if let Some(last) = words.last_mut() {
                *last &= (1u64 << (bits % 64)) - 1;
            }
        }
        Self::with_words(words, bits, first)
    }

    /// Cluster `first + i` is used when `used(i)`, for `i` in `0..bits`.
    pub fn from_fn(bits: u64, first: u64, used: impl Fn(u64) -> bool) -> Self {
        let mut words = vec![0u64; bits.div_ceil(64) as usize];
        for i in 0..bits {
            if used(i) {
                words[(i / 64) as usize] |= 1 << (i % 64);
            }
        }
        Self::with_words(words, bits, first)
    }

    fn with_words(words: Vec<u64>, bits: u64, first: u64) -> Self {
        let mut ranks = Vec::with_capacity(words.len() + 1);
        let mut sum = 0u64;
        ranks.push(0);
        for w in &words {
            sum += u64::from(w.count_ones());
            ranks.push(sum);
        }
        Self {
            words,
            ranks,
            bits,
            first,
        }
    }

    /// Used bits in `0..bit` (`bit <= self.bits`).
    fn used_before(&self, bit: u64) -> u64 {
        let w = (bit / 64) as usize;
        let r = bit % 64;
        let partial = match (r, self.words.get(w)) {
            (0, _) | (_, None) => 0,
            (r, Some(word)) => u64::from((word & ((1u64 << r) - 1)).count_ones()),
        };
        self.ranks[w.min(self.ranks.len() - 1)] + partial
    }

    /// (used, covered) among clusters `start..start + len`.
    pub fn count(&self, start: u64, len: u64) -> (u64, u64) {
        let lo = start.max(self.first) - self.first;
        let hi = start.saturating_add(len).min(self.first + self.bits);
        let hi = hi.saturating_sub(self.first);
        if hi <= lo {
            return (0, 0);
        }
        (self.used_before(hi) - self.used_before(lo), hi - lo)
    }

    pub fn is_used(&self, cluster: u64) -> Option<bool> {
        let (used, covered) = self.count(cluster, 1);
        (covered == 1).then_some(used == 1)
    }

    /// The confidence for data in these cluster runs `(first cluster, count)`.
    pub fn confidence(&self, runs: &[(u64, u64)]) -> Confidence {
        let (mut used, mut covered, mut total) = (0u64, 0u64, 0u64);
        for &(start, len) in runs {
            let (u, c) = self.count(start, len);
            used += u;
            covered += c;
            total = total.saturating_add(len);
        }
        if used == 0 && covered == total {
            Confidence::Good
        } else if used == total {
            Confidence::Overwritten
        } else if used > 0 {
            Confidence::Partial
        } else {
            Confidence::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_ranges() {
        // Clusters 10.. : 0b1010_0110, 0b0000_0001 → used 11, 12, 15, 17, 18.
        let a = Alloc::from_bitmap(&[0b1010_0110, 0b0000_0001], 10, 12);
        assert_eq!(a.count(10, 8), (4, 8));
        assert_eq!(a.count(0, 100), (5, 12));
        assert_eq!(a.count(18, 10), (1, 4));
        assert_eq!(a.count(30, 4), (0, 0));
        assert_eq!(a.is_used(11), Some(true));
        assert_eq!(a.is_used(13), Some(false));
        assert_eq!(a.is_used(99), None);
        assert_eq!(a.confidence(&[(13, 2)]), Confidence::Good);
        assert_eq!(a.confidence(&[(11, 2)]), Confidence::Overwritten);
        assert_eq!(a.confidence(&[(12, 2)]), Confidence::Partial);
        assert_eq!(a.confidence(&[(21, 4)]), Confidence::Unknown);
        assert_eq!(a.confidence(&[]), Confidence::Good);
        let wide: Vec<u8> = (0..40).map(|i| i as u8).collect();
        let b = Alloc::from_bitmap(&wide, 0, u64::MAX);
        let ones: u64 = wide.iter().map(|b| u64::from(b.count_ones())).sum();
        assert_eq!(b.count(0, 320), (ones, 320));
        let c = Alloc::from_fn(130, 2, |i| i % 3 == 0);
        assert_eq!(c.count(2, 130).0, 44);
        assert_eq!(c.count(0, u64::MAX), (44, 130));
    }
}
