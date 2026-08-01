//! MILESTONE 24: real e1000 NIC packet transmission -- Milestone 20's
//! PCI enumeration deliberately stopped at discovery (found QEMU's
//! default Intel 82540EM/e1000 at 00:03.0, vendor 0x8086, class 0x02,
//! but never touched it). This module drives that device for real: maps
//! its BAR0 MMIO register window through the SAME physical_memory_offset
//! mapping memory.rs already relies on, runs the documented Intel 8254x
//! reset/configure sequence, builds a small physically-contiguous
//! transmit descriptor ring, and sends one real Ethernet II frame --
//! proven sent because the hardware itself sets the descriptor's DD
//! (descriptor done) bit after DMA'ing it out, not because a register
//! write merely succeeded.
//!
//! Receive path, interrupts, and multi-packet queuing are all out of
//! scope here, same discipline as pci.rs scoping itself to enumeration
//! only -- this proves ONE real frame can be transmitted and confirmed.
//!
//! MILESTONE 26: real e1000 packet reception. Adds an 8-descriptor RX
//! ring, following the exact same page-aligned/translate_addr pattern
//! Milestone 24 established for TX, programmed into RDBAL/RDBAH/RDLEN/
//! RDH/RDT and RCTL the same honest way init() already programs the TX
//! side.
//!
//! Verification note, reported honestly rather than assumed: the Intel
//! 8254x datasheet's RCTL.LBM field (set to MAC loopback, 01b) is
//! programmed here, but testing against real QEMU showed it has NO
//! effect -- send_test_packet() kept confirming TX DD while recv_packet()
//! never saw an RX DD, across repeated polls. Reading QEMU's own e1000
//! model source (hw/net/e1000.c) confirmed why: its e1000_send_packet()
//! only loops a frame back when `phy_reg[MII_BMCR] & MII_BMCR_LOOPBACK`
//! is set -- a PHY-level register reached over MDIC, not RCTL.LBM, which
//! QEMU's classic e1000 model (unlike its newer e1000e model) simply
//! never reads. So init() ALSO drives the device's real MDIO/MDIC
//! interface (REG_MDIC, offset 0x0020) to write the PHY's standard
//! IEEE 802.3 MII_BMCR register with its loopback bit set -- a genuine
//! hardware mechanism real e1000 silicon implements too (the same one
//! `ethtool -t`'s internal loopback test relies on), not a shortcut
//! around the RX path. With that PHY-level bit set, loopback was
//! confirmed working for real: recv_packet() reads a genuine DD-marked
//! descriptor whose bytes match send_test_packet()'s frame exactly.

use crate::pci::{self, PciDevice};
use alloc::format;
use alloc::string::String;
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{PageTable, PageTableFlags};
use x86_64::{PhysAddr, VirtAddr};

const REG_CTRL: usize = 0x0000;
const REG_STATUS: usize = 0x0008;
const REG_IMC: usize = 0x00D8;
const REG_MDIC: usize = 0x0020;
const REG_RCTL: usize = 0x0100;
const REG_TCTL: usize = 0x0400;
const REG_TIPG: usize = 0x0410;
const REG_RDBAL: usize = 0x2800;
const REG_RDBAH: usize = 0x2804;
const REG_RDLEN: usize = 0x2808;
const REG_RDH: usize = 0x2810;
const REG_RDT: usize = 0x2818;
const REG_TDBAL: usize = 0x3800;
const REG_TDBAH: usize = 0x3804;
const REG_TDLEN: usize = 0x3808;
const REG_TDH: usize = 0x3810;
const REG_TDT: usize = 0x3818;
const REG_RAL0: usize = 0x5400;
const REG_RAH0: usize = 0x5404;

const CTRL_SLU: u32 = 1 << 6; // "set link up" -- needed under QEMU's emulation for the link to report up without real autonegotiation
const CTRL_RST: u32 = 1 << 26;

const STATUS_LU: u32 = 1 << 1;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad short packets -- lets our sub-60-byte test frame stay spec-legal without manual padding
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_COLD_SHIFT: u32 = 12;
const TCTL_RTLC: u32 = 1 << 24;

