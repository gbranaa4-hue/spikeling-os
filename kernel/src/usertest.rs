//! MILESTONE 27: real ring-3 (CPL=3) execution and a minimal `int 0x80`
//! syscall ABI. Every single thing that has run in this kernel through
//! Milestone 26 -- the shell, the neuron simulation, every device
//! driver, all 8 fixed+spawned worker tasks from Milestone 25 -- has run
//! at CPL=0. This module is the first code in spikeling-os that actually
//! drops to CPL=3 and proves it with hardware-recorded evidence, not a
//! self-reported claim -- the real prerequisite for the project's
//! longer-term goal of eventually running code not written specifically
//! for this kernel.
//!
//! Scope, deliberately minimal (see the milestone report for the full
//! list): ONE hardcoded ring-3 program, ONE mapped user code page, ONE
//! mapped user stack page, exactly two syscalls (0 = print a FIXED
//! kernel-owned message -- no general copy-from-user pointer-safety
//! mechanism exists yet, deliberately out of scope this milestone -- and
//! 1 = exit back to whatever called into ring 3). No per-process
//! isolation, no scheduler integration for user tasks: Milestone 28+
//! territory.

use crate::gdt;
use crate::serial;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};

pub const USER_CODE_ADDR: u64 = 0x_5555_5000_0000;
pub const USER_STACK_ADDR: u64 = 0x_5555_6000_0000;
pub const USER_STACK_SIZE: u64 = 4096;

/// Hand-assembled x86_64 machine code, not bytes copied out of a
/// compiled Rust function: `mov eax,0 ; int 0x80 ; mov eax,1 ; int 0x80 ;
/// jmp $`. Hand-assembled deliberately -- a naked Rust function's exact
/// compiled instruction-byte LENGTH isn't something Rust exposes (no
/// linker symbol for "end of this specific function" without a fragile
/// assumption about link order), whereas 5 fixed, well-known x86_64
/// instructions are trivial to encode by hand, so their length is simply
/// KNOWN rather than guessed at.
///
///   B8 00 00 00 00  mov eax, 0     syscall number 0 = print
///   CD 80           int 0x80
///   B8 01 00 00 00  mov eax, 1     syscall number 1 = exit
///   CD 80           int 0x80
///   EB FE           jmp $          safety net: only reached if exit
///                                  somehow returns instead of resuming
///                                  kernel_main -- spins in ring 3
///                                  forever rather than running off into
///                                  whatever bytes follow in the page.
pub(crate) static USER_PROGRAM: [u8; 16] = [
    0xB8, 0x00, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xEB, 0xFE,
];

static MAPPED: AtomicBool = AtomicBool::new(false);
static RUN_COUNT: AtomicU64 = AtomicU64::new(0);

/// Where enter_ring3() stashes the kernel-side rsp (plus, on that same
/// stack, the callee-saved registers and return address it pushed)
/// immediately before iretq-ing into ring 3 -- mirrors tasks.rs's
/// KERNEL_RSP exactly, just scoped to this module's own excursion
/// instead of the task scheduler's. The exit syscall hands this straight
/// to resume_kernel(), which pops back into run() as if enter_ring3()
/// had simply returned normally after some delay.
static KERNEL_RSP: AtomicU64 = AtomicU64::new(0);

/// MILESTONE 27: allocates one physical frame for the user code page and
/// one for the user stack page, maps both PRESENT | WRITABLE |
/// USER_ACCESSIBLE (no NO_EXECUTE set, matching every other mapping
/// already made in this kernel -- allocator.rs's heap pages included --
/// so the code page is executable), and copies USER_PROGRAM into the
/// mapped code page. Idempotent: a second call is a no-op, so repeated
/// `usertest` shell invocations never re-map or re-copy.
///
/// Called once at boot time, while kernel_main's own mapper/
/// frame_allocator are still in scope -- simpler than threading a
/// `Mutex<Option<OffsetPageTable>>` through the rest of the kernel for a
/// one-time setup step that only this module ever needs again.
pub fn setup(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), &'static str> {
    if MAPPED.load(Ordering::SeqCst) {
        return Ok(());
    }

    let code_page = Page::<Size4KiB>::containing_address(VirtAddr::new(USER_CODE_ADDR));
    let stack_page = Page::<Size4KiB>::containing_address(VirtAddr::new(USER_STACK_ADDR));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

    for page in [code_page, stack_page] {
        let frame: PhysFrame<Size4KiB> = frame_allocator.allocate_frame().ok_or("out of physical frames")?;
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .map_err(|_| "map_to failed")?
                .flush();
        }
    }

    unsafe {
        let code_ptr = USER_CODE_ADDR as *mut u8;
        core::ptr::copy_nonoverlapping(USER_PROGRAM.as_ptr(), code_ptr, USER_PROGRAM.len());
    }

    MAPPED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Register layout as saved by syscall_entry's push sequence, read back
