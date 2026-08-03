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
//!
//! MILESTONE 31: syscall 0 stops being "print one hardcoded kernel-side
//! string" and becomes a REAL `write(ptr, len)` syscall -- the ring-3
//! program now passes a real pointer (rdi) and length (rsi) in
//! registers, and syscall_dispatch reads those exact bytes out of
//! whatever address space is CURRENTLY loaded in CR3 (the calling
//! process's own private PML4 for a process.rs process, or the kernel's
//! own shared page tables for the legacy `usertest` path) and writes
//! them, raw, to the serial console. This is the generalization of
//! Milestone 30's read_active_message() (which only ever read ONE fixed
//! offset) to an arbitrary caller-supplied pointer+length. See
//! MAX_WRITE_LEN below for the one real safety net in place -- there is
//! still no general copy-from-user fault-recovery path, disclosed
//! honestly, not hidden.
//!
//! MILESTONE 35: real per-process file descriptors -- syscalls 3 (open),
//! 4 (read), 5 (fdwrite), 6 (close), all NEW syscall numbers, syscall 0
//! left completely untouched. See process.rs's own doc comment (right
//! above MAX_OPEN_FILES/OpenFile) for the full "why new syscalls instead
//! of generalizing write(ptr,len) into write(fd,ptr,len)" reasoning --
//! the short version: generalizing syscall 0 would require regenerating
//! every existing hand-assembled program's register setup (USER_PROGRAM
//! below, process.rs's PROCESS_PROGRAM, loader.rs's
//! build_test_program_image()), for no benefit that outweighs touching
//! three already-verified programs. All four new syscalls reuse the
//! EXACT SAME "read/write raw bytes at a caller-supplied pointer, through
//! whatever CR3 is currently loaded" technique syscall 0 established --
//! open()'s path string and fdwrite()'s data are read out of user memory
//! the same way syscall 0 always has, and read() writes its result back
//! into user memory the same way. The actual open/read/write/close
//! bookkeeping (the per-process fd table, buffering, close-time persist)
//! lives in process.rs, not here -- this file's job is exactly what it
//! already was for syscall 0: validate/cap the raw pointer+length
//! arguments and cross the user/kernel memory boundary safely, nothing
//! filesystem-specific.

use crate::gdt;
use crate::serial;
use crate::tasks::RING3_EXCURSION_ACTIVE;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};

pub const USER_CODE_ADDR: u64 = 0x_5555_5000_0000;
pub const USER_STACK_ADDR: u64 = 0x_5555_6000_0000;
pub const USER_STACK_SIZE: u64 = 4096;

/// MILESTONE 31: offset (from the start of a process's code page) and
/// fixed length (in bytes) of the message region the shared USER_PROGRAM
/// binary's `write(ptr, len)` syscall call reads and writes to serial.
/// USER_PROGRAM is ONE hand-assembled binary, copied byte-for-byte into
/// THREE different code pages (this module's own legacy usertest page,
/// and process.rs's process A / process B pages) -- since a naked
/// hand-assembled program can't compute "the length of whatever string
/// happens to live here" at runtime, both `ptr` and `len` are baked into
/// USER_PROGRAM as fixed immediates at assembly time below. Every caller
/// that installs a message here (this module's setup(), and process.rs's
/// create_process()) goes through write_fixed_message(), which
/// pads/truncates to exactly MESSAGE_LEN bytes -- the thing that keeps
/// the ONE shared `len` immediate valid for every copy of USER_PROGRAM.
/// Still no general program loader: a real one would compute `len` at
/// load/link time instead of requiring every message to fit one fixed
/// slot, explicitly Milestone 32+ territory, not pretended otherwise.
pub(crate) const MESSAGE_OFFSET: u64 = 128;
pub(crate) const MESSAGE_LEN: usize = 64;

/// MILESTONE 31: the legacy (ACTIVE_PROCESS == 0) `usertest` path's own
/// distinguishing message, installed into its code page by setup()
/// below -- deliberately different wording from process A/B's so all
/// three are unambiguous in a serial log.
const LEGACY_MESSAGE: &str = "hello from ring 3, CPL=3 confirmed -- real write() syscall";