const TIPG_STANDARD: u32 = 0x0060_200A; // IPGT=10, IPGR1=8, IPGR2=6 -- Intel's documented default inter-packet gap

const CMD_MEM_SPACE_ENABLE: u32 = 1 << 1;
const CMD_BUS_MASTER_ENABLE: u32 = 1 << 2;

const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;
const TX_STATUS_DD: u8 = 1 << 0;

// RCTL bit positions and LBM encoding straight from Intel's e1000_hw.h
// register definitions (the same constants QEMU's own e1000 model and
// the Linux e1000 driver use) -- EN/BAM are single bits, LBM is a 2-bit
// field at bits 7:6 where 01b (only bit 6 set) is MAC loopback, not
// both bits set.
const RCTL_EN: u32 = 1 << 1;
const RCTL_LBM_MAC: u32 = 1 << 6; // real datasheet MAC loopback field -- QEMU's classic e1000 model ignores it (see module doc comment); MDIC/MII_BMCR loopback below is what actually drives verification here
const RCTL_BAM: u32 = 1 << 15; // broadcast accept mode -- our test frame's destination is the broadcast address

const RX_STATUS_DD: u8 = 1 << 0;

// MDIC bit field layout straight from Intel's e1000_regs.h (the same
// constants QEMU's own e1000 model uses to decode MDIC writes) -- DATA
// is the low 16 bits, REG/PHY addresses are shifted fields above that,
// OP_WRITE picks the operation, and READY/ERROR are hardware-set status
// bits the driver must poll rather than assume.
const MDIC_REG_SHIFT: u32 = 16;
const MDIC_PHY_SHIFT: u32 = 21;
const MDIC_OP_WRITE: u32 = 1 << 26;
const MDIC_READY: u32 = 1 << 28;
const MDIC_ERROR: u32 = 1 << 30;

const MII_PHY_ADDR: u32 = 1; // QEMU's e1000 model only answers MDIC for PHY address 1 -- confirmed against its own set_mdic() source
const MII_BMCR_REG: u32 = 0x00; // standard IEEE 802.3 clause 22 Basic Mode Control Register
const MII_BMCR_SPEED1000: u16 = 1 << 6;
const MII_BMCR_FULLDPLX: u16 = 1 << 8;
const MII_BMCR_ANENABLE: u16 = 1 << 12;
const MII_BMCR_LOOPBACK: u16 = 1 << 14; // real IEEE 802.3 PHY loopback bit -- what QEMU's e1000_send_packet() actually checks, not RCTL.LBM

const NUM_TX_DESC: usize = 8;
const TX_PACKET_BUF_LEN: usize = 256;

const NUM_RX_DESC: usize = 8;
const RX_PACKET_BUF_LEN: usize = 2048; // matches RCTL's BSIZE=00b default (2048 bytes) when BSEX=0 -- left unset below

