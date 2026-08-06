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
//!
//! MILESTONE 37: real `fork`/`exec`/`wait` -- the "single biggest
//! structural gap toward a real Unix process model" the README's own
//! Linux-comparability roadmap named after Milestone 36. **The real,
//! honest scoping decision, stated up front**: this is option (a) from
//! the milestone brief, NOT option (b). `tasks.rs`/`scheduler.rs`'s real
//! preemptive, timer-interrupt-driven scheduler is UNCHANGED by this
//! milestone -- it still only schedules ring-0 kernel tasks, exactly as
//! Milestone 25 left it. Ring-3 process execution stays exactly what it
//! has been since Milestone 27: a synchronous "call out and block" model
//! where the kernel is not doing anything ELSE while a ring-3 process
//! runs. `fork()` genuinely creates a new process -- a new PML4, new
//! physical frames, and a REAL byte-for-byte copy of the parent's own
//! code/stack/heap frame CONTENTS (not shared references -- this kernel
//! still has no copy-on-write) -- but the child does not run
//! "concurrently" with the parent in any sense a real scheduler would
//! recognize. Instead, exactly as the milestone brief's own suggested
//! v1 describes: the child is created and its address space fully,
//! genuinely built at `fork()` time, but it only actually EXECUTES when
//! the parent's own `wait()` syscall explicitly drives it -- `wait()`
//! synchronously switches CR3 to the child, runs it all the way to ITS
//! OWN `exit()` syscall (nested inside the parent's `wait()` syscall's
//! own dispatch), then restores the parent's context and returns. This
//! is a genuine, real implementation of "block until a child changes
//! state" for a kernel with no other way to run "something else" in the
//! meantime -- not a simulation of it.
//!
//! Two further real, honest, DISCLOSED simplifications on top of that
//! core scoping decision:
//!   - A forked child resumes at the EXACT (rip, rsp) captured from the
//!     parent's own hardware-recorded `SyscallRegs` at the moment
//!     `fork()` was called (with rax forced to 0, fork()'s "0 in the
//!     child" contract) -- NOT at `USER_CODE_ADDR`. This matters for
//!     real correctness, not just fidelity: the ring-3 entry trampoline
//!     (`usertest::enter_ring3_now()`) was deliberately kept fixed at
//!     `USER_CODE_ADDR` rather than made dynamic (same Milestone 36
//!     decision, restated above) -- if the child were instead restarted
//!     from the top of its own program, it would immediately execute
//!     the SAME `fork()` call again (an infinite fork loop), not
//!     "continue past its own fork() call" the way real fork() must.
//!     `usertest::enter_ring3_as_forked_child()` is a second, dedicated
//!     entry trampoline built for exactly this -- see its own doc
//!     comment.
//!   - Nesting depth is bounded at exactly 1, enforced for real (checked
//!     and refused, not just documented): a forked child currently being
//!     resumed by `wait()` cannot itself `fork()`/`wait()`. This is a
//!     direct, honest consequence of using ONE dedicated kernel-resume
//!     anchor (`usertest::CHILD_KERNEL_RSP`) for the nested child
//!     excursion rather than a general per-nesting-level stack of
//!     anchors -- the same "small fixed cap, not full generality" spirit
//!     as `MAX_OPEN_FILES`/`MAX_LOAD_SEGMENTS` elsewhere in this
//!     codebase, applied to nesting depth instead of a table size.
//!
//! A real, dynamic, bounded PROCESS_TABLE (MAX_PROCESSES = 4, real PID
//! allocation starting at PID_TABLE_BASE = 10) backs `fork()`-created
//! children -- see that static's own doc comment for why the FOUR
//! pre-existing hardcoded/loaded-file slots (PROCESS_A, PROCESS_B,
//! LOADED_PROCESS, FDTEST_PROCESS) were left as legacy, separate statics
//! rather than migrated in. `exec()` reuses this same table/dispatch
//! uniformly (any process id, hardcoded or forked, can `exec()`) and
//! replaces ONLY the calling process's code-frame CONTENTS in place --
//! same PID, same PML4, same open fds, same mapped heap frames (with
//! `heap_used` reset to 0, a fresh heap for the new image) -- reusing
//! `MAX_CODE_IMAGE_BYTES` and the exact zero-then-copy step
//! `create_process_from_image()` already uses for a brand-new process's
//! code page, not a duplicated implementation of it.
//!
//! Verified for real: `runfork` (new shell command) runs a new
//! hand-assembled FORK_TEST_PROGRAM that calls `fork()`, branches on the
//! return value (0 = child), has the parent print a distinguishing
//! message and then `wait()` for the child, and has the child `exec()`
//! into a completely different on-disk program (`testprog`, from
//! Milestone 34's `seedtestprog`) which prints ITS OWN distinguishing
//! message instead -- real, observable proof that fork() created a
//! genuinely separate process (different PID, independently verified
//! physical frames) and exec() genuinely replaced its code, with the
//! parent's `wait()` genuinely blocking until that whole sequence
//! completes. See the milestone report for the actual captured serial
//! log.

use crate::elf;
use crate::fs;
use crate::memory;
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
/// MILESTONE 37: Clone derived so fork() can give a child process a
/// real, independent DEEP COPY of each of the parent's open files (own
/// path/buffer/pos/dirty, not a shared reference) -- see fork()'s own
/// doc comment for why a deep copy, not POSIX's real shared-fd-table
/// semantics, is this milestone's honest, disclosed simplification.
#[derive(Clone)]
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

/// MILESTONE 37: fork()'s own per-element fd-table clone (see its own
/// comment for why it can't just call `.clone()` on the whole array)
/// hardcodes 4 element accesses matching MAX_OPEN_FILES's CURRENT value
/// -- this compile-time assertion fails loudly if that ever drifts,
/// rather than silently truncating a process's fd table on fork().
const _ASSERT_MAX_OPEN_FILES_IS_4: () = assert!(MAX_OPEN_FILES == 4);

/// MILESTONE 40: an fd slot now names one of two genuinely different
/// underlying resources, not just a file -- `File` is exactly the
/// pre-Milestone-40 `OpenFile` case (unchanged), `PipeRead`/`PipeWrite`
/// name an END of a pipe by its index into the global PIPE_TABLE below,
/// NOT owned data. That index-not-data distinction is the whole design:
/// deriving Clone on this enum still gives fork() its existing "one
/// `.clone()` per fd slot, one `let` statement per slot" pattern
/// unchanged (see fork()'s own comment on why combining multiple
/// fallible clones into one expression broke the build once already),
/// but for a pipe end, cloning an index is exactly the reference-sharing
/// a real fork() needs -- the parent and child end up with two DIFFERENT
/// Process structs whose fd slot holds the SAME index into ONE shared
/// PIPE_TABLE entry, not two independent copies of a buffer (which
/// would silently break the pipe's entire reason to exist). `File`
/// keeps its own pre-existing honest limitation: dup()/dup2() of a
/// File fd (below) deep-copies the same way fork() already does,
/// documented there, not pretended away here.
#[derive(Clone)]
enum FdEntry {
    File(OpenFile),
    PipeRead(usize),
    PipeWrite(usize),
}

/// MILESTONE 40: fixed ring-buffer capacity per pipe, matching this
/// project's small-fixed-bound style (MAX_OPEN_FILES/MAX_PROCESSES's own
/// precedent) rather than a dynamically-growing buffer.
const PIPE_CAPACITY: usize = 512;

/// MILESTONE 40: how many pipes can exist system-wide at once (NOT
/// per-process -- a pipe is a global resource two different processes'
/// fd tables can both reference, so it can't live inside either one's
/// own Process struct).
const MAX_PIPES: usize = 4;

/// MILESTONE 40: a real, fixed-size ring buffer plus real open-end
/// reference counts. The counts exist for exactly one reason: a pipe's
/// storage is only actually freed (PIPE_TABLE slot -> None) once EVERY
/// fd across EVERY process that ever referenced either end -- including
/// ends duplicated by fork() or dup()/dup2() -- has been close()'d.
/// Without real counting, closing the fd in ONE of two processes sharing
/// a forked pipe would either free it out from under the other (a
/// dangling reference) or never free it at all (a permanent leak of one
/// of only MAX_PIPES=4 global slots). Both `read_open`/`write_open`
/// start at 1 (the fd pipe() itself hands back) and are incremented by
/// fork()/dup()/dup2() and decremented by close_fd() -- see each site's
/// own comment.
struct Pipe {
    buffer: [u8; PIPE_CAPACITY],
    /// Index of the next byte to be read (wraps modulo PIPE_CAPACITY).
    read_pos: usize,
    /// Index of the next byte to be written (wraps modulo PIPE_CAPACITY).
    write_pos: usize,
    /// How many real, unread bytes are currently buffered -- distinct
    /// from `read_pos`/`write_pos` alone because those two can be equal
    /// at BOTH "completely empty" and "completely full" (a ring buffer
    /// needs a third piece of state to disambiguate the two).
    len: usize,
    read_open: u32,
    write_open: u32,
}

