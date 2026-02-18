//! DEFLATE format constants, ported from deflate_constants.h.

/// Uncompressed block type.
pub const DEFLATE_BLOCKTYPE_UNCOMPRESSED: u32 = 0;
/// Static Huffman block type.
pub const DEFLATE_BLOCKTYPE_STATIC_HUFFMAN: u32 = 1;
/// Dynamic Huffman block type.
pub const DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN: u32 = 2;

/// Minimum supported match length (in bytes).
pub const DEFLATE_MIN_MATCH_LEN: u32 = 3;
/// Maximum supported match length (in bytes).
pub const DEFLATE_MAX_MATCH_LEN: u32 = 258;

/// Maximum supported match offset (in bytes).
pub const DEFLATE_MAX_MATCH_OFFSET: u32 = 32768;

/// log2 of DEFLATE_MAX_MATCH_OFFSET.
pub const DEFLATE_WINDOW_ORDER: u32 = 15;

/// Number of precode symbols.
pub const DEFLATE_NUM_PRECODE_SYMS: u32 = 19;
/// Number of literal/length symbols (maximum for a given block).
pub const DEFLATE_NUM_LITLEN_SYMS: u32 = 288;
/// Number of offset symbols (maximum for a given block).
pub const DEFLATE_NUM_OFFSET_SYMS: u32 = 32;

/// Maximum number of symbols across all codes.
pub const DEFLATE_MAX_NUM_SYMS: u32 = 288;

/// Number of literal symbols (0-255).
pub const DEFLATE_NUM_LITERALS: u32 = 256;
/// End-of-block symbol.
pub const DEFLATE_END_OF_BLOCK: u32 = 256;
/// First length symbol.
pub const DEFLATE_FIRST_LEN_SYM: u32 = 257;

/// Maximum precode codeword length.
pub const DEFLATE_MAX_PRE_CODEWORD_LEN: u32 = 7;
/// Maximum literal/length codeword length (DEFLATE spec).
pub const DEFLATE_MAX_LITLEN_CODEWORD_LEN: u32 = 15;
/// Maximum offset codeword length.
pub const DEFLATE_MAX_OFFSET_CODEWORD_LEN: u32 = 15;

/// Maximum codeword length across all codes.
pub const DEFLATE_MAX_CODEWORD_LEN: u32 = 15;

/// Maximum possible overrun when decoding codeword lengths.
pub const DEFLATE_MAX_LENS_OVERRUN: u32 = 137;

/// Maximum extra bits for a match length.
pub const DEFLATE_MAX_EXTRA_LENGTH_BITS: u32 = 5;
/// Maximum extra bits for a match offset.
pub const DEFLATE_MAX_EXTRA_OFFSET_BITS: u32 = 13;

// Compression-specific constants (from deflate_compress.h / deflate_compress.c)

/// Maximum litlen codeword length we allow during compression.
/// Using 14 instead of 15 allows 4 literals per 64-bit bitbuf flush (4*14=56 < 63).
pub const MAX_LITLEN_CODEWORD_LEN: u32 = 14;

/// Number of length symbols used (symbols 257-285).
pub const DEFLATE_NUM_LEN_SYMS: u32 = 29;

/// Order of precode code lengths in the DEFLATE header.
pub const DEFLATE_PRECODE_LENS_PERMUTATION: [u8; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Length symbol base values (extra bits added to these).
pub const DEFLATE_LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

/// Number of extra bits for each length symbol.
pub const DEFLATE_LENGTH_EXTRA_BITS: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Offset symbol base values (extra bits added to these).
pub const DEFLATE_OFFSET_BASE: [u32; 32] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577, 0, 0,
];

/// Number of extra bits for each offset symbol.
pub const DEFLATE_OFFSET_EXTRA_BITS: [u8; 32] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 0, 0,
];
