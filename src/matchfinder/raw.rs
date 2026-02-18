//! Raw-pointer hot-path variants for `unchecked` mode.
//!
//! These eliminate Rust fat-pointer overhead (`&[u8]` = ptr+len = 2 registers)
//! by using `*const u8` (1 register each). On x86-64 with 15 GPRs, freeing
//! even 2 registers in a spill-heavy inner loop is significant.
//!
//! # Safety contract
//!
//! Every call site MUST guarantee that pointer + offset is within bounds.
//! The safe code paths enforce this at runtime, so existing callers are
//! correct by construction.

/// Extend a match using raw pointers.
///
/// Equivalent to [`super::lz_extend`] but takes `*const u8` instead of `&[u8]`
/// to avoid fat-pointer register pressure.
///
/// # Safety
///
/// Both `strptr` and `matchptr` must be valid for reads of at least `max_len` bytes.
#[cfg(feature = "unchecked")]
#[inline(always)]
pub(crate) unsafe fn lz_extend_raw(
    strptr: *const u8,
    matchptr: *const u8,
    start_len: u32,
    max_len: u32,
) -> u32 {
    unsafe {
        let mut len = start_len;

        // Word-at-a-time comparison (same algorithm as lz_extend)
        while len + 8 <= max_len {
            let off = len as usize;
            let sw = core::ptr::read_unaligned(strptr.add(off) as *const u64);
            let mw = core::ptr::read_unaligned(matchptr.add(off) as *const u64);
            let xor = sw ^ mw;
            if xor != 0 {
                len += xor.trailing_zeros() >> 3;
                return len.min(max_len);
            }
            len += 8;
        }

        // Byte-at-a-time for remainder
        while len < max_len && *strptr.add(len as usize) == *matchptr.add(len as usize) {
            len += 1;
        }
        len
    }
}
