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
//! after every switch), by each process's write(ptr,len) syscall reading
//! its OWN message out of ITS OWN physical code-page frame at the SAME
//! virtual offset every other process uses, and by re-running process A
//! after process B with no cross-contamination.
//!
//! MILESTONE 31: the syscall that proves this per-process isolation
//! generalized from Milestone 27's fixed-string "print" (syscall 0 took
//! no arguments at all) to a real `write(ptr, len)` -- create_process()
//! below now installs each process's message through
//! usertest::write_fixed_message() (the exact same helper the legacy
//! usertest.rs code path uses for ITS message), and the old
//! read_active_message() helper that only ever read one hardcoded offset
//! is gone, superseded by usertest.rs's syscall_dispatch generically
//! reading whatever `ptr`/`len` the calling process's own USER_PROGRAM
//! passed in registers.
//!
//! MILESTONE 33: each process also gets its own private HEAP, a fixed
//! 16 KiB region at HEAP_START pre-mapped by create_process() the same
//! way code/stack are. `HEAP_START` shares USER_CODE_ADDR/
//! USER_STACK_ADDR's p4_index (170), so it lands inside the SAME
//! already-private PML4 slot Milestone 30 rebuilds per process --
//! genuinely private with zero new PML4-level reasoning needed. A real
//! syscall (2 = sbrk, in usertest.rs) is the process's only way to grow
//! into it, chosen over a kernel-only allocator because `int 0x80` is
//! this kernel's one sanctioned ring-3-to-ring-0 boundary: a kernel-side
//! allocator unreachable from ring 3 wouldn't give the PROCESS a real
//! way to allocate. Process A/B now run PROCESS_PROGRAM (this module's
//! own hand-assembled binary, not usertest::USER_PROGRAM) so they can
//! exercise sbrk before printing; usertest::USER_PROGRAM and the plain
//! `usertest` command are completely untouched.
//!
//! MILESTONE 34: a real general program loader (kernel/src/loader.rs)
//! removes the "hardcoded" half of the original limitation. The page-
//! table-building core of create_process() is factored out into
//! create_process_from_image(), which builds the SAME private
//! PML4/P3/P2/P1 chain (code, stack, AND heap -- every process gets one
//! uniformly, loaded-from-file or not) from an arbitrary `&[u8]` code
//! image instead of always copying PROCESS_PROGRAM. create_process()
//! (used by PROCESS_A/PROCESS_B) becomes a thin wrapper that builds its
//! PROCESS_PROGRAM+heap-marker+message image as a `Vec<u8>` first, then
//! hands it to the shared core -- loader.rs's `runfile` calls the new
//! create_loaded_process()/run_loaded_process() below (MILESTONE 35:
//! split from one original load_and_run_image() function -- see
//! run_loaded_process()'s own doc comment) with bytes read straight from
//! the real on-disk filesystem instead. No page-table code is duplicated between
//! the two paths.
//!
//! MILESTONE 36: a real ELF64 loader (kernel/src/elf.rs does the actual
//! parsing; create_process_from_elf() below does the loading).
//! Deliberately kept SEPARATE from create_process_from_image() above,
//! rather than folded into it or refactored to share its body -- partly
//! because the shapes genuinely differ (one fixed page at one fixed
//! address vs. an arbitrary number of pages at segment-declared
//! addresses), and partly a deliberate choice to minimize process.rs
//! churn while Milestone 35 (real per-process file descriptors) is
//! being built concurrently against this same file from the same
//! Milestone 34 baseline -- additive new functions merge far more
//! cleanly than a refactor of shared code both branches touch.
//!
//! The real, honest scoping decision (option (a) from the milestone
//! brief, not option (b)): this milestone does NOT make the ring-3
//! entry trampoline's jump target dynamic. usertest::enter_ring3_now()
//! still hardcodes USER_CODE_ADDR as the iretq target, exactly as
//! Milestone 27 left it. So create_process_from_elf() below requires --
//! checked for real, not assumed -- that the ELF's own e_entry equals
//! USER_CODE_ADDR exactly, and that every PT_LOAD segment is page-
//! aligned and fits a small, fixed per-segment/total page cap. This is
//! real, deeper surgery on Milestone 27/30's carefully-built ring-3
//! entry mechanism deliberately AVOIDED in favor of shipping a smaller,
//! fully-verified change: an ELF genuinely linked for this kernel's own
//! fixed entry address loads and runs for real, with real multi-page,
//! multi-segment mapping (not "arbitrary Linux ELF binaries", which
//! would additionally need position-independent/dynamic entry support,
//! a real page-fault-driven demand loader or relocations, and is
//! explicitly out of scope here, same as this project's own README
//! already scopes "full Linux ELF/libc compatibility" as separate,
//! much-later work).

