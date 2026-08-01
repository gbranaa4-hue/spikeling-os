//! MILESTONE 30: real per-process address space isolation. Milestone 27
//! got exactly one ring-3 program running, but disclosed honestly in its
//! own report that it ran under the KERNEL's own page tables -- the user
//! code page and user stack page were mapped into the SAME
//! OffsetPageTable kernel_main built at boot, alongside every other
//! kernel mapping, with nothing but the USER_ACCESSIBLE flag standing
//! between "ring 3" and "can see the whole kernel address space". This
//! module closes that gap: each `Process` gets its own top-level page
//! table (PML4) in its own physical frame, entered via a real CR3 switch
//! before `iretq` and switched back on exit -- proven by running two
//! distinct hardcoded processes at the IDENTICAL virtual code address
//! (usertest::USER_CODE_ADDR) and showing each one genuinely executes
//! its own distinct physical memory, invisible to the other.
//!
//! The design deliberately does NOT deep-copy the kernel's page table
//! hierarchy into each process -- a naive full copy would silently go
//! stale the instant the kernel maps anything new later (e.g. heap
//! growth), a real bug class this avoids rather than risks:
//!
//!   - every PML4 entry OUTSIDE the user-space index is a raw COPY OF
//!     THE ENTRY ITSELF (a pointer to the kernel's existing, already-
//!     built P3 table), not a deep copy of the hierarchy underneath it
//!     -- so every process's kernel-space view stays bit-for-bit
//!     identical to the kernel's own forever, automatically, because
//!     it's the literal same physical P3 table in memory. Any future
//!     kernel mapping change is instantly visible to every process, with
//!     zero ongoing sync cost and zero staleness risk.
//!   - the ONE PML4 entry covering both usertest::USER_CODE_ADDR and
//!     usertest::USER_STACK_ADDR -- computed for real below (p4_index()
//!     on the actual addresses, not assumed), both landing on index 170
//!     -- is left zeroed in the new PML4, then `OffsetPageTable::map_to`
//!     is used against that fresh table to build a genuinely new,
//!     private P3/P2/P1 chain backed by this process's own physical
//!     frames. create_process() checks the two addresses actually share
//!     a p4 index at runtime and fails loudly (not silently) if that
//!     assumption is ever violated.
//!
//! Still exactly two hardcoded processes (no general loader, no
//! per-process heap or file descriptors -- later work). The isolation
//! itself is real: verified by CR3 genuinely changing (logged before and
//! after every switch), by each process's print syscall reading its OWN
//! message out of ITS OWN physical code-page frame at the SAME virtual
//! offset every other process uses, and by re-running process A after
//! process B with no cross-contamination.

use crate::serial;
use crate::usertest;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use spin::Mutex;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::page_table::PageTableIndex;
use x86_64::structures::paging::{FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

/// Offset (from the start of a process's own code page) where its
/// distinguishing message string is copied -- well past the 16-byte
/// hand-assembled program at offset 0 (see usertest.rs's USER_PROGRAM
/// doc comment for why that program is hand-assembled machine code
/// rather than compiled Rust).
const MESSAGE_OFFSET: u64 = 128;
const MAX_MESSAGE_LEN: usize = 64;
const PAGE_SIZE: usize = 4096;

pub struct Process {
    label: &'static str,
    pml4_frame: PhysFrame<Size4KiB>,
    code_frame: PhysFrame<Size4KiB>,
    stack_frame: PhysFrame<Size4KiB>,
}

static PROCESS_A: Mutex<Option<Process>> = Mutex::new(None);
static PROCESS_B: Mutex<Option<Process>> = Mutex::new(None);

/// The kernel's own PML4 physical frame + CR3 flags, saved ONCE at boot
/// (save_kernel_cr3(), called from kernel_main before any process is
/// created) via a real Cr3::read() -- restored verbatim on every
/// process's exit syscall, never reconstructed or guessed at.
static KERNEL_PML4_FRAME: AtomicU64 = AtomicU64::new(0);
static KERNEL_CR3_FLAGS_BITS: AtomicU64 = AtomicU64::new(0);

/// 0 = no process currently running (plain `usertest`, or idle);
/// nonzero = the id of the process currently executing in ring 3.
/// usertest.rs's syscall_dispatch reads this to decide whether syscall 0
/// should print the original Milestone 27 fixed kernel string or read a
/// per-process message out of the CURRENTLY-mapped user code page, and
/// whether syscall 1 needs to restore the kernel's own CR3 before
/// resuming kernel code.
pub(crate) static ACTIVE_PROCESS: AtomicU8 = AtomicU8::new(0);

/// MILESTONE 30: called once at boot, before any process's PML4 could
/// ever be loaded into CR3, so there's always a known-good value to
/// restore to on every process exit -- never inferred, never assumed
/// still-current.
pub fn save_kernel_cr3() {
    let (frame, flags) = Cr3::read();
    KERNEL_PML4_FRAME.store(frame.start_address().as_u64(), Ordering::SeqCst);
    KERNEL_CR3_FLAGS_BITS.store(flags.bits(), Ordering::SeqCst);
    let _ = writeln!(
        serial(),
        "milestone 30: saved kernel's own PML4 frame {:#x} (cr3 flags {:#x}) for restore-on-exit",
        frame.start_address().as_u64(),
        flags.bits()
    );
}

/// MILESTONE 30: switches CR3 back to the kernel's own, original PML4.
/// Called from usertest.rs's syscall_dispatch, exit-syscall arm, BEFORE
/// resume_kernel() hands control back to ordinary kernel code -- see
/// that call site's own comment for why this ordering is enforced
/// explicitly rather than relied upon implicitly.
pub(crate) fn restore_kernel_cr3() {
    let frame_addr = KERNEL_PML4_FRAME.load(Ordering::SeqCst);
    let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(frame_addr))
        .expect("saved kernel PML4 frame address was not 4KiB-aligned");
    let flags = Cr3Flags::from_bits_truncate(KERNEL_CR3_FLAGS_BITS.load(Ordering::SeqCst));
    unsafe { Cr3::write(frame, flags) };
}