/// by syscall_dispatch. Field order (top/lowest address first) mirrors
/// the naked trampoline's push order exactly: the LAST register pushed
/// ends up at the LOWEST address, i.e. this struct's FIRST field --
/// getting this backwards silently reads the wrong register into the
/// wrong field without any compiler error, so it was checked by hand
/// against the actual naked_asm! push list below, not assumed.
#[repr(C)]
struct SyscallRegs {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    // CPU-pushed InterruptStackFrame, immediately following the GPRs we
    // pushed -- int 0x80 from ring 3 pushes no error code, so this
    // starts right after rax with no gap.
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// MILESTONE 27: the int 0x80 entry point itself. `extern "x86-interrupt"`
/// handlers do NOT expose general-purpose registers (only the
/// InterruptStackFrame), so syscall number/arguments passed in registers
/// (the standard convention, rax = syscall number here) are unreachable
/// from one -- this naked function pushes every GPR onto the stack
/// itself, calls the ordinary `extern "C"` syscall_dispatch with a
/// pointer to the saved registers, then pops them back and `iretq`s,
/// exactly the technique tasks.rs's switch_to established as this
/// codebase's precedent for hand-written stack/register manipulation.
///
/// Installed directly via `Entry::set_handler_addr` (not
/// `set_handler_fn`, which requires the `extern "x86-interrupt"`
/// signature this function deliberately does NOT have) at IDT vector
/// 0x80, DPL=3 -- interrupts.rs sets that explicitly, since a gate's
/// default DPL is 0 and a ring-3 `int 0x80` against a DPL=0 gate
/// immediately #GP-faults rather than running the handler.
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp",
        "call {dispatch}",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        "iretq",
        dispatch = sym syscall_dispatch,
    );
}

