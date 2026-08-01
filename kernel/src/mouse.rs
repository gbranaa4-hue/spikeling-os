//! MILESTONE 16: real PS/2 mouse input via IRQ12 -- the second real
//! input device (after Milestone 6's keyboard), completing the basic
//! PC peripheral set this kernel now has real drivers for: keyboard,
//! mouse, speaker, RTC, disk.
//!
//! The PS/2 controller (8042) multiplexes keyboard (IRQ1, port 0x60)
//! and, once enabled, an auxiliary mouse device (IRQ12, same data port
//! 0x60, but reached through different controller commands) -- the
//! mouse must be explicitly enabled via controller commands before it
//! sends anything, unlike the keyboard which is live by default.
//!
//! MILESTONE 29: real mouse-driven drawing on top of Milestone 23's
//! framebuffer primitives -- left-button-held movement draws directly
//! onto the screen. Hooked straight into this file's own on_interrupt()
//! packet handler rather than sampled from the timer tick, so every
//! drawn segment corresponds to an actual decoded hardware movement
//! packet (the real reported path), not an approximation resampled at
//! the timer's unrelated rate. A real right-click is the exit back to
//! the shell, edge-detected on the same packet stream.

use spin::Mutex;
use x86_64::instructions::port::Port;

const PS2_DATA: u16 = 0x60;
const PS2_STATUS_OR_COMMAND: u16 = 0x64;

fn wait_write_ready() {
    let mut status: Port<u8> = Port::new(PS2_STATUS_OR_COMMAND);
    while unsafe { status.read() } & 0x02 != 0 {}
}

fn wait_read_ready() {
    let mut status: Port<u8> = Port::new(PS2_STATUS_OR_COMMAND);
    while unsafe { status.read() } & 0x01 == 0 {}
}

fn controller_write(cmd: u8) {
    wait_write_ready();
    unsafe { Port::<u8>::new(PS2_STATUS_OR_COMMAND).write(cmd) };
}

fn data_write(byte: u8) {
    wait_write_ready();
    unsafe { Port::<u8>::new(PS2_DATA).write(byte) };
}

fn data_read() -> u8 {
    wait_read_ready();
    unsafe { Port::<u8>::new(PS2_DATA).read() }
}

/// Enables the PS/2 controller's second (auxiliary/mouse) port and
/// tells the mouse itself to start streaming movement packets --
/// standard PS/2 mouse init sequence.
pub fn init() {
    controller_write(0xA8); // enable auxiliary (mouse) device

    controller_write(0x20); // read controller config byte
    let mut config = data_read();
    config |= 0x02; // enable IRQ12 (bit 1)
    config &= !0x20; // clear mouse clock disable bit (bit 5)
    controller_write(0x60); // write controller config byte
    data_write(config);

    // 0xD4 = "next byte goes to the mouse, not the keyboard"
    controller_write(0xD4);
    data_write(0xF6); // "set defaults"
    let _ = data_read(); // ACK (0xFA)

    controller_write(0xD4);
    data_write(0xF4); // "enable data reporting" -- streaming starts after this
    let _ = data_read(); // ACK
}

#[derive(Default, Clone, Copy)]
pub struct MouseState {
    pub x: i32,
    pub y: i32,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    pub packet_count: u32,
}

static MOUSE: Mutex<MouseState> = Mutex::new(MouseState {
    x: 0,
    y: 0,
    left: false,
    right: false,
    middle: false,
    packet_count: 0,
});

// PS/2 mouse packets are 3 bytes: [status, dx, dy]. Byte 0's bit 3 is
// always 1 -- a real, standard way to detect and resync to packet
// boundaries if a byte is ever dropped or the stream starts mid-packet.
static PACKET: Mutex<[u8; 3]> = Mutex::new([0; 3]);
static PACKET_INDEX: Mutex<usize> = Mutex::new(0);

pub fn on_interrupt() {
    let byte = data_read();

    let mut idx = PACKET_INDEX.lock();
    if *idx == 0 && byte & 0x08 == 0 {
        return; // not a valid first byte -- stay resynced, drop it
    }

    let mut packet = PACKET.lock();
    packet[*idx] = byte;
    *idx += 1;

    if *idx == 3 {
        *idx = 0;
        let status = packet[0];
        let dx_raw = packet[1] as i32;
        let dy_raw = packet[2] as i32;
        // sign bits live in the status byte, not the movement bytes themselves
        let dx = if status & 0x10 != 0 { dx_raw - 256 } else { dx_raw };
        let dy = if status & 0x20 != 0 { dy_raw - 256 } else { dy_raw };

        let (x, y, left, right);
        {
            let mut mouse = MOUSE.lock();
            mouse.x += dx;
            mouse.y -= dy; // PS/2's y-axis convention is inverted vs typical screen coordinates
            mouse.left = status & 0x01 != 0;
            mouse.right = status & 0x02 != 0;
            mouse.middle = status & 0x04 != 0;
            mouse.packet_count += 1;
            x = mouse.x;
            y = mouse.y;
            left = mouse.left;
            right = mouse.right;
        } // MOUSE lock released before touching CONSOLE's, below

        handle_draw_mode(x, y, left, right);
    }
}

pub fn state() -> MouseState {
    *MOUSE.lock()
}

// MILESTONE 29: drawing-mode state, separate from MouseState above --
// x/y there stay the raw unbounded relative-movement accumulator the
// `mouse` shell command already reports; drawing needs those clamped to
// real on-screen coordinates, which is local to this feature only.
static DRAWING_ENABLED: Mutex<bool> = Mutex::new(false);
static DRAW_LAST: Mutex<Option<(usize, usize)>> = Mutex::new(None);
static PREV_RIGHT: Mutex<bool> = Mutex::new(false);

pub fn enter_draw_mode() {
    *DRAWING_ENABLED.lock() = true;
    *DRAW_LAST.lock() = None;
    *PREV_RIGHT.lock() = false;
}

pub fn exit_draw_mode() {
    *DRAWING_ENABLED.lock() = false;
    *DRAW_LAST.lock() = None;
}

pub fn drawing_active() -> bool {
    *DRAWING_ENABLED.lock()
}

/// MILESTONE 29: called for every decoded packet while drawing mode is
/// on. Right-click exits back to the shell (edge-detected via
/// PREV_RIGHT so it fires once per click, not once per packet for as
/// long as the button stays held). Otherwise, holding left draws a real
/// line from the last clamped position to the current one -- or a
/// single dot if this is the first packet since the button went down,
/// so a fresh click doesn't snap a stray line in from wherever the
/// cursor was last drawn before the button was released.
fn handle_draw_mode(x: i32, y: i32, left: bool, right: bool) {
    if !*DRAWING_ENABLED.lock() {
        return;
    }

    let mut prev_right = PREV_RIGHT.lock();
    let right_edge = right && !*prev_right;
    *prev_right = right;
    drop(prev_right);

    if right_edge {
        exit_draw_mode();
        crate::console::write_str("\nexited drawing mode (right-click)\n");
        crate::shell::show_prompt();
        return;
    }

    let Some((sw, sh)) = crate::console::dimensions() else {
        return;
    };
    let cx = x.clamp(0, sw as i32 - 1) as usize;
    let cy = y.clamp(0, sh as i32 - 1) as usize;

    let mut last = DRAW_LAST.lock();
    if left {
        match *last {
            Some((lx, ly)) => crate::console::draw_line(lx, ly, cx, cy),
            None => crate::console::draw_pixel(cx, cy, true),
        }
        *last = Some((cx, cy));
    } else {
        *last = None;
    }
}