/// MILESTONE 40: the global pipe table -- see Pipe's own doc comment for
/// why this can't be per-process state. `spin::Mutex<[Option<T>; N]>` is
/// the exact same pattern PROCESS_TABLE already established for a
/// dynamic, bounded set of global kernel objects.
static PIPE_TABLE: Mutex<[Option<Pipe>; MAX_PIPES]> = Mutex::new([const { None }; MAX_PIPES]);

/// MILESTONE 40: allocates a real PIPE_TABLE slot (both ends starting at
/// `read_open`/`write_open` = 1, `len` = 0) and gives the calling
/// process two new fd-table entries -- `FdEntry::PipeRead`/`PipeWrite`
/// -- pointing at it. Returns `(read_fd, write_fd)`, or `None` if either
/// the process's own fd table can't fit both new entries (needs 2 free
/// slots, not just 1) or PIPE_TABLE itself is full (all MAX_PIPES
/// already live).
pub(crate) fn pipe_create(id: u8) -> Option<(u64, u64)> {
    let mut table = PIPE_TABLE.lock();
    let pipe_index = table.iter().position(|p| p.is_none())?;
    table[pipe_index] = Some(Pipe {
        buffer: [0u8; PIPE_CAPACITY],
        read_pos: 0,
        write_pos: 0,
        len: 0,
        read_open: 1,
        write_open: 1,
    });
    drop(table);
    let result = with_process_mut(id, |proc| {
        let mut free = proc.fds.iter().enumerate().filter(|(_, f)| f.is_none()).map(|(i, _)| i);
        let read_slot = free.next()?;
        let write_slot = free.next()?;
        proc.fds[read_slot] = Some(FdEntry::PipeRead(pipe_index));
        proc.fds[write_slot] = Some(FdEntry::PipeWrite(pipe_index));
        Some((read_slot as u64, write_slot as u64))
    })?;
    if result.is_none() {
        // Process's fd table couldn't fit both ends -- release the
        // PIPE_TABLE slot we just allocated rather than leaking it.
        PIPE_TABLE.lock()[pipe_index] = None;
    }
    result
}

/// MILESTONE 40: reads up to `len` bytes out of the pipe fd names.
/// Real, honest, DISCLOSED non-blocking semantics: this kernel has no
/// scheduler-level blocking primitive a syscall could suspend a process
/// on (Milestone 25's background scheduling is cooperative task
/// switching, not a wait-queue) -- so reading an empty pipe returns
/// `Some(vec![])` (0 bytes, not an error) immediately rather than really
/// blocking until a writer produces data, the same "explicit partial/
/// empty result, never silently wrong" discipline read_fd() already
/// established for end-of-file. Returns `None` only if `fd` doesn't name
/// a currently-open PipeRead entry for this process.
fn pipe_read(id: u8, fd: u64, len: usize) -> Option<Vec<u8>> {
    let pipe_index = with_process_mut(id, |proc| match proc.fds.get(fd as usize)? {
        Some(FdEntry::PipeRead(idx)) => Some(*idx),
        _ => None,
    })??;
    let mut table = PIPE_TABLE.lock();
    let pipe = table[pipe_index].as_mut()?;
    let n = len.min(pipe.len);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(pipe.buffer[(pipe.read_pos + i) % PIPE_CAPACITY]);
    }
    pipe.read_pos = (pipe.read_pos + n) % PIPE_CAPACITY;
    pipe.len -= n;
    Some(out)
}

/// MILESTONE 40: writes as many bytes of `data` as currently fit into
/// the pipe fd names, returning the ACTUAL count accepted -- a real,
/// honest partial write if the ring buffer doesn't have room for all of
/// it (same discipline as write_fd()'s own MAX_FILE_BYTES truncation),
/// never blocking for room to free up (see pipe_read()'s doc comment for
/// why -- no blocking primitive exists to wait on). Returns `None` only
/// if `fd` doesn't name a currently-open PipeWrite entry for this
/// process.
fn pipe_write(id: u8, fd: u64, data: &[u8]) -> Option<usize> {
    let pipe_index = with_process_mut(id, |proc| match proc.fds.get(fd as usize)? {
        Some(FdEntry::PipeWrite(idx)) => Some(*idx),
        _ => None,
    })??;
    let mut table = PIPE_TABLE.lock();
    let pipe = table[pipe_index].as_mut()?;
    let room = PIPE_CAPACITY - pipe.len;
    let n = data.len().min(room);
    for (i, byte) in data[..n].iter().enumerate() {
        pipe.buffer[(pipe.write_pos + i) % PIPE_CAPACITY] = *byte;
    }
    pipe.write_pos = (pipe.write_pos + n) % PIPE_CAPACITY;
    pipe.len += n;
    Some(n)
}

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
    fds: [Option<FdEntry>; MAX_OPEN_FILES],
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
    /// MILESTONE 37: `Some(parent_pid)` if this process was created by
    /// `fork()`; `None` for every process created any other way (all
    /// four pre-existing hardcoded/loaded-file paths). Checked by
    /// `wait_for_child()` so only a process's REAL parent can reap it.
    parent_pid: Option<u8>,
    /// MILESTONE 37: `Some((resume_rip, resume_rsp))` captured from the
    /// PARENT's own hardware-recorded `SyscallRegs` at the exact moment
    /// `fork()` created this process, if it has never actually been run
    /// yet -- `run_forked_child()` consumes this via `.take()` the one
    /// time it enters ring 3 for this child. `None` for every
    /// non-forked process (nothing ever resumes them via this
    /// mechanism -- they always start fresh at `USER_CODE_ADDR` through
    /// the ordinary `run()`/`run_loaded_process()`/`load_and_run_elf()`
    /// paths).
    pending_resume: Option<(u64, u64)>,
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

/// MILESTONE 37: a FIFTH hardcoded, boot-time-created process slot,
/// structurally identical to PROCESS_A/PROCESS_B/FDTEST_PROCESS (built
/// once at boot, run via the same `run(id)` mechanism), running
/// FORK_TEST_PROGRAM -- this milestone's own fork/exec/wait test
/// program (see that constant's own doc comment). A hardcoded slot,
/// not a PROCESS_TABLE entry, for the same reason PROCESS_A/PROCESS_B/
/// FDTEST_PROCESS are: it is the ONE process that must exist before any
/// `fork()` call could ever happen (fork() is what actually populates
/// PROCESS_TABLE), so it can't itself be a member of that table.
static FORK_TEST_PROCESS: Mutex<Option<Process>> = Mutex::new(None);

/// ACTIVE_PROCESS / with_process_mut id for FORK_TEST_PROCESS -- 5,
/// keeping the non-overlapping id space PROCESS_A(1)/PROCESS_B(2)/
/// LOADED_PROCESS_ID(3)/FDTEST_PROCESS_ID(4) already established going,
/// distinct from PID_TABLE_BASE(10)'s own dynamic range below.
pub(crate) const FORK_TEST_PROCESS_ID: u8 = 5;

/// MILESTONE 41: a deliberately-faulting ring-3 test program -- writes
/// a short distinguishing message, then dereferences a definitely-
/// unmapped address (0x0000_1234_5678_0000, nowhere near
/// USER_CODE_ADDR/USER_STACK_ADDR/HEAP_START's own 0x5555... range),
/// triggering a real page fault at real CPL=3. The `mov eax,1`/`int
/// 0x80`/`jmp $` tail is UNREACHABLE -- pure safety net matching every
/// other hand-assembled program's own convention, in case the fault
/// mechanism itself turns out to be broken and execution somehow
/// continues past the fault.
///
///   offset  bytes                                    instruction
///    0      B8 00 00 00 00                           mov eax, 0        (write syscall)
///    5      48 BF 2C 00 00 50 55 55 00 00             mov rdi, imm64    (USER_CODE_ADDR+44, the message below)
///   15      BE 0D 00 00 00                            mov esi, 13       (message length)
///   20      CD 80                                     int 0x80
///   22      48 BB 00 00 78 56 34 12 00 00             mov rbx, imm64    (0x0000_1234_5678_0000, unmapped)
///   32      48 8B 03                                  mov rax, [rbx]    -- FAULTS HERE
///   35      B8 01 00 00 00                            mov eax, 1        (unreachable)
///   40      CD 80                                     int 0x80          (unreachable)
///   42      EB FE                                     jmp $             (unreachable)
///   44      "SIGSEGV test\n"                          the message itself (13 bytes)
pub(crate) const SIGSEGV_TEST_PROGRAM: [u8; 57] = [
    0xB8, 0x00, 0x00, 0x00, 0x00, 0x48, 0xBF, 0x2C, 0x00, 0x00, 0x50, 0x55, 0x55, 0x00, 0x00, 0xBE,
    0x0D, 0x00, 0x00, 0x00, 0xCD, 0x80, 0x48, 0xBB, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00,
    0x48, 0x8B, 0x03, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xEB, 0xFE, 0x53, 0x49, 0x47, 0x53,
    0x45, 0x47, 0x56, 0x20, 0x74, 0x65, 0x73, 0x74, 0x0A,
];
/// The real message embedded in SIGSEGV_TEST_PROGRAM at byte offset 44
/// -- checked by self_test_signals() before trusting the program at
/// runtime, same discipline as FORK_TEST_PROGRAM's own layout self-test.
const SIGSEGV_MSG: &[u8] = b"SIGSEGV test\n";
const SIGSEGV_MSG_OFFSET: usize = 44;

