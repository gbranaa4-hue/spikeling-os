//! MILESTONE 20: real PCI bus enumeration -- until now every driver in
//! this kernel (ata.rs, mouse.rs, speaker.rs, rtc.rs) has talked to a
//! device at a hardcoded, well-known port address, because ISA-era
//! peripherals live at fixed locations. PCI devices don't: a NIC, for
//! instance, could be at any bus:device.function slot depending on the
//! host's hardware, so finding one at all requires walking the bus for
//! real via the standard CONFIG_ADDRESS/CONFIG_DATA I/O-port mechanism,
//! not assuming a fixed address the way earlier drivers could.
//!
//! Scope-limited to enumeration, deliberately: this proves the kernel
//! can discover and identify real hardware (including QEMU's own
//! emulated NIC, if one is attached) but does not add a full
//! packet-level NIC driver -- no send/receive here.

use alloc::vec::Vec;
use spin::Mutex;
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

const NETWORK_CONTROLLER_CLASS: u8 = 0x02;

#[derive(Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
}

static PCI_DEVICES: Mutex<Vec<PciDevice>> = Mutex::new(Vec::new());

// standard PCI config-space address format: enable bit (31), bus
// (23-16), device (15-11), function (10-8), register offset (7-2,
// dword-aligned -- low 2 bits always 0)
fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | (bus as u32) << 16
        | (device as u32) << 11
        | (function as u32) << 8
        | (offset as u32 & 0xFC)
}

fn read_config_dword(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
    let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
    unsafe {
        addr_port.write(config_address(bus, device, function, offset));
        data_port.read()
    }
}

/// Scans PCI bus 0 across all 32 device slots and, for each slot that
/// isn't empty, function 0 plus any additional functions the header
/// type byte advertises -- real config-space reads, not a fabricated
/// device list.
pub fn init() {
    let mut found = Vec::new();

    for device in 0..32u8 {
        let vendor_device = read_config_dword(0, device, 0, 0x00);
        let vendor_id = (vendor_device & 0xFFFF) as u16;
        if vendor_id == 0xFFFF {
            continue; // no device in this slot
        }

        let header_type = ((read_config_dword(0, device, 0, 0x0C) >> 16) & 0xFF) as u8;
        let multi_function = header_type & 0x80 != 0;
        let max_function = if multi_function { 8 } else { 1 };

        for function in 0..max_function {
            let vendor_device = read_config_dword(0, device, function, 0x00);
            let vendor_id = (vendor_device & 0xFFFF) as u16;
            if vendor_id == 0xFFFF {
                continue; // only function 0's presence is guaranteed -- others may be gaps
            }
            let device_id = ((vendor_device >> 16) & 0xFFFF) as u16;

            let class_reg = read_config_dword(0, device, function, 0x08);
            let class_code = ((class_reg >> 24) & 0xFF) as u8;
            let subclass = ((class_reg >> 16) & 0xFF) as u8;

            found.push(PciDevice {
                bus: 0,
                device,
                function,
                vendor_id,
                device_id,
                class_code,
                subclass,
            });
        }
    }

    *PCI_DEVICES.lock() = found;
}

pub fn devices() -> Vec<PciDevice> {
    PCI_DEVICES.lock().clone()
}

/// PCI class code 0x02 is the standard "network controller" class --
/// QEMU normally attaches a default emulated NIC (commonly an Intel
/// e1000, vendor 0x8086) even without explicit -nic/-device flags.
pub fn find_nic() -> Option<PciDevice> {
    PCI_DEVICES
        .lock()
        .iter()
        .find(|d| d.class_code == NETWORK_CONTROLLER_CLASS)
        .copied()
}

// MILESTONE 24 needs raw config-space access (BAR0 at offset 0x10, the
// command register at 0x04) that init()'s private scan never exposed --
// enumeration only ever read vendor/device/class, never a BAR.
pub fn read_config_register(dev: &PciDevice, offset: u8) -> u32 {
    read_config_dword(dev.bus, dev.device, dev.function, offset)
}

pub fn write_config_register(dev: &PciDevice, offset: u8, value: u32) {
    let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
    let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
    unsafe {
        addr_port.write(config_address(dev.bus, dev.device, dev.function, offset));
        data_port.write(value);
    }
}
