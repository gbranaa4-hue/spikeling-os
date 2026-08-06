// MILESTONE 40 test payload: a REAL, standalone, freestanding x86_64
// ELF64 executable, built with this project's own pinned Rust toolchain
// (rustc --target x86_64-unknown-none, rust-lld), same recipe as
// Milestone 36's testelf.rs -- NOT hand-assembled bytes. Single PT_LOAD
// segment at USER_CODE_ADDR (no need for testelf.rs's two-segment
// design here; this milestone is about pipe()/dup2(), not the ELF
// loader itself).
//
// Real, observable proof, via raw syscalls matching spikeling-os's own
// ABI (int 0x80, kernel/src/usertest.rs):
//   1. pipe() (syscall 10) creates a real pipe.
//   2. fdwrite() (syscall 5) writes 8 known bytes into its write end.
//   3. read() (syscall 4) reads them back out of its read end -- proves
//      the ring buffer round-trips real data, not just returns success.
//   4. dup2() (syscall 12) duplicates the write end onto fd 3.
//   5. fdwrite()s a SECOND, different 8 bytes through fd 3 -- the
//      DUPLICATE, not the original write fd.
//   6. read()s AGAIN from the ORIGINAL read end -- if dup2() gave fd 3 a
//      real shared reference to the same pipe (not an independent copy),
//      this read must see the fd-3-written bytes. If dup2 were broken
//      (e.g. silently no-op'd or pointed at a separate pipe), this
//      read would see stale/wrong data instead.
//   7. Writes one final PASS/FAIL message (syscall 0) reporting both
//      results independently, then exit()s (syscall 1).
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const MSG1: [u8; 8] = *b"PIPETEST";
const MSG2: [u8; 8] = *b"DUPTEST!";
const PASS_MSG: &[u8] = b"milestone 40 pipetest: pipe=PASS dup2=PASS\n";
const FAIL_PIPE_MSG: &[u8] = b"milestone 40 pipetest: pipe=FAIL dup2=SKIPPED\n";
const FAIL_DUP_MSG: &[u8] = b"milestone 40 pipetest: pipe=PASS dup2=FAIL\n";

#[unsafe(link_section = ".text.start")]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let mut pipefd: [u64; 2] = [0, 0];
    let mut readbuf1: [u8; 8] = [0; 8];
    let mut readbuf2: [u8; 8] = [0; 8];

    unsafe {
        asm!("int 0x80", in("rax") 10u64, in("rdi") (&mut pipefd as *mut u64) as u64, options(nostack));
    }
    let read_fd = pipefd[0];
    let write_fd = pipefd[1];

    unsafe {
        asm!("int 0x80", in("rax") 5u64, in("rdi") write_fd, in("rsi") MSG1.as_ptr() as u64, in("rdx") 8u64, options(nostack));
        asm!("int 0x80", in("rax") 4u64, in("rdi") read_fd, in("rsi") (&mut readbuf1 as *mut u8) as u64, in("rdx") 8u64, options(nostack));
    }
    let pipe_ok = readbuf1 == MSG1;

    if pipe_ok {
        unsafe {
            asm!("int 0x80", in("rax") 12u64, in("rdi") write_fd, in("rsi") 3u64, options(nostack));
            asm!("int 0x80", in("rax") 5u64, in("rdi") 3u64, in("rsi") MSG2.as_ptr() as u64, in("rdx") 8u64, options(nostack));
            asm!("int 0x80", in("rax") 4u64, in("rdi") read_fd, in("rsi") (&mut readbuf2 as *mut u8) as u64, in("rdx") 8u64, options(nostack));
        }
    }
    let dup_ok = pipe_ok && readbuf2 == MSG2;

    let result: &[u8] = if pipe_ok && dup_ok {
        PASS_MSG
    } else if !pipe_ok {
        FAIL_PIPE_MSG
    } else {
        FAIL_DUP_MSG
    };

    unsafe {
        asm!("int 0x80", in("rax") 0u64, in("rdi") result.as_ptr() as u64, in("rsi") result.len() as u64, options(nostack));
        asm!("int 0x80", in("rax") 1u64, options(nostack, noreturn));
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
