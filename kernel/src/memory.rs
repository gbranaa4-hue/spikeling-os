//! Physical memory management: turns bootloader-reported memory regions
//! into a frame allocator, and provides an OffsetPageTable for creating
//! new virtual mappings -- both required before a heap allocator can
//! carve out and map new pages (see allocator.rs).

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
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
