// MILESTONE 36 test payload: a REAL, standalone, freestanding x86_64
// ELF64 executable, built with this machine's actual installed Rust
// toolchain (rustc --target x86_64-unknown-none, linked by rust-lld,
// the same linker flavor already used to link the kernel itself via the
// bootloader crate) -- NOT hand-assembled bytes pretending to be an
// ELF. A custom linker script (linker.ld, alongside this file) places
// two GENUINELY DISTINCT PT_LOAD segments at two different, non-
// contiguous virtual pages:
//
//   segment 1 (vaddr 0x0000_5555_5000_0000): _start only -- this is
//   spikeling-os's usertest::USER_CODE_ADDR, and e_entry is required to
//   equal it EXACTLY, because Milestone 36's kernel-side loader
//   deliberately keeps the ring-3 entry trampoline's hardcoded jump
//   target (it does NOT make the entry point dynamic -- see the
//   milestone report for why).
//
//   segment 2 (vaddr 0x0000_5555_5000_1000, one page later): seg2_entry
//   (real executable code) AND the distinguishing MESSAGE string,
//   genuinely placed on a SEPARATE page from segment 1 by the linker
//   script below -- reached via a real, linker-resolved `call`
//   instruction crossing the page boundary, not merely referenced as
//   inert data. If the kernel's ELF loader mapped segment 2 at the
//   wrong address, with the wrong permissions, or not at all, this call
//   would page-fault or execute garbage instead of reaching the write
//   syscall below.
//
// Syscall ABI matches spikeling-os's own (kernel/src/usertest.rs):
// int 0x80, rax = syscall number, rax=0 is write(rdi=ptr, rsi=len),
// rax=1 is exit. No libc, no std, no relocations beyond what the
// static, non-PIE x86_64-unknown-none link already resolves at link
// time.
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

// A concrete, fixed-size array (not `&[u8]`) so the byte data itself is
// what carries the #[link_section] attribute -- an `&[u8]` static's
// bytes are instead stored as a separate, anonymous, compiler-promoted
// constant that (empirically, checked by building and inspecting with
// readelf) does NOT inherit the outer static's #[link_section] under
// the large code model this build requires (see the code-model=large
// comment on the build command), landing wherever the compiler's own
// default placement puts anonymous rodata instead of the section this
// file actually asks for.
#[unsafe(link_section = ".rodata.seg2")]
static MESSAGE: [u8; 104] =
    *b"hello from ELF PT_LOAD segment #2 (non-zero vaddr, real call) -- milestone 36 real ELF loader confirmed!";

// Segment 2's code: lives in its own linker-script-placed section so it
// lands on the SECOND PT_LOAD segment's page, genuinely separate from
// _start's page.
#[unsafe(link_section = ".text.seg2")]
extern "C" fn seg2_entry() -> ! {
    let ptr = MESSAGE.as_ptr() as u64;
    let len = MESSAGE.len() as u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") 0u64,
            in("rdi") ptr,
            in("rsi") len,
            options(nostack)
        );
        asm!(
            "int 0x80",
            in("rax") 1u64,
            options(nostack, noreturn)
        );
    }
}

// Segment 1: e_entry lands here (must equal USER_CODE_ADDR exactly --
// enforced by the linker script placing .text.start at that address
// with nothing before it). Does a real cross-page `call` into segment
// 2 -- proof, if it works, that the loader mapped BOTH segments
// correctly, not just segment 0.
#[unsafe(link_section = ".text.start")]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    unsafe {
        asm!("call {}", sym seg2_entry, options(noreturn));
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
