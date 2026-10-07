//! Block splitting statistics for DEFLATE compression.
//!
//! Ported from libdeflate's block split algorithm.
//!
//! Uses 10 observation categories (8 literal types + 2 match types) to detect
//! when the data distribution changes enough to warrant starting a new block.

/// Number of literal observation types (top 2 bits and low 1 bit of literal).
const NUM_LITERAL_OBSERVATION_TYPES: usize = 8;

/// Number of match observation types (short match vs long match).
const NUM_MATCH_OBSERVATION_TYPES: usize = 2;

/// Total number of observation types.
pub(crate) const NUM_OBSERVATION_TYPES: usize =
    NUM_LITERAL_OBSERVATION_TYPES + NUM_MATCH_OBSERVATION_TYPES;

/// Number of observations between block-end checks.
const NUM_OBSERVATIONS_PER_BLOCK_CHECK: u32 = 512;

/// Minimum block length we'll use (5000 uncompressed bytes).
pub(crate) const MIN_BLOCK_LENGTH: usize = 5000;

/// Block split statistics.
#[derive(Clone)]
pub(crate) struct BlockSplitStats {
    pub new_observations: [u32; NUM_OBSERVATION_TYPES],
    pub observations: [u32; NUM_OBSERVATION_TYPES],
    pub num_new_observations: u32,
    pub num_observations: u32,
}

impl BlockSplitStats {
    /// Initialize stats for a new block.
    pub fn new() -> Self {
        Self {
            new_observations: [0; NUM_OBSERVATION_TYPES],
            observations: [0; NUM_OBSERVATION_TYPES],
            num_new_observations: 0,
            num_observations: 0,
        }
    }

    /// Record a literal observation.
    #[inline(always)]
    pub fn observe_literal(&mut self, lit: u8) {
        self.new_observations[((lit >> 5) as usize & 0x6) | (lit as usize & 1)] += 1;
        self.num_new_observations += 1;
    }

    /// Record a match observation.
    #[inline(always)]
    #[allow(dead_code)]
    pub fn observe_match(&mut self, length: u32) {
        self.new_observations[NUM_LITERAL_OBSERVATION_TYPES + (length >= 9) as usize] += 1;
        self.num_new_observations += 1;
    }

    /// Merge new observations into the accumulated totals.
    pub(crate) fn merge_new_observations(&mut self) {
        for i in 0..NUM_OBSERVATION_TYPES {
            self.observations[i] += self.new_observations[i];
            self.new_observations[i] = 0;
        }
        self.num_observations += self.num_new_observations;
        self.num_new_observations = 0;
    }

    /// Check whether we have enough observations and enough block length to consider ending.
    pub fn ready_to_check(&self, in_block_begin: usize, in_next: usize, in_end: usize) -> bool {
        self.num_new_observations >= NUM_OBSERVATIONS_PER_BLOCK_CHECK
            && in_next - in_block_begin >= MIN_BLOCK_LENGTH
            && in_end - in_next >= MIN_BLOCK_LENGTH
    }

    /// Check if the block should be ended at the current position.
    ///
    /// Returns true if the distribution of recent observations differs
    /// enough from the accumulated distribution to warrant a new block.
    pub fn should_end_block(
        &mut self,
        in_block_begin: usize,
        in_next: usize,
        in_end: usize,
    ) -> bool {
        if !self.ready_to_check(in_block_begin, in_next, in_end) {
            return false;
        }
        self.do_end_block_check((in_next - in_block_begin) as u32)
    }

    /// Core block-end check: compare old vs new observation distributions.
    pub(crate) fn do_end_block_check(&mut self, block_length: u32) -> bool {
        if self.num_observations > 0 {
            let mut total_delta = 0u64;

            for i in 0..NUM_OBSERVATION_TYPES {
                let expected = self.observations[i] as u64 * self.num_new_observations as u64;
                let actual = self.new_observations[i] as u64 * self.num_observations as u64;
                let delta = actual.abs_diff(expected);
                total_delta += delta;
            }

            let num_items = self.num_observations + self.num_new_observations;
            let mut cutoff =
                self.num_new_observations as u64 * 200 / 512 * self.num_observations as u64;

            // Short blocks need stronger evidence.
            if block_length < 10000 && num_items < 8192 {
                cutoff += cutoff * (8192 - num_items) as u64 / 8192;
            }

            if total_delta + (block_length as u64 / 4096) * self.num_observations as u64 >= cutoff {
                return true;
            }
        }
        self.merge_new_observations();
        false
    }
}

/// End of the block starting at `begin`, chosen from the input bytes alone
/// (each byte is one observation: a repeat of the previous byte counts as a
/// short match, any other byte as that literal), using the same distribution
/// test as the parse-driven splitter. Parse-independent, so every parser that
/// uses it splits the input identically. Never past `begin + max_len` (or
/// `end`); never leaves a tail shorter than [`MIN_BLOCK_LENGTH`].
/// Literal observation category of each byte value (top 2 bits and low bit).
const LITERAL_CATEGORY: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        t[b] = (((b >> 5) & 0x6) | (b & 1)) as u8;
        b += 1;
    }
    t
};

pub(crate) fn input_block_end(input: &[u8], begin: usize, end: usize, max_len: usize) -> usize {
    let max_end = if end - begin <= max_len + MIN_BLOCK_LENGTH {
        end
    } else {
        begin + max_len
    };
    // Observe the input the way a runs-only parse would see it: a run of 5+
    // equal bytes is a literal plus one match observation, any other byte a
    // literal. That tokenization depends only on the input, so every parser
    // splits alike. Counts accumulate locally and go into `stats` every 512
    // observations, when the end-of-block test runs.
    let data = &input[begin..max_end];
    let mut stats = BlockSplitStats::new();
    let mut hist = [0u32; NUM_OBSERVATION_TYPES];
    let mut n = 0u32;
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        hist[LITERAL_CATEGORY[b as usize] as usize] += 1;
        n += 1;
        let run = match data.get(i + 1..i + 5) {
            Some(&[b1, b2, b3, b4]) => b1 == b && b2 == b && b3 == b && b4 == b,
            _ => false,
        };
        if run {
            let mut r = 5;
            while r < 258 && i + r < data.len() && data[i + r] == b {
                r += 1;
            }
            hist[NUM_LITERAL_OBSERVATION_TYPES + (r > 9) as usize] += 1; // match length r - 1 >= 9
            n += 1;
            i += r;
        } else {
            i += 1;
        }
        if n >= NUM_OBSERVATIONS_PER_BLOCK_CHECK {
            for (o, h) in stats.new_observations.iter_mut().zip(&mut hist) {
                *o += *h;
                *h = 0;
            }
            stats.num_new_observations += n;
            n = 0;
            if stats.should_end_block(begin, begin + i, end) {
                return begin + i;
            }
        }
    }
    max_end
}
