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

/// MILESTONE 37: a SECOND, dedicated ring-0 stack, structurally
/// identical to TSS's own privilege_stack_table[0] stack above but used
/// ONLY while process::run_forked_child() is driving a forked child's
/// nested ring-3 excursion (from inside the PARENT's own already-
/// in-flight `wait()` syscall) -- see set_syscall_stack_top()'s own doc
/// comment for the real bug this exists to fix, found and diagnosed via
/// an ACTUAL page fault (instruction fetch at VirtAddr(0x200)) during
/// real QEMU testing, not guessed at.
const CHILD_EXCURSION_STACK_SIZE: usize = 4096 * 5;
static mut CHILD_EXCURSION_STACK: [u8; CHILD_EXCURSION_STACK_SIZE] = [0; CHILD_EXCURSION_STACK_SIZE];

/// The top-of-stack address for CHILD_EXCURSION_STACK above -- same
/// `&raw const` + VirtAddr::from_ptr() pattern TSS's own two stacks use
/// in the lazy_static! block up top.
pub fn child_excursion_stack_top() -> VirtAddr {
    let stack_start = VirtAddr::from_ptr(&raw const CHILD_EXCURSION_STACK);
    stack_start + CHILD_EXCURSION_STACK_SIZE as u64
}

/// MILESTONE 37: temporarily repoints `privilege_stack_table[0]` --
/// the ONE stack the CPU loads RSP from on EVERY ring3->ring0
/// transition, `int 0x80` included, ALWAYS reset to the exact same
/// fixed top address regardless of what was on it before -- at a
/// caller-supplied stack top, returning the PREVIOUS value so the
/// caller can restore it once done.
///
/// **The real, diagnosed bug this fixes**: process::wait_for_child()
/// drives a forked child to completion by entering ring 3 for it FROM
/// INSIDE the PARENT's own already-in-flight `wait()` syscall dispatch
/// (itself reached via the PARENT's own `int 0x80`, whose CPU-pushed
/// InterruptStackFrame -- the parent's saved rip/cs/rflags/rsp/ss,
/// required to correctly resume the PARENT once wait() returns -- sits
/// at the TOP of the shared privilege_stack_table[0] stack). Without
/// this fix, the FIRST syscall the child itself makes (this
/// milestone's own test child calls `exec()` immediately) triggers
/// ANOTHER `int 0x80`, which -- because privilege_stack_table[0] always
/// resets to the SAME fixed top -- pushes the CHILD's own
/// InterruptStackFrame at the EXACT SAME address as the PARENT's,
/// silently destroying it. Confirmed the hard way in a real QEMU boot:
/// fork()/wait()/exec() all ran and logged correctly (child created,
/// parent's message printed, child resumed at its exact fork()-time
/// rip, exec() replaced its code, the exec()'d program's write()
/// printed correctly) right up until the child's own exit() -- which
/// resumed into a stack frame already overwritten by the exec/write
/// syscalls that ran in between, producing a real page fault
/// (instruction fetch at VirtAddr(0x200), a bogus address popped off a
/// corrupted stack) instead of correctly returning to the parent.
///
/// process::run_forked_child() calls this with
/// `child_excursion_stack_top()` immediately before entering the
/// child, and again with the returned OLD value immediately after --
/// so every syscall the CHILD makes during that window lands on its
/// own separate CHILD_EXCURSION_STACK, leaving the PARENT's own
/// in-flight wait() syscall's saved frame (on the original, shared
/// stack) completely untouched.
///
/// Mutates the lazy_static TSS's live memory directly through an
/// unsafe cast from the shared `&'static` reference lazy_static
/// normally hands out -- safe here specifically because this kernel is
/// single-core and every call to this function brackets one
/// synchronous, non-reentrant excursion (this milestone's own design
/// enforces at most one nested child excursion at a time -- see
/// usertest::is_in_child_resume()). The CPU re-reads this field fresh
/// from the TSS's own memory on every ring3->ring0 transition (it is
/// not cached anywhere else, e.g. in a register), so the new value
/// takes effect immediately, with no need to reload TR / call
/// load_tss() again.
pub fn set_syscall_stack_top(new_top: VirtAddr) -> VirtAddr {
    let tss_ptr: *mut TaskStateSegment = (&*TSS as *const TaskStateSegment).cast_mut();
    unsafe {
        let old = (*tss_ptr).privilege_stack_table[0];
        (*tss_ptr).privilege_stack_table[0] = new_top;
        old
    }
}
