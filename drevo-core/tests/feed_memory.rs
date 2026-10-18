//! Trimming the change feed gives its memory back: the buffer that held the
//! trimmed history shrinks, it does not stay allocated at its peak size.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use drevo_core::model::{NewNode, Properties};
use drevo_core::native::NativeGraph;

/// The system allocator, counting the bytes currently allocated.
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call is forwarded to `System` unchanged; the counter is
// bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with this layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn junction() -> NewNode {
    NewNode {
        kind: "Junction".to_string(),
        title: String::new(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

#[test]
fn a_trimmed_feed_frees_its_buffer() {
    const N: usize = 50_000;
    let g = NativeGraph::new();
    g.create_nodes((0..N).map(|_| junction()).collect())
        .unwrap();
    let before = LIVE.load(Ordering::Relaxed);
    g.trim_before(g.change_head());
    let freed = before - LIVE.load(Ordering::Relaxed);
    // Every retained op holds a whole node record; trimming must release
    // at least the slots that held them, not only what they pointed to.
    let slot = std::mem::size_of::<drevo_core::native::WalOp>() as isize;
    assert!(
        freed >= slot * N as isize,
        "trimming {N} ops freed {freed} bytes, less than their {} bytes of slots",
        slot * N as isize
    );
    assert_eq!(g.change_floor(), g.change_head());
}
