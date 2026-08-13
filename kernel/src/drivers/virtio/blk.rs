//! # VirtIO block device driver
//!
//! Drives a VirtIO block device (QEMU's virtual disk) on top of the generic
//! VirtIO transport in the parent module. Implements the kernel's
//! [`BlockDevice`](crate::drivers::block::BlockDevice) trait so filesystem
//! servers — via the block server — see a plain "read/write these sectors"
//! interface.
//!
//! ## The request protocol
//!
//! Every block operation is a 3-part descriptor chain on the virtqueue:
//!
//! 1. **Header** (16 bytes, device-readable): request type (read/write/flush),
//!    a reserved word, and the starting 512-byte sector.
//! 2. **Data** (device-readable for writes, device-writable for reads): the
//!    payload.
//! 3. **Status** (1 byte, device-writable): the device's result code.
//!
//! We submit the chain, ring the doorbell, poll the used ring until the device
//! returns it, then read the status byte.
//!
//! ## Bounce buffering
//!
//! The device DMAs to/from *physical* addresses and needs each buffer to be
//! physically contiguous. A caller's `&[u8]` lives in the kernel heap, which is
//! not guaranteed contiguous across frames. So we route all data through a
//! dedicated, physically-contiguous **bounce buffer** and copy in/out. Requests
//! larger than the bounce buffer are split into chunks. This trades a memcpy for
//! correctness and simplicity — exactly the right call for a Phase 3 driver.

use alloc::boxed::Box;
use core::ptr::write_volatile;

use crate::drivers::block::{BlockDevice, BlockError};
use crate::drivers::pci::PciDevice;
use crate::mm::addr::PhysAddr;
use crate::mm::frame;
use crate::sync::InterruptMutex;

use super::{
    mmio_read_u32, VirtioError, VirtioTransport, Virtqueue, VirtqDesc, VIRTQ_DESC_F_NEXT,
    VIRTQ_DESC_F_WRITE,
};

/// VirtIO block sector size. The VirtIO-blk protocol always addresses storage
/// in 512-byte units regardless of the backing device's logical block size.
const SECTOR_SIZE: usize = 512;

/// Request type: read from device into memory (`VIRTIO_BLK_T_IN`).
const VIRTIO_BLK_T_IN: u32 = 0;
/// Request type: write memory to device (`VIRTIO_BLK_T_OUT`).
const VIRTIO_BLK_T_OUT: u32 = 1;
/// Request type: flush the device write cache (`VIRTIO_BLK_T_FLUSH`).
const VIRTIO_BLK_T_FLUSH: u32 = 4;

/// Device status: the request completed successfully (`VIRTIO_BLK_S_OK`).
const VIRTIO_BLK_S_OK: u8 = 0;

/// Number of frames backing the bounce buffer (16 × 4 KiB = 64 KiB = 128
/// sectors). Bounds the per-request transfer size; larger reads/writes loop.
const BOUNCE_FRAMES: usize = 16;

/// Descriptor indices for the fixed 3-entry request chain.
const DESC_HEADER: u16 = 0;
const DESC_DATA: u16 = 1;
const DESC_STATUS: u16 = 2;

/// Per-device mutable state, guarded by a mutex so `&self` trait methods can
/// drive the single hardware queue safely.
struct Inner {
    /// The VirtIO transport (kept alive; holds the MMIO mappings).
    #[allow(dead_code)]
    transport: VirtioTransport,
    /// The request virtqueue (queue 0).
    queue: Virtqueue,
    /// Physical address of the 16-byte request header DMA buffer.
    header_phys: PhysAddr,
    /// Physical address of the 1-byte status DMA buffer.
    status_phys: PhysAddr,
    /// Physical base of the bounce buffer for payload data.
    bounce_phys: PhysAddr,
}

/// A VirtIO block device implementing [`BlockDevice`].
pub struct VirtioBlk {
    /// Mutable hardware state (transport, queue, DMA buffers).
    inner: InterruptMutex<Inner>,
    /// Device capacity in 512-byte sectors (read from device config, immutable).
    capacity_sectors: u64,
}

impl VirtioBlk {
    /// Maximum sectors transferable in one request (bounce buffer capacity).
    const BOUNCE_SECTORS: usize = BOUNCE_FRAMES * 4096 / SECTOR_SIZE;