const TEST_PAYLOAD: &[u8] = b"spikeling-os milestone 24 test packet";

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct TxDescriptor {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

const EMPTY_DESC: TxDescriptor = TxDescriptor {
    addr: 0,
    length: 0,
    cso: 0,
    cmd: 0,
    status: 0,
    css: 0,
    special: 0,
};

// Field order and widths per the Intel 8254x software developer's
// manual / the OSDev wiki's e1000 receive descriptor layout -- NOT the
// same layout as TxDescriptor above despite being the same 16 bytes:
// checksum sits where TX has cso+cmd, and errors is its own byte rather
// than being folded into status.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct RxDescriptor {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

const EMPTY_RX_DESC: RxDescriptor = RxDescriptor {
    addr: 0,
    length: 0,
    checksum: 0,
    status: 0,
    errors: 0,
    special: 0,
};

// Single page, aligned so it's exactly one physical frame -- means the
// descriptor ring and every packet buffer are physically contiguous by
// construction (they live inside the same frame), sidestepping the
// question of whether consecutive kernel .bss pages are ever physically
// contiguous. One page-aligned address translated to physical is enough
// to derive every offset inside it.
#[repr(C, align(4096))]
struct DmaRegion {
    descriptors: [TxDescriptor; NUM_TX_DESC],
    buffers: [[u8; TX_PACKET_BUF_LEN]; NUM_TX_DESC],
}

const _: () = assert!(core::mem::size_of::<DmaRegion>() <= 4096);

static mut DMA_REGION: DmaRegion = DmaRegion {
    descriptors: [EMPTY_DESC; NUM_TX_DESC],
    buffers: [[0u8; TX_PACKET_BUF_LEN]; NUM_TX_DESC],
};

// The RX descriptor ring (8 * 16 bytes = 128 bytes) fits comfortably
// alongside a fourth NUM_RX_DESC in a single page just like DmaRegion
// above -- but RX_PACKET_BUF_LEN (2048) makes 8 buffers too big to also
// share that page (16KiB > 4KiB), so each RX packet buffer gets its OWN
// page-aligned home below instead of being packed in here. This struct
// covers ONLY the descriptor ring's contiguity requirement.
#[repr(C, align(4096))]
struct RxDescRegion {
    descriptors: [RxDescriptor; NUM_RX_DESC],
}

const _: () = assert!(core::mem::size_of::<RxDescRegion>() <= 4096);

static mut RX_DESC_REGION: RxDescRegion = RxDescRegion {
    descriptors: [EMPTY_RX_DESC; NUM_RX_DESC],
};

// One full page per RX packet buffer -- wastes (4096 - 2048) bytes per
// buffer, but guarantees each 2048-byte buffer is entirely inside the
// single physical frame translate_addr resolves for it, same
// contiguity argument DmaRegion's own comment makes above, without
// needing 8 buffers to ALSO be mutually contiguous with each other
// (they don't: each RX descriptor carries its own independent addr).
#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct RxBufferPage {
    data: [u8; RX_PACKET_BUF_LEN],
}

const EMPTY_RX_BUFFER_PAGE: RxBufferPage = RxBufferPage {
    data: [0u8; RX_PACKET_BUF_LEN],
};

static mut RX_BUFFERS: [RxBufferPage; NUM_RX_DESC] = [EMPTY_RX_BUFFER_PAGE; NUM_RX_DESC];

struct NicState {
    mmio_base: VirtAddr,
    dma_phys_base: u64,
    mac: [u8; 6],
    mac_valid: bool,
    tx_tail: u16,
    rx_next: u16,
}

static NIC: Mutex<Option<NicState>> = Mutex::new(None);

/// A real received Ethernet II frame's parsed header fields plus a
/// direct comparison against TEST_PAYLOAD -- returned by recv_packet()
/// so the shell can report exactly what hardware actually delivered,
/// not just that "something" arrived.
pub struct ReceivedFrame {
    pub dest_mac: [u8; 6],
    pub src_mac: [u8; 6],
    pub ethertype: u16,
    pub length: usize,
    pub payload_matches_test: bool,
}

unsafe fn reg_read(mmio_base: VirtAddr, offset: usize) -> u32 {
    let ptr = (mmio_base.as_u64() as usize + offset) as *const u32;
    unsafe { core::ptr::read_volatile(ptr) }
}

unsafe fn reg_write(mmio_base: VirtAddr, offset: usize, value: u32) {
    let ptr = (mmio_base.as_u64() as usize + offset) as *mut u32;
    unsafe { core::ptr::write_volatile(ptr, value) }
}

// Drives the device's real MDIO/MDIC interface to write one PHY
// register -- the same indirect mechanism real e1000 silicon uses to
// reach its attached PHY, polled for the hardware's own READY bit
// (not assumed instant) and checked for the hardware's own ERROR bit
// (not assumed successful).
unsafe fn mdic_write(mmio_base: VirtAddr, phy_addr: u32, reg: u32, data: u16) -> Result<(), &'static str> {
    let val = (data as u32) | (reg << MDIC_REG_SHIFT) | (phy_addr << MDIC_PHY_SHIFT) | MDIC_OP_WRITE;
    unsafe { reg_write(mmio_base, REG_MDIC, val) };

    let mut ready = false;
    for _ in 0..100_000u32 {
        let status = unsafe { reg_read(mmio_base, REG_MDIC) };
        if status & MDIC_READY != 0 {
            ready = true;
            break;
        }
    }
    if !ready {
        return Err("MDIC write did not complete -- READY bit never set");
    }

    let status = unsafe { reg_read(mmio_base, REG_MDIC) };
    if status & MDIC_ERROR != 0 {
        return Err("MDIC write reported an error -- hardware rejected the PHY register write");
    }
    Ok(())
}