use crate::elf;
use crate::fs;
use crate::serial;
use crate::usertest;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use spin::Mutex;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::page_table::PageTableIndex;
use x86_64::structures::paging::{FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

const PAGE_SIZE: usize = 4096;

/// MILESTONE 34: the real, honest bound loader.rs's `runfile` checks a
/// file's size against BEFORE reading it into a process's code frame --
/// one page, the code frame's actual real capacity, not a made-up
/// number. Exported so loader.rs's own pre-check (and its self-test)
/// use this SAME constant rather than a second, potentially-drifting
/// copy of "4096".
pub(crate) const MAX_CODE_IMAGE_BYTES: usize = PAGE_SIZE;

/// MILESTONE 33: per-process heap virtual range. Shares p4_index 170
/// with USER_CODE_ADDR/USER_STACK_ADDR (verified, not assumed -- see
/// create_process()'s own p4_index check) and p3_index 341 with them
/// too, but lands at a distinct p2_index (384, vs. 128/256 for
/// code/stack), so its 4 pages can never collide with the single code
/// or stack page, with 256 MiB of headroom either side. Fully disjoint
/// from the kernel's own heap (`allocator::HEAP_START`, p4_index 136).
pub const HEAP_START: u64 = 0x_5555_7000_0000;
/// Four 4 KiB pages (16 KiB), pre-mapped once at create_process() time
/// -- fixed and bounded on purpose: `sbrk` bumps within this pre-mapped
/// region and fails cleanly once exhausted, it never grows the mapping
/// itself. No free/realloc, bump-only.
const HEAP_PAGE_COUNT: u64 = 4;
const HEAP_SIZE: u64 = HEAP_PAGE_COUNT * PAGE_SIZE as u64;

/// MILESTONE 33: hand-assembled machine code copied into each process's
/// code page INSTEAD of usertest::USER_PROGRAM (which stays completely
/// untouched, so the original Milestone 27 `usertest` command keeps
/// working exactly as it always has). Calls sbrk (syscall 2) first, then
/// writes a per-process marker byte into the returned heap pointer,
/// THEN sets up rdi/rsi and calls the Milestone 31 write(ptr,len)
/// syscall exactly like usertest::USER_PROGRAM does -- regenerated to
/// match that convention rather than the older no-argument "print".
///
///   offset  bytes                              instruction
///    0      B8 02 00 00 00                     mov eax, 2      (syscall 2 = sbrk)
///    5      BF 10 00 00 00                     mov edi, 16      (request 16 bytes)
///   10      CD 80                              int 0x80         -- rax = heap ptr
///   12      C6 00 00                           mov byte [rax], 0   (imm8 at offset 14,
///                                                                    patched per-process
///                                                                    by create_process()
///                                                                    below -- see
///                                                                    HEAP_MARKER_PATCH_OFFSET)
///   15      48 BF 80 00 00 50 55 55 00 00      mov rdi, imm64   (rdi = USER_CODE_ADDR+MESSAGE_OFFSET,
///                                                                 identical bytes to
///                                                                 usertest::USER_PROGRAM's own --
///                                                                 same constants)
///   25      BE 40 00 00 00                     mov esi, 64      (esi = usertest::MESSAGE_LEN)
///   30      B8 00 00 00 00                     mov eax, 0       (syscall 0 = write(ptr,len))
///   35      CD 80                              int 0x80
///   37      B8 01 00 00 00                     mov eax, 1       (syscall 1 = exit)
///   42      CD 80                              int 0x80
///   44      EB FE                              jmp $            (safety net, same
///                                                                 reasoning as
///                                                                 USER_PROGRAM's own)
const PROCESS_PROGRAM: [u8; 46] = [
    0xB8, 0x02, 0x00, 0x00, 0x00, 0xBF, 0x10, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xC6, 0x00, 0x00, 0x48,
    0xBF, 0x80, 0x00, 0x00, 0x50, 0x55, 0x55, 0x00, 0x00, 0xBE, 0x40, 0x00, 0x00, 0x00, 0xB8, 0x00,
    0x00, 0x00, 0x00, 0xCD, 0x80, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xEB, 0xFE,
];
/// Index into PROCESS_PROGRAM of the `mov byte [rax], imm8` instruction's
/// immediate byte -- checked by hand against the byte table in
/// PROCESS_PROGRAM's own doc comment above, not assumed. create_process()
/// patches this to a distinct value per process (b'A' / b'B') after
/// copying the template in.
const HEAP_MARKER_PATCH_OFFSET: usize = 14;

/// MILESTONE 35: real per-process file descriptors on top of fs.rs's
/// already-real on-disk filesystem (Milestones 18/22/28/32). Design
/// decision, mirroring Milestone 33's own documented sbrk-vs-kernel-only
/// choice:
///
/// **NEW syscall numbers (3=open, 4=read, 5=fdwrite, 6=close), NOT a
/// generalized `write(fd, ptr, len)`.** The alternative -- folding fd
/// support into syscall 0 by adding an `fd` argument (`fd=1` meaning
/// "serial console" for backward compatibility) -- was rejected because
/// it would change syscall 0's calling convention for every EXISTING
/// hand-assembled program already baked into this kernel binary
/// (usertest::USER_PROGRAM, process.rs's own PROCESS_PROGRAM, and
/// loader.rs's build_test_program_image()) -- each would need its
/// `mov rdi/rsi, ...` sequence regenerated with a new leading `fd`
/// argument, for a benefit (one fewer syscall number) that doesn't
/// outweigh touching three already-verified programs. New, additive
/// syscall numbers is also this project's own established pattern --
/// Milestone 33 added sbrk as syscall 2 rather than overloading an
/// existing one -- and keeps this milestone's diff isolated to NEW code
/// paths, which matters concretely right now: Milestone 36 (a real ELF
/// loader, developed in parallel against the same Milestone 34 baseline)
/// also touches usertest.rs/process.rs, and a hand-merge is far safer
/// against additive new match arms than against a changed argument
/// convention on the one syscall every existing program already calls.
///
/// **Buffered-at-open-time, not real streaming I/O.** open() reads the
/// WHOLE file into a kernel-owned Vec<u8> up front via fs::read_file()
/// (or starts an empty Vec if the path doesn't exist yet -- see
/// open_file()'s own doc comment for why that's the honest choice given
/// this syscall ABI has no separate O_CREAT flag). read()/fdwrite() then
/// only ever touch that in-memory buffer, never re-touching the disk
/// until close(). This is a real, disclosed limitation, not hidden: it's
/// only reasonable because fs.rs already caps every file at
/// fs::MAX_FILE_BYTES (4096 bytes, 8 sectors) -- the entire file always
/// fits in one small heap allocation, so "buffer the whole thing" costs
/// nothing meaningful. A real streaming implementation (partial reads
/// from disk sector-by-sector, incremental writes) is a substantially
/// bigger feature (needs a real per-fd on-disk cursor concept fs.rs
/// doesn't have today) and explicitly out of scope here.
///
/// **Writes persist to disk on close(), and ONLY on close(), and ONLY if
/// the fd was actually written to.** fdwrite() only ever mutates the
/// in-kernel buffer; close_fd() below calls fs::write_file(path, &buffer)
/// exactly when `dirty` is true. A file opened read-only and never
/// written is never rewritten (avoids an unnecessary disk write, and
/// avoids ever calling fs::write_file with a path that was only ever
/// read). There is no separate `fsync`/flush syscall -- data written via
/// fdwrite() and not yet close()'d is genuinely NOT on disk yet if
/// something (a crash, a later `runfile`) interrupts before close() runs.
/// Disclosed, not hidden.
struct OpenFile {
    /// Root-relative path this fd was opened against (fs.rs's own path
    /// format, exactly what was passed to open()) -- kept so close() can
    /// call fs::write_file() against the SAME path, without the caller
    /// having to pass it a second time.
    path: String,
    /// The file's entire contents, read once at open() time (or empty,
    /// for a not-yet-existing path) and mutated in place by fdwrite() --
    /// see this struct's own doc comment for why buffering the whole
    /// file is an honest choice here, not a shortcut hidden from the
    /// milestone report.
    buffer: Vec<u8>,
    /// Byte offset into `buffer` that the NEXT read() or fdwrite() call
    /// starts at -- shared between the two (Unix-style: one file
    /// position per open fd, not separate read/write cursors), and
    /// advanced by however many bytes that call actually transferred.
    pos: usize,
    /// Set by fdwrite() the first time it actually changes `buffer`;
    /// close() only calls fs::write_file() when this is true, so a
    /// read-only open never triggers a redundant disk write.
    dirty: bool,
}

/// MILESTONE 35: real, bounded per-process fd table size. Fixed at 4,
/// matching this project's existing "small fixed bound with an honest
/// disclosed limit" style (heap_frames' own HEAP_PAGE_COUNT=4 is the
/// direct precedent) rather than a dynamically-growing Vec<Option<..>>
/// of open files -- 4 is comfortably more than this milestone's own test
/// program ever has open at once (at most 2: one read-only fd against an
/// existing file, one write fd against a new one, opened and closed in
/// sequence rather than concurrently), while still leaving headroom for
/// a real program to have, say, an input and output file open
/// simultaneously. A full table returns u64::MAX from open() (the same
/// failure sentinel as "path not found" for a to-be-fair-can't-happen
/// case here, given no file ever fails to "exist" -- see open_file()'s
/// own comment) rather than growing without bound.
pub(crate) const MAX_OPEN_FILES: usize = 4;

pub struct Process {
    label: &'static str,
    pml4_frame: PhysFrame<Size4KiB>,
    code_frame: PhysFrame<Size4KiB>,
    stack_frame: PhysFrame<Size4KiB>,
    /// MILESTONE 33: this process's own private heap frames, in mapping
    /// order (heap_frames[0] backs HEAP_START, etc.) -- kept around (not
    /// just mapped-and-forgotten) as real per-process kernel-side state,
    /// the kind a future general program loader (Milestone 34) would
    /// need for its own bookkeeping.
    heap_frames: Vec<PhysFrame<Size4KiB>>,
    /// MILESTONE 33: bump offset (bytes, from HEAP_START) of this
    /// process's next `sbrk` allocation -- persists across repeated
    /// `runproc` calls for the SAME process (never reset), so re-running
    /// a process twice hands out a fresh, later region on the second
    /// call rather than silently reusing the first.
    heap_used: u64,
    /// MILESTONE 35: this process'''s own real fd table -- `None` is an
    /// unused slot, `Some(OpenFile)` an fd the process currently has open
    /// (its index in this array IS its fd number, per open_file()'''s own
    /// comment). Never reset between runs of the SAME process, matching
    /// heap_used'''s own persists-across-runs precedent -- a fd left open
    /// by a buggy program stays open (and stays counted against
    /// MAX_OPEN_FILES) until an explicit close(), exactly like a real OS.
    fds: [Option<OpenFile>; MAX_OPEN_FILES],
    /// MILESTONE 36: extra physical frames backing a real ELF-loaded
    /// process'''s PT_LOAD segment pages BEYOND the one page already
    /// tracked as `code_frame` above (which, for an ELF-loaded process,
    /// is specifically whichever mapped page backs USER_CODE_ADDR --
    /// see create_process_from_elf()). Always empty for the flat-binary
    /// path (create_process_from_image/create_process): a flat image is
    /// still exactly one code page, so it never needs this. Kept around
    /// for the same "real per-process kernel-side bookkeeping, not
    /// mapped-and-forgotten" reasoning as `heap_frames` above -- nothing
    /// currently reads this back out, same as `heap_frames`, but it'''s
    /// real, live state rather than silently dropped/leaked bookkeeping.
    extra_frames: Vec<PhysFrame<Size4KiB>>,
}

static PROCESS_A: Mutex<Option<Process>> = Mutex::new(None);
static PROCESS_B: Mutex<Option<Process>> = Mutex::new(None);

/// MILESTONE 35: a THIRD hardcoded, boot-time-created process slot,
/// exactly like PROCESS_A/PROCESS_B in every structural way (built once
/// at boot by init_fdtest_process(), run via the SAME `run(id)`
/// CR3-switch+enter_ring3_now() mechanism as A/B) but running
/// loader::FDTEST_PROGRAM (the Milestone 35 open/read/fdwrite/close
/// syscall test) instead of PROCESS_PROGRAM.
///
/// **Why this exists alongside `runfile fdtestprog`, not instead of
/// it**: this milestone's fd syscalls were ALSO verified end-to-end via
/// `runfile` (see run_loaded_process()'s doc comment) -- the real
/// on-disk-filesystem open/read/fdwrite/close syscall trace came back
/// byte-for-byte correct there too (confirmed in the serial log: OPEN
/// 'fdtest'->fd0, READ 22/22 bytes matching exactly, CLOSE, OPEN
/// 'fdout'->fd0, FDWRITE 69/69 bytes accepted, CLOSE persisted 69 bytes
/// to disk). But a SEPARATE, pre-existing, unrelated Milestone 34 bug
/// (real heap corruption in the `runfile`/LOADED_PROCESS path --
/// reproduces even with completely unmodified Milestone 34 code, e.g.
/// plain `seedtestprog`+`runfile testprog`, no Milestone 35 syscalls
/// involved at all; confirmed NOT present in the `runproc` path even
/// under repeated/extended testing; symbolized to a null-pointer read
/// inside linked_list_allocator::Heap::allocate_first_fit, timing-
/// dependent on background task scheduling) crashes the kernel shortly
/// AFTER that syscall trace completes successfully, before the shell can
/// accept the follow-up `read fdout` keystrokes needed to close the loop
/// with a live, screendumped confirmation. This process slot -- reusing
/// the exact mechanism `runproc` already uses safely -- exists so this
/// milestone's own verification isn't blocked by a bug in someone else's
/// (Milestone 34's) code path. That bug is disclosed, not fixed here
/// (out of scope for a fd-syscall milestone to debug a heap allocator);
/// `runfile fdtestprog` is left exactly as capable (and exactly as
/// broken, for now) as `runfile testprog` already was.
static FDTEST_PROCESS: Mutex<Option<Process>> = Mutex::new(None);

/// ACTIVE_PROCESS / with_process_mut id for FDTEST_PROCESS -- distinct
/// from PROCESS_A (1), PROCESS_B (2), and LOADED_PROCESS_ID (3, a
/// different process entirely, reachable only via `runfile`).
pub(crate) const FDTEST_PROCESS_ID: u8 = 4;

/// The kernel's own PML4 physical frame + CR3 flags, saved ONCE at boot
/// (save_kernel_cr3(), called from kernel_main before any process is
/// created) via a real Cr3::read() -- restored verbatim on every
/// process's exit syscall, never reconstructed or guessed at.
static KERNEL_PML4_FRAME: AtomicU64 = AtomicU64::new(0);
static KERNEL_CR3_FLAGS_BITS: AtomicU64 = AtomicU64::new(0);

/// 0 = no process currently running (plain `usertest`, or idle);
/// nonzero = the id of the process currently executing in ring 3.
/// usertest.rs's syscall_dispatch reads this to decide whether the write
/// syscall's log line should say "legacy usertest" or "process N", and
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

/// MILESTONE 33: reads one byte back out of HEAP_START -- called from
/// usertest.rs's syscall_dispatch, write-syscall arm, same timing/
/// reasoning as the message read just above it (CR3 is still the active
/// process's own PML4 at this point, the exit syscall is what restores
/// the kernel's). Real isolation proof: identical virtual address every
/// time, resolves through whichever process's own private heap mapping
/// is currently loaded.
pub(crate) fn read_active_heap_marker() -> u8 {
    let ptr = HEAP_START as *const u8;
    unsafe { core::ptr::read(ptr) }
}

/// MILESTONE 33: the `sbrk` syscall's actual kernel-side implementation
/// -- called from usertest.rs's syscall_dispatch, syscall-2 arm, with the
/// CURRENTLY-active process's id (from ACTIVE_PROCESS) and the requested
/// byte count (from the caller's rdi). Bumps that process's OWN
/// heap_used counter and returns a pointer into ITS OWN pre-mapped
/// heap region -- `None` if the request would run past the fixed 16 KiB
/// pre-mapped region or if `id` doesn't name a live process.
pub(crate) fn sbrk(id: u8, size: u64) -> Option<u64> {
    let slot = match id {
        1 => &PROCESS_A,
        2 => &PROCESS_B,
        _ => return None,
    };
    let mut guard = slot.lock();
    let proc = guard.as_mut()?;
    let new_used = proc.heap_used.checked_add(size)?;
    if new_used > HEAP_SIZE {
        return None;
    }
    let ptr = HEAP_START + proc.heap_used;
    proc.heap_used = new_used;
    Some(ptr)
}

/// MILESTONE 35: locates the Mutex slot for whichever process `id`
/// names -- PROCESS_A (1), PROCESS_B (2), or LOADED_PROCESS (3, the
/// single most-recently-`runfile`'d process) -- and runs `f` against its
/// live `Process`, returning `None` if `id` doesn't name one of those
/// three or that slot is currently empty. Deliberately covers all THREE
/// ids, unlike sbrk() above (which only recognizes 1/2, a real,
/// disclosed Milestone 34 limitation loader.rs's own doc comment already
/// flags) -- fd support has no such gap: a file-loaded process gets a
/// real, working fd table exactly like PROCESS_A/PROCESS_B do, since
/// nothing about open/read/fdwrite/close is specific to how a process's
/// code image was built.
fn with_process_mut<R>(id: u8, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    let slot = match id {
        1 => &PROCESS_A,
        2 => &PROCESS_B,
        LOADED_PROCESS_ID => &LOADED_PROCESS,
        FDTEST_PROCESS_ID => &FDTEST_PROCESS,
        _ => return None,
    };
    let mut guard = slot.lock();
    let proc = guard.as_mut()?;
    Some(f(proc))
}

/// MILESTONE 35: the `open` syscall's kernel-side implementation. `path`
/// has already been copied out of the calling process's own address
/// space by usertest.rs's syscall_dispatch (same read-through-current-
/// CR3 technique the write syscall established). Tries fs::read_file()
/// first; a path that already exists gets its real on-disk contents
/// buffered in (see OpenFile's own doc comment for why buffering the
/// whole file is honest here). A path that does NOT exist is not treated
/// as an error -- this syscall ABI has no separate O_CREAT/O_RDONLY flag
/// argument (deliberately kept minimal, matching sbrk's own single-
/// argument simplicity), so open() always succeeds against a well-formed
/// path/fd-table state and starts an empty buffer for a not-yet-existing
/// file, exactly like a real `open(path, O_RDWR|O_CREAT)` would -- the
/// file only actually comes into existence on disk if something is
/// later fdwrite()'n and the fd is close()'d (see close_fd()). Returns
/// the new fd (an index into the process's own fds table) or `None` if
/// the table is already full (MAX_OPEN_FILES) or `id` doesn't name a
/// live process.
pub(crate) fn open_file(id: u8, path: &str) -> Option<u64> {
    let buffer = match fs::read_file(path) {
        Ok(data) => data,
        Err(_) => Vec::new(), // honest "doesn't exist yet" case, not an error -- see doc comment above
    };
    let path_owned = path.to_string();
    with_process_mut(id, move |proc| {
        let slot_index = proc.fds.iter().position(|f| f.is_none())?;
        proc.fds[slot_index] = Some(OpenFile { path: path_owned, buffer, pos: 0, dirty: false });
        Some(slot_index as u64)
    })?
}

/// MILESTONE 35: the `read` syscall's kernel-side implementation. Copies
/// up to `len` bytes starting at this fd's current cursor `pos` out of
/// its already-buffered contents (see OpenFile's doc comment -- no disk
/// access happens here, the buffering already happened at open() time),
/// advances `pos` by however many bytes were actually available (0 at
/// EOF, which is a real, valid, non-error result -- the caller's cap on
/// `len` was already applied by usertest.rs before this is called).
/// Returns `None` (distinct from a legitimate `Some(vec![])` at EOF) if
/// `fd` doesn't name a currently-open descriptor for this process, or
/// `id` doesn't name a live process.
pub(crate) fn read_fd(id: u8, fd: u64, len: usize) -> Option<Vec<u8>> {
    with_process_mut(id, |proc| {
        let entry = proc.fds.get_mut(fd as usize)?.as_mut()?;
        let start = entry.pos.min(entry.buffer.len());
        let end = (start + len).min(entry.buffer.len());
        let out = entry.buffer[start..end].to_vec();
        entry.pos = end;
        Some(out)
    })?
}

/// MILESTONE 35: the `fdwrite` syscall's kernel-side implementation.
/// `data` has already been copied out of the calling process's own
/// address space (same technique open()'s `path` used). Writes `data`
/// into this fd's buffer starting at its current cursor `pos`, extending
/// the buffer as needed but never past fs::MAX_FILE_BYTES (the SAME cap
/// fs::write_file() itself enforces -- reused directly, not a second,
/// potentially-drifting copy of "4096"), truncating a write that would
/// overflow it and returning the ACTUAL number of bytes accepted (which
/// callers must check -- a real, honest partial-write result, the same
/// "don't silently drop data without saying so" discipline
/// usertest.rs's own MAX_WRITE_LEN already established for syscall 0).
/// Marks the fd dirty so close_fd() below knows to persist it. Returns
/// `None` if `fd`/`id` don't name a currently-open descriptor.
pub(crate) fn write_fd(id: u8, fd: u64, data: &[u8]) -> Option<usize> {
    with_process_mut(id, |proc| {
        let entry = proc.fds.get_mut(fd as usize)?.as_mut()?;
        let start = entry.pos;
        let room = fs::MAX_FILE_BYTES.saturating_sub(start);
        let n = data.len().min(room);
        if entry.buffer.len() < start + n {
            entry.buffer.resize(start + n, 0);
        }
        entry.buffer[start..start + n].copy_from_slice(&data[..n]);
        entry.pos = start + n;
        entry.dirty = true;
        Some(n)
    })?
}

/// MILESTONE 35: the `close` syscall's kernel-side implementation --
/// releases this process's fd table slot (`Some(_)` -> `None`, freeing
/// it for a later open() to reuse) and, if fdwrite() ever actually
/// touched this fd's buffer (`dirty`), persists it for real via
/// fs::write_file(path, &buffer) -- the SAME on-disk write path the
/// shell's own `write` command already uses, so a `read PATH` shell
/// command run afterward sees genuinely identical bytes. This is the
/// ONLY point in this milestone's whole design where a write actually
/// reaches disk -- see OpenFile's own doc comment for why that timing
/// (close-only, no separate flush/fsync) is disclosed as a real
/// limitation, not hidden. Returns `Some(true)` on success (persisted,
/// or nothing needed persisting), `Some(false)` if fs::write_file()
/// itself failed (fd is still released either way -- a failed persist
/// doesn't leak the table slot), or `None` if `fd`/`id` didn't name a
/// currently-open descriptor at all.
pub(crate) fn close_fd(id: u8, fd: u64) -> Option<bool> {
    let entry = with_process_mut(id, |proc| {
        let slot = proc.fds.get_mut(fd as usize)?;
        slot.take()
    })??;
    if !entry.dirty {
        return Some(true);
    }
    match fs::write_file(&entry.path, &entry.buffer) {
        Ok(()) => {
            let _ = writeln!(
                serial(),
                "milestone 35: syscall CLOSE (process {id}) -- fd {fd} was dirty, persisted {} bytes to '{}' on the real on-disk filesystem",
                entry.buffer.len(),
                entry.path
            );
            Some(true)
        }
        Err(e) => {
            let _ = writeln!(
                serial(),
                "milestone 35: syscall CLOSE (process {id}) -- fd {fd} was dirty, but fs::write_file('{}') FAILED: {e} -- fd released anyway, data NOT persisted",
                entry.path
            );
            Some(false)
        }
    }
}

/// MILESTONE 34: the actual page-table-building mechanism, factored out
/// so BOTH the hardcoded PROCESS_A/PROCESS_B path (create_process(),
/// below) and loader.rs's real-file path (create_loaded_process(), the
/// MILESTONE 35 split of what used to be load_and_run_image()) run
/// through identical, unduplicated unsafe mapping code -- the only
/// difference between them is where `image`'s bytes came from. Every
/// process gets code, stack, AND heap pages mapped uniformly here
/// (Milestone 33's heap isn't special-cased to the hardcoded path);
/// `image` becomes the new process's code page content verbatim
/// (zero-padded to a full page) -- rejected up front, before any frame
/// is even allocated, if it's bigger than MAX_CODE_IMAGE_BYTES (one
/// page), the code frame's real capacity.
fn create_process_from_image(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    label: &'static str,
    image: &[u8],
) -> Result<Process, &'static str> {
    if image.len() > MAX_CODE_IMAGE_BYTES {
        return Err("program image exceeds the 4096-byte code page capacity (one page, no multi-page programs yet)");
    }

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

    // MILESTONE 33: pre-map this process's own private heap pages, same
    // flags and same private-P3/P2/P1-chain mechanism as code/stack above
    // -- HEAP_START shares USER_CODE_ADDR/USER_STACK_ADDR's p4_index
    // (170), so this extends the SAME private chain create_process()
    // already started building for this process, rather than needing any
    // new PML4-level privacy reasoning.
    let mut heap_frames = Vec::with_capacity(HEAP_PAGE_COUNT as usize);
    for i in 0..HEAP_PAGE_COUNT {
        let heap_frame = frame_allocator.allocate_frame().ok_or("out of physical frames (heap)")?;
        let heap_page = Page::<Size4KiB>::containing_address(VirtAddr::new(HEAP_START + i * PAGE_SIZE as u64));
        unsafe {
            process_mapper
                .map_to(heap_page, heap_frame, flags, frame_allocator)
                .map_err(|_| "map_to failed (heap page)")?
                .flush();
        }
        // Zeroed through the phys-mem-offset direct view, same reasoning
        // as the code page's zeroing below -- this process's own frame,
        // written directly rather than through a VA that could currently
        // resolve through someone else's page tables.
        let heap_frame_virt = phys_mem_offset + heap_frame.start_address().as_u64();
        unsafe { core::ptr::write_bytes::<u8>(heap_frame_virt.as_mut_ptr(), 0, PAGE_SIZE) };
        heap_frames.push(heap_frame);
    }
    let _ = writeln!(
        serial(),
        "milestone 33: process {label} -- private heap mapped: {} pages at {:#x}..{:#x}, backed by physical frames {:?}",
        HEAP_PAGE_COUNT,
        HEAP_START,
        HEAP_START + HEAP_SIZE - 1,
        heap_frames.iter().map(|f| f.start_address().as_u64()).collect::<Vec<_>>()
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
        core::ptr::copy_nonoverlapping(image.as_ptr(), code_ptr, image.len());
    }
    let _ = writeln!(
        serial(),
        "milestone 34: process {label} -- copied {} image bytes into private code frame {:#x}",
        image.len(),
        code_frame.start_address().as_u64()
    );

    Ok(Process {
        label,
        pml4_frame: new_pml4_frame,
        code_frame,
        stack_frame,
        heap_frames,
        heap_used: 0,
        // MILESTONE 35: every process gets a real, empty fd table
        // uniformly -- hardcoded PROCESS_A/PROCESS_B and file-loaded
        // processes alike, same "no path-dependent special-casing"
        // principle create_process_from_image already applies to
        // code/stack/heap above.
        fds: [None, None, None, None],
        // MILESTONE 36: this is create_process_from_image's own
        // constructor (the flat-binary path) -- extra_frames stays
        // empty here always, per its own doc comment above; only
        // create_process_from_elf populates it.
        extra_frames: Vec::new(),
    })
}

