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

const MAGIC: u32 = 0x53504B4C; // "SPKL" -- distinguishes "we wrote this" from uninitialized/garbage disk content
const PERSIST_LBA: u32 = 0;

pub fn save_weights(left: f32, right: f32) -> Result<(), &'static str> {
    let mut buf = [0u8; 512];
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4..8].copy_from_slice(&left.to_le_bytes());
    buf[8..12].copy_from_slice(&right.to_le_bytes());
    write_sector(PERSIST_LBA, &buf)
}

pub fn load_weights() -> Option<(f32, f32)> {
    let mut buf = [0u8; 512];
    read_sector(PERSIST_LBA, &mut buf).ok()?;
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return None;
    }
    let left = f32::from_le_bytes(buf[4..8].try_into().unwrap());
    let right = f32::from_le_bytes(buf[8..12].try_into().unwrap());
    Some((left, right))
}
