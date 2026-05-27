//! # Capability object registry
//!
//! The kernel-side registry of all capability objects. While `CSpace` holds
//! per-process references to capabilities (lightweight handles), this module
//! tracks the actual kernel objects those capabilities point to.
//!
//! ## Purpose
//!
//! In a capability system, the same kernel object (e.g., an IPC endpoint)
//! can be referenced by capabilities in multiple processes' CSpaces. The
//! object registry provides a single source of truth for:
//!
//! - **Object lifecycle**: knowing when all references to an object are gone
//!   so it can be destroyed
//! - **Cross-process revocation**: finding all capabilities that reference
//!   a specific object, regardless of which process holds them
//! - **Object metadata**: storing per-object state that doesn't belong in
//!   individual capability slots (e.g., endpoint message queues)
//!
//! ## Phase 2 scope
//!
//! For Phase 2, the registry is a simple `Vec<Option<CapObject>>` behind an
//! `InterruptMutex`. Each object has a unique ID, a type, and a reference count.
//! This is sufficient for the capability tests and process creation. The
//! registry will be extended in later phases with per-object state (endpoint
//! queues in Phase 2.6, memory region tracking, etc.).

extern crate alloc;

use alloc::vec::Vec;
use crate::sync::InterruptMutex;
use super::CapType;

/// A kernel-managed capability object.
///
/// This is the "real" object behind one or more capability handles in
/// various processes' CSpaces. For example, an IPC endpoint is a single
/// `CapObject` that may be referenced by capabilities in both the sender's
/// and receiver's CSpaces.
#[derive(Debug, Clone)]
pub struct CapObject {
    /// Unique identifier for this object, assigned at creation time.
    /// Object IDs are never reused — a monotonically increasing counter
    /// ensures that even after an object is destroyed, its ID won't be
    /// assigned to a new object.
    pub id: u64,

    /// The type of this object. Matches the `CapType` variants but
    /// represents the actual kernel object, not a capability's view of it.
    pub cap_type: CapType,

    /// Number of capability handles across all processes that reference
    /// this object. When this drops to zero, the object can be destroyed
    /// and its resources freed.
    pub ref_count: usize,
}

/// Global registry of all capability objects in the system.
///
/// Protected by an `InterruptMutex` because capability operations can be
/// triggered from both syscall context (normal code path) and potentially
/// from interrupt context (e.g., IRQ delivery that needs to check an IRQ
/// capability).
///
/// The registry is a flat vector indexed by position. Destroyed objects
/// leave `None` holes that are reused by future allocations. Object IDs
/// are separate from vector indices and are never reused.
static OBJECT_REGISTRY: InterruptMutex<ObjectRegistry> =
    InterruptMutex::new(ObjectRegistry::new_empty());

/// Internal state of the capability object registry.
struct ObjectRegistry {
    /// The object slots. `None` means the slot is free for reuse.
    objects: Vec<Option<CapObject>>,

    /// Monotonically increasing counter for assigning unique object IDs.
    /// Starts at 1 (ID 0 is reserved as "no object").
    next_id: u64,
}

impl ObjectRegistry {
    /// Create an empty registry. Used for the static initializer.
    ///
    /// The Vec starts empty and grows on first use (after heap init).
    const fn new_empty() -> Self {
        Self {
            objects: Vec::new(),
            next_id: 1,
        }
    }
}

/// Register a new kernel object and return its unique ID.
///
/// Creates a `CapObject` with the given type, assigns it a unique ID,
/// and stores it in the global registry. The initial reference count is 1
/// (the caller is expected to immediately create a capability referencing
/// this object).
///
/// ## Example
///
/// ```ignore
/// let obj_id = cap::object::register(CapType::Endpoint {
///     endpoint_id: 42,
///     badge: 0,
/// });
/// // obj_id is now a unique identifier for this endpoint object
/// ```
pub fn register(cap_type: CapType) -> u64 {
    let mut registry = OBJECT_REGISTRY.lock();
    let id = registry.next_id;
    registry.next_id += 1;

    let object = CapObject {
        id,
        cap_type,
        ref_count: 1,
    };

    // Try to reuse a free slot first
    for slot in registry.objects.iter_mut() {
        if slot.is_none() {
            *slot = Some(object);
            return id;
        }
    }

    // No free slot — append a new one
    registry.objects.push(Some(object));
    id
}

/// Look up an object by its unique ID.
///
/// Returns a clone of the object if found, or `None` if the ID doesn't
/// match any registered object. We return a clone rather than a reference
/// because the registry is behind an InterruptMutex — the guard would
/// be dropped at the end of this function.
pub fn lookup(object_id: u64) -> Option<CapObject> {
    let registry = OBJECT_REGISTRY.lock();
    registry.objects.iter()
        .filter_map(|slot| slot.as_ref())
        .find(|obj| obj.id == object_id)
        .cloned()
}

/// Increment the reference count for an object.
///
/// Called when a new capability referencing this object is created
/// (e.g., via `grant()` or cross-process transfer).
pub fn add_ref(object_id: u64) {
    let mut registry = OBJECT_REGISTRY.lock();
    for slot in registry.objects.iter_mut() {
        if let Some(ref mut obj) = slot {
            if obj.id == object_id {
                obj.ref_count += 1;
                return;
            }
        }
    }
}

/// Decrement the reference count for an object.
///
/// Called when a capability referencing this object is removed or revoked.
/// If the reference count drops to zero, the object is removed from the
/// registry (freeing the slot for reuse). Returns `true` if the object
/// was destroyed (ref_count hit zero), `false` if it still has references.
pub fn release(object_id: u64) -> bool {
    let mut registry = OBJECT_REGISTRY.lock();
    for slot in registry.objects.iter_mut() {
        if let Some(ref mut obj) = slot {
            if obj.id == object_id {
                obj.ref_count = obj.ref_count.saturating_sub(1);
                if obj.ref_count == 0 {
                    *slot = None;
                    return true;
                }
                return false;
            }
        }
    }
    false
}

/// Return the total number of registered objects (non-None slots).
///
/// Useful for diagnostics and test assertions.
pub fn object_count() -> usize {
    let registry = OBJECT_REGISTRY.lock();
    registry.objects.iter().filter(|s| s.is_some()).count()
}

/// Return the total number of registry slots (including free ones).
///
/// Useful for diagnostics to see how much the registry has grown.
pub fn registry_size() -> usize {
    let registry = OBJECT_REGISTRY.lock();
    registry.objects.len()
}