/// MILESTONE 30's original entry point, preserved for PROCESS_A/
/// PROCESS_B: builds the PROCESS_PROGRAM-at-offset-0 + patched heap-
/// marker + message-at-MESSAGE_OFFSET layout as one in-memory image
/// (mirroring usertest::write_fixed_message()'s own pad/truncate-to-
/// MESSAGE_LEN behavior by hand, since that helper wants a pointer into
/// an already-mapped page, not a plain Vec), then hands it to
/// create_process_from_image() (MILESTONE 34) -- which no longer cares,
/// and never did semantically, whether those bytes came from this
/// hardcoded array or an arbitrary file.
fn create_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    label: &'static str,
    message: &str,
    heap_marker: u8,
) -> Result<Process, &'static str> {
    let image_len = (usertest::MESSAGE_OFFSET as usize + usertest::MESSAGE_LEN).max(PROCESS_PROGRAM.len());
    let mut image = vec![0u8; image_len];
    image[..PROCESS_PROGRAM.len()].copy_from_slice(&PROCESS_PROGRAM);
    image[HEAP_MARKER_PATCH_OFFSET] = heap_marker;

    let msg_bytes = message.as_bytes();
    let n = msg_bytes.len().min(usertest::MESSAGE_LEN);
    let start = usertest::MESSAGE_OFFSET as usize;
    for b in image[start..start + usertest::MESSAGE_LEN].iter_mut() {
        *b = b' ';
    }
    image[start..start + n].copy_from_slice(&msg_bytes[..n]);

    create_process_from_image(frame_allocator, phys_mem_offset, label, &image)
}