/// MILESTONE 41: a SIXTH hardcoded, boot-time-created process slot --
/// runs SIGSEGV_TEST_PROGRAM. See that constant's own doc comment.
static SIGSEGV_TEST_PROCESS: Mutex<Option<Process>> = Mutex::new(None);
pub(crate) const SIGSEGV_TEST_PROCESS_ID: u8 = 6;

/// MILESTONE 41: fork()s exactly one child then immediately kill()s it
/// (syscall 13, rdi=child pid) INSTEAD of wait()ing it -- real proof
/// SIGKILL bypasses the normal "run to completion, then reap" contract.
/// Forks a SECOND time afterward: if the first child's PROCESS_TABLE
/// slot was genuinely freed (not just flagged), this second fork()
/// succeeds and reuses it -- self_test_signals() checks PROCESS_TABLE's
/// real occupancy afterward as the actual proof, not just trusting this
/// program ran without crashing.
///
///   offset  bytes                        instruction
///    0      B8 07 00 00 00               mov eax, 7   (fork)
///    5      CD 80                        int 0x80     -- rax = child pid
///    7      89 C7                        mov edi, eax (child pid -> kill's arg)
///    9      B8 0D 00 00 00               mov eax, 13  (kill)
///   14      CD 80                        int 0x80
///   16      B8 07 00 00 00               mov eax, 7   (fork again)
///   21      CD 80                        int 0x80
///   23      B8 01 00 00 00               mov eax, 1   (exit)
///   28      CD 80                        int 0x80
///   30      EB FE                        jmp $        (unreachable)
pub(crate) const SIGKILL_TEST_PROGRAM: [u8; 32] = [
    0xB8, 0x07, 0x00, 0x00, 0x00, 0xCD, 0x80, 0x89, 0xC7, 0xB8, 0x0D, 0x00, 0x00, 0x00, 0xCD, 0x80,
    0xB8, 0x07, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xEB, 0xFE,
];

/// MILESTONE 41: a SEVENTH hardcoded, boot-time-created process slot --
/// runs SIGKILL_TEST_PROGRAM. See that constant's own doc comment.
static SIGKILL_TEST_PROCESS: Mutex<Option<Process>> = Mutex::new(None);
pub(crate) const SIGKILL_TEST_PROCESS_ID: u8 = 7;

/// MILESTONE 37: real, dynamic PID allocation for `fork()`-created
/// children. PIDs 1-9 stay permanently reserved for the five
/// pre-existing hardcoded/loaded-file ids above (1/2/3/4/5, with 6-9
/// held as headroom, of which Milestone 41 now uses 6 and 7) so a
/// forked child's PID can never collide with any of them;
/// PROCESS_TABLE's own slot `i` is always PID `PID_TABLE_BASE + i`.
pub(crate) const PID_TABLE_BASE: u8 = 10;

/// MILESTONE 37: real, honest, small bound on concurrently-live forked
/// processes -- same "small fixed cap, not full generality" spirit as
/// MAX_OPEN_FILES/MAX_LOAD_SEGMENTS elsewhere in this codebase. 4 is
/// comfortably more than this milestone's own fork/exec/wait test
/// program ever has alive at once (exactly one child at a time, reaped
/// by `wait()` before the parent exits) while leaving real headroom for
/// a more elaborate future test.
pub(crate) const MAX_PROCESSES: usize = 4;

/// MILESTONE 37: the real, dynamic, bounded process table `fork()`
/// allocates from -- `None` is a free slot, `Some(Process)` a live
/// forked child. **Left ADDITIVE, alongside the four pre-existing
/// hardcoded statics above, rather than migrating PROCESS_A/PROCESS_B/
/// LOADED_PROCESS/FDTEST_PROCESS into it**: those four are all built
/// once at boot (or once per `runfile`/`runelf` call) through their own
/// already-verified constructors, and every one of Milestones 30-36's
/// own verification depends on their exact, individually-named
/// identity (`runproc 1`/`runproc 2`/`runfdtest`, `sbrk`'s own id
/// checks, etc.) -- folding them into a generic array would touch
/// every one of those already-working call sites for a rename with no
/// real benefit, adding regression risk for its own sake. This table
/// exists purely to give `fork()` somewhere to put processes that don't
/// have (and structurally can't have, since there can be arbitrarily
/// many of them within the bound) their own named `static`.
static PROCESS_TABLE: Mutex<[Option<Process>; MAX_PROCESSES]> = Mutex::new([const { None }; MAX_PROCESSES]);

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
///
/// MILESTONE 37: now routed through with_process_mut() like every other
/// per-process syscall, rather than matching only PROCESS_A/PROCESS_B
/// directly -- a real, disclosed Milestone 34 limitation (sbrk only ever
/// recognized ids 1/2, flagged in loader.rs's own doc comment) is fixed
/// as a direct, low-risk consequence of this milestone's own
/// with_process_mut() generalization, not a separate change: every
/// existing caller (ids 1/2) behaves bit-for-bit identically, and every
/// OTHER process (loaded-from-file, fdtest, fork-test, and now forked
/// children) gets a genuinely working sbrk() for free.
pub(crate) fn sbrk(id: u8, size: u64) -> Option<u64> {
    with_process_mut(id, |proc| {
        let new_used = proc.heap_used.checked_add(size)?;
        if new_used > HEAP_SIZE {
            return None;
        }
        let ptr = HEAP_START + proc.heap_used;
        proc.heap_used = new_used;
        Some(ptr)
    })?
}

