//! # PCI bus enumeration (x86_64)
//!
//! The Peripheral Component Interconnect (PCI) bus is how the kernel discovers
//! the virtual hardware QEMU exposes. On the x86_64 Q35 machine model, VirtIO
//! devices (block, network, console) all present themselves as PCI devices, so
//! before we can drive a virtual disk we must first walk the PCI bus, identify
//! each device by its vendor/device ID, and read where its registers live.
//!
//! ## PCI configuration space
//!
//! Every PCI function has a 256-byte *configuration space* — a small bank of
//! registers describing the device (who made it, what it is, where its memory
//! lives, which interrupt it uses). On x86, the legacy way to reach this space
//! is through two 32-bit I/O ports:
//!
//! - **0xCF8 — CONFIG_ADDRESS**: write the (bus, device, function, offset) you
//!   want to access, encoded as a 32-bit value with the enable bit set.
//! - **0xCFC — CONFIG_DATA**: read or write the 32-bit register selected by
//!   CONFIG_ADDRESS.
//!
//! This "address port + data port" indirection is a classic hardware pattern:
//! you can't address all of config space directly in the tiny 16-bit I/O port
//! space, so you point at a register through one port and transfer it through
//! another.
//!
//! ## Bus topology
//!
//! PCI is organised as up to 256 *buses*, each with up to 32 *device* slots,
//! each device having up to 8 *functions*. A simple device uses only function
//! 0; multi-function devices (bit 7 of the header type register) expose more.
//! QEMU's default Q35 topology is small, so a brute-force scan of bus 0 (and
//! any buses behind bridges, which we don't yet recurse into) finds everything
//! we care about.
//!
//! ## Base Address Registers (BARs)
//!
//! A device's registers (MMIO) or legacy I/O ports are advertised through up to
//! six 32-bit BARs at offsets 0x10–0x24. The low bit distinguishes memory BARs
//! (bit 0 = 0) from I/O BARs (bit 0 = 1). For memory BARs, bits 1–2 encode the
//! width: `0b00` = 32-bit, `0b10` = 64-bit (which consumes two consecutive BAR
//! slots). The size of a BAR region is discovered by writing all-ones and
//! reading back the mask of writable address bits.
//!
//! ## Microkernel note
//!
//! PCI enumeration lives in the kernel because it is part of bootstrapping the
//! trusted hardware path (per the Phase 3 hybrid-microkernel design: the thin
//! VirtIO transport stays in ring 0, while filesystem parsing runs in ring 3).
//! The scan itself is read-mostly and side-effect-free, talking only to the
//! hypervisor's emulated PCI host bridge.

use alloc::vec::Vec;

use crate::arch::x86_64::cpu;
use crate::sync::InterruptMutex;

/// The PCI CONFIG_ADDRESS I/O port. Writing here selects which configuration
/// register the next CONFIG_DATA access will hit.
const CONFIG_ADDRESS: u16 = 0xCF8;

/// The PCI CONFIG_DATA I/O port. Reads/writes transfer the 32-bit register
/// previously selected via CONFIG_ADDRESS.
const CONFIG_DATA: u16 = 0xCFC;

/// Value read back for vendor ID when no device is present in a slot.
///
/// The PCI host bridge returns all-ones (0xFFFF) for the vendor ID of an
/// absent function — there's no device to drive the data lines, so they float
/// high. This is the canonical "nothing here" sentinel.
const INVALID_VENDOR: u16 = 0xFFFF;

/// PCI vendor ID assigned to VirtIO devices (Red Hat / Qumranet).
///
/// All VirtIO devices — block, net, console — share this vendor ID. We use it
/// to pick VirtIO devices out of the full device list during enumeration.
pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;

// --- Configuration space register offsets (within the 256-byte space) ---
//
// Only the fields of the standard (type 0) header we actually consume are
// named here. Offsets are byte offsets; config space is accessed in 32-bit
// units, so reads are aligned down to a 4-byte boundary and the desired
// sub-field is shifted out.

