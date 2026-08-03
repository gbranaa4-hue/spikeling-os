//! MILESTONE 11: real ATA PIO disk I/O -- persisting Milestone 10's
//! learned synaptic weights across reboots, directly fulfilling a
//! roadmap item Spikeling's OWN original core/README.md left
//! unimplemented: "Weight persistence (save/load trained networks)".
//!
//! Targets the SECONDARY ATA bus (ports 0x170-0x177), master drive --
//! deliberately NOT the primary bus the boot drive lives on, so
//! persistence testing can never risk touching the bootloader/kernel
//! image being booted from. A separate, dedicated disk image is
//! attached there specifically for this in the verification harness.
//!
//! MILESTONE 38: the on-disk format at LBA 0 (still the same,
//! dedicated sector Milestone 18's fs.rs deliberately leaves untouched)
//! is now GENERALIZED and TERNARY-COMPRESSED:
//!   - generalized: was two hardcoded f32s (LeftKey->Motor,
//!     RightKey->Motor); now an arbitrary, NAME-KEYED list of
//!     (from, to, weight) entries, one per synapse that existed in the
//!     network at save time (see network.rs::all_synapse_weights()).
//!     This is a deliberate, disclosed partial generalization: it
//!     persists WEIGHTS for however many synapses exist, but not the
//!     network's TOPOLOGY (neuron thresholds/leaks, which synapses
//!     exist) -- only the two fixed LeftKey/RightKey->Motor synapses
//!     are guaranteed to exist again at the next boot (seeded by
//!     neurons::init() before load_weights() is even consulted), so
//!     those are the only entries a REAL reboot can currently restore.
//!     Extra DSL-added (`addsynapse`) entries round-trip correctly
//!     within the SAME boot session (save then reload without
//!     rebooting) but are honestly reported as unmatched, not silently
//!     dropped, if the network was rebuilt from scratch first.
//!   - ternary-compressed: each weight is packed via ternary.rs's real
//!     port of OBSERVE's pack_ternary/unpack_ternary (10 trits / 2
//!     bytes per weight, vs. 4 bytes for the f32 it replaces -- see
//!     ternary.rs for the full precision/range justification).
//!
//! New magic ("SPK2", not Milestone 11's original "SPKL") so a disk
//! written by pre-M38 code -- or a blank disk -- is safely recognized
//! as NOT this format and load_weights() honestly falls back to
//! `None` (neurons.rs then uses neutral defaults) rather than
//! misreading raw f32 bytes as a ternary-packed header.
//!
//! Sector layout (512 bytes, LBA 0):
//!   [0..4)   magic ("SPK2", LE u32)
//!   [4]      format version (u8, =1)
//!   [5]      entry count (u8)
//!   [6]      trits per weight (u8, self-describing -- =10 currently)
//!   [7]      reserved (=0)
//!   [8..)    `count` entries, each ENTRY_LEN=34 bytes:
//!              [0..16)  `from` name, UTF-8, NUL-padded
//!              [16..32) `to` name, UTF-8, NUL-padded
//!              [32..34) ternary-packed weight (2 bytes)
//!            up to MAX_SYNAPSES=14 entries fit in one 512-byte sector.

use crate::ternary;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use x86_64::instructions::port::Port;

const DATA: u16 = 0x170;
const SECTOR_COUNT: u16 = 0x172;
const LBA_LOW: u16 = 0x173;
const LBA_MID: u16 = 0x174;
const LBA_HIGH: u16 = 0x175;
const DRIVE_HEAD: u16 = 0x176;
const STATUS_OR_COMMAND: u16 = 0x177;

const STATUS_ERR: u8 = 0x01;
const STATUS_DRQ: u8 = 0x08;
const STATUS_BSY: u8 = 0x80;

const CMD_READ_SECTORS: u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_CACHE_FLUSH: u8 = 0xE7;

fn wait_not_busy() {
    let mut status_port: Port<u8> = Port::new(STATUS_OR_COMMAND);
    loop {
        if unsafe { status_port.read() } & STATUS_BSY == 0 {
            break;
        }
    }
}

fn wait_drq() -> Result<(), &'static str> {
    let mut status_port: Port<u8> = Port::new(STATUS_OR_COMMAND);
    loop {
        let status = unsafe { status_port.read() };
        if status & STATUS_ERR != 0 {
            return Err("ATA error bit set");
        }
        if status & STATUS_DRQ != 0 {
            return Ok(());
        }
    }
}

fn select_and_setup(lba: u32) {
    unsafe {
        Port::<u8>::new(DRIVE_HEAD).write(0xE0 | (((lba >> 24) & 0x0F) as u8));
        Port::<u8>::new(SECTOR_COUNT).write(1u8);
        Port::<u8>::new(LBA_LOW).write((lba & 0xFF) as u8);
        Port::<u8>::new(LBA_MID).write(((lba >> 8) & 0xFF) as u8);
        Port::<u8>::new(LBA_HIGH).write(((lba >> 16) & 0xFF) as u8);
    }
}

