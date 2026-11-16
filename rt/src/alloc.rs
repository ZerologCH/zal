//! The ZAL runtime allocator: thread-local size-class free lists for small
//! blocks, the system allocator for everything else.
//!
//! ZAL programs allocate many small, short-lived objects (strings, records,
//! list headers, task slots). A free list per thread and size class turns
//! most allocations into a pointer pop and most frees into a pointer push,
//! without locks. Memory of small blocks is recycled, never returned to the
//! operating system. A block freed on another thread joins that thread's
//! list, which is still correct (a block is only ever in one list).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr::null_mut;

/// Block sizes 16, 32, …, 512 bytes.
const STEP: usize = 16;
const CLASSES: usize = 32;
const MAX: usize = STEP * CLASSES;
/// Fresh memory is carved from chunks of this size.
const CHUNK: usize = 256 * 1024;

pub struct ZalAlloc;

thread_local! {
    static FREE: [Cell<*mut u8>; CLASSES] = const { [const { Cell::new(null_mut()) }; CLASSES] };
    static BUMP: Cell<(*mut u8, *mut u8)> = const { Cell::new((null_mut(), null_mut())) };
}

#[inline]
fn class(size: usize) -> usize {
    (size.max(1) + STEP - 1) / STEP - 1
}

#[inline]
fn small(l: &Layout) -> bool {
    l.size() <= MAX && l.align() <= STEP
}

unsafe fn carve(bytes: usize) -> *mut u8 {
    BUMP.try_with(|b| {
        let (mut p, mut end) = b.get();
        if p.is_null() || (end as usize - p as usize) < bytes {
            p = System.alloc(Layout::from_size_align_unchecked(CHUNK, 4096));
            if p.is_null() {
                return null_mut();
            }
            end = p.add(CHUNK);
        }
        b.set((p.add(bytes), end));
        p
    })
    .unwrap_or(null_mut())
}

unsafe impl GlobalAlloc for ZalAlloc {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if !small(&l) {
            return System.alloc(l);
        }
        let c = class(l.size());
        // try_with: during thread teardown the lists may be gone; fall back.
        let p = FREE
            .try_with(|f| {
                let head = f[c].get();
                if !head.is_null() {
                    f[c].set(*(head as *mut *mut u8));
                }
                head
            })
            .unwrap_or(null_mut());
        if !p.is_null() {
            return p;
        }
        let p = carve((c + 1) * STEP);
        // The fallback block must have the full class size: it may be reused
        // for any request of this class once freed.
        if p.is_null() { System.alloc(Layout::from_size_align_unchecked((c + 1) * STEP, STEP)) } else { p }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if !small(&l) {
            return System.dealloc(p, l);
        }
        let c = class(l.size());
        // A small block can never be handed back to System (it may sit inside a
        // chunk); if the lists are gone (thread exit) it is simply leaked.
        let _ = FREE.try_with(|f| {
            *(p as *mut *mut u8) = f[c].get();
            f[c].set(p);
        });
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let nl = Layout::from_size_align_unchecked(new_size, l.align());
        match (small(&l), small(&nl)) {
            (false, false) => System.realloc(p, l, new_size),
            // Same size class: the block already has room.
            (true, true) if class(l.size()) == class(new_size) => p,
            _ => {
                let q = self.alloc(nl);
                if !q.is_null() {
                    std::ptr::copy_nonoverlapping(p, q, l.size().min(new_size));
                    self.dealloc(p, l);
                }
                q
            }
        }
    }
}
