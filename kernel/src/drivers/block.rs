//! # Block device abstraction
//!
//! A *block device* presents storage as a flat array of fixed-size blocks
//! (sectors) that can be read and written by index. Disks, SSDs, and virtual
//! disks all fit this model. The kernel talks to storage exclusively through
//! the [`BlockDevice`] trait, which decouples filesystem and block-server code
//! from the specific hardware underneath.
//!
//! ## Why a trait
//!
//! In Phase 3 the only implementation is VirtIO-blk (QEMU's virtual disk). But
//! the immutable-OS roadmap runs on real cloud instances too, where storage is
//! NVMe (Phase 8) or VirtIO-SCSI. Those drivers will implement this exact trait,
//! and the filesystem servers above won't change at all — they only ever see
//! "read these blocks / write these blocks".
//!
//! ## Where this sits in the hybrid microkernel
//!
//! The block driver is one of the few device drivers that stays in ring 0: it
//! is thin, it only talks to trusted hypervisor hardware, and it exposes a
//! dead-simple read/write interface. The actual filesystem parsing — the real
//! attack surface — runs in userspace (ring 3) and reaches this driver through
//! the block server's IPC interface (sub-phase 3.3). A bug here is a bug in a
//! few hundred lines of well-scoped DMA code, not in a sprawling parser.

use alloc::vec::Vec;

use crate::sync::InterruptMutex;

/// Errors returned by [`BlockDevice`] operations.
///
/// Deliberately small and concrete: every variant maps to a specific failure a
/// caller can act on (retry, propagate as an I/O error to a filesystem, or fail
/// a request). Richer diagnostics go to the audit log, not into the error type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// I/O error reported by the device (e.g. VirtIO status byte != OK).
    DeviceError,
    /// The request addressed sectors beyond the device's capacity.
    OutOfRange,
    /// The device is read-only and a write was attempted.
    ReadOnly,
    /// The device is not ready or has not been initialised.
    NotReady,
    /// The request could not be submitted because the virtqueue was full.
    QueueFull,
    /// The supplied buffer length is not a whole number of blocks.
    BadBufferLength,
}

/// A device that stores data as a flat array of fixed-size blocks.
///
/// Implementors must be `Send + Sync`: block devices are registered globally
/// and accessed from multiple kernel tasks (notably the block server). Methods
/// take `&self` and use interior mutability for any per-request hardware state,
/// so a single shared reference can drive the device.
pub trait BlockDevice: Send + Sync {
    /// Read consecutive blocks starting at `start_lba` into `buf`.
    ///
    /// `buf.len()` must be a whole multiple of [`block_size`](Self::block_size).
    /// Reads `buf.len() / block_size` blocks. Returns [`BlockError::OutOfRange`]
    /// if the range exceeds the device capacity.
    fn read_blocks(&self, start_lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;

    /// Write `buf` to consecutive blocks starting at `start_lba`.
    ///
    /// `buf.len()` must be a whole multiple of [`block_size`](Self::block_size).
    fn write_blocks(&self, start_lba: u64, buf: &[u8]) -> Result<(), BlockError>;

    /// The block (sector) size in bytes. VirtIO-blk uses 512.
    fn block_size(&self) -> u32;

    /// The total number of blocks on the device.
    fn block_count(&self) -> u64;

    /// Flush any device-side write cache to stable storage.
    fn flush(&self) -> Result<(), BlockError>;
}

// ----- Global block device registry -----
//
// Discovered block devices are registered here at boot so the block server and
// diagnostics can look them up by index ("virtio-blk0" = index 0, etc.). Devices
// live for the kernel's lifetime, so we store `&'static dyn BlockDevice` handles
// (obtained by leaking the boxed driver) — no reference counting needed.

/// A registered block device: a stable name plus a static handle to the driver.
pub struct RegisteredBlockDevice {
    /// Human-readable name, e.g. "virtio-blk0".
    pub name: &'static str,
    /// The driver, behind a `'static` reference (the driver is leaked at
    /// registration since block devices are never torn down).
    pub device: &'static dyn BlockDevice,
}

/// The global block device registry.
static REGISTRY: InterruptMutex<Vec<RegisteredBlockDevice>> = InterruptMutex::new(Vec::new());

/// Register a block device under `name`, returning its registry index.
///
/// Takes ownership of the boxed driver and leaks it to obtain a `'static`
/// handle (block devices live forever). The returned index is stable for the
/// kernel's lifetime.
pub fn register(name: &'static str, device: alloc::boxed::Box<dyn BlockDevice>) -> usize {
    // Leak the box: the driver must outlive every reference handed out, and it
    // is never deallocated, so a permanent leak is the correct lifetime.
    let static_dev: &'static dyn BlockDevice = alloc::boxed::Box::leak(device);
    let mut reg = REGISTRY.lock();
    let index = reg.len();
    reg.push(RegisteredBlockDevice { name, device: static_dev });
    index
}

/// Look up a registered block device by index.
pub fn get(index: usize) -> Option<&'static dyn BlockDevice> {
    REGISTRY.lock().get(index).map(|r| r.device)
}

/// Number of registered block devices.
pub fn count() -> usize {
    REGISTRY.lock().len()
}

/// Print the block device registry to the serial console for diagnostics.
pub fn print_registry() {
    use crate::println;
    let reg = REGISTRY.lock();
    println!("Block devices: {} registered", reg.len());
    for (i, r) in reg.iter().enumerate() {
        let bytes = r.device.block_count() * r.device.block_size() as u64;
        println!(
            "  [{}] {}  {} blocks x {} B = {} KiB",
            i, r.name, r.device.block_count(), r.device.block_size(), bytes / 1024,
        );
    }
}