/// Offset 0x00: vendor ID (low 16 bits) and device ID (high 16 bits).
const OFF_VENDOR_DEVICE: u8 = 0x00;
/// Offset 0x08: revision (byte 0), prog-IF (byte 1), subclass (byte 2),
/// class code (byte 3).
const OFF_REVISION_CLASS: u8 = 0x08;
/// Offset 0x0C: cache line size, latency timer, header type (byte 2),
/// BIST (byte 3). Bit 7 of the header type byte flags a multi-function device.
const OFF_HEADER_TYPE: u8 = 0x0C;
/// Offset 0x06: status register (16 bits). Bit 4 = capabilities list present.
const OFF_STATUS: u8 = 0x06;
/// Offset 0x10: the first of six Base Address Registers (BAR0).
const OFF_BAR0: u8 = 0x10;
/// Offset 0x34: capabilities pointer (byte) — offset of the first capability
/// in config space when the status register's capabilities-list bit is set.
const OFF_CAP_PTR: u8 = 0x34;

/// Status register bit 4: the device implements a PCI capabilities list.
const STATUS_CAP_LIST: u16 = 1 << 4;

/// Generic PCI capability ID for a vendor-specific capability (0x09). VirtIO
/// modern devices describe their register layout through vendor capabilities.
pub const CAP_ID_VENDOR: u8 = 0x09;

/// A PCI capability discovered by walking the capabilities list.
#[derive(Clone, Copy, Debug)]
pub struct PciCapability {
    /// Capability ID (e.g. 0x09 for vendor-specific / VirtIO).
    pub id: u8,
    /// Byte offset of this capability within the function's config space.
    /// VirtIO-specific fields are read relative to this offset.
    pub offset: u8,
}

/// Build the 32-bit CONFIG_ADDRESS value for a given location and register.
///
/// Layout (per the PCI spec):
/// ```text
///  31     30        24 23   16 15   11 10    8 7      2 1  0
/// ┌──┬───────────────┬───────┬───────┬───────┬────────┬────┐
/// │EN│   reserved    │  bus  │ slot  │ func  │ offset │ 00 │
/// └──┴───────────────┴───────┴───────┴───────┴────────┴────┘
/// ```
/// Bit 31 (EN) must be set to enable the config access. The low two bits are
/// always zero because config reads are 32-bit aligned.
fn config_address(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    (1 << 31)
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC)
}

/// Read a 32-bit register from PCI configuration space.
///
/// Selects the register via CONFIG_ADDRESS, then reads the 32-bit value from
/// CONFIG_DATA. `offset` is rounded down to a 4-byte boundary by the address
/// encoding.
fn config_read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let addr = config_address(bus, slot, func, offset);
    // SAFETY: 0xCF8/0xCFC are the architecturally fixed PCI config ports on
    // x86. Writing an address then reading the data port has no side effect
    // beyond returning the selected register — it cannot corrupt state.
    unsafe {
        cpu::outl(CONFIG_ADDRESS, addr);
        cpu::inl(CONFIG_DATA)
    }
}

/// Write a 32-bit register to PCI configuration space.
///
/// Used during BAR sizing (write all-ones, read back the size mask, then
/// restore the original value). The original value must always be restored —
/// leaving the sizing pattern in a BAR would mis-program the device's MMIO
/// window.
fn config_write32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let addr = config_address(bus, slot, func, offset);
    // SAFETY: as in config_read32, these are the fixed PCI config ports.
    // Writes only take effect on the selected register; callers restore any
    // register they temporarily overwrite for BAR sizing.
    unsafe {
        cpu::outl(CONFIG_ADDRESS, addr);
        cpu::outl(CONFIG_DATA, value);
    }
}

/// A decoded Base Address Register: where a device's MMIO (or I/O) window lives.
#[derive(Clone, Copy, Debug)]
pub struct Bar {
    /// The base address of the region. For memory BARs this is a physical
    /// address (reach it through the HHDM); for I/O BARs it's an I/O port base.
    pub address: u64,
    /// The size of the region in bytes, discovered via the write-ones/read-back
    /// sizing dance. Zero means the BAR is unimplemented.
    pub size: u64,
    /// True if this is a memory-mapped (MMIO) BAR, false if it's an I/O-port BAR.
    pub is_memory: bool,
    /// True if this is a 64-bit memory BAR (which consumed the following BAR
    /// slot for its high 32 bits).
    pub is_64bit: bool,
}