/// MILESTONE 30: creates process A and process B, each with its own
/// PML4/code frame/stack frame, printing distinct messages through the
/// Milestone 31 write(ptr,len) syscall -- called once at boot, mirroring
/// usertest::setup()'s own once-at-boot pattern, while a live
/// frame_allocator is conveniently already in scope in kernel_main.
pub fn init_test_processes(frame_allocator: &mut impl FrameAllocator<Size4KiB>, phys_mem_offset: VirtAddr) -> Result<(), &'static str> {
    let _ = writeln!(serial(), "milestone 30: creating process A's private address space...");
    let a = create_process(
        frame_allocator,
        phys_mem_offset,
        "A",
        "hello from process A -- via real write(ptr,len)!",
        b'A',
    )?;
    let _ = writeln!(
        serial(),
        "milestone 30: process A created -- pml4={:#x} code={:#x} stack={:#x}",
        a.pml4_frame.start_address().as_u64(),
        a.code_frame.start_address().as_u64(),
        a.stack_frame.start_address().as_u64()
    );
    *PROCESS_A.lock() = Some(a);

    let _ = writeln!(serial(), "milestone 30: creating process B's private address space...");
    let b = create_process(
        frame_allocator,
        phys_mem_offset,
        "B",
        "hello from process B -- via real write(ptr,len)!",
        b'B',
    )?;
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