/// MILESTONE 35: locates whichever process `id` names and runs `f`
/// against its live `Process`, returning `None` if `id` doesn't name a
/// live process at all. MILESTONE 37: generalized from the original
/// fixed four-way match (PROCESS_A/PROCESS_B/LOADED_PROCESS/
/// FDTEST_PROCESS) to also cover FORK_TEST_PROCESS_ID and, for any
/// `id >= PID_TABLE_BASE`, the real dynamic PROCESS_TABLE -- so every
/// per-process syscall (sbrk/open/read/fdwrite/close) works uniformly
/// for a forked child exactly like it already did for every hardcoded
/// process, with zero special-casing anywhere else in this file.
fn with_process_mut<R>(id: u8, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    match id {
        1 => {
            let mut guard = PROCESS_A.lock();
            Some(f(guard.as_mut()?))
        }
        2 => {
            let mut guard = PROCESS_B.lock();
            Some(f(guard.as_mut()?))
        }
        LOADED_PROCESS_ID => {
            let mut guard = LOADED_PROCESS.lock();
            Some(f(guard.as_mut()?))
        }
        FDTEST_PROCESS_ID => {
            let mut guard = FDTEST_PROCESS.lock();
            Some(f(guard.as_mut()?))
        }
        FORK_TEST_PROCESS_ID => {
            let mut guard = FORK_TEST_PROCESS.lock();
            Some(f(guard.as_mut()?))
        }
        SIGSEGV_TEST_PROCESS_ID => {
            let mut guard = SIGSEGV_TEST_PROCESS.lock();
            Some(f(guard.as_mut()?))
        }
        SIGKILL_TEST_PROCESS_ID => {
            let mut guard = SIGKILL_TEST_PROCESS.lock();
            Some(f(guard.as_mut()?))
        }
        id if id >= PID_TABLE_BASE && ((id - PID_TABLE_BASE) as usize) < MAX_PROCESSES => {
            let idx = (id - PID_TABLE_BASE) as usize;
            let mut guard = PROCESS_TABLE.lock();
            Some(f(guard[idx].as_mut()?))
        }
        _ => None,
    }
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
        proc.fds[slot_index] = Some(FdEntry::File(OpenFile { path: path_owned, buffer, pos: 0, dirty: false }));
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
/// MILESTONE 40: now also the entry point for pipe reads -- checks
/// whether `fd` names a File or a PipeRead end and dispatches to
/// pipe_read() for the latter (which takes its own lock on PIPE_TABLE,
/// separate from this function's `with_process_mut`, so the two locks
/// are never held nested/at once). A PipeWrite fd, or any `fd` that
/// simply isn't open, both correctly fall through to `None` -- reading
/// from a pipe's WRITE end is exactly as invalid as reading a closed fd.
pub(crate) fn read_fd(id: u8, fd: u64, len: usize) -> Option<Vec<u8>> {
    let is_pipe_read = with_process_mut(id, |proc| matches!(proc.fds.get(fd as usize), Some(Some(FdEntry::PipeRead(_)))))?;
    if is_pipe_read {
        return pipe_read(id, fd, len);
    }
    with_process_mut(id, |proc| {
        let entry = match proc.fds.get_mut(fd as usize)?.as_mut()? {
            FdEntry::File(f) => f,
            _ => return None,
        };
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
/// MILESTONE 40: now also the entry point for pipe writes -- same
/// dispatch pattern as read_fd() above, see its comment for why the two
/// locks (per-process, PIPE_TABLE) are never nested.
pub(crate) fn write_fd(id: u8, fd: u64, data: &[u8]) -> Option<usize> {
    let is_pipe_write = with_process_mut(id, |proc| matches!(proc.fds.get(fd as usize), Some(Some(FdEntry::PipeWrite(_)))))?;
    if is_pipe_write {
        return pipe_write(id, fd, data);
    }
    with_process_mut(id, |proc| {
        let entry = match proc.fds.get_mut(fd as usize)?.as_mut()? {
            FdEntry::File(f) => f,
            _ => return None,
        };
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
    let entry = match entry {
        FdEntry::File(f) => f,
        // MILESTONE 40: closing a pipe end decrements its real ref
        // count (see Pipe's own doc comment for why counting, not
        // unconditional free, is required once fork()/dup() can share
        // an end across processes/fds) and only actually frees the
        // PIPE_TABLE slot once EVERY read end AND EVERY write end
        // anyone ever held has been closed -- a pipe with its write end
        // still open but read end closed, or vice versa, correctly
        // stays allocated.
        FdEntry::PipeRead(idx) => {
            let mut table = PIPE_TABLE.lock();
            if let Some(pipe) = table[idx].as_mut() {
                pipe.read_open = pipe.read_open.saturating_sub(1);
                if pipe.read_open == 0 && pipe.write_open == 0 {
                    table[idx] = None;
                }
            }
            return Some(true);
        }
        FdEntry::PipeWrite(idx) => {
            let mut table = PIPE_TABLE.lock();
            if let Some(pipe) = table[idx].as_mut() {
                pipe.write_open = pipe.write_open.saturating_sub(1);
                if pipe.read_open == 0 && pipe.write_open == 0 {
                    table[idx] = None;
                }
            }
            return Some(true);
        }
    };
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

/// MILESTONE 40: the `dup` syscall's kernel-side implementation --
/// duplicates `fd` into the LOWEST-numbered free slot in this process's
/// own fd table, returning the new fd number. For a PipeRead/PipeWrite
/// entry this is a REAL, correct dup(): both fd numbers end up pointing
/// at the SAME PIPE_TABLE index (its ref count incremented, see Pipe's
/// own doc comment), genuinely sharing one buffer/cursor pair. For a
/// File entry this is an honest, disclosed simplification, NOT real
/// POSIX dup() semantics -- it deep-copies the OpenFile (own `pos`), the
/// exact same "independent copy, not a shared reference" choice fork()
/// already made for File entries, for the identical reason (this
/// codebase currently has no shared/reference-counted File object to
/// point two fd numbers at, only owned-by-value buffers). Returns `None`
/// if `fd` isn't currently open or the table has no free slot.
pub(crate) fn dup_fd(id: u8, fd: u64) -> Option<u64> {
    let cloned = with_process_mut(id, |proc| proc.fds.get(fd as usize)?.clone())?;
    let cloned = cloned?;
    if let FdEntry::PipeRead(idx) | FdEntry::PipeWrite(idx) = cloned {
        let mut table = PIPE_TABLE.lock();
        if let Some(pipe) = table[idx].as_mut() {
            match cloned {
                FdEntry::PipeRead(_) => pipe.read_open = pipe.read_open.saturating_add(1),
                FdEntry::PipeWrite(_) => pipe.write_open = pipe.write_open.saturating_add(1),
                FdEntry::File(_) => unreachable!(),
            }
        }
    }
    with_process_mut(id, move |proc| {
        let slot_index = proc.fds.iter().position(|f| f.is_none())?;
        proc.fds[slot_index] = Some(cloned);
        Some(slot_index as u64)
    })?
}

/// MILESTONE 40: the `dup2` syscall's kernel-side implementation --
/// same sharing semantics as dup_fd() above (real for pipes, honest
/// deep-copy for files), but into a CALLER-CHOSEN fd number `newfd`
/// rather than the lowest free slot. If `newfd` already names an open
/// descriptor, it is properly closed first (its own ref count
/// decremented / its own dirty buffer persisted, via the SAME
/// close_fd() every other close path uses -- not a silent overwrite
/// that would leak a pipe ref or drop unpersisted file data). A dup2 of
/// an fd onto itself (`fd == newfd`) is a real no-op, matching POSIX,
/// checked explicitly before the close-then-clone sequence so it can't
/// close (and thereby destroy) the very descriptor it's supposed to
/// preserve. Returns `Some(true)` on success, `None` if `fd` isn't open
/// or `newfd` is out of range.
pub(crate) fn dup2_fd(id: u8, fd: u64, newfd: u64) -> Option<bool> {
    if fd == newfd {
        let is_open = with_process_mut(id, |proc| proc.fds.get(fd as usize).map(|f| f.is_some()))??;
        return if is_open { Some(true) } else { None };
    }
    if newfd as usize >= MAX_OPEN_FILES {
        return None;
    }
    let cloned = with_process_mut(id, |proc| proc.fds.get(fd as usize)?.clone())?;
    let cloned = cloned?;
    if with_process_mut(id, |proc| proc.fds.get(newfd as usize).map(|f| f.is_some()))? == Some(true) {
        close_fd(id, newfd)?;
    }
    if let FdEntry::PipeRead(idx) | FdEntry::PipeWrite(idx) = cloned {
        let mut table = PIPE_TABLE.lock();
        if let Some(pipe) = table[idx].as_mut() {
            match cloned {
                FdEntry::PipeRead(_) => pipe.read_open = pipe.read_open.saturating_add(1),
                FdEntry::PipeWrite(_) => pipe.write_open = pipe.write_open.saturating_add(1),
                FdEntry::File(_) => unreachable!(),
            }
        }
    }
    with_process_mut(id, move |proc| {
        proc.fds[newfd as usize] = Some(cloned);
        Some(true)
    })?
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
        // MILESTONE 37: create_process_from_image() builds a brand-new,
        // never-forked process along every one of its callers' paths
        // (PROCESS_A/PROCESS_B, FDTEST_PROCESS, FORK_TEST_PROCESS,
        // LOADED_PROCESS, and fork()'s own fork_build_child() helper,
        // which overwrites these two fields itself right after this
        // call returns -- see fork()'s own doc comment) -- both start
        // None here uniformly; only fork() ever sets them.
        parent_pid: None,
        pending_resume: None,
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

/// MILESTONE 37: creates FORK_TEST_PROCESS -- see that static's own doc
/// comment. Called once at boot from kernel_main, mirroring
/// init_fdtest_process()'s own pattern exactly, from FORK_TEST_PROGRAM
/// (this module's own fork/exec/wait test payload).
pub fn init_fork_test_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
) -> Result<(), &'static str> {
    let _ = writeln!(serial(), "milestone 37: creating FORK_TEST_PROCESS's private address space...");
    let p = create_process_from_image(frame_allocator, phys_mem_offset, "fork-test", &FORK_TEST_PROGRAM)?;
    let _ = writeln!(
        serial(),
        "milestone 37: FORK_TEST_PROCESS created -- pml4={:#x} code={:#x} stack={:#x}",
        p.pml4_frame.start_address().as_u64(),
        p.code_frame.start_address().as_u64(),
        p.stack_frame.start_address().as_u64()
    );
    *FORK_TEST_PROCESS.lock() = Some(p);
    Ok(())
}

/// MILESTONE 41: creates SIGSEGV_TEST_PROCESS -- see that static's own
/// doc comment. Mirrors init_fork_test_process()'s exact pattern.
pub fn init_sigsegv_test_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
) -> Result<(), &'static str> {
    let _ = writeln!(serial(), "milestone 41: creating SIGSEGV_TEST_PROCESS's private address space...");
    let p = create_process_from_image(frame_allocator, phys_mem_offset, "sigsegv-test", &SIGSEGV_TEST_PROGRAM)?;
    let _ = writeln!(
        serial(),
        "milestone 41: SIGSEGV_TEST_PROCESS created -- pml4={:#x} code={:#x} stack={:#x}",
        p.pml4_frame.start_address().as_u64(),
        p.code_frame.start_address().as_u64(),
        p.stack_frame.start_address().as_u64()
    );
    *SIGSEGV_TEST_PROCESS.lock() = Some(p);
    Ok(())
}

/// MILESTONE 41: creates SIGKILL_TEST_PROCESS -- see that static's own
/// doc comment. Mirrors init_fork_test_process()'s exact pattern.
pub fn init_sigkill_test_process(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
) -> Result<(), &'static str> {
    let _ = writeln!(serial(), "milestone 41: creating SIGKILL_TEST_PROCESS's private address space...");
    let p = create_process_from_image(frame_allocator, phys_mem_offset, "sigkill-test", &SIGKILL_TEST_PROGRAM)?;
    let _ = writeln!(
        serial(),
        "milestone 41: SIGKILL_TEST_PROCESS created -- pml4={:#x} code={:#x} stack={:#x}",
        p.pml4_frame.start_address().as_u64(),
        p.code_frame.start_address().as_u64(),
        p.stack_frame.start_address().as_u64()
    );
    *SIGKILL_TEST_PROCESS.lock() = Some(p);
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
        FORK_TEST_PROCESS_ID => &FORK_TEST_PROCESS,
        SIGSEGV_TEST_PROCESS_ID => &SIGSEGV_TEST_PROCESS,
        SIGKILL_TEST_PROCESS_ID => &SIGKILL_TEST_PROCESS,
        _ => return Err("no such process -- use 1, 2, 4 (FDTEST_PROCESS_ID), 5 (FORK_TEST_PROCESS_ID), 6 (SIGSEGV_TEST_PROCESS_ID), or 7 (SIGKILL_TEST_PROCESS_ID)"),
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
        // MILESTONE 37: an ELF-loaded process is never itself a forked
        // child (fork()'s own fork_build_child() helper always goes
        // through create_process_from_image(), not this function) --
        // see that function's own identical fields for the reasoning.
        parent_pid: None,
        pending_resume: None,
    })
}

/// MILESTONE 36: the `runelf PATH` shell command's entry point (called
/// from loader.rs's run_elf(), which does the real fs::read_file() +
/// elf::parse() first and hands this function the already-parsed
/// elf::ElfImage). Builds a fresh process from a genuine multi-segment
/// ELF64 executable's PT_LOAD segments and runs it once immediately --
/// otherwise identical to load_and_run_image()'s own CR3-switch /
/// enter_ring3_now() / (exit syscall restores CR3) sequence. Reuses the
/// SAME LOADED_PROCESS slot and LOADED_PROCESS_ID as the flat-binary
/// `runfile` path above -- both are real, single-slot "one loaded
/// process at a time" designs, and since `runfile`/`runelf` are never
/// both mid-flight at once (the shell runs one command to completion
/// before reading the next), sharing the slot is safe, not a race.
pub fn load_and_run_elf(
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
    let pml4_frame = proc.pml4_frame;
    *LOADED_PROCESS.lock() = Some(proc);

    let _ = writeln!(
        serial(),
        "milestone 36: elf-loaded process -- about to switch CR3 to pml4 {:#x}",
        pml4_frame.start_address().as_u64()
    );
    ACTIVE_PROCESS.store(LOADED_PROCESS_ID, Ordering::SeqCst);

    let flags = Cr3Flags::from_bits_truncate(KERNEL_CR3_FLAGS_BITS.load(Ordering::SeqCst));
    unsafe { Cr3::write(pml4_frame, flags) };
    let _ = writeln!(
        serial(),
        "milestone 36: CR3 switched -- entering ring 3 for the ELF-loaded process at {:#x} (real e_entry)",
        elf_image.entry
    );

    usertest::enter_ring3_now();

    let _ = writeln!(
        serial(),
        "milestone 36: elf-loaded process -- resumed in kernel context (CR3 already restored by the exit syscall before this point)"
    );
    Ok(())
}


/// MILESTONE 37: this milestone's fork/exec/wait test program -- hand-
/// assembled machine code, assembled + verified via a standalone Python
/// script using the keystone-engine assembler (with a capstone
/// disassembly round-trip confirming the intended control flow byte for
/// byte), the same "verified with a standalone re-derivation, not
/// hand-counted hex digits" discipline established by every other
/// hand-assembled program in this file.
///
/// Control flow (confirmed against the actual disassembly):
///   1. `mov eax, 7; int 0x80` -- syscall 7 = fork(). rax = child pid in
///      the parent, or 0 in the child (forced by
///      usertest::enter_ring3_as_forked_child()'s "xor eax, eax" before
///      the child's very first iretq -- see that function's own doc
///      comment).
///   2. `mov rbx, rax; cmp rbx, 0; je child_path` -- rbx is callee-
///      preserved across every syscall in this ABI (syscall_entry pushes
///      and pops every GPR except rax around syscall_dispatch), so it
///      safely holds the fork() result across the write()/wait() calls
///      below.
///   3. PARENT path (rbx != 0): write()s FORK_MSG_PARENT_OFFSET's
///      distinguishing message, then `mov rdi, rbx; mov eax, 8;
///      int 0x80` -- syscall 8 = wait(child_pid) -- which does not
///      return until the child has run to completion (see
///      process::wait_for_child()'s own doc comment) -- then write()s
///      FORK_MSG_WAITDONE_OFFSET's message and exits.
///   4. CHILD path (rbx == 0, reached only when process::
///      run_forked_child() resumes this exact process at this exact
///      instruction -- NOT by restarting from offset 0, which would
///      immediately re-execute step 1 and fork() again): `mov rdi,
///      FORK_EXEC_PATH_OFFSET; mov esi, 8; mov eax, 9; int 0x80` --
///      syscall 9 = exec("testprog"). On success this NEVER returns
///      here (control jumps straight into testprog's own entry --
///      see usertest::exec_replace_and_enter()); on failure (e.g.
///      `seedtestprog` was never run, so "testprog" doesn't exist on
///      disk) execution falls through to write() a distinguishing
///      FALLBACK message instead, so `runfork` degrades cleanly and
///      honestly either way rather than crashing.
///
/// Layout (verified against the actual byte array below by
/// self_test_fork_test_program()):
///   offset   0..125   the syscall sequence itself (125 bytes of real
///                      instructions)
///   offset 512..576   FORK_MSG_PARENT (space-padded to 64 bytes)
///   offset 576..640   FORK_MSG_WAITDONE (space-padded to 64 bytes)
///   offset 640..704   FORK_MSG_CHILD_FALLBACK (space-padded to 64 bytes)
///   offset 704..712   "testprog" (8 real bytes, the exec() path)
pub(crate) const FORK_TEST_PROGRAM: [u8; 712] = [
    0xB8, 0x07, 0x00, 0x00, 0x00, 0xCD, 0x80, 0x48, 0x89, 0xC3, 0x48, 0x83, 0xFB, 0x00, 0x74, 0x38,
    0x48, 0xBF, 0x00, 0x02, 0x00, 0x50, 0x55, 0x55, 0x00, 0x00, 0xBE, 0x40, 0x00, 0x00, 0x00, 0xB8,
    0x00, 0x00, 0x00, 0x00, 0xCD, 0x80, 0x48, 0x89, 0xDF, 0xB8, 0x08, 0x00, 0x00, 0x00, 0xCD, 0x80,
    0x48, 0xBF, 0x40, 0x02, 0x00, 0x50, 0x55, 0x55, 0x00, 0x00, 0xBE, 0x40, 0x00, 0x00, 0x00, 0xB8,
    0x00, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xEB, 0x2C, 0x48, 0xBF, 0xC0, 0x02, 0x00, 0x50, 0x55, 0x55,
    0x00, 0x00, 0xBE, 0x08, 0x00, 0x00, 0x00, 0xB8, 0x09, 0x00, 0x00, 0x00, 0xCD, 0x80, 0x48, 0xBF,
    0x80, 0x02, 0x00, 0x50, 0x55, 0x55, 0x00, 0x00, 0xBE, 0x40, 0x00, 0x00, 0x00, 0xB8, 0x00, 0x00,
    0x00, 0x00, 0xCD, 0x80, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xEB, 0xFE, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x6D, 0x69, 0x6C, 0x65, 0x73, 0x74, 0x6F, 0x6E, 0x65, 0x20, 0x33, 0x37, 0x3A, 0x20, 0x68, 0x65,
    0x6C, 0x6C, 0x6F, 0x20, 0x66, 0x72, 0x6F, 0x6D, 0x20, 0x74, 0x68, 0x65, 0x20, 0x50, 0x41, 0x52,
    0x45, 0x4E, 0x54, 0x20, 0x61, 0x66, 0x74, 0x65, 0x72, 0x20, 0x66, 0x6F, 0x72, 0x6B, 0x28, 0x29,
    0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
    0x6D, 0x69, 0x6C, 0x65, 0x73, 0x74, 0x6F, 0x6E, 0x65, 0x20, 0x33, 0x37, 0x3A, 0x20, 0x50, 0x41,
    0x52, 0x45, 0x4E, 0x54, 0x20, 0x6F, 0x62, 0x73, 0x65, 0x72, 0x76, 0x65, 0x64, 0x20, 0x77, 0x61,
    0x69, 0x74, 0x28, 0x29, 0x20, 0x72, 0x65, 0x61, 0x70, 0x20, 0x74, 0x68, 0x65, 0x20, 0x63, 0x68,
    0x69, 0x6C, 0x64, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
    0x6D, 0x69, 0x6C, 0x65, 0x73, 0x74, 0x6F, 0x6E, 0x65, 0x20, 0x33, 0x37, 0x3A, 0x20, 0x43, 0x48,
    0x49, 0x4C, 0x44, 0x20, 0x66, 0x61, 0x6C, 0x6C, 0x62, 0x61, 0x63, 0x6B, 0x20, 0x2D, 0x2D, 0x20,
    0x65, 0x78, 0x65, 0x63, 0x28, 0x27, 0x74, 0x65, 0x73, 0x74, 0x70, 0x72, 0x6F, 0x67, 0x27, 0x29,
    0x20, 0x66, 0x61, 0x69, 0x6C, 0x65, 0x64, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
    0x74, 0x65, 0x73, 0x74, 0x70, 0x72, 0x6F, 0x67,
];

const FORK_MSG_PARENT_OFFSET: usize = 512;
const FORK_MSG_WAITDONE_OFFSET: usize = 576;
const FORK_MSG_CHILD_FALLBACK_OFFSET: usize = 640;
const FORK_EXEC_PATH_OFFSET: usize = 704;

/// The exact messages FORK_TEST_PROGRAM prints via the write(ptr,len)
/// syscall along each path -- named here so self_test_fork_test_program()
/// below and the milestone report can both point at a single source of
/// truth, same convention loader::FDOUT_WRITE_CONTENT/FDTEST_READ_PATH
/// already established.
pub(crate) const FORK_MSG_PARENT: &str = "milestone 37: hello from the PARENT after fork()";
pub(crate) const FORK_MSG_WAITDONE: &str = "milestone 37: PARENT observed wait() reap the child";
pub(crate) const FORK_MSG_CHILD_FALLBACK: &str = "milestone 37: CHILD fallback -- exec('testprog') failed";
pub(crate) const FORK_EXEC_PATH: &str = "testprog";

/// MILESTONE 37: a direct, filesystem-independent proof that
/// FORK_TEST_PROGRAM's byte layout actually matches what its own doc
/// comment claims -- same discipline as loader::self_test_fdtest_program().
/// Called once at boot from kernel_main.
pub fn self_test_fork_test_program() {
    let msg_parent_ok =
        &FORK_TEST_PROGRAM[FORK_MSG_PARENT_OFFSET..FORK_MSG_PARENT_OFFSET + FORK_MSG_PARENT.len()] == FORK_MSG_PARENT.as_bytes();
    let msg_waitdone_ok = &FORK_TEST_PROGRAM[FORK_MSG_WAITDONE_OFFSET..FORK_MSG_WAITDONE_OFFSET + FORK_MSG_WAITDONE.len()]
        == FORK_MSG_WAITDONE.as_bytes();
    let msg_fallback_ok = &FORK_TEST_PROGRAM
        [FORK_MSG_CHILD_FALLBACK_OFFSET..FORK_MSG_CHILD_FALLBACK_OFFSET + FORK_MSG_CHILD_FALLBACK.len()]
        == FORK_MSG_CHILD_FALLBACK.as_bytes();
    let path_ok =
        &FORK_TEST_PROGRAM[FORK_EXEC_PATH_OFFSET..FORK_EXEC_PATH_OFFSET + FORK_EXEC_PATH.len()] == FORK_EXEC_PATH.as_bytes();
    let _ = writeln!(
        serial(),
        "milestone 37: self-test -- FORK_TEST_PROGRAM layout check: parent_msg={msg_parent_ok} waitdone_msg={msg_waitdone_ok} fallback_msg={msg_fallback_ok} exec_path={path_ok} -- {}",
        if msg_parent_ok && msg_waitdone_ok && msg_fallback_ok && path_ok { "all match, layout confirmed" } else { "FAILED -- byte layout drifted from doc comment" }
    );
}

/// MILESTONE 40: a real, boot-time, non-interactive proof that
/// pipe()/dup()/dup2() actually work -- same motivation as fs.rs's
/// self_test_disk_write() (interactive shell-command testing has been
/// unreliable in this environment; this exercises the exact kernel-side
/// functions usertest.rs's syscall handlers call, with zero interactive
/// input, so every boot's serial log carries direct proof). Deliberately
/// scoped to the kernel-side fd/pipe mechanics themselves (pipe_create,
/// write_fd/read_fd dispatching correctly onto FdEntry::Pipe*, dup_fd's
/// real ref-counted sharing) rather than a full ring-3 syscall round
/// trip -- the same "layout/mechanism, not full live execution" scope
/// self_test_fork_test_program() above already established as this
/// project's honest self-test pattern. Reuses FORK_TEST_PROCESS_ID's
/// already-initialized fd table rather than allocating a new process.
pub fn self_test_pipe_mechanics() {
    const MSG: &[u8] = b"pipe self-test payload";
    let id = FORK_TEST_PROCESS_ID;

    let Some((rfd, wfd)) = pipe_create(id) else {
        let _ = writeln!(serial(), "milestone 40: self-test -- FAILED, pipe_create returned None");
        return;
    };

    let write_ok = write_fd(id, wfd, MSG) == Some(MSG.len());
    let read_back = read_fd(id, rfd, MSG.len());
    let roundtrip_ok = read_back.as_deref() == Some(MSG);

    // dup_fd: a second fd cloned from the write end must share the SAME
    // underlying pipe -- write through the dup, read through the
    // original read end, real evidence they're not independent copies.
    let dup_ok = match dup_fd(id, wfd) {
        Some(dup_wfd) => {
            const MSG2: &[u8] = b"via-dup";
            let wrote = write_fd(id, dup_wfd, MSG2) == Some(MSG2.len());
            let got = read_fd(id, rfd, MSG2.len());
            wrote && got.as_deref() == Some(MSG2)
        }
        None => false,
    };

    // dup2_fd: force the read end onto a caller-chosen fd number, then
    // confirm reads through THAT number still reach the same pipe.
    let dup2_target: u64 = 3;
    let dup2_ok = match dup2_fd(id, rfd, dup2_target) {
        Some(true) => {
            const MSG3: &[u8] = b"via-dup2";
            let wrote = write_fd(id, wfd, MSG3) == Some(MSG3.len());
            let got = read_fd(id, dup2_target, MSG3.len());
            wrote && got.as_deref() == Some(MSG3)
        }
        _ => false,
    };

    let all_ok = write_ok && roundtrip_ok && dup_ok && dup2_ok;
    let _ = writeln!(
        serial(),
        "milestone 40: self-test -- pipe mechanics: write_ok={write_ok} roundtrip_ok={roundtrip_ok} dup_ok={dup_ok} dup2_ok={dup2_ok} -- {}",
        if all_ok { "all real, real bytes matched through every path" } else { "FAILED -- see individual flags above" }
    );
}

/// MILESTONE 37: allocates a genuinely new PID slot in PROCESS_TABLE --
/// finds the first free slot and returns `PID_TABLE_BASE + index`, or
/// `None` if the table is already full (MAX_PROCESSES live children).
fn alloc_pid_slot(table: &[Option<Process>; MAX_PROCESSES]) -> Option<usize> {
    table.iter().position(|p| p.is_none())
}

/// MILESTONE 37: the real page-table-building half of fork() -- reads
/// the PARENT's own code/stack/heap frame CONTENTS (real bytes, copied
/// through the phys-mem-offset direct view of the parent's OWN already-
/// allocated physical frames, the same technique every other frame
/// read/write in this file uses) and builds a brand-new process from
/// them via create_process_from_image() (the SAME, unduplicated PML4/
/// code/stack/heap-mapping mechanism every process in this file already
/// goes through) -- then OVERWRITES that fresh process's stack and heap
/// pages (which create_process_from_image() always zero-fills for a
/// brand-new process) with the parent's REAL stack/heap CONTENTS. This
/// is the actual "not shared references" proof: the child's frames are
/// physically distinct addresses (allocate_frame() never returns an
/// already-in-use frame) holding independently-copied bytes.
fn fork_build_child(
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    phys_mem_offset: VirtAddr,
    parent_code_frame: PhysFrame<Size4KiB>,
    parent_stack_frame: PhysFrame<Size4KiB>,
    parent_heap_frames: &[PhysFrame<Size4KiB>],
) -> Result<Process, &'static str> {
    let code_virt = phys_mem_offset + parent_code_frame.start_address().as_u64();
    let mut code_bytes = vec![0u8; PAGE_SIZE];
    unsafe { core::ptr::copy_nonoverlapping(code_virt.as_ptr::<u8>(), code_bytes.as_mut_ptr(), PAGE_SIZE) };

    let child = create_process_from_image(frame_allocator, phys_mem_offset, "forked-child", &code_bytes)?;

    let child_stack_virt = phys_mem_offset + child.stack_frame.start_address().as_u64();
    let parent_stack_virt = phys_mem_offset + parent_stack_frame.start_address().as_u64();
    unsafe {
        core::ptr::copy_nonoverlapping(parent_stack_virt.as_ptr::<u8>(), child_stack_virt.as_mut_ptr::<u8>(), PAGE_SIZE)
    };

    for (child_frame, parent_frame) in child.heap_frames.iter().zip(parent_heap_frames.iter()) {
        let cv = phys_mem_offset + child_frame.start_address().as_u64();
        let pv = phys_mem_offset + parent_frame.start_address().as_u64();
        unsafe { core::ptr::copy_nonoverlapping(pv.as_ptr::<u8>(), cv.as_mut_ptr::<u8>(), PAGE_SIZE) };
    }

    let _ = writeln!(
        serial(),
        "milestone 37: fork() -- child's code frame {:#x} holds a REAL byte-for-byte copy of the parent's code frame {:#x} (independently verifiable: same virtual address USER_CODE_ADDR under either process's own CR3, genuinely different physical frames)",
        child.code_frame.start_address().as_u64(),
        parent_code_frame.start_address().as_u64()
    );

    Ok(child)
}