/// A discovered PCI function and the fields we care about.
///
/// One of these is recorded per present function during enumeration. It is a
/// flat snapshot — re-reading config space later would yield the same values
/// for our static QEMU topology.
#[derive(Clone, Debug)]
pub struct PciDevice {
    /// Bus number (0–255). QEMU's default devices all sit on bus 0.
    pub bus: u8,
    /// Device/slot number (0–31).
    pub slot: u8,
    /// Function number (0–7).
    pub func: u8,
    /// Vendor ID (e.g. 0x1AF4 for VirtIO).
    pub vendor_id: u16,
    /// Device ID — interpreted relative to the vendor (e.g. VirtIO block).
    pub device_id: u16,
    /// Class code (high-level category, e.g. 0x01 = mass storage).
    pub class: u8,
    /// Subclass (refines the class, e.g. 0x00 = SCSI under mass storage).
    pub subclass: u8,
    /// Decoded Base Address Registers (only implemented ones are populated;
    /// the high half of a 64-bit BAR is folded into the low BAR's entry).
    pub bars: Vec<Bar>,
}

impl PciDevice {
    /// Read a 32-bit register from this function's config space at `offset`.
    ///
    /// `offset` is rounded down to a 4-byte boundary by the address encoding,
    /// so callers wanting a sub-word field must shift/mask the result.
    pub fn read_config_u32(&self, offset: u8) -> u32 {
        config_read32(self.bus, self.slot, self.func, offset)
    }

    /// Read a 16-bit field from this function's config space at `offset`.
    pub fn read_config_u16(&self, offset: u8) -> u16 {
        let aligned = offset & 0xFC;
        let shift = (offset & 0x3) * 8;
        (config_read32(self.bus, self.slot, self.func, aligned) >> shift) as u16
    }

    /// Read an 8-bit field from this function's config space at `offset`.
    pub fn read_config_u8(&self, offset: u8) -> u8 {
        let aligned = offset & 0xFC;
        let shift = (offset & 0x3) * 8;
        (config_read32(self.bus, self.slot, self.func, aligned) >> shift) as u8
    }

    /// Walk the PCI capabilities list and return every capability found.
    ///
    /// Returns an empty list if the device declares no capabilities (status
    /// register bit 4 clear). Each capability is a node in a singly linked list
    /// threaded through config space: a 1-byte ID, a 1-byte "next" pointer, and
    /// capability-specific data. The walk is bounded to 48 hops as a safety net
    /// against a malformed (cyclic) list.
    pub fn capabilities(&self) -> Vec<PciCapability> {
        let mut caps = Vec::new();

        // Only valid if the device advertises a capabilities list.
        if self.read_config_u16(OFF_STATUS) & STATUS_CAP_LIST == 0 {
            return caps;
        }

        // The capability pointer's low two bits are reserved — mask them off.
        let mut ptr = self.read_config_u8(OFF_CAP_PTR) & 0xFC;
        let mut guard = 0;
        while ptr != 0 && guard < 48 {
            let id = self.read_config_u8(ptr);
            let next = self.read_config_u8(ptr + 1);
            caps.push(PciCapability { id, offset: ptr });
            ptr = next & 0xFC;
            guard += 1;
        }
        caps
    }

    /// Read and decode BAR `index` (0–5) by its raw slot, regardless of how the
    /// folded `bars` list is laid out.
    ///
    /// VirtIO capabilities reference a BAR by its raw index, so the transport
    /// layer needs to resolve a raw slot to its base address even when an
    /// earlier 64-bit BAR consumed two slots. Returns `None` for an
    /// unimplemented BAR.
    pub fn bar(&self, index: u8) -> Option<Bar> {
        if index >= 6 {
            return None;
        }
        self.read_bar(index)
    }

