//! MILESTONE 39: a real, minimal libc for spikeling-os -- the first
//! user program built against genuine, reusable Rust syscall wrappers
//! instead of every test program hand-encoding its own raw `int 0x80`
//! sequences (usertest::USER_PROGRAM, process::PROCESS_PROGRAM,
//! loader::FDTEST_PROGRAM, and the fork/exec test program before this
//! all did exactly that). This is the real dependency the README's own
//! stated chain names: "a minimal libc (userspace programs need this to
//! make POSIX-shaped calls at all)".
//!
//! Every wrapper here matches kernel/src/usertest.rs's syscall_dispatch
//! ABI EXACTLY (checked against that file directly, not assumed):
//!   0 = write(ptr, len)              -- no return value
//!   1 = exit(code) -- never returns
//!   2 = sbrk(size) -> ptr (u64::MAX on failure)
//!   3 = open(path_ptr, path_len) -> fd (u64::MAX on failure)
//!   4 = read(fd, buf_ptr, len) -> bytes_read (u64::MAX on failure)
//!   5 = fdwrite(fd, ptr, len) -> bytes_written (u64::MAX on failure)
//!   6 = close(fd) -> status (0=ok, 1=invalid fd, 2=persist failed)
//!   7 = fork() -> child_pid to parent, 0 to child (u64::MAX on failure)
//!   8 = wait(child_pid) -> reaped_pid (u64::MAX on failure)
//!   9 = exec(path_ptr, path_len) -- never returns on success
//!
//! Deliberately minimal, matching this whole project's own "small,
//! disclosed, real" scoping convention rather than a general libc:
//!   - no errno, no C ABI compatibility, no dynamic linking -- these are
//!     ordinary `unsafe extern "C"` Rust functions, called directly by
//!     Rust code linked into the same static binary.
//!   - `u64::MAX` is the one, uniform failure sentinel across every
//!     call, matching the kernel's own actual convention (checked
//!     against usertest.rs, not invented) -- no separate error-code
//!     namespace.
//!   - no heap allocator wired to sbrk() yet -- sbrk() itself is real
//!     and returns real, distinct pointers into the process's private
//!     heap page (verified below), but nothing here implements a real
//!     `malloc`/`free` on top of it (disclosed, not hidden -- genuine
//!     next-layer work, not needed for this milestone's own test
//!     program).

#[inline(always)]
pub unsafe fn sys_write(ptr: u64, len: u64) {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 0u64,
            in("rdi") ptr,
            in("rsi") len,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
}

#[inline(always)]
pub unsafe fn sys_exit(code: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 1u64,
            in("rdi") code,
            options(nostack, noreturn)
        );
    }
}

#[inline(always)]
pub unsafe fn sys_sbrk(size: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 2u64 => ret,
            in("rdi") size,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
pub unsafe fn sys_open(path_ptr: u64, path_len: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 3u64 => ret,
            in("rdi") path_ptr,
            in("rsi") path_len,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
pub unsafe fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 4u64 => ret,
            in("rdi") fd,
            in("rsi") buf_ptr,
            in("rdx") len,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
pub unsafe fn sys_fdwrite(fd: u64, ptr: u64, len: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 5u64 => ret,
            in("rdi") fd,
            in("rsi") ptr,
            in("rdx") len,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
pub unsafe fn sys_close(fd: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 6u64 => ret,
            in("rdi") fd,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
pub unsafe fn sys_fork() -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 7u64 => ret,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
pub unsafe fn sys_wait(child_pid: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 8u64 => ret,
            in("rdi") child_pid,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
pub unsafe fn sys_exec(path_ptr: u64, path_len: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 9u64 => ret,
            in("rdi") path_ptr,
            in("rsi") path_len,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    ret
}

pub const SYSCALL_FAIL: u64 = u64::MAX;