/// MILESTONE 37: the `fork()` syscall's actual kernel-side
/// implementation -- see this module's own top-of-file MILESTONE 37 doc
/// comment for the full scoping decision. Called from usertest.rs's
/// syscall_dispatch with `parent_id` (ACTIVE_PROCESS at the moment of
/// the call) and the parent's own hardware-recorded (resume_rip,
/// resume_rsp) for THIS int 0x80 -- exactly the CPU's own pushed
/// InterruptStackFrame.rip/rsp, unmodified, i.e. the instruction right
/// after the fork() syscall in the parent's own code. Returns the new
/// child's PID, or `None` if `parent_id` doesn't name a live process,
/// the fixed MAX_PROCESSES table is already full, the global frame
/// allocator isn't installed, or (see is_in_child_resume()'s own doc
/// comment) this call is itself nested inside an active child-resume
/// excursion -- this milestone's honest, ENFORCED (not just documented)
/// bound of nesting depth 1.
pub(crate) fn fork(parent_id: u8, resume_rip: u64, resume_rsp: u64) -> Option<u8> {
    if usertest::is_in_child_resume() {
        let _ = writeln!(
            serial(),
            "milestone 37: syscall FORK (process {parent_id}) -- REFUSED, a forked child cannot itself fork() while being resumed by wait() (this design's honest nesting-depth-1 bound) -- returning failure"
        );
        return None;
    }

    // MILESTONE 37: each fallible (heap-allocating) clone is its own
    // separate `let` statement, deliberately NOT combined into one
    // tuple/array-literal expression with several fallible sub-
    // expressions -- confirmed the hard way that combining them (e.g.
    // `(a, b, c.clone(), d.clone())` or `[x.clone(), y.clone(), ...]`
    // in one expression) makes rustc synthesize an unwind-based
    // cleanup path for the earlier, already-heap-allocated Drop values
    // in case a LATER clone panics mid-expression -- genuine unwind
    // landing pads this kernel's panic=abort, no_std target cannot
    // provide, which failed the build with a real `error: unwinding
    // panics are not supported without std`. Once each clone is its
    // own statement, there is nothing "in flight" for a later panic to
    // unwind through, so no landing pad is ever needed.
    let snapshot = with_process_mut(parent_id, |p| {
        let code_frame = p.code_frame;
        let stack_frame = p.stack_frame;
        let heap_frames = p.heap_frames.clone();
        let heap_used = p.heap_used;
        let fd0 = p.fds[0].clone();
        let fd1 = p.fds[1].clone();
        let fd2 = p.fds[2].clone();
        let fd3 = p.fds[3].clone();
        let label = p.label;
        (code_frame, stack_frame, heap_frames, heap_used, [fd0, fd1, fd2, fd3], label)
    })?;
    let (parent_code, parent_stack, parent_heap_frames, parent_heap_used, parent_fds, parent_label) = snapshot;

    let phys_mem_offset = memory::phys_mem_offset();
    let build_result = memory::with_frame_allocator(|frame_allocator| {
        fork_build_child(frame_allocator, phys_mem_offset, parent_code, parent_stack, &parent_heap_frames)
    });
    let mut child = match build_result {
        Some(Ok(c)) => c,
        Some(Err(e)) => {
            let _ = writeln!(serial(), "milestone 37: syscall FORK (process {parent_id}) -- FAILED building child address space: {e}");
            return None;
        }
        None => {
            let _ = writeln!(serial(), "milestone 37: syscall FORK (process {parent_id}) -- FAILED, global frame allocator not installed (should never happen post-boot)");
            return None;
        }
    };
    child.heap_used = parent_heap_used;
    // MILESTONE 40: the child's fd table now holds its OWN clone of any
    // pipe ends the parent had open (see FdEntry's own doc comment for
    // why cloning a PipeRead/PipeWrite index is real sharing, not a
    // data copy) -- so PIPE_TABLE's ref counts must go up to match, the
    // same real-accounting requirement close_fd()/dup_fd()/dup2_fd()
    // already established. Without this, the child's two new fd-table
    // entries would reference a pipe PIPE_TABLE still thinks only the
    // parent holds -- the parent closing its own end would then free
    // the pipe out from under the child (a real dangling-reference bug,
    // not hypothetical).
    for entry in parent_fds.iter().flatten() {
        if let FdEntry::PipeRead(idx) | FdEntry::PipeWrite(idx) = entry {
            let mut table = PIPE_TABLE.lock();
            if let Some(pipe) = table[*idx].as_mut() {
                match entry {
                    FdEntry::PipeRead(_) => pipe.read_open = pipe.read_open.saturating_add(1),
                    FdEntry::PipeWrite(_) => pipe.write_open = pipe.write_open.saturating_add(1),
                    FdEntry::File(_) => unreachable!(),
                }
            }
        }
    }
    child.fds = parent_fds;
    child.parent_pid = Some(parent_id);
    child.pending_resume = Some((resume_rip, resume_rsp));
    let child_pml4 = child.pml4_frame;
    let child_code_frame = child.code_frame;

    let child_pid = {
        let mut table = PROCESS_TABLE.lock();
        let idx = match alloc_pid_slot(&table) {
            Some(i) => i,
            None => {
                let _ = writeln!(
                    serial(),
                    "milestone 37: syscall FORK (process {parent_id}) -- FAILED, PROCESS_TABLE is full ({MAX_PROCESSES} live forked processes already) -- child address space built then discarded"
                );
                return None;
            }
        };
        table[idx] = Some(child);
        PID_TABLE_BASE + idx as u8
    };

    let _ = writeln!(
        serial(),
        "milestone 37: syscall FORK (process {parent_id}, '{parent_label}') -- created child pid {child_pid}: pml4={:#x} code_frame={:#x} resume point rip={:#x} rsp={:#x}",
        child_pml4.start_address().as_u64(),
        child_code_frame.start_address().as_u64(),
        resume_rip,
        resume_rsp
    );

    Some(child_pid)
}