pub fn read_sector(lba: u32, buf: &mut [u8; 512]) -> Result<(), &'static str> {
    wait_not_busy();
    select_and_setup(lba);
    unsafe {
        Port::<u8>::new(STATUS_OR_COMMAND).write(CMD_READ_SECTORS);
    }
    wait_not_busy();
    wait_drq()?;
    let mut data_port: Port<u16> = Port::new(DATA);
    for chunk in buf.chunks_exact_mut(2) {
        let word = unsafe { data_port.read() };
        chunk[0] = (word & 0xFF) as u8;
        chunk[1] = (word >> 8) as u8;
    }
    Ok(())
}

pub fn write_sector(lba: u32, buf: &[u8; 512]) -> Result<(), &'static str> {
    wait_not_busy();
    select_and_setup(lba);
    unsafe {
        Port::<u8>::new(STATUS_OR_COMMAND).write(CMD_WRITE_SECTORS);
    }
    wait_not_busy();
    wait_drq()?;
    let mut data_port: Port<u16> = Port::new(DATA);
    for chunk in buf.chunks_exact(2) {
        let word = (chunk[0] as u16) | ((chunk[1] as u16) << 8);
        unsafe {
            data_port.write(word);
        }
    }
    unsafe {
        Port::<u8>::new(STATUS_OR_COMMAND).write(CMD_CACHE_FLUSH);
    }
    wait_not_busy();
    Ok(())
}

const MAGIC: u32 = 0x53504B32; // "SPK2" -- MILESTONE 38's ternary, name-keyed format (see module doc)
const PERSIST_LBA: u32 = 0;
const FORMAT_VERSION: u8 = 1;
const NAME_LEN: usize = 16;
const ENTRY_LEN: usize = NAME_LEN * 2 + ternary::PACKED_BYTES_PER_WEIGHT; // 34
const HEADER_LEN: usize = 8;
const MAX_SYNAPSES: usize = (512 - HEADER_LEN) / ENTRY_LEN; // 14

fn write_name(buf: &mut [u8; 512], off: usize, name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(NAME_LEN);
    buf[off..off + n].copy_from_slice(&bytes[..n]);
    // any remaining bytes up to NAME_LEN stay 0 (NUL padding) -- buf
    // starts all-zero in save_weights below.
}

fn read_name(buf: &[u8; 512], off: usize) -> String {
    let slice = &buf[off..off + NAME_LEN];
    let end = slice.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
    core::str::from_utf8(&slice[..end]).unwrap_or("").to_string()
}

/// MILESTONE 38: persists WHATEVER synapses currently exist (an
/// arbitrary, name-keyed list -- see network.rs::all_synapse_weights,
/// and the module doc above for the honest scoping decision), each
/// weight ternary-packed via ternary.rs instead of raw f32. Entries
/// beyond MAX_SYNAPSES (14) are dropped with the count clamped, rather
/// than overflowing the sector -- not expected to matter for this
/// kernel's actual DSL-built networks, but a real, disclosed limit
/// rather than an unbounded assumption.
pub fn save_weights(entries: &[(String, String, f32)]) -> Result<(), &'static str> {
    if entries.is_empty() {
        return Err("no synapses to save");
    }
    let count = entries.len().min(MAX_SYNAPSES);
    let mut buf = [0u8; 512];
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4] = FORMAT_VERSION;
    buf[5] = count as u8;
    buf[6] = ternary::TRITS_PER_WEIGHT as u8;
    // buf[7] reserved, stays 0

    let mut off = HEADER_LEN;
    for (from, to, w) in entries.iter().take(count) {
        write_name(&mut buf, off, from);
        write_name(&mut buf, off + NAME_LEN, to);
        let packed = ternary::encode_weight(*w);
        buf[off + NAME_LEN * 2..off + NAME_LEN * 2 + ternary::PACKED_BYTES_PER_WEIGHT].copy_from_slice(&packed);
        off += ENTRY_LEN;
    }
    write_sector(PERSIST_LBA, &buf)
}

/// MILESTONE 38: returns every (from, to, weight) entry found on disk,
/// weights decoded from their ternary-packed form. `None` if the
/// sector doesn't carry this format's magic (blank disk, or a disk
/// written by pre-M38 code) -- the same honest fallback-to-defaults
/// signal Milestone 11 established, now for a variable-length list
/// instead of a fixed pair.
pub fn load_weights() -> Option<Vec<(String, String, f32)>> {
    let mut buf = [0u8; 512];
    read_sector(PERSIST_LBA, &mut buf).ok()?;
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return None;
    }
    let count = (buf[5] as usize).min(MAX_SYNAPSES);
    let mut out = Vec::with_capacity(count);
    let mut off = HEADER_LEN;
    for _ in 0..count {
        let from = read_name(&buf, off);
        let to = read_name(&buf, off + NAME_LEN);
        let w = ternary::decode_weight(&buf[off + NAME_LEN * 2..off + NAME_LEN * 2 + ternary::PACKED_BYTES_PER_WEIGHT]);
        out.push((from, to, w));
        off += ENTRY_LEN;
    }
    Some(out)
}