/// MILESTONE 31: writes `message` into the code page pointed to by
/// `code_ptr`, at MESSAGE_OFFSET, padded with ASCII spaces (0x20 -- not
/// zero bytes, so the raw bytes the write syscall actually reads back
/// and prints stay human-readable instead of trailing NUL junk) out to
/// exactly MESSAGE_LEN bytes, truncated if the source string is longer.
/// Shared by this module's setup() (legacy usertest path) and
/// process.rs's create_process() (process A/B) so every code page's
/// message region is always exactly MESSAGE_LEN bytes regardless of the
/// source string's own length.
///
/// # Safety
/// `code_ptr` must point to at least `MESSAGE_OFFSET + MESSAGE_LEN`
/// writable bytes (true for any page mapped the way setup()/
/// create_process() map the code page).
pub(crate) unsafe fn write_fixed_message(code_ptr: *mut u8, message: &str) {
    let bytes = message.as_bytes();
    let n = bytes.len().min(MESSAGE_LEN);
    unsafe {
        let dst = code_ptr.add(MESSAGE_OFFSET as usize);
        core::ptr::write_bytes(dst, b' ', MESSAGE_LEN);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
    }
}

/// Hand-assembled x86_64 machine code, not bytes copied out of a
/// compiled Rust function: a naked Rust function's exact compiled
/// instruction-byte LENGTH isn't something Rust exposes (no linker
/// symbol for "end of this specific function" without a fragile
/// assumption about link order), whereas a handful of fixed, well-known
/// x86_64 instructions are trivial to encode by hand, so their length is
/// simply KNOWN rather than guessed at.
///
/// MILESTONE 31: regenerated from the Milestone 27 5-instruction program
/// -- syscall 0 now passes REAL arguments (rdi = pointer, rsi = length)
/// instead of taking none. `ptr` is baked in as USER_CODE_ADDR +
/// MESSAGE_OFFSET and `len` as MESSAGE_LEN, both compile-time constants
/// of THIS file, so the encoding below is regenerated deterministically
/// from them (verified with a standalone byte-for-byte re-derivation,
/// not hand-counted hex digits):
///
///   48 BF <8 bytes LE>   mov rdi, imm64   rdi = USER_CODE_ADDR+MESSAGE_OFFSET
///   BE <4 bytes LE>      mov esi, imm32   esi = MESSAGE_LEN
///   B8 00 00 00 00       mov eax, 0       syscall number 0 = write(ptr,len)
///   CD 80                int 0x80
///   B8 01 00 00 00       mov eax, 1       syscall number 1 = exit
///   CD 80                int 0x80
///   EB FE                jmp $            safety net: only reached if exit
///                                         somehow returns instead of resuming
///                                         kernel_main -- spins in ring 3
///                                         forever rather than running off into
///                                         whatever bytes follow in the page.
///
/// Total 31 bytes, comfortably inside MESSAGE_OFFSET (128) so it never
/// overlaps the message region that follows it in the same page.
pub(crate) static USER_PROGRAM: [u8; 31] = [
    0x48, 0xBF, 0x80, 0x00, 0x00, 0x50, 0x55, 0x55, 0x00, 0x00, // mov rdi, USER_CODE_ADDR+MESSAGE_OFFSET
    0xBE, 0x40, 0x00, 0x00, 0x00, // mov esi, MESSAGE_LEN (64 = 0x40)
    0xB8, 0x00, 0x00, 0x00, 0x00, // mov eax, 0
    0xCD, 0x80, // int 0x80
    0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xCD, 0x80, // int 0x80
    0xEB, 0xFE, // jmp $
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
        // MILESTONE 31: the legacy path's own message, installed at the
        // same MESSAGE_OFFSET every copy of USER_PROGRAM reads from --
        // without this, the legacy `usertest` command's write syscall
        // would read 64 bytes of whatever happened to already be in this
        // freshly-allocated physical frame (zeroed by the bootloader's
        // frame allocator, but not guaranteed to STAY zero -- explicit
        // is safer than implicit here).
        write_fixed_message(code_ptr, LEGACY_MESSAGE);
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

/// MILESTONE 31: hard cap on syscall 0 (write)'s `len` argument. This is
/// `unsafe` kernel code dereferencing a raw, ring-3-supplied pointer with
/// NO general copy-from-user fault-recovery path (e.g. a page-fault
/// handler that aborts the syscall cleanly instead of taking down the
/// kernel) -- that's real, disclosed, out-of-scope-for-this-milestone
/// work, not hidden behind this cap. What this cap DOES stop: a wildly
/// large `len` (e.g. an accidentally-sign-extended -1 arriving as
/// u64::MAX) walking the read loop off the end of the mapped code/stack
/// pages into unmapped address space and page-faulting the kernel. A
/// SHORT but genuinely bad pointer (len=8 at some unmapped address) can
/// still fault today -- that gap is real and this comment says so rather
/// than letting the cap read as a complete safety net.
const MAX_WRITE_LEN: u64 = 4096;

/// MILESTONE 35: cap on the `path_len` argument to syscall 3 (open).
/// Same reasoning as MAX_WRITE_LEN above (stops a wildly large/corrupt
/// length from walking a read loop off mapped memory), sized much
/// smaller than MAX_WRITE_LEN/MAX_FD_IO_LEN because fs.rs's own path
/// component names are already capped short (NAME_LEN=16 bytes per
/// component) -- 64 bytes is comfortably enough for this milestone's own
/// deepest test paths with real headroom, not a tight fit.
const MAX_PATH_LEN: u64 = 64;

/// MILESTONE 35: cap on the `len` argument to syscall 4 (read) and
/// syscall 5 (fdwrite) -- identical value and identical reasoning to
/// MAX_WRITE_LEN above, kept as a separate named constant (rather than
/// reusing MAX_WRITE_LEN directly) since it bounds a conceptually
/// different thing (fd I/O length, not the legacy write syscall's
/// length) even though the number happens to match fs::MAX_FILE_BYTES
/// today.
const MAX_FD_IO_LEN: u64 = 4096;

/// Ordinary Rust, called (via a plain "call") from syscall_entry's asm
/// with rdi = pointer to the just-saved SyscallRegs. Reads the syscall
/// number from the saved rax; for syscall 0 (MILESTONE 31: write(ptr,
/// len)) reads `len` bytes starting at `ptr` -- both real arguments now,
/// taken from the saved rdi/rsi exactly as the SysV-mirroring convention
/// enter_ring3()/USER_PROGRAM already use elsewhere in this file -- out
/// of whatever address space is CURRENTLY loaded in CR3, and writes them
/// raw to serial, then returns, letting syscall_entry pop registers and
/// iretq back into ring 3 for the second `int 0x80`; for syscall 1
/// (exit) never returns to syscall_entry at all -- calls resume_kernel()
/// directly, which abandons this call's own stack frame (on the
/// TSS.privilege_stack_table[0] syscall stack) entirely and jumps back
/// into run()'s caller instead.
extern "C" fn syscall_dispatch(regs: *mut SyscallRegs) {
    // MILESTONE 33: mutable now (was `&*regs`) -- syscall 2 (sbrk, below)
    // needs to write its return value into the saved rax slot so
    // syscall_entry's "pop rax" delivers it back into the real register
    // ring-3 code reads after `int 0x80` returns. Every read-only use
    // elsewhere in this function still works unchanged through a `&mut`.
    let regs = unsafe { &mut *regs };
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
            // MILESTONE 31: real write(ptr, len) -- rdi = ptr, rsi = len,
            // the same register convention USER_PROGRAM's hand-assembled
            // `mov rdi, imm64` / `mov esi, imm32` sets up before `int
            // 0x80`. See MAX_WRITE_LEN's own comment for exactly what
            // safety net this cap is (and is NOT).
            let ptr = regs.rdi;
            let requested_len = regs.rsi;
            let truncated = requested_len > MAX_WRITE_LEN;
            let len = if truncated { MAX_WRITE_LEN } else { requested_len } as usize;

            if truncated {
                let _ = writeln!(
                    serial(),
                    "milestone 31: syscall WRITE -- requested len {requested_len} exceeds MAX_WRITE_LEN {MAX_WRITE_LEN}, truncating"
                );
            }

            if active != 0 {
                let _ = writeln!(
                    serial(),
                    "milestone 31: syscall WRITE (process {active}) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- ptr={:#x} len={len} -- raw bytes >>>",
                    regs.cs, ptr
                );
            } else {
                let _ = writeln!(
                    serial(),
                    "milestone 31: syscall WRITE (legacy usertest, no active process) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- ptr={:#x} len={len} -- raw bytes >>>",
                    regs.cs, ptr
                );
            }

            // MILESTONE 31, the actual isolation proof, generalized from
            // Milestone 30's read_active_message(): this reads `len`
            // bytes starting at the CALLER-SUPPLIED virtual address `ptr`
            // through whatever CR3 is CURRENTLY loaded -- still the
            // active process's own private PML4 when active != 0 (the
            // exit syscall below is what switches CR3 back, not this
            // one), or the kernel's own shared page tables when active
            // == 0 -- so an identical virtual `ptr` genuinely resolves to
            // different physical bytes depending on which process called
            // in, exactly like Milestone 30 proved for one fixed offset,
            // now for an arbitrary pointer+length pair.
            let mut port = serial();
            for i in 0..len {
                let byte = unsafe { core::ptr::read((ptr as *const u8).wrapping_add(i)) };
                port.send(byte);
            }
            let _ = writeln!(port, "\n<<< end of write ({len} bytes)");

            if active != 0 {
                // MILESTONE 33: same technique, same timing (still under
                // this process's own CR3, the exit syscall below is what
                // restores the kernel's) -- reads the marker byte this
                // process's own machine code wrote via the sbrk syscall
                // into its own private heap, at the identical virtual
                // address HEAP_START every process uses. This is the
                // real per-process-heap isolation proof.
                let heap_marker = crate::process::read_active_heap_marker();
                let _ = writeln!(
                    serial(),
                    "milestone 33: syscall WRITE (process {active}) also reads its own private heap at {:#x} -- marker byte {:#04x} ('{}')",
                    crate::process::HEAP_START,
                    heap_marker,
                    heap_marker as char
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
        2 => {
            // MILESTONE 33: sbrk-style heap-grow syscall -- rdi holds the
            // requested byte count (ring-3 code loads it via `mov edi,
            // N` before `int 0x80`, per the SysV-ish convention this
            // ABI's SyscallRegs struct already captures every argument
            // register for). Only meaningful with an active per-process
            // heap (process.rs's create_process() pre-maps one per
            // process); the plain, unmodified `usertest` excursion (no
            // process.rs involvement, active == 0) has no heap mapped at
            // all, so it's honestly refused rather than silently
            // returning a pointer into nothing.
            if active != 0 {
                let size = regs.rdi;
                match crate::process::sbrk(active, size) {
                    Some(ptr) => {
                        let _ = writeln!(
                            serial(),
                            "milestone 33: syscall SBRK (process {active}) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- requested {size} bytes, returning heap pointer {:#x}",
                            regs.cs, ptr
                        );
                        regs.rax = ptr;
                    }
                    None => {
                        let _ = writeln!(
                            serial(),
                            "milestone 33: syscall SBRK (process {active}) -- FAILED (requested {size} bytes would exceed this process's fixed per-process heap) -- returning 0"
                        );
                        regs.rax = 0;
                    }
                }
            } else {
                let _ = writeln!(
                    serial(),
                    "milestone 33: syscall SBRK called with no active process (plain usertest excursion has no per-process heap mapped) -- ignoring, returning 0"
                );
                regs.rax = 0;
            }
        }
        3 => {
            // MILESTONE 35: open(path_ptr, path_len) -> fd (u64::MAX on
            // failure). rdi = path_ptr, rsi = path_len -- reads the path
            // string out of whatever CR3 is CURRENTLY loaded, the EXACT
            // same technique syscall 0 (write) already uses to read its
            // own ptr/len argument, just applied to a path instead of a
            // message. See MAX_PATH_LEN's own comment for what this cap
            // does and does not protect against.
            let path_ptr = regs.rdi;
            let requested_len = regs.rsi;
            let truncated = requested_len > MAX_PATH_LEN;
            let len = if truncated { MAX_PATH_LEN } else { requested_len } as usize;
            if truncated {
                let _ = writeln!(
                    serial(),
                    "milestone 35: syscall OPEN -- requested path_len {requested_len} exceeds MAX_PATH_LEN {MAX_PATH_LEN}, truncating"
                );
            }
            let mut path_bytes = alloc::vec::Vec::with_capacity(len);
            for i in 0..len {
                path_bytes.push(unsafe { core::ptr::read((path_ptr as *const u8).wrapping_add(i)) });
            }
            match (active, core::str::from_utf8(&path_bytes)) {
                (0, _) => {
                    let _ = writeln!(
                        serial(),
                        "milestone 35: syscall OPEN called with no active process (plain usertest excursion has no fd table) -- ignoring, returning u64::MAX"
                    );
                    regs.rax = u64::MAX;
                }
                (_, Err(_)) => {
                    let _ = writeln!(serial(), "milestone 35: syscall OPEN (process {active}) -- path is not valid UTF-8, returning u64::MAX");
                    regs.rax = u64::MAX;
                }
                (_, Ok(path)) => match crate::process::open_file(active, path) {
                    Some(fd) => {
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall OPEN (process {active}) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- path='{path}' -> fd {fd}",
                            regs.cs
                        );
                        regs.rax = fd;
                    }
                    None => {
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall OPEN (process {active}) -- FAILED for path='{path}' (fd table full -- max {} open files per process) -- returning u64::MAX",
                            crate::process::MAX_OPEN_FILES
                        );
                        regs.rax = u64::MAX;
                    }
                },
            }
        }
        4 => {
            // MILESTONE 35: read(fd, buf_ptr, len) -> bytes_read
            // (u64::MAX if fd is invalid). rdi = fd, rsi = buf_ptr, rdx =
            // len. Copies out of the fd's already-buffered contents
            // (process::read_fd) into the CALLER's own memory at
            // buf_ptr, through whatever CR3 is currently loaded -- same
            // write-into-user-memory technique as the sbrk pointer
            // return, just copying a whole byte range instead of a
            // single pointer value.
            let fd = regs.rdi;
            let buf_ptr = regs.rsi;
            let requested_len = regs.rdx;
            let truncated = requested_len > MAX_FD_IO_LEN;
            let len = if truncated { MAX_FD_IO_LEN } else { requested_len } as usize;
            if truncated {
                let _ = writeln!(
                    serial(),
                    "milestone 35: syscall READ -- requested len {requested_len} exceeds MAX_FD_IO_LEN {MAX_FD_IO_LEN}, truncating"
                );
            }
            if active == 0 {
                let _ = writeln!(
                    serial(),
                    "milestone 35: syscall READ called with no active process (plain usertest excursion has no fd table) -- ignoring, returning u64::MAX"
                );
                regs.rax = u64::MAX;
            } else {
                match crate::process::read_fd(active, fd, len) {
                    Some(data) => {
                        let n = data.len();
                        for (i, b) in data.iter().enumerate() {
                            unsafe { core::ptr::write((buf_ptr as *mut u8).wrapping_add(i), *b) };
                        }
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall READ (process {active}) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- fd={fd} requested={len} actual={n} bytes",
                            regs.cs
                        );
                        regs.rax = n as u64;
                    }
                    None => {
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall READ (process {active}) -- FAILED, fd {fd} is not open -- returning u64::MAX"
                        );
                        regs.rax = u64::MAX;
                    }
                }
            }
        }
        5 => {
            // MILESTONE 35: fdwrite(fd, ptr, len) -> bytes_written
            // (u64::MAX if fd is invalid). rdi = fd, rsi = ptr, rdx =
            // len. Deliberately a DIFFERENT syscall number from syscall
            // 0 (write) -- see this file's own module doc comment and
            // process.rs's OpenFile doc comment for the full "new
            // syscalls, not a generalized write(fd,ptr,len)" reasoning.
            // Reads `len` bytes out of the CALLER's own memory at `ptr`
            // (same read-through-current-CR3 technique syscall 0/open
            // already use), hands them to process::write_fd(), which may
            // accept FEWER bytes than requested if this would overflow
            // fs::MAX_FILE_BYTES -- the real return value here is
            // whatever write_fd() actually accepted, not just an echo of
            // `len`.
            let fd = regs.rdi;
            let ptr = regs.rsi;
            let requested_len = regs.rdx;
            let truncated = requested_len > MAX_FD_IO_LEN;
            let len = if truncated { MAX_FD_IO_LEN } else { requested_len } as usize;
            if truncated {
                let _ = writeln!(
                    serial(),
                    "milestone 35: syscall FDWRITE -- requested len {requested_len} exceeds MAX_FD_IO_LEN {MAX_FD_IO_LEN}, truncating"
                );
            }
            if active == 0 {
                let _ = writeln!(
                    serial(),
                    "milestone 35: syscall FDWRITE called with no active process (plain usertest excursion has no fd table) -- ignoring, returning u64::MAX"
                );
                regs.rax = u64::MAX;
            } else {
                let mut data = alloc::vec::Vec::with_capacity(len);
                for i in 0..len {
                    data.push(unsafe { core::ptr::read((ptr as *const u8).wrapping_add(i)) });
                }
                match crate::process::write_fd(active, fd, &data) {
                    Some(n) => {
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall FDWRITE (process {active}) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- fd={fd} requested={len} accepted={n} bytes{}",
                            regs.cs,
                            if n < len { " (TRUNCATED -- would have exceeded fs::MAX_FILE_BYTES)" } else { "" }
                        );
                        regs.rax = n as u64;
                    }
                    None => {
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall FDWRITE (process {active}) -- FAILED, fd {fd} is not open -- returning u64::MAX"
                        );
                        regs.rax = u64::MAX;
                    }
                }
            }
        }
        6 => {
            // MILESTONE 35: close(fd) -> status (0=success, 1=invalid
            // fd, 2=fd released but the on-disk persist failed). rdi =
            // fd. This is the ONLY point in this milestone's design
            // where a write actually reaches disk (process::close_fd's
            // own doc comment explains why) -- see the serial log lines
            // it emits for exactly what got persisted.
            let fd = regs.rdi;
            if active == 0 {
                let _ = writeln!(
                    serial(),
                    "milestone 35: syscall CLOSE called with no active process (plain usertest excursion has no fd table) -- ignoring, returning u64::MAX"
                );
                regs.rax = u64::MAX;
            } else {
                match crate::process::close_fd(active, fd) {
                    Some(true) => {
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall CLOSE (process {active}) -- hardware-recorded CS={:#x} (CPL={hardware_cpl}) -- fd={fd} closed successfully",
                            regs.cs
                        );
                        regs.rax = 0;
                    }
                    Some(false) => {
                        regs.rax = 2;
                    }
                    None => {
                        let _ = writeln!(
                            serial(),
                            "milestone 35: syscall CLOSE (process {active}) -- FAILED, fd {fd} was not open -- returning status 1"
                        );
                        regs.rax = 1;
                    }
                }
            }
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

    // BUGFIX: see tasks::RING3_EXCURSION_ACTIVE's own doc comment -- this
    // excursion runs with interrupts enabled, so a background-scheduler
    // timer tick could otherwise switch a task away mid-excursion,
    // corrupting state once it unwinds. Set for exactly this call's
    // duration, cleared unconditionally right after (enter_ring3 always
    // returns here -- the exit syscall's resume_kernel() is what makes
    // that true -- so there's no path that could leave this stuck true).
    RING3_EXCURSION_ACTIVE.store(true, Ordering::SeqCst);
    unsafe {
        enter_ring3(KERNEL_RSP.as_ptr(), user_stack_top, USER_CODE_ADDR, user_cs, user_ss, rflags);
    }
    RING3_EXCURSION_ACTIVE.store(false, Ordering::SeqCst);

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

    // BUGFIX: same guard as run() above -- this is the exact call path
    // (runproc/runfile/runelf, via process.rs) the Milestone 36 disclosed
    // intermittent page fault was root-caused to. See
    // tasks::RING3_EXCURSION_ACTIVE's doc comment for the full mechanism.
    RING3_EXCURSION_ACTIVE.store(true, Ordering::SeqCst);
    unsafe {
        enter_ring3(KERNEL_RSP.as_ptr(), user_stack_top, USER_CODE_ADDR, user_cs, user_ss, rflags);
    }
    RING3_EXCURSION_ACTIVE.store(false, Ordering::SeqCst);
}