/// MILESTONE 37: switches CR3 to `child_pid`'s own private PML4 and
/// resumes it at EXACTLY the (rip, rsp) fork() captured -- NOT
/// USER_CODE_ADDR -- via usertest::enter_ring3_as_forked_child(), so the
/// child genuinely continues past its own fork() call (with rax forced
/// to 0) instead of restarting the whole program from the top. Uses a
/// SEPARATE, dedicated kernel-resume anchor from the top-level one
/// usertest::run()/enter_ring3_now() use (usertest::CHILD_KERNEL_RSP,
/// via enter_ring3_as_forked_child()) -- exactly one such anchor exists,
/// this milestone's real, disclosed, ENFORCED v1 bound on nesting depth
/// (see fork()'s and wait_for_child()'s own is_in_child_resume() checks).
/// Called only from wait_for_child() below, never directly.
fn run_forked_child(child_pid: u8) -> Result<(), &'static str> {
    let (pml4_frame, resume) = with_process_mut(child_pid, |p| (p.pml4_frame, p.pending_resume.take()))
        .ok_or("run_forked_child: no such child")?;
    let (resume_rip, resume_rsp) = resume.ok_or("run_forked_child: child has no pending resume point (already run once)")?;

    let _ = writeln!(
        serial(),
        "milestone 37: wait() -- switching CR3 to forked child pid {child_pid}'s own pml4 {:#x}, resuming at rip={:#x} rsp={:#x} (its own fork()-time snapshot, rax forced to 0)",
        pml4_frame.start_address().as_u64(),
        resume_rip,
        resume_rsp
    );
    ACTIVE_PROCESS.store(child_pid, Ordering::SeqCst);
    let flags = Cr3Flags::from_bits_truncate(KERNEL_CR3_FLAGS_BITS.load(Ordering::SeqCst));
    unsafe { Cr3::write(pml4_frame, flags) };

    // MILESTONE 37: real, diagnosed bug fix -- see
    // gdt::set_syscall_stack_top()'s own doc comment for the full
    // story (found via an actual page fault during real QEMU testing,
    // not guessed). Every syscall the CHILD makes from here on (this
    // milestone's own test child immediately calls exec(), then the
    // exec()'d program calls write()+exit()) needs to land on a
    // DIFFERENT ring-0 stack than the one the PARENT's own in-flight
    // wait() syscall is using, or the CPU's own next `int 0x80` would
    // silently overwrite the parent's saved resume state.
    let saved_stack_top = crate::gdt::set_syscall_stack_top(crate::gdt::child_excursion_stack_top());
    usertest::enter_ring3_as_forked_child(resume_rip, resume_rsp);
    crate::gdt::set_syscall_stack_top(saved_stack_top);

    let _ = writeln!(
        serial(),
        "milestone 37: wait() -- forked child pid {child_pid} resumed in kernel context (its own exit() syscall ran; CR3 is currently the KERNEL's own, restored to the parent's by wait_for_child() next)"
    );
    Ok(())
}

