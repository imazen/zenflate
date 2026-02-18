//! Fast byte load/store helpers.
//!
//! When the `unchecked` feature is enabled, these skip bounds checks using raw
//! pointer arithmetic.  When disabled, they use normal safe indexing.
//!
//! # Safety contract (unchecked mode)
//!
//! Every call site MUST guarantee that the offset + access width is within bounds.
//! The safe code paths already enforce this at runtime, so existing callers are
//! correct by construction.

/// Load a little-endian `u32` from `data[off..off+4]`.
#[inline(always)]
pub(crate) fn load_u32_le(data: &[u8], off: usize) -> u32 {
    #[cfg(feature = "unchecked")]
    {
        debug_assert!(off + 4 <= data.len());
        // SAFETY: Caller guarantees off + 4 <= data.len().
        // [u8; 4] has alignment 1, so the pointer cast is always valid.
        unsafe { u32::from_le_bytes(*(data.as_ptr().add(off) as *const [u8; 4])) }
    }
    #[cfg(not(feature = "unchecked"))]
    {
        u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
    }
}

/// Load a little-endian `u64` from `data[off..off+8]`.
#[inline(always)]
pub(crate) fn load_u64_le(data: &[u8], off: usize) -> u64 {
    #[cfg(feature = "unchecked")]
    {
        debug_assert!(off + 8 <= data.len());
        // SAFETY: Caller guarantees off + 8 <= data.len().
        unsafe { u64::from_le_bytes(*(data.as_ptr().add(off) as *const [u8; 8])) }
    }
    #[cfg(not(feature = "unchecked"))]
    {
        u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
    }
}

/// Store a little-endian `u64` at `data[off..off+8]`.
#[inline(always)]
pub(crate) fn store_u64_le(data: &mut [u8], off: usize, val: u64) {
    #[cfg(feature = "unchecked")]
    {
        debug_assert!(off + 8 <= data.len());
        // SAFETY: Caller guarantees off + 8 <= data.len().
        unsafe {
            *(data.as_mut_ptr().add(off) as *mut [u8; 8]) = val.to_le_bytes();
        }
    }
    #[cfg(not(feature = "unchecked"))]
    {
        data[off..off + 8].copy_from_slice(&val.to_le_bytes());
    }
}

/// Get a single byte at `data[idx]`.
#[inline(always)]
pub(crate) fn get_byte(data: &[u8], idx: usize) -> u8 {
    #[cfg(feature = "unchecked")]
    {
        debug_assert!(idx < data.len());
        // SAFETY: Caller guarantees idx < data.len().
        unsafe { *data.get_unchecked(idx) }
    }
    #[cfg(not(feature = "unchecked"))]
    {
        data[idx]
    }
}

/// Index into a slice, skipping bounds checks in `unchecked` mode.
#[inline(always)]
pub(crate) fn idx<T: Copy>(slice: &[T], i: usize) -> T {
    #[cfg(feature = "unchecked")]
    {
        debug_assert!(i < slice.len());
        // SAFETY: Caller guarantees i < slice.len().
        unsafe { *slice.get_unchecked(i) }
    }
    #[cfg(not(feature = "unchecked"))]
    {
        slice[i]
    }
}

/// Get a mutable reference into a slice, skipping bounds checks in `unchecked` mode.
#[inline(always)]
pub(crate) fn idx_mut<T>(slice: &mut [T], i: usize) -> &mut T {
    #[cfg(feature = "unchecked")]
    {
        debug_assert!(i < slice.len());
        // SAFETY: Caller guarantees i < slice.len().
        unsafe { slice.get_unchecked_mut(i) }
    }
    #[cfg(not(feature = "unchecked"))]
    {
        &mut slice[i]
    }
}

/// Prefetch a value for upcoming write access.
///
/// Emits `PREFETCHW` on x86_64 when the `unchecked` feature is enabled,
/// matching C libdeflate's `__builtin_prefetch(addr, 1)`. Write-intent
/// prefetch brings the cache line into Modified/Exclusive state, avoiding
/// a later Read-For-Ownership transition when the hash table entry is
/// updated. No-op on other platforms or without `unchecked`.
#[inline(always)]
pub(crate) fn prefetch<T>(_r: &T) {
    #[cfg(all(feature = "unchecked", target_arch = "x86_64"))]
    {
        // SAFETY: prefetchw is a hint; the pointer comes from a valid reference.
        unsafe {
            core::arch::asm!(
                "prefetchw [{}]",
                in(reg) _r as *const T,
                options(nostack, preserves_flags),
            );
        }
    }
}
