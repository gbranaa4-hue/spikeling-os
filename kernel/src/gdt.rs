//! GDT + TSS, required before the IDT: the double-fault handler needs a
//! known-good, separate stack (via the TSS's Interrupt Stack Table) --
//! a double fault often happens BECAUSE the current stack is already
//! corrupted (e.g. a stack overflow), so handling it on the same stack
//! would just triple-fault instead of producing a readable panic.
//!
//! MILESTONE 27: adds a user code segment and user data segment (DPL=3)
//! -- the first two GDT entries in this kernel that anything other than
//! ring 0 can legally use -- plus TSS.privilege_stack_table[0], the
//! stack the CPU automatically switches RSP to on any ring3->ring0
//! transition through an interrupt/trap gate (e.g. the int 0x80 syscall
//! gate in interrupts.rs). Leaving that entry zeroed/garbage is a
//! classic real bug: the CPU would load RSP=0 for the very first
//! privilege-elevating interrupt and immediately fault trying to push
//! the interrupt frame there.

use lazy_static::lazy_static;
use x86_64::VirtAddr;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            stack_start + STACK_SIZE as u64
        };
        // MILESTONE 27: this is what the CPU loads into RSP (with SS
        // auto-zeroed, per the same long-mode "SS is unused/nulled on a
        // privilege-elevating stack switch" behavior the comment in
        // init() below already documents) the instant a ring3 program
        // executes `int 0x80` -- a dedicated stack, entirely separate
        // from the double-fault IST stack above and from any task's own
        // stack, since a syscall can be taken while literally any task
        // (or kernel_main itself) is current.
        tss.privilege_stack_table[0] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            stack_start + STACK_SIZE as u64
        };
        tss
    };
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        // MILESTONE 27: order doesn't matter for correctness here (we
        // enter/return via a hand-built iretq frame + int 0x80, never
        // via SYSCALL/SYSRET, which is the only mechanism that cares
        // about a fixed kernel/user segment ordering) -- data segment
        // appended before code segment simply to keep the two new
        // ring-3 entries adjacent and readable.
        let user_data_selector = gdt.append(Descriptor::user_data_segment());
        let user_code_selector = gdt.append(Descriptor::user_code_segment());
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));
        (
            gdt,
            Selectors {
                code_selector,
                user_data_selector,
                user_code_selector,
                tss_selector,
            },
        )
    };
}

struct Selectors {
    code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

pub fn init() {
    use x86_64::instructions::segmentation::{CS, SS, Segment};
    use x86_64::instructions::tables::load_tss;
    use x86_64::structures::gdt::SegmentSelector;
    use x86_64::PrivilegeLevel;

    GDT.0.load();
    unsafe {
        // DIAGNOSED: without this, SS keeps whatever selector value the
        // BOOTLOADER's own GDT left behind -- bootloader_api 0.11's GDT
        // layout differs from what older tutorials assumed, and that
        // stale value (index 2) happened to land on THIS table's TSS
        // descriptor once loaded, not a valid data segment. Confirmed
        // via a real breakpoint test: the handler fired correctly, but
        // returning from it double-faulted (no GP-fault handler
        // registered, so an invalid segment on iretq escalated
        // straight to double fault) until SS was explicitly reloaded
        // to null here -- valid in 64-bit long mode, where SS's actual
        // descriptor contents are mostly unused.
        SS::set_reg(SegmentSelector::new(0, PrivilegeLevel::Ring0));
        CS::set_reg(GDT.1.code_selector);
        load_tss(GDT.1.tss_selector);
    }
}

/// MILESTONE 27: the ring-3 code selector, RPL already baked in as 3 --
/// `GlobalDescriptorTable::append` returns `SegmentSelector::new(index,
/// entry.dpl())`, and `Descriptor::user_code_segment()`'s dpl() is
/// Ring3, so this value is already legal to load directly into CS via
/// an iretq frame with no further `| 3` needed.
pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code_selector
}

/// MILESTONE 27: the ring-3 data selector (used for SS on the way into
/// ring 3) -- same RPL-already-baked-in note as user_code_selector().
pub fn user_data_selector() -> SegmentSelector {
    GDT.1.user_data_selector
}
