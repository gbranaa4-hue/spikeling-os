//! Physical memory management: turns bootloader-reported memory regions
//! into a frame allocator, and provides an OffsetPageTable for creating
//! new virtual mappings -- both required before a heap allocator can
//! carve out and map new pages (see allocator.rs).
//!
//! MILESTONE 34: every frame allocation through Milestone 30 happened
//! with a `frame_allocator` local to kernel_main, threaded explicitly
//! through a handful of ONE-TIME boot setup calls (init_heap,
//! usertest::setup, process::init_test_processes). loader.rs's
//! `runfile` shell command is the first thing that needs to allocate a
//! fresh physical frame from ARBITRARY, LATER, shell-command-driven
//! code -- long after kernel_main's own boot sequence has finished --
//! so a real place to reach a live BootInfoFrameAllocator (and the
//! phys_mem_offset needed to translate its frames into accessible
//! pointers) from has to exist. install_frame_allocator()/
//! set_phys_mem_offset() publish exactly that, called once from
//! kernel_main right after the last boot-time consumer is done with
//! them.

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::{
    PhysAddr, VirtAddr,
    registers::control::Cr3,
    structures::paging::{FrameAllocator, OffsetPageTable, PageTable, PhysFrame, Size4KiB},
};

/// Builds an OffsetPageTable from the currently active level-4 page
/// table, using `physical_memory_offset` (mapped by the bootloader
/// because BOOTLOADER_CONFIG in main.rs requests it) to translate
/// physical frame addresses into accessible virtual addresses.
///
/// # Safety
/// Caller must guarantee the complete physical memory is actually
/// mapped at `physical_memory_offset`, and this must only be called
/// once (aliasing more than one `&mut` to the level 4 table is UB).
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level_4_table = unsafe { active_level_4_table(physical_memory_offset) };
    unsafe { OffsetPageTable::new(level_4_table, physical_memory_offset) }
}

unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

/// A frame allocator that returns usable frames from the bootloader's
/// memory map -- real physical memory the firmware reported as free,
/// not a synthetic pool. Bump-allocates only (never frees); fine for
/// milestone 3's fixed-size heap, revisit if/when frames need to be
/// reclaimed.
pub struct BootInfoFrameAllocator {
    memory_regions: &'static MemoryRegions,
    next: usize,
}

impl BootInfoFrameAllocator {
    /// # Safety
    /// The passed memory regions must be accurate -- frames marked
    /// `Usable` must genuinely not be used elsewhere (kernel code/data,
    /// the boot info structure itself, etc.). True of what
    /// bootloader_api reports; would not be true of an arbitrary or
    /// synthetic memory map.
    pub unsafe fn init(memory_regions: &'static MemoryRegions) -> Self {
        BootInfoFrameAllocator {
            memory_regions,
            next: 0,
        }
    }

    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        self.memory_regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .map(|r| r.start..r.end)
            .flat_map(|r| r.step_by(4096))
            .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}

/// MILESTONE 34: the globally-published frame allocator -- see the
/// module doc comment above. None until install_frame_allocator() runs
/// (once, from kernel_main); Some(...) permanently after that, for the
/// rest of the kernel's uptime.
static FRAME_ALLOCATOR: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);

/// The physical-memory-offset VirtAddr, stored as a raw u64 (VirtAddr
/// itself isn't atomic-friendly) -- same pattern process.rs's
/// KERNEL_PML4_FRAME already established for publishing a single,
/// boot-computed value for later, arbitrary-time reads.
static PHYS_MEM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Called once from kernel_main, after `allocator` (the very same
/// BootInfoFrameAllocator that already ran init_heap/usertest::setup/
/// process::init_test_processes) has nothing left to do at boot time --
/// moves it into this static so loader.rs's `runfile` can allocate
/// fresh frames for a NEW process's private page tables at any later
/// point, driven by an arbitrary shell command instead of one of the
/// fixed boot-time setup calls.
pub fn install_frame_allocator(allocator: BootInfoFrameAllocator) {
    *FRAME_ALLOCATOR.lock() = Some(allocator);
}

/// Called once from kernel_main, alongside install_frame_allocator().
pub fn set_phys_mem_offset(offset: VirtAddr) {
    PHYS_MEM_OFFSET.store(offset.as_u64(), Ordering::SeqCst);
}

/// The published phys_mem_offset -- 0 (an invalid/unused value in
/// practice; the bootloader's real dynamic mapping is never placed at
/// virtual address 0) until set_phys_mem_offset() has run.
pub fn phys_mem_offset() -> VirtAddr {
    VirtAddr::new(PHYS_MEM_OFFSET.load(Ordering::SeqCst))
}

/// Runs `f` with mutable access to the globally-published frame
/// allocator, returning None (rather than panicking) if
/// install_frame_allocator() hasn't run yet -- shouldn't be reachable
/// once the shell is accepting commands (kernel_main installs it well
/// before shell::init()/interrupts are enabled), but reported honestly
/// through the Option rather than assumed.
pub fn with_frame_allocator<R>(f: impl FnOnce(&mut BootInfoFrameAllocator) -> R) -> Option<R> {
    let mut guard = FRAME_ALLOCATOR.lock();
    guard.as_mut().map(f)
}
