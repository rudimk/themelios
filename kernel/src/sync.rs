//! # Kernel synchronization primitives
//!
//! Provides interrupt-safe synchronization for kernel use. The standard
//! `spin::Mutex` is NOT safe when interrupt handlers might try to acquire
//! the same lock — if an interrupt fires while the lock is held, the handler
//! will spin forever waiting for a lock that can never be released (deadlock
//! on a single-core system).
//!
//! ## InterruptMutex
//!
//! `InterruptMutex` wraps a `spin::Mutex` with interrupt disable/restore
//! logic. The sequence is:
//!
//! 1. Save current interrupt state (are interrupts enabled or disabled?)
//! 2. Disable interrupts (CLI on x86_64)
//! 3. Acquire the inner spinlock
//! 4. ... critical section (caller uses the guard) ...
//! 5. Release the inner spinlock
//! 6. Restore the previous interrupt state
//!
//! Steps 5-6 happen in that exact order when the guard is dropped. This
//! ordering is critical — if we re-enabled interrupts before releasing
//! the lock, an interrupt handler could fire and try to acquire the same
//! lock, deadlocking.
//!
//! ## Nested locking
//!
//! `InterruptMutex` correctly handles nested acquisition of different
//! locks. If code acquires lock A (disabling interrupts) and then lock B,
//! releasing B will NOT re-enable interrupts because it remembers that
//! interrupts were already disabled when B was acquired. Only releasing A
//! (the outermost lock) will restore the original interrupt state.

use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

/// A mutex that disables CPU interrupts while the lock is held.
///
/// This prevents the deadlock scenario where an interrupt handler tries to
/// acquire a lock that was already held when the interrupt fired. Used for
/// all kernel global state that might be accessed from both normal code and
/// interrupt handlers (e.g., the serial writer, the heap allocator).
///
/// Uses `spin::Mutex` internally for the actual mutual exclusion. The
/// interrupt disable/restore is layered on top.
pub struct InterruptMutex<T> {
    inner: spin::Mutex<T>,
}

// SAFETY: InterruptMutex provides mutual exclusion via spinlock + interrupt
// disable. It is safe to share across execution contexts (main kernel code
// and interrupt handlers) as long as T is Send. The spin::Mutex inside
// already requires T: Send for Sync, and our interrupt-disable wrapper
// strengthens the guarantee.
unsafe impl<T: Send> Sync for InterruptMutex<T> {}
unsafe impl<T: Send> Send for InterruptMutex<T> {}

impl<T> InterruptMutex<T> {
    /// Create a new `InterruptMutex` wrapping the given value.
    ///
    /// This is a `const fn` so it can be used in `static` declarations
    /// without lazy initialization.
    pub const fn new(value: T) -> Self {
        Self {
            inner: spin::Mutex::new(value),
        }
    }

    /// Acquire the lock, disabling interrupts first.
    ///
    /// Returns an `InterruptMutexGuard` that dereferences to the inner value.
    /// When the guard is dropped, the lock is released and the previous
    /// interrupt state is restored.
    ///
    /// If interrupts were already disabled (e.g., by an outer `InterruptMutex`),
    /// they will remain disabled after this guard is dropped — the original
    /// state is preserved, not blindly re-enabled.
    pub fn lock(&self) -> InterruptMutexGuard<'_, T> {
        // Save the current interrupt state BEFORE disabling. This lets us
        // correctly handle nested locks — if interrupts were already off,
        // we won't accidentally turn them back on when this lock is released.
        let were_enabled = arch_interrupts_enabled();
        arch_disable_interrupts();

        let guard = self.inner.lock();

        InterruptMutexGuard {
            guard: ManuallyDrop::new(guard),
            interrupts_were_enabled: were_enabled,
        }
    }
}

/// RAII guard for `InterruptMutex`.
///
/// Holds the inner spinlock and remembers the previous interrupt state.
/// When dropped, releases the lock and restores the interrupt state in
/// that exact order (unlock first, then restore — never the reverse).
pub struct InterruptMutexGuard<'a, T> {
    /// The inner spinlock guard. Wrapped in `ManuallyDrop` so we control
    /// exactly when it's dropped — we need to unlock the spinlock BEFORE
    /// restoring interrupts to avoid a race window.
    guard: ManuallyDrop<spin::MutexGuard<'a, T>>,

    /// Whether interrupts were enabled when this lock was acquired.
    /// Used to restore the exact previous state on drop.
    interrupts_were_enabled: bool,
}

impl<T> Deref for InterruptMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for InterruptMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for InterruptMutexGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: The guard was wrapped in ManuallyDrop during construction,
        // and this is the only place it gets dropped. It happens exactly once.
        //
        // ORDERING: We MUST unlock the spinlock (by dropping the guard) BEFORE
        // re-enabling interrupts. If we did it the other way around, there would
        // be a window where interrupts are enabled but the lock is still held —
        // an interrupt handler could fire in that window and deadlock trying to
        // acquire the same lock.
        unsafe {
            ManuallyDrop::drop(&mut self.guard);
        }

        // Restore the previous interrupt state. If interrupts were disabled
        // before we acquired this lock (nested locking), they stay disabled.
        if self.interrupts_were_enabled {
            arch_enable_interrupts();
        }
    }
}

// --- Architecture-specific interrupt control ---
//
// These thin wrappers dispatch to the appropriate arch module.
// When aarch64 support is added (Phase 7), equivalent implementations
// using DAIF masking will be added here.

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn arch_disable_interrupts() {
    crate::arch::irq::disable();
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn arch_enable_interrupts() {
    crate::arch::irq::enable();
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn arch_interrupts_enabled() -> bool {
    crate::arch::irq::are_enabled()
}