/// MILESTONE 30: reads the currently-active process's distinguishing
/// message directly out of USER_CODE_ADDR+MESSAGE_OFFSET -- called from
/// syscall_dispatch's PRINT arm while CR3 is STILL the active process's
/// own PML4 (the exit syscall is what switches CR3 back, not this one),
/// so this pointer read resolves through THAT process's private P3/P2/P1
/// chain to ITS OWN physical frame's content. This is the actual
/// isolation proof: same virtual address every time, genuinely different
/// bytes depending on which process is currently loaded in CR3.
pub(crate) fn read_active_message() -> String {
    let ptr = (usertest::USER_CODE_ADDR + MESSAGE_OFFSET) as *const u8;
    let mut buf = Vec::with_capacity(MAX_MESSAGE_LEN);
    for i in 0..MAX_MESSAGE_LEN {
        let b = unsafe { core::ptr::read(ptr.add(i)) };
        if b == 0 {
            break;
        }
        buf.push(b);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn create_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    label: &'static str,
    message: &str,
) -> Result<Process, &'static str> {
    let user_p4_index = VirtAddr::new(usertest::USER_CODE_ADDR).p4_index();
    let stack_p4_index = VirtAddr::new(usertest::USER_STACK_ADDR).p4_index();
    if user_p4_index != stack_p4_index {
        return Err("USER_CODE_ADDR/USER_STACK_ADDR fall in different PML4 indices -- design assumption violated");
    }
    let _ = writeln!(
        serial(),
        "milestone 30: process {label} -- user p4 index computed as {} (code {:#x}, stack {:#x})",
        u16::from(user_p4_index),
        usertest::USER_CODE_ADDR,
        usertest::USER_STACK_ADDR
    );

    let new_pml4_frame = frame_allocator.allocate_frame().ok_or("out of physical frames (pml4)")?;
    let new_pml4_ptr: *mut PageTable = (phys_mem_offset + new_pml4_frame.start_address().as_u64()).as_mut_ptr();
    let new_pml4: &mut PageTable = unsafe { &mut *new_pml4_ptr };
    new_pml4.zero();

    let (kernel_pml4_frame, _) = Cr3::read();
    let kernel_pml4_ptr: *const PageTable = (phys_mem_offset + kernel_pml4_frame.start_address().as_u64()).as_ptr();
    let kernel_pml4: &PageTable = unsafe { &*kernel_pml4_ptr };

    // MILESTONE 30, the core design step: share every kernel-space PML4
    // entry by copying the ENTRY (a pointer to the kernel's existing P3
    // table), NOT the hierarchy underneath it -- only user_p4_index is
    // left zeroed here, so map_to() below gives it a genuinely fresh,
    // private chain instead.
    for i in 0u16..512 {
        let idx = PageTableIndex::new(i);
        if idx != user_p4_index {
            new_pml4[idx] = kernel_pml4[idx].clone();
        }
    }
    let _ = writeln!(
        serial(),
        "milestone 30: process {label} -- new pml4 {:#x} populated: 511 kernel-space entries shared, index {} left private",
        new_pml4_frame.start_address().as_u64(),
        u16::from(user_p4_index)
    );

    let mut process_mapper = unsafe { OffsetPageTable::new(new_pml4, phys_mem_offset) };

    let code_frame = frame_allocator.allocate_frame().ok_or("out of physical frames (code)")?;
    let stack_frame = frame_allocator.allocate_frame().ok_or("out of physical frames (stack)")?;
    let code_page = Page::<Size4KiB>::containing_address(VirtAddr::new(usertest::USER_CODE_ADDR));
    let stack_page = Page::<Size4KiB>::containing_address(VirtAddr::new(usertest::USER_STACK_ADDR));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

    unsafe {
        process_mapper
            .map_to(code_page, code_frame, flags, frame_allocator)
            .map_err(|_| "map_to failed (code page)")?
            .flush();
        process_mapper
            .map_to(stack_page, stack_frame, flags, frame_allocator)
            .map_err(|_| "map_to failed (stack page)")?
            .flush();
    }
    let _ = writeln!(
        serial(),
        "milestone 30: process {label} -- private code page mapped to frame {:#x}, private stack page mapped to frame {:#x}",
        code_frame.start_address().as_u64(),
        stack_frame.start_address().as_u64()
    );

    // Written through the phys-mem-offset DIRECT view of this process's
    // own physical frame -- deliberately NOT through USER_CODE_ADDR
    // under the currently-loaded (kernel) CR3, which would write into
    // whatever THAT table's user_p4_index chain currently maps (a
    // different physical frame entirely) instead of this new process's
    // own, freshly-mapped one.
    let code_virt = phys_mem_offset + code_frame.start_address().as_u64();
    let code_ptr: *mut u8 = code_virt.as_mut_ptr();
    unsafe {
        core::ptr::write_bytes(code_ptr, 0, PAGE_SIZE);
        core::ptr::copy_nonoverlapping(usertest::USER_PROGRAM.as_ptr(), code_ptr, usertest::USER_PROGRAM.len());

        let msg_bytes = message.as_bytes();
        let len = msg_bytes.len().min(MAX_MESSAGE_LEN - 1);
        let msg_ptr = code_ptr.add(MESSAGE_OFFSET as usize);
        core::ptr::copy_nonoverlapping(msg_bytes.as_ptr(), msg_ptr, len);
        core::ptr::write(msg_ptr.add(len), 0u8);
    }

    Ok(Process {
        label,
        pml4_frame: new_pml4_frame,
        code_frame,
        stack_frame,
    })
}