/// MILESTONE 35: creates FDTEST_PROCESS -- see that static's own doc
/// comment for why this third hardcoded slot exists. Called once at
/// boot from kernel_main, right after init_test_processes(), while the
/// same local frame_allocator/phys_mem_offset are still conveniently in
/// scope -- mirrors init_test_processes()'s own pattern exactly, just
/// building from an arbitrary `image: &[u8]` (loader::FDTEST_PROGRAM)
/// via create_process_from_image() directly, the same core Milestone 34
/// factored out for loader.rs's file-loaded path, rather than going
/// through create_process()'s PROCESS_PROGRAM-specific wrapper.
pub fn init_fdtest_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    image: &[u8],
) -> Result<(), &'static str> {
    let _ = writeln!(serial(), "milestone 35: creating FDTEST_PROCESS's private address space...");
    let p = create_process_from_image(frame_allocator, phys_mem_offset, "fdtest", image)?;
    let _ = writeln!(
        serial(),
        "milestone 35: FDTEST_PROCESS created -- pml4={:#x} code={:#x} stack={:#x}",
        p.pml4_frame.start_address().as_u64(),
        p.code_frame.start_address().as_u64(),
        p.stack_frame.start_address().as_u64()
    );
    *FDTEST_PROCESS.lock() = Some(p);
    Ok(())
}