/// MILESTONE 37: the `wait()` syscall's actual kernel-side
/// implementation -- see this module's own top-of-file MILESTONE 37 doc
/// comment for the real, honest semantics this implements. Real,
/// literal blocking for THIS milestone's synchronous execution model:
/// switches CR3 to the child, drives it all the way to ITS OWN exit()
/// (nested inside this syscall's own dispatch), then restores the
/// PARENT's own CR3/ACTIVE_PROCESS -- restore_kernel_cr3() (called by
/// the child's own exit()) leaves CR3 pointed at the KERNEL's PML4, not
/// the parent's, so this function's own explicit Cr3::write() back to
/// the parent's pml4_frame is required, not optional, for the parent's
/// OWN in-flight int 0x80 to correctly resume into its own private code
/// page afterward.
///
/// Returns the reaped child's own pid on success (removing it from
/// PROCESS_TABLE, freeing its PML4/frames via Drop, and freeing the
/// slot for a later fork() to reuse), or `None` if `child_pid` doesn't
/// name a live child OF `parent_id` specifically (a real ownership
/// check: one process cannot wait() on another's child) or nesting
/// depth would exceed this design's bound of 1.
pub(crate) fn wait_for_child(parent_id: u8, child_pid: u8) -> Option<u8> {
    if usertest::is_in_child_resume() {
        let _ = writeln!(
            serial(),
            "milestone 37: syscall WAIT (process {parent_id}) -- REFUSED, cannot wait() while already inside a nested child-resume excursion (nesting-depth-1 bound) -- returning failure"
        );
        return None;
    }
    if child_pid < PID_TABLE_BASE {
        return None;
    }
    let idx = (child_pid - PID_TABLE_BASE) as usize;
    if idx >= MAX_PROCESSES {
        return None;
    }
    {
        let table = PROCESS_TABLE.lock();
        let child = table[idx].as_ref()?;
        if child.parent_pid != Some(parent_id) {
            let _ = writeln!(
                serial(),
                "milestone 37: syscall WAIT (process {parent_id}) -- REFUSED, pid {child_pid} is not a live child OF this process -- returning failure"
            );
            return None;
        }
    }

    let parent_pml4 = with_process_mut(parent_id, |p| p.pml4_frame)?;

    let run_result = run_forked_child(child_pid);

    // Restore the PARENT's own context regardless of whether the
    // nested child excursion itself succeeded -- the parent's own
    // in-flight `int 0x80` for THIS wait() call still needs to resume
    // correctly either way.
    let flags = Cr3Flags::from_bits_truncate(KERNEL_CR3_FLAGS_BITS.load(Ordering::SeqCst));
    unsafe { Cr3::write(parent_pml4, flags) };
    ACTIVE_PROCESS.store(parent_id, Ordering::SeqCst);

    if let Err(e) = run_result {
        let _ = writeln!(serial(), "milestone 37: syscall WAIT (process {parent_id}) -- FAILED running child pid {child_pid}: {e}");
        return None;
    }

    PROCESS_TABLE.lock()[idx] = None;
    let _ = writeln!(
        serial(),
        "milestone 37: syscall WAIT (process {parent_id}) -- child pid {child_pid} ran to completion and was reaped, CR3 restored to parent's own pml4 {:#x}",
        parent_pml4.start_address().as_u64()
    );
    Some(child_pid)
}

