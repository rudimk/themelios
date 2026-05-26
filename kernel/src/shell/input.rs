//! # Serial input handling
//!
//! Provides a fixed-size ring buffer for bytes received from the serial port
//! (COM1, IRQ4) and the mechanism to wake the shell task when input arrives.
//!
//! ## Data flow
//!
//! 1. User types a character in the QEMU terminal
//! 2. QEMU delivers it to the virtual COM1 UART
//! 3. The UART fires IRQ4
//! 4. The IRQ4 handler calls `push_byte()` to store the byte in the ring buffer
//! 5. The IRQ4 handler calls `wake_shell()` to unblock the shell task
//! 6. The shell task wakes up, calls `pop_byte()` to read the character
//!
//! ## Ring buffer
//!
//! A fixed 256-byte circular buffer. If the buffer is full when a byte arrives
//! (user typing faster than the shell can process), the byte is dropped. At
//! 115200 baud this would require the shell to stall for >22ms with a full
//! buffer, which shouldn't happen in practice.

use core::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::InterruptMutex;
use crate::sched;

/// Ring buffer capacity in bytes. 256 is more than enough for interactive
/// line-editing — even a 200-character command line fits with room to spare.
const BUFFER_SIZE: usize = 256;

/// Fixed-size ring buffer for received serial bytes.
struct RingBuffer {
    /// The data storage. Only bytes between `read` and `write` (modulo
    /// BUFFER_SIZE) are valid.
    data: [u8; BUFFER_SIZE],
    /// Index of the next byte to read (consumer side).
    read: usize,
    /// Index of the next byte to write (producer side).
    write: usize,
    /// Number of bytes currently in the buffer.
    count: usize,
}

impl RingBuffer {
    const fn new() -> Self {
        Self {
            data: [0; BUFFER_SIZE],
            read: 0,
            write: 0,
            count: 0,
        }
    }

    /// Push a byte into the buffer. Returns false if the buffer is full.
    fn push(&mut self, byte: u8) -> bool {
        if self.count >= BUFFER_SIZE {
            return false;
        }
        self.data[self.write] = byte;
        self.write = (self.write + 1) % BUFFER_SIZE;
        self.count += 1;
        true
    }

    /// Pop a byte from the buffer. Returns None if empty.
    fn pop(&mut self) -> Option<u8> {
        if self.count == 0 {
            return None;
        }
        let byte = self.data[self.read];
        self.read = (self.read + 1) % BUFFER_SIZE;
        self.count -= 1;
        Some(byte)
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Global input ring buffer, protected by an interrupt-disabling mutex.
/// Written by the IRQ4 handler (producer), read by the shell task (consumer).
static INPUT_BUFFER: InterruptMutex<RingBuffer> = InterruptMutex::new(RingBuffer::new());

/// The task ID of the shell task. Set by `set_shell_task_id()` during shell
/// initialization. The IRQ4 handler reads this to know which task to wake.
/// Using AtomicUsize so it can be read without acquiring a lock in the IRQ handler.
static SHELL_TASK_ID: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Register the shell task's ID so the IRQ4 handler can wake it.
/// Called once during shell initialization.
pub fn set_shell_task_id(id: sched::task::TaskId) {
    SHELL_TASK_ID.store(id, Ordering::Release);
}

/// Push a received byte into the input ring buffer.
///
/// Called from the IRQ4 handler. If the buffer is full, the byte is dropped.
/// This function is interrupt-safe (uses InterruptMutex).
pub fn push_byte(byte: u8) {
    INPUT_BUFFER.lock().push(byte);
}

/// Pop a byte from the input ring buffer.
///
/// Called by the shell task to consume received input. Returns None if
/// the buffer is empty.
pub fn pop_byte() -> Option<u8> {
    INPUT_BUFFER.lock().pop()
}

/// Check if there's any input waiting in the buffer.
pub fn has_input() -> bool {
    !INPUT_BUFFER.lock().is_empty()
}

/// Wake the shell task if it's registered and the scheduler is running.
///
/// Called from the IRQ4 handler after pushing bytes into the ring buffer.
pub fn wake_shell() {
    let id = SHELL_TASK_ID.load(Ordering::Acquire);
    if id != usize::MAX {
        sched::wake_task(id);
    }
}