    /// Read this function's BAR `index` (0–5) and decode it.
    ///
    /// Returns `None` for an unimplemented BAR (reads back as zero after
    /// sizing). For a 64-bit memory BAR, reads the following BAR slot to
    /// assemble the full 64-bit base; the caller is responsible for skipping
    /// that consumed slot.
    fn read_bar(&self, index: u8) -> Option<Bar> {
        let offset = OFF_BAR0 + index * 4;
        let original = config_read32(self.bus, self.slot, self.func, offset);

        // An all-zero BAR is unimplemented.
        if original == 0 {
            return None;
        }

        // Bit 0: 0 = memory BAR, 1 = I/O BAR.
        let is_memory = original & 0x1 == 0;
        // For memory BARs, bits 1-2 give the type: 0b10 (=2) means 64-bit.
        let is_64bit = is_memory && ((original >> 1) & 0x3) == 0x2;

        // --- Size discovery ---
        //
        // Write all-ones to the BAR; the device clears the bits it doesn't
        // decode (the low address bits and any reserved bits). The size is
        // then (~mask + 1) over the writable address bits. We must mask off
        // the low flag bits before inverting.
        config_write32(self.bus, self.slot, self.func, offset, 0xFFFF_FFFF);
        let size_low = config_read32(self.bus, self.slot, self.func, offset);
        // Restore the original value immediately — never leave the sizing
        // pattern in place.
        config_write32(self.bus, self.slot, self.func, offset, original);

        // Mask off the low flag bits: 4 bits for memory BARs (bit0 type,
        // bits1-2 width, bit3 prefetchable), 2 bits for I/O BARs.
        let addr_mask: u32 = if is_memory { !0xF } else { !0x3 };

        if is_64bit {
            // The high 32 bits live in the next BAR slot. Read and size it too.
            let hi_offset = offset + 4;
            let original_hi = config_read32(self.bus, self.slot, self.func, hi_offset);
            config_write32(self.bus, self.slot, self.func, hi_offset, 0xFFFF_FFFF);
            let size_high = config_read32(self.bus, self.slot, self.func, hi_offset);
            config_write32(self.bus, self.slot, self.func, hi_offset, original_hi);

            let address =
                ((original_hi as u64) << 32) | ((original & addr_mask) as u64);

            // Combine the 64-bit size mask, invert, add one.
            let size_mask =
                ((size_high as u64) << 32) | ((size_low & addr_mask) as u64);
            let size = (!size_mask).wrapping_add(1);

            Some(Bar { address, size, is_memory, is_64bit })
        } else {
            let address = (original & addr_mask) as u64;
            let size_mask = (size_low & addr_mask) as u64;
            // For a 32-bit BAR the unused high bits of the mask are zero, so
            // sign-extend the inversion into 32 bits only.
            let size = ((!size_mask) as u32).wrapping_add(1) as u64;

            Some(Bar { address, size, is_memory, is_64bit })
        }
    }

    /// Read and decode all six BARs, folding 64-bit BAR pairs into one entry.
    fn read_all_bars(&mut self) {
        let mut index = 0u8;
        while index < 6 {
            if let Some(bar) = self.read_bar(index) {
                let consumed_two = bar.is_64bit;
                self.bars.push(bar);
                // A 64-bit BAR occupies two slots; skip the high half.
                index += if consumed_two { 2 } else { 1 };
            } else {
                index += 1;
            }
        }
    }
}

/// Global registry of discovered PCI devices, populated by `scan()`.
///
/// Stored behind an `InterruptMutex` so later driver-binding code (and shell
/// commands) can read the device list from any context. Populated once at boot
/// and treated as read-only thereafter.
static DEVICES: InterruptMutex<Vec<PciDevice>> = InterruptMutex::new(Vec::new());

/// Probe a single (bus, slot, func) location and, if present, decode it.
///
/// Returns `None` if the slot is empty (vendor ID reads back as 0xFFFF).
fn probe_function(bus: u8, slot: u8, func: u8) -> Option<PciDevice> {
    let vendor_device = config_read32(bus, slot, func, OFF_VENDOR_DEVICE);
    let vendor_id = (vendor_device & 0xFFFF) as u16;
    if vendor_id == INVALID_VENDOR {
        return None;
    }
    let device_id = (vendor_device >> 16) as u16;

    let revision_class = config_read32(bus, slot, func, OFF_REVISION_CLASS);
    // Byte 3 = class, byte 2 = subclass.
    let class = (revision_class >> 24) as u8;
    let subclass = (revision_class >> 16) as u8;

    let mut device = PciDevice {
        bus,
        slot,
        func,
        vendor_id,
        device_id,
        class,
        subclass,
        bars: Vec::new(),
    };
    device.read_all_bars();
    Some(device)
}