// Same page-table walk memory.rs's active_level_4_table already uses to
// reach the level-4 table -- extended down to a leaf entry, and made to
// handle huge-page (2MiB/1GiB) intermediate entries honestly instead of
// assuming every kernel mapping is a plain 4KiB page. Read-only: takes
// no &mut, so it can't violate the aliasing contract memory::init()
// documents against the live page tables.
unsafe fn translate_addr(phys_mem_offset: VirtAddr, addr: VirtAddr) -> Option<PhysAddr> {
    let (level_4_frame, _) = Cr3::read();
    let indexes = [addr.p4_index(), addr.p3_index(), addr.p2_index(), addr.p1_index()];
    let mut frame = level_4_frame;

    for (level, &index) in indexes.iter().enumerate() {
        let virt = phys_mem_offset + frame.start_address().as_u64();
        let table: &PageTable = unsafe { &*(virt.as_ptr() as *const PageTable) };
        let entry = &table[index];

        if entry.is_unused() {
            return None;
        }

        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            let page_size: u64 = match level {
                1 => 1 << 30, // P3 entry -> 1GiB page
                2 => 1 << 21, // P2 entry -> 2MiB page
                _ => return None,
            };
            let mask = page_size - 1;
            return Some(PhysAddr::new(entry.addr().as_u64() + (addr.as_u64() & mask)));
        }

        frame = match entry.frame() {
            Ok(f) => f,
            Err(_) => return None,
        };
    }

    Some(frame.start_address() + u64::from(addr.page_offset()))
}

pub fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

pub fn mac_address() -> Option<[u8; 6]> {
    NIC.lock().as_ref().map(|s| s.mac)
}

pub fn mac_is_valid() -> Option<bool> {
    NIC.lock().as_ref().map(|s| s.mac_valid)
}

pub fn link_up() -> Option<bool> {
    let guard = NIC.lock();
    let state = guard.as_ref()?;
    let status = unsafe { reg_read(state.mmio_base, REG_STATUS) };
    Some(status & STATUS_LU != 0)
}

