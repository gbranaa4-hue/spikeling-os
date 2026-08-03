//! IDT setup: CPU exception handlers first (this file), hardware
//! interrupts (PIC remap + timer) added once exceptions are proven
//! working -- getting exception handling wrong produces a silent
//! triple-fault reboot with zero diagnostic output, so this is verified
//! as its own step before anything depends on it.

use crate::gdt;
use crate::usertest;
use crate::{hlt_loop, serial};
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::{PrivilegeLevel, VirtAddr};

pub const SYSCALL_VECTOR: u8 = 0x80;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
        idt[InterruptIndex::Mouse.as_u8()].set_handler_fn(mouse_interrupt_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        unsafe {
            // MILESTONE 27: usertest::syscall_entry is a naked function,
            // not an `extern "x86-interrupt" fn` -- set_handler_fn's
            // signature requires that ABI, so the raw address goes in
            // via set_handler_addr instead. DPL=3 set explicitly: every
            // gate defaults to DPL=0 (Ring0), which would make a ring-3
            // `int 0x80` immediately #GP-fault (the CPU checks the
            // CALLER's CPL against the gate's DPL for software
            // interrupts) before the handler ever ran.
            idt[SYSCALL_VECTOR]
                .set_handler_addr(VirtAddr::new(usertest::syscall_entry as *const () as u64))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        idt
    };
}

// STAGE B: PIC remap + timer interrupt -- the actual preemption clock.
// Hardware IRQs default to vectors 0-15, which collide with CPU
// exceptions (0-31); remapped to 32-47 (PIC_1_OFFSET/PIC_2_OFFSET),
// the standard convention.
pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

/// Real count of timer interrupts actually delivered -- the ground
/// truth Milestone 5 stage B is verified against (a busy hlt loop that
/// merely "returns" doesn't prove the timer fired periodically; this
/// atomic, incremented only inside the real handler, does).
pub static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard = PIC_1_OFFSET + 1,
    Mouse = PIC_1_OFFSET + 12, // IRQ12, on the slave PIC
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

pub fn init_pics() {
    unsafe { PICS.lock().initialize() };
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    if crate::tasks::RING3_EXCURSION_ACTIVE.load(Ordering::SeqCst) {
        crate::tasks::TIMER_DURING_EXCURSION.fetch_add(1, Ordering::SeqCst);
    }
    unsafe {
        // EOI first: the PIC needs to know this IRQ is acknowledged
        // before we potentially switch stacks underneath it, in case
        // another timer interrupt is already pending.
        PICS.lock().notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
    // MILESTONE 9: real LIF neuron dynamics ticked on every real
    // hardware timer interrupt, not a simulated/synthetic clock.
    // MILESTONE 21: neurons.rs no longer has its own tick() -- LeftKey/
    // RightKey/Motor are now ordinary named neurons inside this single
    // GenericNetwork (seeded by neurons::init() at boot), so one tick()
    // call now drives the fixed network AND every shell-defined
    // (`addneuron`) network together, on the same real hardware clock.
    crate::network::tick();

    // MILESTONE 5, STAGE C: may switch to a different task's stack and
    // not return here directly -- it returns to whichever context gets
    // switched back into later, at which point iretq resumes THAT
    // context's own interrupted point, not necessarily this one.
    crate::tasks::timer_tick_switch();
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    if crate::tasks::RING3_EXCURSION_ACTIVE.load(Ordering::SeqCst) {
        crate::tasks::KEYBOARD_DURING_EXCURSION.fetch_add(1, Ordering::SeqCst);
    }
    crate::keyboard::on_interrupt();
    unsafe {
        PICS.lock().notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::mouse::on_interrupt();
    unsafe {
        // notify_end_of_interrupt handles sending EOI to both PICs when
        // the interrupt is on the slave (IRQ >= 8), as this one is --
        // standard PIC cascading, built into the pic8259 crate.
        PICS.lock().notify_end_of_interrupt(InterruptIndex::Mouse.as_u8());
    }
}

pub fn init_idt() {
    IDT.load();
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    writeln!(serial(), "EXCEPTION: BREAKPOINT\n{:#?}", stack_frame).unwrap();
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    // deliberately not using the shared serial() helper's own SerialPort
    // instance here -- if the double fault happened WHILE that was
    // mid-write, re-entering it could double-fault again; a fresh
    // handle to the same hardware port is safe either way
    let mut port = serial();
    let _ = writeln!(port, "EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
    hlt_loop();
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    let mut port = serial();
    let _ = writeln!(port, "EXCEPTION: PAGE FAULT");
    let _ = writeln!(port, "Accessed Address: {:?}", Cr2::read());
    let _ = writeln!(port, "Error Code: {:?}", error_code);
    let _ = writeln!(port, "{:#?}", stack_frame);
    hlt_loop();
}
