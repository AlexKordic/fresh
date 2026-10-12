//! Heap allocation counts for the scene channel's frame telemetry.
//!
//! With the `alloc-stats` feature, the global allocator counts every
//! allocation and reallocation each thread makes, so a scene push can report
//! how many it made and how many bytes they asked for. The feature is for
//! benchmarks: without it [`counts`] is `None` and the system allocator is
//! used untouched.

#[cfg(feature = "alloc-stats")]
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        // Allocations and requested bytes, this thread, since it started.
        static COUNTS: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    }

    fn note(bytes: usize) {
        // `try_with`: a thread's last frees and allocations can run after its
        // locals are gone.
        COUNTS
            .try_with(|c| {
                let (n, b) = c.get();
                c.set((n + 1, b + bytes as u64));
            })
            .unwrap_or_default();
    }

    /// The system allocator, counting.
    pub struct CountingAllocator;

    // SAFETY: every call is forwarded unchanged to the system allocator; the
    // counting touches only a thread-local `Cell`, which never allocates.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            // SAFETY: the caller's contract, passed on.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            // SAFETY: the caller's contract, passed on.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller's contract, passed on.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note(new_size);
            // SAFETY: the caller's contract, passed on.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    pub fn counts() -> Option<(u64, u64)> {
        COUNTS.try_with(|c| c.get()).ok()
    }
}

/// Allocations and requested bytes on this thread so far, with the
/// `alloc-stats` feature; `None` without it.
pub fn counts() -> Option<(u64, u64)> {
    #[cfg(feature = "alloc-stats")]
    {
        counting::counts()
    }
    #[cfg(not(feature = "alloc-stats"))]
    {
        None
    }
}

/// The counts made between `before` (from [`counts`]) and now.
pub fn since(before: Option<(u64, u64)>) -> Option<(u64, u64)> {
    let (n0, b0) = before?;
    let (n1, b1) = counts()?;
    Some((n1.saturating_sub(n0), b1.saturating_sub(b0)))
}

#[cfg(all(test, feature = "alloc-stats"))]
mod tests {
    #[test]
    fn counts_the_allocations_this_thread_makes() {
        let before = super::counts();
        let buffer: Vec<u8> = Vec::with_capacity(1000);
        let (allocs, bytes) = super::since(before).expect("counting");
        drop(buffer);
        assert!(allocs >= 1 && bytes >= 1000, "{allocs} allocations, {bytes} bytes");
    }
}