/// Finds the e1000 via pci::find_nic(), maps its BAR0 MMIO window at
/// phys_mem_offset + (BAR0 & !0xF), and runs the real Intel 8254x
/// reset/configure sequence, ending with a live 8-descriptor TX ring
/// AND a live 8-descriptor RX ring (MILESTONE 26) both programmed into
/// hardware. Reports failure honestly at every step (no NIC found,
/// BAR0 not memory-mapped, reset never completes, either DMA region
/// can't be translated to a physical address) rather than assuming
/// success.
pub fn init(phys_mem_offset: VirtAddr) -> Result<(), &'static str> {
    let dev: PciDevice = pci::find_nic().ok_or("no PCI network controller found on bus 0")?;

    if dev.vendor_id != 0x8086 {
        return Err("network controller found but not an Intel (0x8086) device -- this driver is e1000-specific");
    }

    let cmd_status = pci::read_config_register(&dev, 0x04);
    let command = cmd_status & 0xFFFF;
    pci::write_config_register(&dev, 0x04, command | CMD_MEM_SPACE_ENABLE | CMD_BUS_MASTER_ENABLE);

    let bar0 = pci::read_config_register(&dev, 0x10);
    if bar0 & 0x1 != 0 {
        return Err("BAR0 is I/O-space, not memory-mapped -- unexpected for e1000, refusing to guess a register layout");
    }
    let mmio_phys_base = (bar0 & 0xFFFF_FFF0) as u64;
    let mmio_base = phys_mem_offset + mmio_phys_base;

    unsafe {
        let ctrl = reg_read(mmio_base, REG_CTRL);
        reg_write(mmio_base, REG_CTRL, ctrl | CTRL_RST);
    }

    let mut reset_done = false;
    for _ in 0..100_000u32 {
        let ctrl = unsafe { reg_read(mmio_base, REG_CTRL) };
        if ctrl & CTRL_RST == 0 {
            reset_done = true;
            break;
        }
    }
    if !reset_done {
        return Err("device reset did not complete -- CTRL.RST never cleared");
    }

    unsafe {
        reg_write(mmio_base, REG_IMC, 0xFFFF_FFFF); // mask all interrupts -- this driver only polls, never handles a NIC IRQ
        let ctrl = reg_read(mmio_base, REG_CTRL);
        reg_write(mmio_base, REG_CTRL, ctrl | CTRL_SLU);
    }

    // Real hardware auto-loads RAL0/RAH0 from the attached EEPROM at
    // reset -- reading them directly after CTRL.RST clears is the
    // documented way to get the permanent station address without a
    // separate EEPROM (EERD) read.
    let (ral, rah) = unsafe { (reg_read(mmio_base, REG_RAL0), reg_read(mmio_base, REG_RAH0)) };
    let mac = [
        (ral & 0xFF) as u8,
        ((ral >> 8) & 0xFF) as u8,
        ((ral >> 16) & 0xFF) as u8,
        ((ral >> 24) & 0xFF) as u8,
        (rah & 0xFF) as u8,
        ((rah >> 8) & 0xFF) as u8,
    ];
    let mac_valid = rah & (1 << 31) != 0; // RAH0's Address Valid bit -- reported honestly, not assumed

    let dma_virt = VirtAddr::from_ptr(core::ptr::addr_of!(DMA_REGION));
    let dma_phys = unsafe { translate_addr(phys_mem_offset, dma_virt) }
        .ok_or("failed to translate the TX descriptor ring's virtual address to a physical address")?;
    let dma_phys_base = dma_phys.as_u64();

    unsafe {
        reg_write(mmio_base, REG_TDBAL, (dma_phys_base & 0xFFFF_FFFF) as u32);
        reg_write(mmio_base, REG_TDBAH, (dma_phys_base >> 32) as u32);
        reg_write(mmio_base, REG_TDLEN, (NUM_TX_DESC * core::mem::size_of::<TxDescriptor>()) as u32);
        reg_write(mmio_base, REG_TDH, 0);
        reg_write(mmio_base, REG_TDT, 0);

        let tctl = TCTL_EN | TCTL_PSP | TCTL_RTLC | (0x0Fu32 << TCTL_CT_SHIFT) | (0x40u32 << TCTL_COLD_SHIFT);
        reg_write(mmio_base, REG_TCTL, tctl);
        reg_write(mmio_base, REG_TIPG, TIPG_STANDARD);
    }

    // MILESTONE 26: RX ring setup, mirroring the TX setup immediately
    // above -- one translate_addr() call for the descriptor ring's own
    // physical base, then one MORE translate_addr() call per buffer
    // page since (unlike TX's packed buffers) each RX buffer is its own
    // independently-placed page.
    let rx_desc_virt = VirtAddr::from_ptr(core::ptr::addr_of!(RX_DESC_REGION));
    let rx_desc_phys = unsafe { translate_addr(phys_mem_offset, rx_desc_virt) }
        .ok_or("failed to translate the RX descriptor ring's virtual address to a physical address")?;
    let rx_desc_phys_base = rx_desc_phys.as_u64();

    for i in 0..NUM_RX_DESC {
        let buf_virt = VirtAddr::from_ptr(unsafe { core::ptr::addr_of!(RX_BUFFERS[i]) });
        let buf_phys = unsafe { translate_addr(phys_mem_offset, buf_virt) }
            .ok_or("failed to translate an RX packet buffer's virtual address to a physical address")?;
        unsafe {
            let desc_ptr = core::ptr::addr_of_mut!(RX_DESC_REGION.descriptors[i]);
            (*desc_ptr).addr = buf_phys.as_u64();
            (*desc_ptr).length = 0;
            (*desc_ptr).checksum = 0;
            (*desc_ptr).status = 0;
            (*desc_ptr).errors = 0;
            (*desc_ptr).special = 0;
        }
    }

    unsafe {
        reg_write(mmio_base, REG_RDBAL, (rx_desc_phys_base & 0xFFFF_FFFF) as u32);
        reg_write(mmio_base, REG_RDBAH, (rx_desc_phys_base >> 32) as u32);
        reg_write(mmio_base, REG_RDLEN, (NUM_RX_DESC * core::mem::size_of::<RxDescriptor>()) as u32);
        reg_write(mmio_base, REG_RDH, 0);
        // RDT = NUM_RX_DESC - 1, not NUM_RX_DESC -- RDT marks one past
        // the last descriptor available to hardware, so this leaves the
        // ring's very last slot deliberately unused (the same head/tail
        // gap convention TX rings use), trading one of 8 buffers for an
        // unambiguous empty-vs-full ring state.
        reg_write(mmio_base, REG_RDT, (NUM_RX_DESC - 1) as u32);

        // RCTL_LBM_MAC is programmed because it's the real datasheet
        // field for MAC-level loopback -- harmless to set, but testing
        // showed QEMU's classic e1000 model never reads it (see the
        // module doc comment). BAM/EN alone would work identically
        // under QEMU; RCTL_LBM_MAC is kept here because a real 8254x
        // chip DOES honor it, and it costs nothing to also set on real
        // hardware or e1000e-class QEMU models that do implement it.
        let rctl = RCTL_EN | RCTL_BAM | RCTL_LBM_MAC;
        reg_write(mmio_base, REG_RCTL, rctl);
    }

    // MILESTONE 26: the loopback path QEMU's classic e1000 model
    // actually implements -- see the module doc comment for how this
    // was discovered. Sets the real PHY's standard MII_BMCR loopback
    // bit over MDIC, alongside the speed/duplex/autoneg bits the
    // device's own phy_reg_init default already carries, so this write
    // only adds loopback rather than silently changing other PHY state.
    let bmcr = MII_BMCR_SPEED1000 | MII_BMCR_FULLDPLX | MII_BMCR_ANENABLE | MII_BMCR_LOOPBACK;
    unsafe { mdic_write(mmio_base, MII_PHY_ADDR, MII_BMCR_REG, bmcr) }?;

    *NIC.lock() = Some(NicState {
        mmio_base,
        dma_phys_base,
        mac,
        mac_valid,
        tx_tail: 0,
        rx_next: 0,
    });

    Ok(())
}

