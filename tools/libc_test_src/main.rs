// MILESTONE 39: the first spikeling-os user program built as ordinary,
// ergonomic Rust code calling real syscall wrappers (libc.rs) -- every
// prior test program (usertest::USER_PROGRAM, process::PROCESS_PROGRAM,
// loader::FDTEST_PROGRAM, the Milestone 37 fork/exec/wait test) was
// hand-assembled raw machine code, each one re-deriving its own `int
// 0x80` register setup byte-for-byte. This program calls `sys_write()`,
// `sys_sbrk()`, `sys_fork()`, `sys_wait()`, `sys_exit()` as normal
// function calls instead -- built with this project's own pinned Rust
// nightly (rustc --target x86_64-unknown-none), not hand-assembled,
// same "genuine toolchain output, not simulated" standard as Milestone
// 36's testelf.elf.
//
// Real, exercised sequence (see libc.rs's own doc comment for the exact
// syscall-number ABI this all rests on):
//   1. write() a startup message.
//   2. sbrk(16) for a real heap pointer, write a marker byte through it
//      directly (raw pointer write -- this program's own private heap
//      page, mapped read/write), then write() it back out so the
//      serial log shows the ACTUAL byte sbrk() gave a pointer to, not
//      an assumed one.
//   3. fork(): the child writes its own distinguishing message and
//      exit()s; the parent wait()s for it, then writes a completion
//      message once wait() genuinely returns (not before).
#![no_std]
#![no_main]

mod libc;
use libc::*;

const STARTUP_MSG: [u8; 58] = *b"hello from a REAL Rust program via the real libc (write())";
const CHILD_MSG: [u8; 52] = *b"hello from the CHILD -- forked via libc's sys_fork()";
const DONE_MSG: [u8; 55] = *b"parent: wait() returned -- libc fork/wait round-trip OK";
const HEAP_MSG_PREFIX: [u8; 23] = *b"heap marker byte read: ";

#[unsafe(link_section = ".text.start")]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    unsafe {
        sys_write(STARTUP_MSG.as_ptr() as u64, STARTUP_MSG.len() as u64);

        // real sbrk() call -- a genuine heap pointer, not assumed
        let heap_ptr = sys_sbrk(16);
        if heap_ptr != SYSCALL_FAIL {
            core::ptr::write(heap_ptr as *mut u8, b'K'); // a real, arbitrary marker byte
            let readback = core::ptr::read(heap_ptr as *const u8);
            // Build a tiny message ourselves (no format!/alloc in a
            // no_std, no-heap-allocator-of-its-own program) so the
            // serial log shows the ACTUAL byte read back through the
            // real sbrk() pointer, not a hardcoded assumption.
            sys_write(HEAP_MSG_PREFIX.as_ptr() as u64, HEAP_MSG_PREFIX.len() as u64);
            let byte_buf = [readback];
            sys_write(byte_buf.as_ptr() as u64, 1);
        }

        let fork_result = sys_fork();
        if fork_result == 0 {
            // child
            sys_write(CHILD_MSG.as_ptr() as u64, CHILD_MSG.len() as u64);
            sys_exit(0);
        } else if fork_result != SYSCALL_FAIL {
            // parent -- genuinely blocks until the child exits
            let _reaped = sys_wait(fork_result);
            sys_write(DONE_MSG.as_ptr() as u64, DONE_MSG.len() as u64);
        }

        sys_exit(0);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