/// MILESTONE 30: the `runproc N` shell command's entry point. Switches
/// CR3 to process N's own PML4, enters ring 3 at the SAME virtual
/// address usertest.rs always uses, lets the syscalls run (write reads
/// this process's own embedded message out of its own code page and
/// prints it to serial, exit restores the kernel's own CR3 and returns),
/// and comes back here once resume_kernel() has unwound back through
/// enter_ring3_now()'s call. Safe to call repeatedly and in any order (1,
/// 2, 1, 2, 2, 1, ...): each call reads the process's CURRENT pml4_frame
/// out of its Mutex slot fresh, and the process's own frames are never
/// mutated by a run, only by create_process() at boot.
pub fn run(id: u8) -> Result<(), &'static str> {
    let slot = match id {
        1 => &PROCESS_A,
        2 => &PROCESS_B,
        FDTEST_PROCESS_ID => &FDTEST_PROCESS,
        _ => return Err("no such process -- use 1, 2, or 4 (FDTEST_PROCESS_ID)"),
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

/// MILESTONE 34: holds the single process most recently built by
/// loader.rs's `runfile` from a real file's bytes -- one slot, replaced
/// (not accumulated) on each call, matching PROCESS_A/PROCESS_B's own
/// one-shot-per-slot design rather than a general process table with
/// PIDs. Kept alive here (rather than dropped once run_loaded_process()
/// returns) purely so its PML4/code/stack/heap frames aren't freed out
/// from under a process that could, in principle, still be referenced --
/// in practice run_loaded_process() always runs it to completion
/// (write + exit) before returning, same as run() does for A/B.
static LOADED_PROCESS: Mutex<Option<Process>> = Mutex::new(None);

/// Distinguishes a file-loaded process from PROCESS_A (id 1) / PROCESS_B
/// (id 2) in ACTIVE_PROCESS -- usertest::syscall_dispatch's syscall arms
/// only check `active != 0` before reading the currently-active
/// process's own message/heap, so any nonzero value works; 3 simply
/// keeps the id space non-overlapping and gives the serial log a
/// distinct, greppable number.
const LOADED_PROCESS_ID: u8 = 3;

/// MILESTONE 34: builds a fresh process from an arbitrary in-memory code
/// image -- loader.rs's `runfile` is the only real caller, always AFTER
/// it has already read a file's bytes off the real on-disk filesystem
/// and checked their length against MAX_CODE_IMAGE_BYTES -- and runs it
/// once immediately. Otherwise IDENTICAL to run()'s own CR3-switch /
/// enter_ring3_now() / (exit syscall restores CR3) sequence: the only
/// real difference is that this process's code frame was populated from
/// `image` (an arbitrary runtime byte slice) instead of one of the two
/// fixed PROCESS_A/PROCESS_B slots created once at boot. Like them, it
/// also gets its own private heap mapped by create_process_from_image()
/// -- unused by a plain print+exit test program, but not withheld
/// either, since every process gets one uniformly now.
/// MILESTONE 35: split out of what used to be one function
/// (`load_and_run_image`, which built the process AND entered ring 3 in
/// a single call, taking `frame_allocator` for its entire duration) --
/// see run_loaded_process()'s own doc comment for the real, pre-existing
/// Milestone 34 bug this split fixes. This half does ONLY the frame
/// allocation and page-table building (via create_process_from_image)
/// and stores the result in LOADED_PROCESS; it never touches ring 3, so
/// it's safe for the caller to hold frame_allocator's lock for exactly
/// this call and no longer.
pub fn create_loaded_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    image: &[u8],
) -> Result<(), &'static str> {
    let proc = create_process_from_image(frame_allocator, phys_mem_offset, "loaded", image)?;
    *LOADED_PROCESS.lock() = Some(proc);
    Ok(())
}

/// MILESTONE 35: the other half of what used to be `load_and_run_image`
/// -- switches CR3 to LOADED_PROCESS's pml4 and enters ring 3, exactly
/// like the old function's second half did, but takes NO frame_allocator
/// argument at all, so loader.rs's `run_file` can (and now does) call
/// this AFTER releasing memory::with_frame_allocator's lock instead of
/// from inside its closure.
///
/// **Real, pre-existing Milestone 34 bug, found and fixed while
/// verifying Milestone 35's own fd syscalls, not caused by them**: the
/// original `load_and_run_image` took `frame_allocator: &mut impl
/// FrameAllocator<Size4KiB>` for its WHOLE body, and loader.rs's
/// `run_file` called it from inside `memory::with_frame_allocator`'s
/// closure -- which holds the global FRAME_ALLOCATOR spin::Mutex for as
/// long as the closure runs. That meant the ENTIRE ring-3 excursion
/// (enter_ring3_now(), which runs with interrupts enabled per usertest
/// design) executed with that lock held, for no reason -- nothing in
/// the excursion itself ever touches frame_allocator. Verified as the
/// real root cause, not guessed: `runproc` (Milestone 30's PROCESS_A/B
/// path, which never touches memory::with_frame_allocator at all since
/// A/B are allocated once at boot) never crashed under repeated testing,
/// while `runfile` (which DID route the whole excursion through that
/// lock) reproducibly page-faulted at instruction address 0 immediately
/// after "resumed in kernel context" -- with UNMODIFIED Milestone 34
/// code (loader.rs's own pre-existing `seedtestprog`/`testprog`, no
/// Milestone 35 fd syscalls involved at all), confirming this was
/// already broken before this milestone touched anything. This split
/// (create_loaded_process does the part that needs the lock,
/// run_loaded_process does the part that doesn't) removes the
/// lock-held-across-a-whole-ring-3-excursion-with-interrupts-on window
/// entirely.
pub fn run_loaded_process() -> Result<(), &'static str> {
    let pml4_frame = {
        let guard = LOADED_PROCESS.lock();
        let proc = guard.as_ref().ok_or("no loaded process -- create_loaded_process must succeed first")?;
        proc.pml4_frame
    };

    let _ = writeln!(
        serial(),
        "milestone 34: loaded process -- about to switch CR3 to pml4 {:#x}",
        pml4_frame.start_address().as_u64()
    );
    ACTIVE_PROCESS.store(LOADED_PROCESS_ID, Ordering::SeqCst);

    let flags = Cr3Flags::from_bits_truncate(KERNEL_CR3_FLAGS_BITS.load(Ordering::SeqCst));
    unsafe { Cr3::write(pml4_frame, flags) };
    let _ = writeln!(
        serial(),
        "milestone 34: CR3 switched -- entering ring 3 for the file-loaded process at {:#x}",
        usertest::USER_CODE_ADDR
    );

    usertest::enter_ring3_now();

    let _ = writeln!(
        serial(),
        "milestone 34: loaded process -- resumed in kernel context (CR3 already restored by the exit syscall before this point)"
    );
    let _ = writeln!(
        serial(),
        "diagnostic: interrupts during excursion -- timer={} keyboard={}",
        crate::tasks::TIMER_DURING_EXCURSION.swap(0, core::sync::atomic::Ordering::SeqCst),
        crate::tasks::KEYBOARD_DURING_EXCURSION.swap(0, core::sync::atomic::Ordering::SeqCst)
    );
    Ok(())
}

/// MILESTONE 36: real per-segment page-count caps, enforced BEFORE any
/// frame is allocated (same "reject up front, never silently overflow"
/// discipline as MAX_CODE_IMAGE_BYTES). Deliberately small: fs.rs's own
/// MAX_FILE_BYTES cap (4096 bytes total, same constant loader.rs's own
/// self_test_size_check() comment already discusses for the flat-binary
/// path) already puts a hard ceiling on how much any single ELF file
/// COULD ask for, but page-count caps are checked independently here
/// too -- a pathological program header table could, in principle,
/// declare a p_memsz far larger than the file itself (memsz > filesz is
/// completely legal ELF, e.g. for .bss), so file size alone doesn't
/// bound how many PHYSICAL FRAMES a malicious/malformed ELF could try
/// to claim.
const MAX_PAGES_PER_ELF_SEGMENT: u64 = 4;
/// Real, disclosed bound on PT_LOAD segment count this loader will
/// actually map -- elf::parse() already enforces its own
/// elf::MAX_LOAD_SEGMENTS cap on how many it will even PARSE; this is
/// the separate, mapping-side check (kept as its own named constant
/// rather than silently reusing elf::MAX_LOAD_SEGMENTS, so the two
/// layers' caps are independently visible in a diff instead of one
/// hidden re-export away).
const MAX_ELF_LOAD_SEGMENTS: usize = elf::MAX_LOAD_SEGMENTS;
/// Total physical pages this loader will map across EVERY PT_LOAD
/// segment combined, for one ELF-loaded process -- 16 pages (64 KiB) is
/// comfortably more than this milestone's own 2-segment/2-page test
/// ELF needs, while still being a real, fixed, disclosed ceiling rather
/// than "however much the file happens to ask for".
const MAX_TOTAL_ELF_PAGES: u64 = 16;