/// MILESTONE 30: creates process A and process B, each with its own
/// PML4/code frame/stack frame, printing "hello from process A" /
/// "hello from process B" respectively -- called once at boot, mirroring
/// usertest::setup()'s own once-at-boot pattern, while a live
/// frame_allocator is conveniently already in scope in kernel_main.
pub fn init_test_processes(frame_allocator: &mut impl FrameAllocator<Size4KiB>, phys_mem_offset: VirtAddr) -> Result<(), &'static str> {
    let _ = writeln!(serial(), "milestone 30: creating process A's private address space...");
    let a = create_process(frame_allocator, phys_mem_offset, "A", "hello from process A")?;
    let _ = writeln!(
        serial(),
        "milestone 30: process A created -- pml4={:#x} code={:#x} stack={:#x}",
        a.pml4_frame.start_address().as_u64(),
        a.code_frame.start_address().as_u64(),
        a.stack_frame.start_address().as_u64()
    );
    *PROCESS_A.lock() = Some(a);

    let _ = writeln!(serial(), "milestone 30: creating process B's private address space...");
    let b = create_process(frame_allocator, phys_mem_offset, "B", "hello from process B")?;
    let _ = writeln!(
        serial(),
        "milestone 30: process B created -- pml4={:#x} code={:#x} stack={:#x}",
        b.pml4_frame.start_address().as_u64(),
        b.code_frame.start_address().as_u64(),
        b.stack_frame.start_address().as_u64()
    );
    *PROCESS_B.lock() = Some(b);

    Ok(())
}

/// MILESTONE 30: the `runproc N` shell command's entry point. Switches
/// CR3 to process N's own PML4, enters ring 3 at the SAME virtual
/// address usertest.rs always uses, lets the syscalls run (print reads
/// this process's own embedded message, exit restores the kernel's own
/// CR3 and returns), and comes back here once resume_kernel() has
/// unwound back through enter_ring3_now()'s call. Safe to call
/// repeatedly and in any order (1, 2, 1, 2, 2, 1, ...): each call reads
/// the process's CURRENT pml4_frame out of its Mutex slot fresh, and the
/// process's own frames are never mutated by a run, only by
/// create_process() at boot.
pub fn run(id: u8) -> Result<(), &'static str> {
    let slot = match id {
        1 => &PROCESS_A,
        2 => &PROCESS_B,
        _ => return Err("no such process -- use 1 or 2"),
    };
    let (pml4_frame, label) = {
        let guard = slot.lock();
        let proc = guard.as_ref().ok_or("process not initialized")?;
        (proc.pml4_frame, proc.label)
    };

    let _ = writeln!(
        serial(),
        "milestone 30: runproc {id} (process {label}) -- about to switch CR3 to process pml4 {:#x}",
        pml4_frame.start_address().as_u64()
    );
    ACTIVE_PROCESS.store(id, Ordering::SeqCst);

    let flags = Cr3Flags::from_bits_truncate(KERNEL_CR3_FLAGS_BITS.load(Ordering::SeqCst));
    unsafe { Cr3::write(pml4_frame, flags) };
    let _ = writeln!(
        serial(),
        "milestone 30: CR3 switched -- entering ring 3 for process {id} at {:#x}",
        usertest::USER_CODE_ADDR
    );

    usertest::enter_ring3_now();

    let _ = writeln!(
        serial(),
        "milestone 30: runproc {id} -- resumed in kernel context (CR3 already restored by the exit syscall before this point)"
    );
    Ok(())
}
