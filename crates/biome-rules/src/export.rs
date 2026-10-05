//! Buffer plumbing for the WebAssembly exports (wasm32 only; see
//! [`crate::export!`]). The host allocates input buffers with `br_alloc`,
//! writes into linear memory, calls a function, reads the returned
//! `(ptr << 32) | len` buffer and releases everything with `br_free`.
//!
//! Pointers are `u32` because this module only exists on wasm32.

/// Allocate `len` bytes the host may write into. Zero-length requests return
/// a dangling (never dereferenced, never freed) pointer.
pub fn alloc(len: u32) -> u32 {
    if len == 0 {
        return core::ptr::NonNull::<u8>::dangling().as_ptr() as u32;
    }
    // Allocate with the exact layout `free` will hand back. (A
    // `Vec::with_capacity(len)` may round its capacity up, and freeing with
    // a different size than was allocated is undefined behaviour.)
    let layout = layout(len);
    // SAFETY: `len > 0`, so the layout has non-zero size.
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    ptr as u32
}

fn layout(len: u32) -> core::alloc::Layout {
    // Align 1 and a size that fits in u32 can never be an invalid layout.
    core::alloc::Layout::from_size_align(len as usize, 1).unwrap_or(core::alloc::Layout::new::<u8>())
}

/// Release a buffer from [`alloc`] or from [`give`].
///
/// # Safety
/// `ptr` and `len` must be exactly what `alloc(len)` returned / what a
/// result's packed value described, and the buffer must not be used after.
pub unsafe fn free(ptr: u32, len: u32) {
    if len == 0 {
        return;
    }
    std::alloc::dealloc(ptr as *mut u8, layout(len));
}

/// Hand a result to the host: leak it and return `(ptr << 32) | len`.
pub fn give(bytes: Vec<u8>) -> u64 {
    // A boxed slice is allocated with exactly Layout(len, align 1): the
    // layout `free` deallocates with.
    let mut boxed = bytes.into_boxed_slice();
    let len = boxed.len() as u64;
    if len == 0 {
        return 0;
    }
    let ptr = boxed.as_mut_ptr() as u64;
    core::mem::forget(boxed);
    (ptr << 32) | len
}

/// View a host-written buffer.
///
/// # Safety
/// `ptr`/`len` must describe a live buffer from [`alloc`].
pub unsafe fn slice(ptr: u32, len: u32) -> &'static [u8] {
    if len == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(ptr as *const u8, len as usize)
    }
}

/// Copy the 32-byte seed the host wrote at `ptr`.
///
/// # Safety
/// `ptr` must point at 32 readable bytes.
pub unsafe fn seed(ptr: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(core::slice::from_raw_parts(ptr as *const u8, 32));
    out
}

/// Seat argument from the host. Seats are `0..=253` plus 255 (spectator);
/// anything that does not fit in a byte must not wrap around into a real
/// seat, so it becomes 254, which is never seated.
pub fn seat(raw: u32) -> u8 {
    if raw > u8::MAX as u32 {
        NOBODY
    } else {
        raw as u8
    }
}

/// A seat number no match can contain (`MAX_SEATS` is at most 254, so seat
/// indices stop at 253).
pub const NOBODY: u8 = 254;