/// Ordinary Rust, called (via a plain "call") from syscall_entry's asm
/// with rdi = pointer to the just-saved SyscallRegs. Reads the syscall
/// number from the saved rax; for syscall 0 (print) simply returns,
/// letting syscall_entry pop registers and iretq back into ring 3 for
/// the second `int 0x80`; for syscall 1 (exit) never returns to
/// syscall_entry at all -- calls resume_kernel() directly, which
/// abandons this call's own stack frame (on the TSS.privilege_stack_table[0]
/// syscall stack) entirely and jumps back into run()'s caller instead.
extern "C" fn syscall_dispatch(regs: *mut SyscallRegs) {
    let regs = unsafe { &*regs };
    // MILESTONE 27, the crucial verification detail: this CS came from
    // the CPU's OWN interrupt-frame push, not anything the (potentially
    // buggy) ring-3 code claimed about itself -- its low 2 bits are the
    // hardware-recorded CPL at the moment `int 0x80` executed. This is
    // the actual proof CPL=3 was real, logged unconditionally on every
    // syscall regardless of which one it is.
    let hardware_cpl = regs.cs & 0b11;
    // MILESTONE 30: ACTIVE_PROCESS is nonzero exactly while a
    // process.rs-owned process is the one running in ring 3 (set by
    // process::run() right before the CR3 switch, cleared below right
    // after restoring the kernel's own CR3). 0 means this is a plain,
    // unmodified `usertest` excursion -- still under the kernel's own
    // page tables the whole time, exactly as Milestone 27 left it.
    let active = crate::process::ACTIVE_PROCESS.load(Ordering::SeqCst);
    match regs.rax {
        0 => {
            if active != 0 {
                // Real proof of isolation, not a label: this reads
                // USER_CODE_ADDR+128 through whatever CR3 is CURRENTLY
                // loaded (still the active process's own PML4 -- the
                // exit syscall below is what switches CR3 back, not
                // this one), so it resolves to THIS process's own
                // physical code-page frame, not the other process's or
                // any kernel-owned string.
                let msg = crate::process::read_active_message();
                let _ = writeln!(
                    serial(),
                    "milestone 30: syscall PRINT (process {active}) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- \"{msg}\"",
                    regs.cs
                );
            } else {
                let _ = writeln!(
                    serial(),
                    "milestone 27: syscall PRINT -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- \"hello from ring 3, CPL=3 confirmed\"",
                    regs.cs
                );
            }
        }
        1 => {
            let _ = writeln!(
                serial(),
                "milestone {}: syscall EXIT -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- discarding ring-3 context, resuming kernel context",
                if active != 0 { 30 } else { 27 },
                regs.cs
            );
            if active != 0 {
                // MILESTONE 30: restore the kernel's own original PML4
                // BEFORE resume_kernel() hands control back to ordinary
                // kernel code, per the milestone's required ordering --
                // real belt-and-suspenders here, since the shared-entry
                // design above means kernel code would actually stay
                // reachable even under the process's own CR3 (every
                // kernel-space PML4 entry is a shared pointer into the
                // SAME P3 tables), but restoring explicitly and
                // immediately is the honest, verified-not-assumed
                // choice rather than relying on that as an excuse to
                // skip it.
                crate::process::restore_kernel_cr3();
                crate::process::ACTIVE_PROCESS.store(0, Ordering::SeqCst);
            }
            let saved = KERNEL_RSP.load(Ordering::SeqCst);
            unsafe { resume_kernel(saved) };
        }
        other => {
            let _ = writeln!(
                serial(),
                "milestone 27: unknown syscall {other} from CPL={hardware_cpl} -- ignoring"
            );
        }
    }
}

/// MILESTONE 27: switches from the current (kernel) context into ring 3.
/// Mirrors tasks.rs's switch_to's own opening half exactly -- saves the
/// callee-saved registers a normal `extern "C"` caller (run(), below)
/// would expect preserved across a call, and the resulting rsp, into
/// `*kernel_rsp_slot` -- then, instead of switching to another kernel
/// stack like switch_to does, builds a genuine iretq frame by hand on
/// the CURRENT stack and executes it. Args arrive per the SysV ABI (6
/// integer args: rdi, rsi, rdx, rcx, r8, r9), matching the parameter
/// list below 1:1 -- checked against the actual asm, not assumed.
#[unsafe(naked)]
unsafe extern "C" fn enter_ring3(
    _kernel_rsp_slot: *mut u64,
    _user_rsp: u64,
    _user_rip: u64,
    _user_cs: u64,
    _user_ss: u64,
    _rflags: u64,
) {
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "push r8",  // SS
        "push rsi", // RSP (user stack top)
        "push r9",  // RFLAGS
        "push rcx", // CS
        "push rdx", // RIP
        "iretq",
    );
}