    /// Initialise a VirtIO block device from a discovered PCI function.
    ///
    /// Brings the transport up (handshake + feature negotiation + queue 0),
    /// reads the device capacity, allocates the header/status/bounce DMA
    /// buffers, and signals DRIVER_OK. The returned driver is ready for I/O.
    pub fn init_from_pci(dev: &PciDevice) -> Result<Self, VirtioError> {
        let transport = VirtioTransport::init(dev)?;
        // We need no device-specific feature bits for basic R/W — just the
        // mandatory VERSION_1 negotiated by the transport.
        transport.negotiate_features(0)?;
        let queue = transport.setup_queue(0)?;

        // Read capacity (u64, in 512-byte sectors) from device config offset 0.
        let cfg = transport
            .device_config()
            .ok_or(VirtioError::MissingCapability)?;
        // SAFETY: device config is a mapped MMIO region; capacity is at offset 0.
        let cap_low = unsafe { mmio_read_u32(cfg, 0) } as u64;
        let cap_high = unsafe { mmio_read_u32(cfg, 4) } as u64;
        let capacity_sectors = (cap_high << 32) | cap_low;

        // Allocate DMA buffers. The header (16 B) and status (1 B) share one
        // frame; the bounce buffer gets its own contiguous run.
        let hs_frame = frame::allocate_frame().ok_or(VirtioError::OutOfMemory)?;
        let header_phys = hs_frame;
        // Status byte placed well clear of the 16-byte header within the frame.
        let status_phys = PhysAddr::new(hs_frame.as_u64() + 64);
        let bounce_phys =
            frame::allocate_contiguous_frames(BOUNCE_FRAMES).ok_or(VirtioError::OutOfMemory)?;

        // The device is fully set up — let it run.
        transport.set_driver_ok();

        Ok(Self {
            inner: InterruptMutex::new(Inner {
                transport,
                queue,
                header_phys,
                status_phys,
                bounce_phys,
            }),
            capacity_sectors,
        })
    }
}