/// Builds one real broadcast Ethernet II frame carrying TEST_PAYLOAD,
/// places it in the TX ring, advances TDT so the hardware DMA's it out,
/// then polls the descriptor's own status byte for the DD (descriptor
/// done) bit -- the hardware's own confirmation it actually transmitted
/// the frame, not just that the driver wrote TDT. Returns Ok(false)
/// (not an Err) if DD never sets within the timeout, since the frame
/// WAS queued -- that's a real, reportable "sent but unconfirmed"
/// outcome, not a driver error.
pub fn send_test_packet() -> Result<bool, &'static str> {
    let mut guard = NIC.lock();
    let state = guard.as_mut().ok_or("NIC not initialized")?;

    let mut frame = [0u8; 64];
    frame[0..6].copy_from_slice(&[0xFF; 6]); // broadcast destination
    frame[6..12].copy_from_slice(&state.mac); // real source MAC read from RAL0/RAH0
    frame[12] = 0x88;
    frame[13] = 0xB5; // ethertype 0x88B5, "local experimental"
    frame[14..14 + TEST_PAYLOAD.len()].copy_from_slice(TEST_PAYLOAD);
    let frame_len = 14 + TEST_PAYLOAD.len();

    let tail = state.tx_tail as usize;

    let buf_virt = unsafe { core::ptr::addr_of_mut!(DMA_REGION.buffers[tail]) } as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buf_virt, frame_len);
    }

    let buf_phys =
        state.dma_phys_base + (core::mem::offset_of!(DmaRegion, buffers) + tail * TX_PACKET_BUF_LEN) as u64;

    unsafe {
        let desc_ptr = core::ptr::addr_of_mut!(DMA_REGION.descriptors[tail]);
        (*desc_ptr).addr = buf_phys;
        (*desc_ptr).length = frame_len as u16;
        (*desc_ptr).cso = 0;
        (*desc_ptr).cmd = TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS;
        (*desc_ptr).status = 0;
        (*desc_ptr).css = 0;
        (*desc_ptr).special = 0;
    }

    let next_tail = ((tail + 1) % NUM_TX_DESC) as u16;
    unsafe {
        reg_write(state.mmio_base, REG_TDT, next_tail as u32);
    }

    let mut dd_confirmed = false;
    for _ in 0..1_000_000u32 {
        let status = unsafe { core::ptr::addr_of!(DMA_REGION.descriptors[tail]).read().status };
        if status & TX_STATUS_DD != 0 {
            dd_confirmed = true;
            break;
        }
    }

    state.tx_tail = next_tail;
    Ok(dd_confirmed)
}