/// MILESTONE 27: the exit syscall's actual mechanism -- switches rsp to
/// the kernel-side stack enter_ring3() saved, pops back the same 6
/// callee-saved registers it pushed there, then `ret`s using the return
/// address still sitting on that stack from enter_ring3()'s own call
/// site: control resumes in run(), immediately after its
/// `enter_ring3(...)` call, as if that call had simply returned
/// normally.
///
/// DIAGNOSED AND FIXED: the first real test of this (see the milestone
/// report) copied tasks.rs's switch_to pattern exactly, including its
/// unconditional `sti` before `ret` -- and hung the shell completely
/// after exactly one `usertest` run, identically every time. Root cause:
/// switch_to's `sti` is safe ONLY because every one of its call sites is
/// a single, well-understood nesting level (directly inside
/// timer_interrupt_handler, or ordinary kernel_main code). run() here is
/// nested arbitrarily deep inside the KEYBOARD interrupt handler's own
/// call chain instead (on_interrupt -> shell::on_char -> run_command ->
/// usertest::run), which itself is holding keyboard.rs's KEYBOARD mutex
/// guard for that entire chain's duration. An `sti` here re-enables
/// interrupts too early -- while still nested that deep, on whatever
/// stack was current -- opening a real window for a nested timer tick to
/// call tasks::timer_tick_switch() -> switch_to(), which saves the
/// CURRENT (mid-excursion, nested) rsp into some task's own context and
/// jumps away to a completely different task's stack, permanently
/// abandoning this whole call chain -- KEYBOARD mutex guard included,
/// never dropped -- and silently deadlocking every keystroke from then
/// on (confirmed via a real two-`usertest`-run serial+screenshot test:
/// the first run completed and even printed the next shell prompt, but
/// the second run's keystrokes were never even echoed to the console).
/// Removing `sti` fixes this: this `ret` always lands back inside code
/// nested inside the keyboard ISR, which does its OWN correct `iretq`
/// once it finally unwinds, naturally restoring the original RFLAGS
/// (IF=1) from before the keyboard interrupt fired -- exactly like every
/// other shell command already relies on, with no explicit re-enable
/// needed here at all.
#[unsafe(naked)]
unsafe extern "C" fn resume_kernel(_saved_rsp: u64) -> ! {
    core::arch::naked_asm!(
        "mov rsp, rdi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}

/// MILESTONE 27: the `usertest` shell command's entry point -- enters
/// ring 3 at the mapped user code page, lets the tiny hardcoded program
/// there call the print syscall then the exit syscall, and returns once
/// resume_kernel() has switched back. Safe to call repeatedly: setup()
/// already guarantees the pages are mapped exactly once, and every run
/// starts the user program from the same fixed RIP with a fresh top-of-
/// stack RSP, so there's no accumulated state between calls to leak or
/// corrupt.
pub fn run() -> Result<(), &'static str> {
    if !MAPPED.load(Ordering::SeqCst) {
        return Err("user test pages not mapped (setup() should have run at boot)");
    }

    let run_id = RUN_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = writeln!(
        serial(),
        "milestone 27: usertest run #{run_id} -- entering ring 3 at {:#x}",
        USER_CODE_ADDR
    );

    let user_cs = gdt::user_code_selector().0 as u64;
    let user_ss = gdt::user_data_selector().0 as u64;
    let user_stack_top = USER_STACK_ADDR + USER_STACK_SIZE;
    // bit 1 is reserved and must be 1; bit 9 (IF) set so ring-3 code
    // runs with interrupts enabled, per the milestone's own requirement
    // -- NOT a hardening measure against the timer/keyboard ISRs firing
    // mid-excursion, which is a real, disclosed, low-probability
    // limitation (see the milestone report).
    let rflags: u64 = 0x202;

    unsafe {
        enter_ring3(KERNEL_RSP.as_ptr(), user_stack_top, USER_CODE_ADDR, user_cs, user_ss, rflags);
    }

    let _ = writeln!(
        serial(),
        "milestone 27: usertest run #{run_id} -- resumed in kernel context after ring-3 exit syscall"
    );
    Ok(())
}

/// MILESTONE 30: the same enter_ring3 mechanism run() above uses,
/// factored out so process::run() can drive it too -- process.rs is
/// responsible for the CR3 switch (both directions) around this call;
/// this function only builds the iretq frame and executes it, identical
/// to what run() already did for the single-process Milestone 27 case.
/// Reuses the same KERNEL_RSP slot as run() -- safe because the shell
/// only ever runs one command (and therefore at most one ring-3
/// excursion, `usertest` or `runproc`) at a time.
pub(crate) fn enter_ring3_now() {
    let user_cs = gdt::user_code_selector().0 as u64;
    let user_ss = gdt::user_data_selector().0 as u64;
    let user_stack_top = USER_STACK_ADDR + USER_STACK_SIZE;
    let rflags: u64 = 0x202;

    unsafe {
        enter_ring3(KERNEL_RSP.as_ptr(), user_stack_top, USER_CODE_ADDR, user_cs, user_ss, rflags);
    }
}