impl Inner {
    /// Issue one request and wait for completion.
    ///
    /// Builds the header, lays out the descriptor chain (header → optional data
    /// → status), kicks the device, polls for completion, and checks the status
    /// byte. `data_sectors == 0` (flush) omits the data descriptor.
    /// `dev_writes_data` is true for reads (the device writes into the data
    /// buffer) and false for writes (the device reads from it).
    fn request(
        &mut self,
        op: u32,
        lba: u64,
        data_sectors: usize,
        dev_writes_data: bool,
    ) -> Result<(), BlockError> {
        // Bail out BEFORE staging anything. The header, status byte, bounce buffer
        // and all three descriptors are fixed, single-instance DMA memory; if a
        // previous chain was abandoned the device may still be reading or writing
        // them, so the staging writes below would race it. Checking at submit time
        // would be too late — they have already happened by then.
        if self.queue.is_failed() {
            return Err(BlockError::DeviceError);
        }

        // --- Build the 16-byte request header in the header DMA buffer ---
        // Layout: type:u32, reserved:u32, sector:u64 (all little-endian).
        // SAFETY: header_phys points to a 16-byte-aligned DMA buffer we own.
        unsafe {
            let h = self.header_phys.to_virt().as_mut_ptr::<u8>();
            write_volatile(h.cast::<u32>(), op);
            write_volatile(h.add(4).cast::<u32>(), 0u32);
            write_volatile(h.add(8).cast::<u64>(), lba);
        }

        // Initialise the status byte to a sentinel so we can tell it was written.
        // SAFETY: status_phys is a 1-byte DMA buffer we own.
        unsafe {
            write_volatile(self.status_phys.to_virt().as_mut_ptr::<u8>(), 0xFF);
        }

        // --- Lay out the descriptor chain ---
        let data_len = (data_sectors * SECTOR_SIZE) as u32;

        if data_sectors == 0 {
            // Header → status (2 descriptors). Used for FLUSH.
            self.queue.write_desc(
                DESC_HEADER,
                VirtqDesc {
                    addr: self.header_phys.as_u64(),
                    len: 16,
                    flags: VIRTQ_DESC_F_NEXT,
                    next: DESC_STATUS,
                },
            );
        } else {
            // Header → data → status (3 descriptors).
            self.queue.write_desc(
                DESC_HEADER,
                VirtqDesc {
                    addr: self.header_phys.as_u64(),
                    len: 16,
                    flags: VIRTQ_DESC_F_NEXT,
                    next: DESC_DATA,
                },
            );
            // The data buffer is device-writable for reads, device-readable for
            // writes.
            let data_flags = VIRTQ_DESC_F_NEXT
                | if dev_writes_data { VIRTQ_DESC_F_WRITE } else { 0 };
            self.queue.write_desc(
                DESC_DATA,
                VirtqDesc {
                    addr: self.bounce_phys.as_u64(),
                    len: data_len,
                    flags: data_flags,
                    next: DESC_STATUS,
                },
            );
        }

        // Status is always device-writable, terminates the chain.
        self.queue.write_desc(
            DESC_STATUS,
            VirtqDesc {
                addr: self.status_phys.as_u64(),
                len: 1,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );

        // Submit the chain (head = header) and poll until the device returns it.
        //
        // On failure we must NOT fall through to the status read below: the device
        // never wrote the status byte for this request, and its descriptors and
        // bounce buffer are still device-owned. `submit_and_wait` disables the queue
        // in that case, so every later request fails too rather than picking up this
        // one's late completion and returning its data as their own.
        if let Err(e) = self.queue.submit_and_wait(DESC_HEADER) {
            if !matches!(e, VirtioError::QueueDesynchronised) {
                crate::println!("[virtio-blk] request failed: {e:?}");
            }
            return Err(BlockError::DeviceError);
        }

        // Check the device's status byte.
        // SAFETY: the device has completed the request and written the status.
        let status = unsafe {
            core::ptr::read_volatile(self.status_phys.to_virt().as_ptr::<u8>())
        };
        if status == VIRTIO_BLK_S_OK {
            Ok(())
        } else {
            Err(BlockError::DeviceError)
        }
    }

    /// Borrow the bounce buffer as a mutable byte slice of `len` bytes.
    fn bounce_slice(&self, len: usize) -> &mut [u8] {
        // SAFETY: bounce_phys backs BOUNCE_FRAMES contiguous frames in the HHDM;
        // callers pass len <= bounce capacity, and access is serialised by the
        // surrounding mutex.
        unsafe {
            core::slice::from_raw_parts_mut(self.bounce_phys.to_virt().as_mut_ptr::<u8>(), len)
        }
    }
}

impl BlockDevice for VirtioBlk {
    fn read_blocks(&self, start_lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.len() % SECTOR_SIZE != 0 {
            return Err(BlockError::BadBufferLength);
        }
        let total_sectors = buf.len() / SECTOR_SIZE;
        if start_lba + total_sectors as u64 > self.capacity_sectors {
            return Err(BlockError::OutOfRange);
        }

        let mut inner = self.inner.lock();
        let mut done = 0usize;
        while done < total_sectors {
            let chunk = (total_sectors - done).min(Self::BOUNCE_SECTORS);
            // Device reads `chunk` sectors into the bounce buffer...
            inner.request(VIRTIO_BLK_T_IN, start_lba + done as u64, chunk, true)?;
            // ...then we copy them out to the caller's buffer.
            let bytes = chunk * SECTOR_SIZE;
            let off = done * SECTOR_SIZE;
            buf[off..off + bytes].copy_from_slice(&inner.bounce_slice(bytes)[..bytes]);
            done += chunk;
        }
        Ok(())
    }

    fn write_blocks(&self, start_lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        if buf.len() % SECTOR_SIZE != 0 {
            return Err(BlockError::BadBufferLength);
        }
        let total_sectors = buf.len() / SECTOR_SIZE;
        if start_lba + total_sectors as u64 > self.capacity_sectors {
            return Err(BlockError::OutOfRange);
        }

        let mut inner = self.inner.lock();
        let mut done = 0usize;
        while done < total_sectors {
            let chunk = (total_sectors - done).min(Self::BOUNCE_SECTORS);
            let bytes = chunk * SECTOR_SIZE;
            let off = done * SECTOR_SIZE;
            // Copy the caller's data into the bounce buffer, then the device
            // reads it out.
            inner.bounce_slice(bytes)[..bytes].copy_from_slice(&buf[off..off + bytes]);
            inner.request(VIRTIO_BLK_T_OUT, start_lba + done as u64, chunk, false)?;
            done += chunk;
        }
        Ok(())
    }

    fn block_size(&self) -> u32 {
        SECTOR_SIZE as u32
    }

    fn block_count(&self) -> u64 {
        self.capacity_sectors
    }

    fn flush(&self) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        inner.request(VIRTIO_BLK_T_FLUSH, 0, 0, false)
    }
}