/// MILESTONE 26: polls the next expected RX descriptor's status byte
/// for the DD (descriptor done) bit -- the hardware's own confirmation
/// a frame actually landed in that buffer via DMA, not merely that RDT
/// was advanced. Bounded the same order of magnitude as
/// send_test_packet's TX poll; returns Ok(None) (not an Err) if nothing
/// arrived in that window, since "no packet yet" is a real, reportable
/// outcome, not a driver failure -- callers are expected to have
/// already called send_test_packet() with the PHY's MII_BMCR loopback
/// bit enabled over MDIC (set unconditionally in init(), see the module
/// doc comment for why that's the mechanism used instead of RCTL.LBM)
/// so the frame the driver just transmitted is what the hardware loops
/// back here, without requiring any real external network round trip.
///
/// On a genuine received descriptor, the errors byte is checked too
/// (not just DD) -- descriptor recycling (clearing status, advancing
/// RDT) happens either way, since a hardware-flagged bad frame still
/// consumed a real ring slot that must be given back.
pub fn recv_packet() -> Result<Option<ReceivedFrame>, &'static str> {
    let mut guard = NIC.lock();
    let state = guard.as_mut().ok_or("NIC not initialized")?;

    let idx = state.rx_next as usize;

    let mut dd_seen = false;
    for _ in 0..1_000_000u32 {
        let status = unsafe { core::ptr::addr_of!(RX_DESC_REGION.descriptors[idx]).read().status };
        if status & RX_STATUS_DD != 0 {
            dd_seen = true;
            break;
        }
    }

    if !dd_seen {
        return Ok(None);
    }

    let (length, errors) = unsafe {
        let desc = core::ptr::addr_of!(RX_DESC_REGION.descriptors[idx]).read();
        (desc.length as usize, desc.errors)
    };

    let copy_len = length.min(RX_PACKET_BUF_LEN);
    let mut frame = [0u8; RX_PACKET_BUF_LEN];
    unsafe {
        let buf_ptr = core::ptr::addr_of!(RX_BUFFERS[idx].data) as *const u8;
        core::ptr::copy_nonoverlapping(buf_ptr, frame.as_mut_ptr(), copy_len);
    }

    unsafe {
        let desc_ptr = core::ptr::addr_of_mut!(RX_DESC_REGION.descriptors[idx]);
        (*desc_ptr).status = 0;
        (*desc_ptr).length = 0;
        (*desc_ptr).errors = 0;
    }

    unsafe {
        reg_write(state.mmio_base, REG_RDT, idx as u32);
    }
    state.rx_next = ((idx + 1) % NUM_RX_DESC) as u16;

    if errors != 0 {
        return Err("hardware flagged the received frame's RX descriptor errors byte non-zero");
    }

    if copy_len < 14 {
        return Err("received frame shorter than a 14-byte Ethernet header -- refusing to parse garbage");
    }

    let dest_mac = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
    let src_mac = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
    let ethertype = ((frame[12] as u16) << 8) | frame[13] as u16;
    let payload_matches_test =
        copy_len >= 14 + TEST_PAYLOAD.len() && &frame[14..14 + TEST_PAYLOAD.len()] == TEST_PAYLOAD;

    Ok(Some(ReceivedFrame {
        dest_mac,
        src_mac,
        ethertype,
        length: copy_len,
        payload_matches_test,
    }))
}