/// Read the header-type byte for a (bus, slot) at function 0.
///
/// Bit 7 indicates a multi-function device — if clear, only function 0 exists
/// and we can skip probing functions 1–7.
fn header_type(bus: u8, slot: u8) -> u8 {
    let value = config_read32(bus, slot, 0, OFF_HEADER_TYPE);
    // Header type is byte 2 of this register.
    (value >> 16) as u8
}

/// Enumerate the PCI bus and populate the global device registry.
///
/// Performs a brute-force scan of bus 0: every slot's function 0 is probed,
/// and if a device declares itself multi-function (header type bit 7), the
/// remaining functions are probed too. This is sufficient to find QEMU's
/// default Q35 devices and any VirtIO devices regardless of which slot the
/// hypervisor assigned them.
///
/// Must be called after the heap is initialised (it allocates `Vec`s). Safe to
/// call once during boot; the discovered devices are cached in `DEVICES`.
pub fn scan() {
    let mut found: Vec<PciDevice> = Vec::new();

    // Only bus 0 is scanned directly. PCI-to-PCI bridges expose secondary
    // buses, but QEMU's default Q35 layout places everything we need on bus 0;
    // recursive bridge traversal is deferred until a device demands it.
    let bus = 0u8;
    for slot in 0..32u8 {
        // If function 0 is absent, the whole slot is empty — skip it.
        let func0 = match probe_function(bus, slot, 0) {
            Some(dev) => dev,
            None => continue,
        };

        // Determine whether this is a multi-function device before pushing,
        // so we know whether to probe functions 1-7.
        let multifunction = header_type(bus, slot) & 0x80 != 0;
        found.push(func0);

        if multifunction {
            for func in 1..8u8 {
                if let Some(dev) = probe_function(bus, slot, func) {
                    found.push(dev);
                }
            }
        }
    }

    print_devices(&found);
    *DEVICES.lock() = found;
}

/// Print every discovered device to the serial console for diagnostics.
///
/// Format: `bus:slot.func  vendor:device  class:subclass  [virtio]` followed by
/// each implemented BAR's base/size. Mirrors the style of `lspci` so output is
/// recognisable.
fn print_devices(devices: &[PciDevice]) {
    use crate::println;

    println!();
    println!("PCI bus scan: {} device(s) found", devices.len());
    for dev in devices {
        let tag = if dev.vendor_id == VIRTIO_VENDOR_ID {
            " [virtio]"
        } else {
            ""
        };
        println!(
            "  {:02x}:{:02x}.{}  {:04x}:{:04x}  class {:02x}:{:02x}{}",
            dev.bus, dev.slot, dev.func, dev.vendor_id, dev.device_id, dev.class,
            dev.subclass, tag,
        );
        for (i, bar) in dev.bars.iter().enumerate() {
            let kind = if !bar.is_memory {
                "io"
            } else if bar.is_64bit {
                "mem64"
            } else {
                "mem32"
            };
            println!(
                "      bar{}: {} base {:#x} size {:#x}",
                i, kind, bar.address, bar.size,
            );
        }
    }
}

/// Return a clone of all discovered PCI devices.
///
/// Used by driver-binding code and shell diagnostics. Returns a snapshot copy
/// so callers don't hold the registry lock.
pub fn devices() -> Vec<PciDevice> {
    DEVICES.lock().clone()
}

/// Return all discovered devices matching the given vendor ID.
///
/// The primary use is finding VirtIO devices (`VIRTIO_VENDOR_ID`) to bind the
/// block driver to in later sub-phases.
pub fn devices_by_vendor(vendor_id: u16) -> Vec<PciDevice> {
    DEVICES
        .lock()
        .iter()
        .filter(|d| d.vendor_id == vendor_id)
        .cloned()
        .collect()
}

/// Number of PCI devices discovered during the scan.
pub fn device_count() -> usize {
    DEVICES.lock().len()
}
