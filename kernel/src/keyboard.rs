//! MILESTONE 6: PS/2 keyboard input via IRQ1 -- the first real input
//! device, making the OS interactive for the first time rather than
//! only ever producing output. Uses the well-established `pc-keyboard`
//! crate for scancode-set-1 -> ASCII decoding rather than hand-rolling
//! a translation table.

use alloc::string::String;
use lazy_static::lazy_static;
use pc_keyboard::{DecodedKey, HandleControl, Keyboard, ScancodeSet1, layouts};
use spin::Mutex;
use x86_64::instructions::port::Port;

lazy_static! {
    static ref KEYBOARD: Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>> = Mutex::new(
        Keyboard::new(ScancodeSet1::new(), layouts::Us104Key, HandleControl::Ignore)
    );
}

/// Accumulates every decoded Unicode character received so far --
/// read out by kernel_main after a bounded wait window to verify real
/// keystrokes (sent via QEMU's monitor `sendkey`, standing in for a
/// human at the keyboard) actually got decoded correctly.
pub static TYPED: Mutex<String> = Mutex::new(String::new());

/// Called from the keyboard interrupt handler (interrupts.rs).
pub fn on_interrupt() {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };

    let mut keyboard = KEYBOARD.lock();
    if let Ok(Some(key_event)) = keyboard.add_byte(scancode) {
        if let Some(key) = keyboard.process_keyevent(key_event) {
            if let DecodedKey::Unicode(character) = key {
                TYPED.lock().push(character);
                // MILESTONE 9: 'a'/'d' always stimulate the LIF network,
                // even while also being consumed as shell text (e.g.
                // typing "about") -- a real key press is a real sensor
                // event either way; this is a deliberately simple shared
                // input stream, not a claim that they're cleanly
                // separated input modes.
                crate::neurons::on_key_stimulus(character);
                crate::shell::on_char(character); // MILESTONE 8: shell owns echo/backspace/enter now
            }
        }
    }
}