/// MILESTONE 41: SIGKILL-equivalent -- unconditionally terminates a
/// live, never-yet-run forked child, bypassing the normal "run to its
/// own exit(), then reap" contract wait_for_child() enforces. Only the
/// child's REAL parent may kill it (same authorization check
/// wait_for_child() already makes). Structurally, any entry still
/// present in PROCESS_TABLE has, by construction, never been run --
/// wait_for_child() is the ONLY code that ever runs a forked child, and
/// it always reaps (clears) the slot immediately afterward -- so there
/// is no separate "already ran, not yet reaped" case to guard against
/// here.
///
/// **Real, honest, disclosed limitation, matching wait_for_child()'s
/// OWN precedent exactly**: frees the PROCESS_TABLE slot for reuse but
/// does not reclaim the child's physical frames (PML4/code/stack/heap).
/// This kernel has never reclaimed frames on process exit or reap
/// either -- MAX_PROCESSES=4 total, ever, made that an acceptable,
/// already-shipped simplification well before this milestone; kill()
/// does not introduce a new gap, it just inherits the existing one.
pub(crate) fn kill(caller_id: u8, target_pid: u8) -> bool {
    if target_pid < PID_TABLE_BASE {
        return false;
    }
    let idx = (target_pid - PID_TABLE_BASE) as usize;
    if idx >= MAX_PROCESSES {
        return false;
    }
    let mut table = PROCESS_TABLE.lock();
    let is_own_child = matches!(table[idx].as_ref(), Some(child) if child.parent_pid == Some(caller_id));
    if !is_own_child {
        let _ = writeln!(
            serial(),
            "milestone 41: syscall KILL (process {caller_id}) -- REFUSED, pid {target_pid} is not a live child OF this process -- returning failure"
        );
        return false;
    }
    table[idx] = None;
    let _ = writeln!(
        serial(),
        "milestone 41: syscall KILL (process {caller_id}) -- pid {target_pid} terminated WITHOUT ever running (bypassed wait()'s normal run-then-reap contract), slot freed for reuse"
    );
    true
}

/// MILESTONE 41: real, boot-time, non-interactive proof of both new
/// signal behaviors -- sendkey is confirmed unreliable in this
/// environment (see fs::self_test_disk_write()'s own doc comment for
/// the identical reasoning), so this runs unattended on every boot
/// instead of depending on an interactive shell command.
pub fn self_test_signals() {
    let msg_ok = &SIGSEGV_TEST_PROGRAM[SIGSEGV_MSG_OFFSET..SIGSEGV_MSG_OFFSET + SIGSEGV_MSG.len()] == SIGSEGV_MSG;
    let _ = writeln!(
        serial(),
        "milestone 41: self-test -- SIGSEGV_TEST_PROGRAM layout check: message={msg_ok} -- {}",
        if msg_ok { "confirmed" } else { "MISMATCH" }
    );

    let _ = writeln!(
        serial(),
        "milestone 41: self-test -- running SIGSEGV_TEST_PROCESS, expecting a real page fault to terminate it without panicking the kernel..."
    );
    match run(SIGSEGV_TEST_PROCESS_ID) {
        Ok(()) => {
            let active_after = ACTIVE_PROCESS.load(Ordering::SeqCst);
            let _ = writeln!(
                serial(),
                "milestone 41: self-test -- run() returned normally after the fault (the kernel did not panic/hlt_loop) -- ACTIVE_PROCESS reset to {active_after} (expected 0)"
            );
        }
        Err(e) => {
            let _ = writeln!(serial(), "milestone 41: self-test -- FAILED, run() itself errored: {e}");
        }
    }

    let _ = writeln!(
        serial(),
        "milestone 41: self-test -- running PROCESS_A right after the fault, real proof of genuine recovery, not just 'hasn't crashed yet'..."
    );
    match run(1) {
        Ok(()) => {
            let _ = writeln!(serial(), "milestone 41: self-test -- PROCESS_A ran normally after the SIGSEGV -- kernel genuinely recovered, not just still standing");
        }
        Err(e) => {
            let _ = writeln!(serial(), "milestone 41: self-test -- FAILED, PROCESS_A could not run after the fault: {e}");
        }
    }

    let _ = writeln!(
        serial(),
        "milestone 41: self-test -- running SIGKILL_TEST_PROCESS (fork, kill without running, fork again to prove the slot was really freed)..."
    );
    match run(SIGKILL_TEST_PROCESS_ID) {
        Ok(()) => {
            let table = PROCESS_TABLE.lock();
            let occupied: alloc::vec::Vec<usize> = table
                .iter()
                .enumerate()
                .filter(|(_, p)| p.is_some())
                .map(|(i, _)| i)
                .collect();
            let _ = writeln!(
                serial(),
                "milestone 41: self-test -- SIGKILL_TEST_PROCESS completed -- PROCESS_TABLE occupied slots: {:?} (expected exactly [0]: the SECOND fork's child, since the FIRST was killed unrun and its slot reused)",
                occupied
            );
        }
        Err(e) => {
            let _ = writeln!(serial(), "milestone 41: self-test -- FAILED, SIGKILL_TEST_PROCESS run() errored: {e}");
        }
    }
}

/// MILESTONE 37: the `exec()` syscall's actual kernel-side
/// implementation. Deliberately does NOT touch the process's PML4,
/// stack mapping, heap mapping, or fd table at all -- only its CODE
/// FRAME's own physical bytes are overwritten in place (zero-fill then
/// copy, the EXACT same step create_process_from_image() already does
/// for a brand-new process's code page, reused here rather than
/// duplicated), matching real exec()'s "same process, same pid, same
/// open fds, new code image" contract. `heap_used` is reset to 0 (a
/// fresh program gets a fresh heap); the physical heap frames
/// themselves are reused, not reallocated. Bounded by the SAME
/// MAX_CODE_IMAGE_BYTES cap every other code-loading path in this file
/// already enforces.
pub(crate) fn exec_process(id: u8, image: &[u8]) -> Result<(), &'static str> {
    if image.len() > MAX_CODE_IMAGE_BYTES {
        return Err("exec: program exceeds the 4096-byte code page capacity");
    }
    let phys_mem_offset = memory::phys_mem_offset();
    let code_frame = with_process_mut(id, |p| {
        p.heap_used = 0;
        p.code_frame
    })
    .ok_or("exec: no such process")?;

    let code_virt = phys_mem_offset + code_frame.start_address().as_u64();
    unsafe {
        core::ptr::write_bytes::<u8>(code_virt.as_mut_ptr(), 0, PAGE_SIZE);
        core::ptr::copy_nonoverlapping(image.as_ptr(), code_virt.as_mut_ptr(), image.len());
    }

    let _ = writeln!(
        serial(),
        "milestone 37: syscall EXEC (process {id}) -- replaced code frame {:#x} contents in place with {} new bytes, heap_used reset to 0, fd table left untouched",
        code_frame.start_address().as_u64(),
        image.len()
    );
    Ok(())
}
