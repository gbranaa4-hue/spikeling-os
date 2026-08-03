//! Heap setup: maps a fixed virtual region and hands it to
//! `linked_list_allocator` as the global allocator, so `alloc::vec::Vec`,
//! `Box`, etc. actually work in the kernel -- required before any of
//! Spikeling's runtime (which allocates) can run in-kernel.

use linked_list_allocator::LockedHeap;
use x86_64::{
    VirtAddr,
    structures::paging::{
        FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB, mapper::MapToError,
    },
};

pub const HEAP_START: usize = 0x_4444_4444_0000;
// TESTED AND REFUTED (root-causing the Milestone 36 disclosed page
// fault): tried bumping this 10x (100 KiB -> 1 MiB) on the theory that
// heap exhaustion/fragmentation pressure -- from network.rs's tick()
// allocating+freeing a `fired: Vec` on every timer tick any neuron
// fires, running continuously in the background (Milestone 25) the
// entire time the shell sits idle -- was the real mechanism behind the
// disclosed runfile/runelf page fault. Reverted: 10 repeated real trials
// at 1 MiB still failed 6/10 times, statistically indistinguishable from
// the ~60-70% rate at 100 KiB. Heap SIZE is not the mechanism. See
// README.md's Milestone 36 entry for the full, still-open investigation
// (scheduler-preemption also directly refuted; both real, honest
// negative results, not guesses).
pub const HEAP_SIZE: usize = 100 * 1024; // 100 KiB -- small on purpose for
// milestone 3; the point here is proving map+alloc actually work end to
// end, not sizing for real workloads yet.

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }

    unsafe {
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }

    Ok(())
}