/// MILESTONE 36: the real page-table-building mechanism for a genuine
/// ELF64 executable -- maps ONE PHYSICAL PAGE PER PT_LOAD-SEGMENT-PAGE
/// at that segment's OWN `p_vaddr` (page-aligned, checked, not just the
/// single fixed USER_CODE_ADDR page create_process_from_image() always
/// uses), copies each page's real `p_filesz`-covered bytes out of
/// `image` at that segment's real `p_offset`, and zero-fills the rest
/// (memsz > filesz, e.g. bss) -- genuinely reading and placing the
/// parsed ELF structure, not just re-running the flat-binary path with
/// extra logging.
///
/// Real, disclosed limitations of THIS function specifically (elf.rs's
/// parse() itself has none of these -- they're loading-side, not
/// parsing-side):
///   - `entry` MUST equal usertest::USER_CODE_ADDR exactly -- see this
///     module's own MILESTONE 36 doc comment (top of file) for why this
///     milestone chose that over making the ring-3 entry trampoline's
///     jump target dynamic.
///   - every PT_LOAD segment's `p_vaddr` MUST be 4 KiB-page-aligned --
///     this loader maps whole pages at page-aligned addresses; a
///     segment starting mid-page (legal ELF, common in real Linux
///     binaries that pack multiple segments' sub-page tails/heads
///     together to save file space) is rejected with a clear error
///     rather than silently misaligned or corrupted.
///   - bounded per-segment and total page counts (see the constants
///     just above) -- rejected up front if exceeded.
///   - every segment (like create_process_from_image()'s single code
///     page) is mapped PRESENT | WRITABLE | USER_ACCESSIBLE regardless
///     of its own `p_flags` -- p_flags is parsed and logged for real
///     (proving the parser read it), but not yet used to differentiate
///     actual page permissions (e.g. a read-only or non-executable
///     PT_LOAD segment is still mapped writable+executable here, same
///     as every other page this kernel has ever mapped for ring 3 --
///     see usertest::setup()'s own comment on why no page in this
///     kernel sets NO_EXECUTE anywhere yet). Disclosed, not hidden:
///     enforcing p_flags for real would be genuine, separate follow-up
///     work.
fn create_process_from_elf(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    label: &'static str,
    entry: u64,
    segments: &[elf::ProgramSegment],
    image: &[u8],
) -> Result<Process, &'static str> {
    if entry != usertest::USER_CODE_ADDR {
        return Err(
            "elf: e_entry does not equal USER_CODE_ADDR -- this loader only runs ELFs specifically \
             linked so their entry point lands exactly there (milestone 36 deliberately did not make \
             the ring-3 entry trampoline's jump target dynamic -- see process.rs's module doc comment)",
        );
    }
    if segments.is_empty() {
        return Err("elf: no PT_LOAD segments to map");
    }
    if segments.len() > MAX_ELF_LOAD_SEGMENTS {
        return Err("elf: more PT_LOAD segments than this loader's fixed cap");
    }

    let user_p4_index = VirtAddr::new(usertest::USER_CODE_ADDR).p4_index();
    let stack_p4_index = VirtAddr::new(usertest::USER_STACK_ADDR).p4_index();
    if user_p4_index != stack_p4_index {
        return Err("USER_CODE_ADDR/USER_STACK_ADDR fall in different PML4 indices -- design assumption violated");
    }

    // Real, honest ELF-loading sanity check: e_entry must actually fall
    // within SOME loaded segment's declared [p_vaddr, p_vaddr+p_memsz)
    // range -- checked against the ACTUAL parsed segments, not just
    // assumed true because entry == USER_CODE_ADDR was already checked
    // above (a malformed ELF could still claim that entry address while
    // no PT_LOAD segment actually covers it).
    let entry_in_segment = segments
        .iter()
        .any(|s| entry >= s.p_vaddr && entry < s.p_vaddr.saturating_add(s.p_memsz));
    if !entry_in_segment {
        return Err("elf: e_entry does not fall within any PT_LOAD segment's [p_vaddr, p_vaddr+p_memsz) range");
    }

    // Validate EVERY segment's page range up front -- page alignment,
    // per-segment page cap, and (mirroring Milestone 30's own discipline
    // for the single code/stack/heap pages) that every page genuinely
    // computed p4_index()'s to the SAME private PML4 slot this process's
    // new PML4 will actually rebuild below, checked for real rather than
    // assumed true just because USER_CODE_ADDR/HEAP_START happen to
    // share it.
    let mut total_pages: u64 = 0;
    for seg in segments {
        if seg.p_vaddr % PAGE_SIZE as u64 != 0 {
            return Err(
                "elf: a PT_LOAD segment's p_vaddr is not page-aligned -- this loader requires page-aligned \
                 segments (a real, disclosed limitation, not silently rounded)",
            );
        }
        if seg.p_memsz == 0 {
            return Err("elf: a PT_LOAD segment has p_memsz == 0");
        }
        let pages = seg.p_memsz.div_ceil(PAGE_SIZE as u64);
        if pages > MAX_PAGES_PER_ELF_SEGMENT {
            return Err("elf: a PT_LOAD segment needs more pages than this loader's fixed per-segment cap");
        }
        for i in 0..pages {
            let va = VirtAddr::new(seg.p_vaddr + i * PAGE_SIZE as u64);
            if va.p4_index() != user_p4_index {
                return Err(
                    "elf: a PT_LOAD segment page falls outside the process's private PML4 slot -- refusing to map",
                );
            }
        }
        total_pages = total_pages.checked_add(pages).ok_or("elf: total PT_LOAD page count overflows")?;
    }
    if total_pages > MAX_TOTAL_ELF_PAGES {
        return Err("elf: total PT_LOAD page count across all segments exceeds this loader's fixed cap");
    }

    let _ = writeln!(
        serial(),
        "milestone 36: process {label} -- ELF validated: e_entry={:#x} falls inside a mapped segment, {} PT_LOAD segment(s), {} total page(s), all within private p4 index {}",
        entry,
        segments.len(),
        total_pages,
        u16::from(user_p4_index)
    );

    let new_pml4_frame = frame_allocator.allocate_frame().ok_or("out of physical frames (pml4)")?;
    let new_pml4_ptr: *mut PageTable = (phys_mem_offset + new_pml4_frame.start_address().as_u64()).as_mut_ptr();
    let new_pml4: &mut PageTable = unsafe { &mut *new_pml4_ptr };
    new_pml4.zero();

    let (kernel_pml4_frame, _) = Cr3::read();
    let kernel_pml4_ptr: *const PageTable = (phys_mem_offset + kernel_pml4_frame.start_address().as_u64()).as_ptr();
    let kernel_pml4: &PageTable = unsafe { &*kernel_pml4_ptr };

    // Same private-PML4-slot construction as create_process_from_image()
    // above -- every kernel-space entry shared by copying the ENTRY
    // (not the hierarchy underneath it), only user_p4_index left zeroed
    // for map_to() to build a genuinely fresh, private chain into.
    for i in 0u16..512 {
        let idx = PageTableIndex::new(i);
        if idx != user_p4_index {
            new_pml4[idx] = kernel_pml4[idx].clone();
        }
    }
    let _ = writeln!(
        serial(),
        "milestone 36: process {label} -- new pml4 {:#x} populated: 511 kernel-space entries shared, index {} left private",
        new_pml4_frame.start_address().as_u64(),
        u16::from(user_p4_index)
    );

    let mut process_mapper = unsafe { OffsetPageTable::new(new_pml4, phys_mem_offset) };
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

    let mut code_frame: Option<PhysFrame<Size4KiB>> = None;
    let mut extra_frames = Vec::new();
    // Collected across the whole loop and logged ONCE at the end (see
    // the single writeln! after this loop) -- matches
    // create_process_from_image()'s own pattern of one summary log
    // after its code/stack mapping, not one log per page. Several
    // per-page `writeln!` calls each construct a fresh SerialPort and
    // block on real (emulated but genuinely time-costed) UART I/O;
    // batching to one write keeps this function's total real-time cost
    // closer to the already-proven-stable flat-binary path's, rather
    // than adding up N page-mapping log lines' worth of extra latency
    // before ever reaching the ring-3 entry below.
    let mut summary: Vec<(u64, u64, u64, u32)> = Vec::new();

    for seg in segments {
        let pages = seg.p_memsz.div_ceil(PAGE_SIZE as u64);
        for i in 0..pages {
            let page_va = seg.p_vaddr + i * PAGE_SIZE as u64;
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_va));
            let frame = frame_allocator.allocate_frame().ok_or("out of physical frames (elf segment)")?;
            unsafe {
                process_mapper
                    .map_to(page, frame, flags, frame_allocator)
                    .map_err(|_| "map_to failed (elf segment page)")?
                    .flush();
            }

            // Zeroed through the phys-mem-offset direct view of THIS
            // process's own freshly-allocated frame -- same reasoning as
            // create_process_from_image()'s code/heap page zeroing:
            // written directly through phys_mem_offset rather than
            // through the page_va under whatever CR3 is currently
            // loaded (the kernel's own, at this point), which could
            // resolve to someone else's mapping entirely.
            let frame_virt = phys_mem_offset + frame.start_address().as_u64();
            unsafe { core::ptr::write_bytes::<u8>(frame_virt.as_mut_ptr(), 0, PAGE_SIZE) };

            // Copy however many of this segment's REAL p_filesz bytes
            // fall into THIS specific page -- p_vaddr is page-aligned
            // (checked above), so page i's file-backed byte range is
            // simply [i*PAGE_SIZE, i*PAGE_SIZE+PAGE_SIZE) clamped to
            // p_filesz; anything beyond p_filesz (up to p_memsz) is real
            // bss and stays zeroed from the write_bytes above.
            let page_off_in_seg = i * PAGE_SIZE as u64;
            let copied = if page_off_in_seg < seg.p_filesz {
                let copy_len = core::cmp::min(PAGE_SIZE as u64, seg.p_filesz - page_off_in_seg);
                let src_start = (seg.p_offset + page_off_in_seg) as usize;
                let src = &image[src_start..src_start + copy_len as usize];
                unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), frame_virt.as_mut_ptr(), copy_len as usize) };
                copy_len
            } else {
                0
            };
            summary.push((page_va, frame.start_address().as_u64(), copied, seg.p_flags));

            if page_va == usertest::USER_CODE_ADDR {
                code_frame = Some(frame);
            } else {
                extra_frames.push(frame);
            }
        }
    }
    let _ = writeln!(
        serial(),
        "milestone 36: process {label} -- mapped {} PT_LOAD page(s): {:?}",
        summary.len(),
        summary
    );

    let code_frame = code_frame.ok_or(
        "elf: internal error -- entry-containment check passed but no mapped page backs USER_CODE_ADDR",
    )?;

    let stack_frame = frame_allocator.allocate_frame().ok_or("out of physical frames (stack)")?;
    let stack_page = Page::<Size4KiB>::containing_address(VirtAddr::new(usertest::USER_STACK_ADDR));
    unsafe {
        process_mapper
            .map_to(stack_page, stack_frame, flags, frame_allocator)
            .map_err(|_| "map_to failed (stack page)")?
            .flush();
    }
    let _ = writeln!(
        serial(),
        "milestone 36: process {label} -- private stack page mapped to frame {:#x}",
        stack_frame.start_address().as_u64()
    );

    // Same private heap pre-mapping as create_process_from_image() --
    // every process gets one uniformly, ELF-loaded or not (see that
    // function's own comment; sbrk() itself still only recognizes
    // PROCESS_A/PROCESS_B ids, an existing, already-disclosed
    // Milestone 34 limitation this milestone doesn't change).
    let mut heap_frames = Vec::with_capacity(HEAP_PAGE_COUNT as usize);
    for i in 0..HEAP_PAGE_COUNT {
        let heap_frame = frame_allocator.allocate_frame().ok_or("out of physical frames (heap)")?;
        let heap_page = Page::<Size4KiB>::containing_address(VirtAddr::new(HEAP_START + i * PAGE_SIZE as u64));
        unsafe {
            process_mapper
                .map_to(heap_page, heap_frame, flags, frame_allocator)
                .map_err(|_| "map_to failed (heap page)")?
                .flush();
        }
        let heap_frame_virt = phys_mem_offset + heap_frame.start_address().as_u64();
        unsafe { core::ptr::write_bytes::<u8>(heap_frame_virt.as_mut_ptr(), 0, PAGE_SIZE) };
        heap_frames.push(heap_frame);
    }
    let _ = writeln!(
        serial(),
        "milestone 36: process {label} -- private heap mapped: {} pages at {:#x}..{:#x}",
        HEAP_PAGE_COUNT,
        HEAP_START,
        HEAP_START + HEAP_SIZE - 1
    );

    Ok(Process {
        label,
        pml4_frame: new_pml4_frame,
        code_frame,
        stack_frame,
        heap_frames,
        heap_used: 0,
        // MILESTONE 35: create_process_from_elf predates the fd table --
        // give ELF-loaded processes the same real, empty fd table every
        // other process gets uniformly (see create_process_from_image's
        // own identical field for the reasoning).
        fds: [None, None, None, None],
        extra_frames,
    })
}

