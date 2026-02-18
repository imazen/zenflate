//! Sequence store for deferred literal/match encoding.
//!
//! Ported from libdeflate's `struct deflate_sequence` and related helpers.
//!
//! A "sequence" represents a run of literals followed by a match (or end-of-block).
//! Sequences are accumulated during parsing, then encoded all at once after
//! the block's Huffman codes have been computed.

/// Shift for the match length within `litrunlen_and_length`.
pub(crate) const SEQ_LENGTH_SHIFT: u32 = 23;

/// Mask for the literal run length within `litrunlen_and_length`.
pub(crate) const SEQ_LITRUNLEN_MASK: u32 = (1 << SEQ_LENGTH_SHIFT) - 1;

/// A run of literals followed by a match or end-of-block.
#[derive(Clone, Copy, Default)]
pub(crate) struct Sequence {
    /// Bits 0..22: number of literals in this run.
    /// Bits 23..31: length of the following match (0 = end of block).
    pub litrunlen_and_length: u32,

    /// Match offset (only valid if length > 0).
    pub offset: u16,

    /// Match offset slot (only valid if length > 0).
    pub offset_slot: u16,
}

impl Sequence {
    /// Get the literal run length.
    #[inline(always)]
    pub fn litrunlen(self) -> u32 {
        self.litrunlen_and_length & SEQ_LITRUNLEN_MASK
    }

    /// Get the match length (0 = last sequence in block).
    #[inline(always)]
    pub fn length(self) -> u32 {
        self.litrunlen_and_length >> SEQ_LENGTH_SHIFT
    }
}
