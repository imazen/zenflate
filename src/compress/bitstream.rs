//! Output bitstream writer for DEFLATE compression.
//!
//! Ported from libdeflate's `deflate_compress.c` (struct deflate_output_bitstream,
//! ADD_BITS, FLUSH_BITS).
//!
//! Uses a 64-bit bitbuffer, LSB-first, with word-at-a-time flushing when possible.

/// Number of usable bits in the bitbuffer (1 less than u64 size to avoid UB-equivalent shifts).
pub(crate) const BITBUF_NBITS: u32 = 63;

/// Can we always buffer `n` bits after a flush? (Up to 7 bits may remain after flush.)
#[allow(dead_code)]
pub(crate) const fn can_buffer(n: u32) -> bool {
    7 + n <= BITBUF_NBITS
}

/// Output bitstream for writing compressed data.
pub(crate) struct OutputBitstream<'a> {
    /// Bits that haven't yet been written to the output buffer.
    pub bitbuf: u64,
    /// Number of bits currently held in bitbuf (0..=BITBUF_NBITS, 0..=7 after flush).
    pub bitcount: u32,
    /// Current write position in the output buffer.
    pub pos: usize,
    /// Output buffer.
    pub buf: &'a mut [u8],
    /// Whether the output buffer ran out of space.
    pub overflow: bool,
}

impl<'a> OutputBitstream<'a> {
    /// Create a new output bitstream writing to `buf`.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            bitbuf: 0,
            bitcount: 0,
            pos: 0,
            buf,
            overflow: false,
        }
    }

    /// Add bits to the bitbuffer. Caller must ensure `bitcount + n <= BITBUF_NBITS`.
    #[inline(always)]
    pub fn add_bits(&mut self, bits: u32, n: u32) {
        self.bitbuf |= (bits as u64) << self.bitcount;
        self.bitcount += n;
        debug_assert!(self.bitcount <= BITBUF_NBITS);
    }

    /// Flush bits from the bitbuffer to the output buffer.
    /// After this, the bitbuffer contains at most 7 bits (a partial byte).
    #[inline(always)]
    pub fn flush_bits(&mut self) {
        // Fast path: write a full u64 word if there's room
        if self.pos + 8 <= self.buf.len() {
            crate::fast_bytes::store_u64_le(self.buf, self.pos, self.bitbuf);
            self.pos += (self.bitcount >> 3) as usize;
            self.bitbuf >>= self.bitcount & !7;
            self.bitcount &= 7;
        } else {
            // Slow path: write a byte at a time
            while self.bitcount >= 8 {
                if self.pos < self.buf.len() {
                    self.buf[self.pos] = self.bitbuf as u8;
                    self.pos += 1;
                    self.bitcount -= 8;
                    self.bitbuf >>= 8;
                } else {
                    self.overflow = true;
                    return;
                }
            }
        }
    }

    /// Write a single byte directly to the output.
    #[inline(always)]
    pub fn write_byte(&mut self, b: u8) {
        if self.pos < self.buf.len() {
            self.buf[self.pos] = b;
            self.pos += 1;
        } else {
            self.overflow = true;
        }
    }

    /// Write a 16-bit little-endian value directly to the output.
    #[inline(always)]
    pub fn write_le16(&mut self, v: u16) {
        if self.pos + 2 <= self.buf.len() {
            self.buf[self.pos..self.pos + 2].copy_from_slice(&v.to_le_bytes());
            self.pos += 2;
        } else {
            self.overflow = true;
        }
    }

    /// Write a 32-bit little-endian value directly to the output.
    #[inline(always)]
    #[allow(dead_code)]
    pub fn write_le32(&mut self, v: u32) {
        if self.pos + 4 <= self.buf.len() {
            self.buf[self.pos..self.pos + 4].copy_from_slice(&v.to_le_bytes());
            self.pos += 4;
        } else {
            self.overflow = true;
        }
    }

    /// Write a slice of bytes directly to the output.
    #[inline]
    pub fn write_bytes(&mut self, data: &[u8]) {
        if self.pos + data.len() <= self.buf.len() {
            self.buf[self.pos..self.pos + data.len()].copy_from_slice(data);
            self.pos += data.len();
        } else {
            self.overflow = true;
        }
    }

    /// Remaining capacity in the output buffer.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Position beyond which we can't safely do word-at-a-time flushes.
    /// (end - 7, but clamped to avoid underflow)
    #[allow(dead_code)]
    pub fn fast_end(&self) -> usize {
        self.buf.len().saturating_sub(7)
    }
}