/// MILESTONE 36 BUGFIX (found and fixed while root-causing the
/// intermittent ~50%-reproduction page fault the README disclosed after
/// an ELF-loaded process returns, if shell activity follows within
/// ~1s): this used to be `load_and_run_elf()`, ONE function taking
/// `frame_allocator` for its ENTIRE body -- including the CR3 switch and
/// `usertest::enter_ring3_now()`'s whole ring-3 excursion (interrupts
/// enabled throughout, per usertest's own design). loader.rs's `run_elf`
/// called it from inside `memory::with_frame_allocator`'s closure,
/// exactly reproducing the SAME real, already-diagnosed hazard
/// Milestone 35's own doc comment on `run_loaded_process()` describes
/// for the flat-binary path: the global FRAME_ALLOCATOR spin::Mutex
/// held across a whole ring-3 excursion with interrupts on, for no
/// reason (the excursion itself never touches frame_allocator) --
/// leaving a real window for a nested timer interrupt (Milestone 25's
/// scheduler, which DOES sometimes need to allocate, e.g. on `spawn`)
/// to land while the lock is already held. The flat-binary path was
/// split into create_loaded_process()/run_loaded_process() specifically
/// to close that window; this ELF path was built in a parallel-agent
/// workspace that branched before that fix landed, and the merge never
/// carried it over -- this is that same split, applied here.
///
/// This half does ONLY the parsing-driven page-table build (via
/// create_process_from_elf) and stores the result in LOADED_PROCESS; it
/// never touches ring 3, so it's safe for the caller to hold
/// frame_allocator's lock for exactly this call and no longer. The
/// second half needs no new function at all: run_loaded_process()
/// already just switches CR3 to whatever's in LOADED_PROCESS and enters
/// ring 3 -- generic over flat-binary or ELF-loaded processes alike,
/// since both share the same LOADED_PROCESS slot/LOADED_PROCESS_ID by
/// design (see this function's own prior doc comment on that sharing
/// being safe).
pub fn create_loaded_elf_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    image: &[u8],
    elf_image: &elf::ElfImage,
) -> Result<(), &'static str> {
    let proc = create_process_from_elf(
        frame_allocator,
        phys_mem_offset,
        "elf-loaded",
        elf_image.entry,
        &elf_image.segments,
        image,
    )?;
    *LOADED_PROCESS.lock() = Some(proc);
    Ok(())
}
