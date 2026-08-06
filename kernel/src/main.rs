#![no_std] // don't link the Rust standard library
#![no_main] // disable all Rust-level entry points
#![feature(abi_x86_interrupt)] // MILESTONE 5: extern "x86-interrupt" fn handlers

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use core::fmt::Write;
use x86_64::VirtAddr;

mod allocator;
mod ata;
mod fs;
mod console;
mod elf;
mod gdt;
mod interrupts;
mod keyboard;
mod loader;
mod memory;
mod mouse;
mod network;
mod neurons;
mod nic;
mod pci;
mod process;
mod rtc;
mod scheduler;
mod shell;
mod speaker;
mod tasks;
mod ternary;
mod usertest;

// MILESTONE 2: real pixel output via boot_info.framebuffer.
//
// Draws a horizontal RGB gradient (red -> green -> blue across the full
// width, repeated on every row) instead of a solid fill deliberately --
// a solid fill can look "correct" even with a broken stride/bytes-per-
// pixel calculation, since every byte written happens to be the same
// value anyway. A gradient only renders correctly if the x/y -> byte
// offset math (including `stride`, which the bootloader_api docs note
// can be WIDER than `width` due to hardware alignment padding -- using
// `width` there instead would silently skew every row after the first)
// and the channel ordering per PixelFormat are both actually right.
fn draw_gradient(buffer: &mut [u8], info: FrameBufferInfo) {
    let bpp = info.bytes_per_pixel;
    for y in 0..info.height {
        let row_start = y * info.stride * bpp;
        for x in 0..info.width {
            let offset = row_start + x * bpp;
            if offset + bpp > buffer.len() {
                continue;
            }
            // position across the row, 0.0 at the left edge to 1.0 at
            // the right, driving a red->green->blue sweep
            let t = x as f32 / info.width.max(1) as f32;
            let (r, g, b) = if t < 0.5 {
                let s = t / 0.5;
                (((1.0 - s) * 255.0) as u8, (s * 255.0) as u8, 0u8)
            } else {
                let s = (t - 0.5) / 0.5;
                (0u8, ((1.0 - s) * 255.0) as u8, (s * 255.0) as u8)
            };
            match info.pixel_format {
                PixelFormat::Rgb => {
                    buffer[offset] = r;
                    buffer[offset + 1] = g;
                    buffer[offset + 2] = b;
                }
                PixelFormat::Bgr => {
                    buffer[offset] = b;
                    buffer[offset + 1] = g;
                    buffer[offset + 2] = r;
                }
                PixelFormat::U8 => {
                    // grayscale: standard luminance weighting, not a
                    // plain average, so the gradient still reads as a
                    // visible sweep instead of a flat mid-gray band
                    let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
                    buffer[offset] = gray;
                }
                _ => {
                    // PixelFormat is #[non_exhaustive] and Unknown's
                    // exact bit layout isn't worth guessing at here --
                    // leave those pixels untouched rather than write
                    // channel data into the wrong bit positions.
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QemuExitCode {
    Success = 0x10,
    Failed = 0x11,
}

pub fn exit_qemu(exit_code: QemuExitCode) -> ! {
    use x86_64::instructions::port::Port;

    unsafe {
        let mut port = Port::new(0xf4);
        port.write(exit_code as u32);
    }

    hlt_loop();
}

pub fn hlt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

pub fn serial() -> uart_16550::SerialPort {
    let mut port = unsafe { uart_16550::SerialPort::new(0x3F8) };
    port.init();
    port
}

// MILESTONE 3 needs the bootloader to actually map all physical memory
// somewhere in virtual address space (the standard "physical memory
// offset" trick) so the kernel can build page tables and allocate
// frames -- off by default, so request it explicitly.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

// MILESTONE 1: boots, proves the bootloader -> kernel handoff and serial
// output work, then halts (not exits) so this behaves like a real OS
// staying alive rather than a test harness that shuts itself down. Every
// later piece -- Spikeling's spiking-network runtime as the scheduler,
// memory management, drivers -- gets built on top of this confirmed-working
// boot path, not assumed to work.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let mut port = serial();
    writeln!(port, "spikeling-os: kernel entered").unwrap();
    writeln!(port, "boot info: {boot_info:?}").unwrap();
    writeln!(port, "milestone 1: boot -> kernel handoff confirmed working").unwrap();

    if let Some(fb) = boot_info.framebuffer.as_mut() {
        let info = fb.info();
        draw_gradient(fb.buffer_mut(), info);
        writeln!(
            port,
            "milestone 2: drew {}x{} {:?} gradient to framebuffer",
            info.width, info.height, info.pixel_format
        )
        .unwrap();
    } else {
        writeln!(port, "milestone 2: FAILED -- no framebuffer provided by bootloader").unwrap();
    }

    // MILESTONE 7: takes ownership of the framebuffer (Milestone 2's
    // gradient was only ever a transient borrow) so keyboard.rs can
    // render live, on-screen text on every keystroke -- combining
    // Milestone 2's pixel output and Milestone 6's keyboard input into
    // something actually visible and interactive, not just serial text.
    if let Some(fb) = boot_info.framebuffer.take() {
        let info = fb.info();
        console::init(fb.into_buffer(), info);
        writeln!(port, "milestone 7: console initialized on the framebuffer").unwrap();
    } else {
        writeln!(port, "milestone 7: FAILED -- no framebuffer to build a console on").unwrap();
    }

    // MILESTONE 3: paging + a heap allocator, so alloc::{Vec, Box, ...}
    // actually work in-kernel -- required before anything resembling
    // Spikeling's runtime (which allocates) can run here.
    //
    // MILESTONE 21: this now has to run BEFORE the neuron-network init
    // below, not after (the original ordering, harmless when M9's
    // network was a fixed-size Option<Network> with no heap use at
    // all). Unifying onto network.rs's GenericNetwork means
    // seed_fixed_network() now does real heap allocation (String::from
    // for each neuron name, Vec growth for neurons/synapses/
    // pending_stimulus) -- calling it before the heap exists produced a
    // real, reproducible panic ("memory allocation of 7 bytes failed",
    // 7 being len("LeftKey")), caught by an actual boot/serial-log test
    // during this milestone's verification, not assumed away.
    let phys_mem_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("bootloader did not map physical memory (check BOOTLOADER_CONFIG)");
    let phys_mem_offset = VirtAddr::new(phys_mem_offset);

    let mut mapper = unsafe { memory::init(phys_mem_offset) };
    let mut frame_allocator = unsafe { memory::BootInfoFrameAllocator::init(&boot_info.memory_regions) };

    allocator::init_heap(&mut mapper, &mut frame_allocator)
        .expect("heap initialization failed");
    writeln!(port, "milestone 3: heap mapped at {:#x}, {} bytes", allocator::HEAP_START, allocator::HEAP_SIZE).unwrap();

    // Real allocation test, not just "it compiled": a growing Vec (forces
    // at least one internal reallocation, exercising realloc/free, not
    // just a single alloc call) and a heap-allocated Box, both read back
    // and checked against expected values before being trusted.
    let mut v = Vec::new();
    for i in 0..500u64 {
        v.push(i);
    }
    let sum: u64 = v.iter().sum();
    let expected_sum: u64 = (0..500u64).sum();
    let boxed = Box::new(12345u64);

    if sum == expected_sum && *boxed == 12345 {
        writeln!(
            port,
            "milestone 3: heap allocation verified -- Vec<u64> of {} elements summed correctly ({sum}), Box<u64> round-tripped correctly",
            v.len()
        )
        .unwrap();
    } else {
        writeln!(
            port,
            "milestone 3: FAILED -- heap allocation produced wrong data (sum={sum}, expected={expected_sum}, boxed={})",
            *boxed
        )
        .unwrap();
    }

    // MILESTONE 27: maps the user code + stack pages for the real
    // ring-3 test now, while `mapper`/`frame_allocator` are still local,
    // in-scope variables from the heap setup just above -- simpler than
    // threading them through a global `Mutex<Option<OffsetPageTable>>`
    // for a one-time setup step nothing else in the kernel needs.
    // usertest::run() (wired to the shell's `usertest` command) just
    // reuses the fixed addresses these pages were mapped at; setup() is
    // idempotent so this only ever actually maps once regardless of how
    // many times `usertest` is later run.
    match usertest::setup(&mut mapper, &mut frame_allocator) {
        Ok(()) => writeln!(
            port,
            "milestone 27: user-mode test page mapped -- code at {:#x}, stack top at {:#x}",
            usertest::USER_CODE_ADDR,
            usertest::USER_STACK_ADDR + usertest::USER_STACK_SIZE
        )
        .unwrap(),
        Err(e) => writeln!(port, "milestone 27: FAILED to map user test pages -- {e}").unwrap(),
    }

    // MILESTONE 30: real per-process address space isolation, built on
    // top of Milestone 27's ring-3 mechanism above. save_kernel_cr3()
    // must run before ANY process's PML4 could ever be loaded into CR3,
    // so there's always a known-good value for the exit syscall to
    // restore to -- called here, immediately after the kernel's own
    // paging is fully set up and before process::init_test_processes()
    // creates the two processes' private page tables.
    process::save_kernel_cr3();
    match process::init_test_processes(&mut frame_allocator, phys_mem_offset) {
        Ok(()) => writeln!(
            port,
            "milestone 30: process A + process B private address spaces created -- see serial log above for each one's own pml4/code/stack physical frames"
        )
        .unwrap(),
        Err(e) => writeln!(port, "milestone 30: FAILED to create test processes -- {e}").unwrap(),
    }

    // MILESTONE 35: a third hardcoded process slot (FDTEST_PROCESS),
    // running loader::FDTEST_PROGRAM via the SAME safe `runproc`-style
    // mechanism as process A/B -- see process.rs's FDTEST_PROCESS static
    // doc comment for why this exists alongside (not instead of)
    // `runfile fdtestprog`. Created here, right after process A/B, while
    // the same local frame_allocator/phys_mem_offset are conveniently
    // still in scope.
    match process::init_fdtest_process(&mut frame_allocator, phys_mem_offset, &loader::FDTEST_PROGRAM) {
        Ok(()) => writeln!(port, "milestone 35: FDTEST_PROCESS private address space created").unwrap(),
        Err(e) => writeln!(port, "milestone 35: FAILED to create FDTEST_PROCESS -- {e}").unwrap(),
    }

    // MILESTONE 37: a fifth hardcoded process slot (FORK_TEST_PROCESS),
    // running process::FORK_TEST_PROGRAM -- this milestone's own real
    // fork()/wait()/exec() demo. Created here, right after
    // FDTEST_PROCESS, while the same local frame_allocator/
    // phys_mem_offset are conveniently still in scope (this is the
    // LAST boot-time consumer of frame_allocator before it moves into
    // memory.rs's global static just below -- process::fork()'s own
    // runtime allocations reach the frame allocator through THAT global
    // static instead, exactly like loader.rs's `runfile`/`runelf`
    // already do).
    match process::init_fork_test_process(&mut frame_allocator, phys_mem_offset) {
        Ok(()) => writeln!(port, "milestone 37: FORK_TEST_PROCESS private address space created").unwrap(),
        Err(e) => writeln!(port, "milestone 37: FAILED to create FORK_TEST_PROCESS -- {e}").unwrap(),
    }

    // MILESTONE 41: two more hardcoded process slots (SIGSEGV_TEST_PROCESS,
    // SIGKILL_TEST_PROCESS), created here for the same "frame_allocator/
    // phys_mem_offset conveniently still in scope" reason as
    // FORK_TEST_PROCESS just above.
    match process::init_sigsegv_test_process(&mut frame_allocator, phys_mem_offset) {
        Ok(()) => writeln!(port, "milestone 41: SIGSEGV_TEST_PROCESS private address space created").unwrap(),
        Err(e) => writeln!(port, "milestone 41: FAILED to create SIGSEGV_TEST_PROCESS -- {e}").unwrap(),
    }
    match process::init_sigkill_test_process(&mut frame_allocator, phys_mem_offset) {
        Ok(()) => writeln!(port, "milestone 41: SIGKILL_TEST_PROCESS private address space created").unwrap(),
        Err(e) => writeln!(port, "milestone 41: FAILED to create SIGKILL_TEST_PROCESS -- {e}").unwrap(),
    }

    // MILESTONE 34: real general program loader. `frame_allocator`'s
    // last boot-time consumer was process::init_test_processes just
    // above -- nothing later in kernel_main needs it -- so it's moved
    // (by value) into memory.rs's global static here, alongside
    // phys_mem_offset, so loader.rs's `runfile` shell command can
    // allocate fresh physical frames for a NEW process's private page
    // tables from arbitrary, LATER, shell-command-driven code, not just
    // the handful of one-time boot-time setup calls every earlier
    // milestone threaded it through directly.
    memory::set_phys_mem_offset(phys_mem_offset);
    memory::install_frame_allocator(frame_allocator);
    writeln!(
        port,
        "milestone 34: frame allocator + phys-mem offset published globally -- 'runfile PATH' can now load and run programs from the real filesystem"
    )
    .unwrap();
    loader::self_test_size_check();
    loader::self_test_fdtest_program();
    // MILESTONE 36: real ELF64 parser self-test -- parses the embedded
    // testelf.elf (a genuine, externally-built, multi-segment ELF64
    // executable) once at boot, independent of whether `seedtestelf`/
    // `runelf` are ever typed at the shell, so every boot's serial log
    // carries direct proof elf::parse() reads the real ELF structure
    // correctly.
    loader::self_test_elf_parse();
    // MILESTONE 37: real, filesystem-independent proof that
    // FORK_TEST_PROGRAM's own byte layout matches what its doc comment
    // claims -- same "every boot's serial log carries direct proof"
    // reasoning as the self-tests just above.
    process::self_test_fork_test_program();
    // Real, boot-time, non-interactive proof that fs::write_file()
    // actually reaches the disk -- every prior "verified" disk write in
    // this kernel's history relied on an interactive shell command
    // gated on QEMU's sendkey reaching the guest. This runs unattended
    // on every single boot instead, so a broken sendkey path or a
    // missing secondary-ATA-drive QEMU argument shows up in the serial
    // log immediately rather than only when someone remembers to test
    // it by hand.
    fs::self_test_disk_write();
    // MILESTONE 40: same non-interactive-proof reasoning as the
    // disk-write self-test just above -- pipe()/dup/dup2 mechanics
    // checked directly on every boot, no interactive shell command
    // needed.
    process::self_test_pipe_mechanics();

    // MILESTONE 9: real LIF neuron network -- initialized before
    // interrupts are enabled (stage 5b, below) so it's ready the
    // instant the first real timer tick or keyboard event could arrive.
    // MILESTONE 11: init() now also tries loading learned weights from
    // the persistence disk (ata.rs) -- reported honestly either way,
    // not silently.
    // MILESTONE 21: init() now seeds LeftKey/RightKey/Motor into the
    // single shared GenericNetwork (network.rs) instead of a separate
    // fixed-size struct -- needs the heap above, see the comment there.
    // MILESTONE 38: init() now loads a ternary-packed, NAME-KEYED
    // synapse list (ata.rs/ternary.rs) instead of two hardcoded f32s,
    // and reports how many of the saved entries actually matched a
    // synapse that exists in the network at boot (only LeftKey->Motor
    // and RightKey->Motor exist this early -- DSL-added synapses from
    // a previous session are NOT recreated at boot, an honest,
    // disclosed scope limit: topology itself isn't persisted, only
    // weights of synapses that already exist).
    let weights_loaded = neurons::init();
    writeln!(port, "milestone 9: LIF neuron network initialized (LeftKey/RightKey -> Motor)").unwrap();
    match weights_loaded {
        Some(report) => {
            writeln!(
                port,
                "milestone 11/38: {} of {} saved synapse weight(s) matched and loaded from disk (ternary-packed) -- persisted across a real reboot",
                report.matched, report.total_in_file
            )
            .unwrap();
            let packed_bytes = report.total_in_file * ternary::PACKED_BYTES_PER_WEIGHT;
            let f32_bytes = report.total_in_file * 4;
            writeln!(
                port,
                "milestone 38: ternary weight compression -- {} weight(s): {} packed bytes vs {} equivalent f32 bytes ({:.2}x smaller per weight, {} trits/weight)",
                report.total_in_file,
                packed_bytes,
                f32_bytes,
                f32_bytes as f32 / packed_bytes as f32,
                ternary::TRITS_PER_WEIGHT
            )
            .unwrap();
        }
        None => {
            writeln!(port, "milestone 11: no saved weights found on disk -- starting from neutral defaults").unwrap();
        }
    }

    // MILESTONE 4: topologically-coupled task scheduler (scheduler.rs).
    // Same self-healing question topological_bank.py asked of an
    // 8-Resonator bank, now asked of the kernel's own task-selection
    // logic: does killing one slot's neuron crash or starve the rest,
    // or does a topologically-dimerized (g>0) bank stay fairer under
    // that defect than a trivial (g<0) one? Report honestly either way,
    // same discipline as the Python original -- no assumed outcome.
    const N_SLOTS: usize = 8; // matches topological_bank.spk's 8 resonators
    const DEFECT_SLOT: usize = 3; // interior slot, same spirit as DEFECT_SITES there
    const WARMUP_TICKS: u32 = 400;
    const POST_DEFECT_TICKS: u32 = 2000;

    fn run_trial(g: f32) -> (u32, u32, usize) {
        let mut sched = scheduler::TopologicalScheduler::new(N_SLOTS, g);
        for _ in 0..WARMUP_TICKS {
            sched.step();
        }
        sched.kill(DEFECT_SLOT);
        for _ in 0..POST_DEFECT_TICKS {
            sched.step();
        }
        let counts: Vec<u32> = sched
            .slots
            .iter()
            .filter(|s| s.alive)
            .map(|s| s.fire_count)
            .collect();
        let min = *counts.iter().min().unwrap();
        let max = *counts.iter().max().unwrap();
        (min, max, counts.len())
    }

    let (min_t, max_t, n_alive) = run_trial(0.6);
    let (min_v, max_v, _) = run_trial(-0.6);

    writeln!(
        port,
        "milestone 4: kernel survived defect injection at slot {DEFECT_SLOT} -- {n_alive} slots still alive and scheduled, no panic"
    )
    .unwrap();
    writeln!(
        port,
        "milestone 4: topological (g=+0.6) post-defect fire counts: min={min_t} max={max_t} fairness={:.3}",
        min_t as f32 / max_t as f32
    )
    .unwrap();
    writeln!(
        port,
        "milestone 4: trivial     (g=-0.6) post-defect fire counts: min={min_v} max={max_v} fairness={:.3}",
        min_v as f32 / max_v as f32
    )
    .unwrap();
    let topo_fairer = (min_t as f32 / max_t as f32) > (min_v as f32 / max_v as f32);
    writeln!(
        port,
        "milestone 4: result -- {}",
        if topo_fairer {
            "topological coupling stayed fairer under the defect (matches topological_bank.py's hypothesis)"
        } else {
            "topological coupling did NOT stay fairer under the defect here (reporting honestly, not the assumed outcome)"
        }
    )
    .unwrap();

    // MILESTONE 5, STAGE A: GDT+TSS (needed first: the double-fault
    // handler's separate IST stack) and a real IDT with CPU exception
    // handlers. Verified with an actual int3 breakpoint, not just "it
    // compiled": if the handler is wired correctly, execution resumes
    // cleanly on the very next line, proving the IDT entry fired AND
    // returned properly -- a broken handler either triple-faults
    // (silent reboot, no serial output at all after this point) or
    // never returns.
    gdt::init();
    interrupts::init_idt();
    writeln!(port, "milestone 5a: GDT/TSS + IDT loaded").unwrap();
    x86_64::instructions::interrupts::int3();
    writeln!(port, "milestone 5a: resumed after breakpoint exception -- handler returned correctly").unwrap();

    // MILESTONE 41: real signals (SIGSEGV: a ring-3 page fault
    // terminates only the faulting process, kernel keeps running;
    // SIGKILL: unconditional termination of a live, never-yet-run
    // forked child). Real root-cause fix: this was originally called
    // right after the pipe self-test above, well BEFORE this exact
    // point -- entering ring 3 (which `run()` does) fundamentally
    // requires this kernel's own GDT/IDT to already be loaded via
    // `lgdt`/`lidt` (gdt::init()/interrupts::init_idt(), directly
    // above), not just allocated in memory as the lazy_static structs
    // always were. Calling it before this point faulted immediately on
    // ring-3 entry with a #GP whose error code decoded to GDT index 3
    // (the user code selector, which only exists in THIS kernel's own,
    // not-yet-loaded GDT) -- confirmed the real mechanism, not process-
    // or program-content-specific, by observing the IDENTICAL fault
    // signature on PROCESS_A (the most basic, most-verified process in
    // the kernel) when called from the old position, proving every
    // prior "verified" ring-3 entry only ever happened via the
    // interactive shell's command loop (which naturally runs after this
    // point), never non-interactively from boot until this milestone's
    // own self-test tried it for the first time.
    process::self_test_signals();

    // MILESTONE 5, STAGE B: PIC remap + timer interrupt -- the real
    // preemption clock a future context switch will ride on. Verified
    // against the actual TIMER_TICKS atomic (incremented only inside
    // the real handler), not just "hlt returned N times" -- hlt wakes
    // on any interrupt, so counting wakeups alone wouldn't prove the
    // timer specifically fired.
    // MILESTONE 16: PS/2 mouse init -- talks to the 8042 controller via
    // synchronous port I/O, doesn't need interrupts enabled yet; must
    // run before enable() below so the mouse is already streaming by
    // the time IRQ12 could first fire.
    // MILESTONE 19: the shell's prompt must exist on-screen before
    // interrupts (and therefore real keystrokes) can reach it -- this
    // used to run at the bottom of this function, after milestone 5b's
    // timer-wait and 5c's full preemption demo both had a chance to
    // burn real wall-clock time with interrupts already live. A
    // keystroke arriving in that gap got fully processed by
    // shell::on_char (echoed, run, even its own post-command prompt
    // printed) before shell::init()'s own first "> " ever ran, which
    // then printed unconditionally on top of whatever was already
    // there -- a real, reproducible missing/doubled prompt, caught via
    // a Milestone 18 filesystem-test screenshot under host load.
    mouse::init();

    // MILESTONE 20: real PCI bus enumeration via CONFIG_ADDRESS/
    // CONFIG_DATA port I/O -- synchronous, like mouse::init() above,
    // so it can run here too before interrupts are enabled. Verified
    // against the actual devices found, not assumed: QEMU's own PCI
    // host bridge is almost always vendor 8086 device 1237 at
    // 00:00.0, so its absence would mean the enumeration itself is
    // broken, not that QEMU has no PCI bus.
    pci::init();
    let pci_devices = pci::devices();
    writeln!(port, "milestone 20: PCI bus 0 scan found {} device(s)", pci_devices.len()).unwrap();
    for d in &pci_devices {
        writeln!(
            port,
            "milestone 20: {:02x}:{:02x}.{} vendor={:04x} device={:04x} class={:02x} subclass={:02x}",
            d.bus, d.device, d.function, d.vendor_id, d.device_id, d.class_code, d.subclass
        )
        .unwrap();
    }
    match pci::find_nic() {
        Some(nic) => writeln!(
            port,
            "milestone 20: network controller found -- vendor={:04x} device={:04x}",
            nic.vendor_id, nic.device_id
        )
        .unwrap(),
        None => writeln!(port, "milestone 20: no network controller found on bus 0").unwrap(),
    }

    // MILESTONE 24: real e1000 packet transmission -- builds on M20's
    // enumeration by actually mapping the device's BAR0 MMIO region
    // (through the same phys_mem_offset M3 already set up above) and
    // driving it through a genuine reset + TX-ring-setup sequence.
    // Reused here rather than re-derived, per memory::init()'s own
    // pattern.
    match nic::init(phys_mem_offset) {
        Ok(()) => writeln!(
            port,
            "milestone 24: e1000 NIC initialized -- MAC {} (address-valid bit: {})",
            nic::mac_address().map(nic::format_mac).unwrap_or_else(|| "??".into()),
            nic::mac_is_valid().unwrap_or(false)
        )
        .unwrap(),
        Err(e) => writeln!(port, "milestone 24: e1000 NIC init FAILED -- {e}").unwrap(),
    }

    interrupts::init_pics();
    shell::init();
    x86_64::instructions::interrupts::enable();
    writeln!(port, "milestone 5b: PIC initialized, interrupts enabled").unwrap();

    for _ in 0..80 {
        x86_64::instructions::hlt();
    }
    let ticks = interrupts::TIMER_TICKS.load(core::sync::atomic::Ordering::Relaxed);
    writeln!(
        port,
        "milestone 5b: {ticks} real timer interrupts observed after 80 hlt cycles -- {}",
        if ticks > 0 { "PIT firing confirmed" } else { "FAILED: no ticks observed" }
    )
    .unwrap();

    // MILESTONE 5, STAGE C: real per-task stacks + a genuine preemptive
    // context switch (tasks.rs), selection driven by the SAME
    // TopologicalScheduler as Milestone 4 -- now with a real scheduler
    // slot to plug into, per the README's own stated goal. Three
    // worker tasks spin on their own counters forever; NONE of them
    // ever call back into scheduling code -- only the timer interrupt
    // moves execution between them. Verified by checking all three
    // counters actually grew, not just the first one: a broken switch
    // would either hang (never returns here) or leave later tasks at
    // exactly 0 (never really preempted into).
    tasks::run_preemption_demo();
    let c0 = tasks::TASK_COUNTERS[0].load(core::sync::atomic::Ordering::Relaxed);
    let c1 = tasks::TASK_COUNTERS[1].load(core::sync::atomic::Ordering::Relaxed);
    let c2 = tasks::TASK_COUNTERS[2].load(core::sync::atomic::Ordering::Relaxed);
    writeln!(
        port,
        "milestone 5c: returned from preemption demo -- worker counters: task0={c0} task1={c1} task2={c2}"
    )
    .unwrap();
    let all_ran = c0 > 0 && c1 > 0 && c2 > 0;
    writeln!(
        port,
        "milestone 5c: {}",
        if all_ran {
            "all three tasks genuinely preempted and ran -- real preemptive multitasking confirmed"
        } else {
            "FAILED -- at least one task never ran, preemption is not working correctly"
        }
    )
    .unwrap();

    // MILESTONE 6: PS/2 keyboard input via IRQ1 -- the first real input
    // device, making the OS interactive for the first time. Verified
    // with REAL synthetic keystrokes sent through QEMU's own monitor
    // (`sendkey`, standing in for a human at the keyboard) during this
    // bounded wait window, driving genuine hardware IRQ1 interrupts --
    // not a unit test calling the decode function directly.
    writeln!(port, "milestone 6: waiting for keyboard input (external test harness will type)...").unwrap();

    // MILESTONE 8: the interactive shell (shell.rs) -- ties together
    // the console (M7), keyboard (M6), heap (M3), and real
    // introspection into M4's scheduler / M5's tasks. Its prompt is
    // now printed earlier (Milestone 19, above, before interrupts are
    // even enabled) so a real typed session (command text, backspace
    // edits, command output) can be exercised and screenshot-verified
    // from the very first keystroke, not just raw character
    // accumulation.
    for _ in 0..150 {
        x86_64::instructions::hlt();
    }
    let typed = keyboard::TYPED.lock().clone();
    writeln!(port, "milestone 6: received {} chars: {:?}", typed.len(), typed).unwrap();
    writeln!(
        port,
        "milestone 6: {}",
        if !typed.is_empty() {
            "real PS/2 keyboard input confirmed via genuine IRQ1 interrupts"
        } else {
            "FAILED -- no keystrokes received"
        }
    )
    .unwrap();
    writeln!(port, "milestone 8: interactive shell active -- see framebuffer for real typed session").unwrap();

    // MILESTONE 25: dynamic task spawn/kill needs the scheduler to keep
    // switching past the bounded milestone 5c demo window, or a spawned
    // task would never actually get real CPU time -- safe to flip on
    // here, last, since kernel_main has nothing left to do from this
    // point on but hlt forever, so losing control to a worker task is
    // harmless.
    tasks::enable_background_scheduling();
    writeln!(port, "milestone 25: background task scheduling enabled -- spawn/kill available via the shell").unwrap();

    // the shell keeps responding to keystrokes forever from here on --
    // entirely driven by the keyboard ISR firing asynchronously, same
    // as the timer ISR already does; kernel_main itself has nothing
    // further to do but stay parked.
    hlt_loop();
}

/// This function is called on panic.
#[panic_handler]
#[cfg(not(test))]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(serial(), "PANIC: {info}");
    exit_qemu(QemuExitCode::Failed);
}
