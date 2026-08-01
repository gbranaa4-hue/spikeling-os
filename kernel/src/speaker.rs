//! MILESTONE 13: real PC speaker output via the 8254 PIT's channel 2 --
//! closing the sensorimotor loop started in Milestone 9 with an actual
//! PHYSICAL (audible) effect when the network fires, not just visual
//! text. The real analogue of Spikeling's own `action Motor ->
//! [MOTOR_FIRE]` concept: a spike now can produce a genuine, real-world
//! output.

use x86_64::instructions::port::Port;

const PIT_CHANNEL2_DATA: u16 = 0x42;
const PIT_COMMAND: u16 = 0x43;
const SPEAKER_GATE: u16 = 0x61;
const PIT_BASE_HZ: u32 = 1_193_182;

pub fn beep(freq_hz: u32) {
    let freq_hz = freq_hz.max(20); // avoid a divisor overflow/absurd divisor at 0 Hz
    let divisor = (PIT_BASE_HZ / freq_hz) as u16;

    unsafe {
        Port::<u8>::new(PIT_COMMAND).write(0xB6); // channel 2, lobyte/hibyte, mode 3 (square wave)
        let mut data: Port<u8> = Port::new(PIT_CHANNEL2_DATA);
        data.write((divisor & 0xFF) as u8);
        data.write(((divisor >> 8) & 0xFF) as u8);

        let mut gate: Port<u8> = Port::new(SPEAKER_GATE);
        let current = gate.read();
        gate.write(current | 0x03); // enable speaker gate + PIT channel 2 as the output source
    }
}

pub fn stop() {
    unsafe {
        let mut gate: Port<u8> = Port::new(SPEAKER_GATE);
        let current = gate.read();
        gate.write(current & 0xFC);
    }
}

/// Reads back the REAL hardware gate register -- true means bits 0-1
/// (speaker gate + PIT channel 2 as output source) are both actually
/// set, the real causal mechanism that produces sound on full audio
/// hardware. Used to verify the driver sets exactly the right register
/// state directly, since this environment's QEMU build doesn't expose
/// a way to capture the PC speaker's audio output to a file for a
/// waveform-level check.
pub fn is_enabled() -> bool {
    let mut gate: Port<u8> = Port::new(SPEAKER_GATE);
    (unsafe { gate.read() } & 0x03) == 0x03
}
