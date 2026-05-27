//! # Capability space (CSpace)
//!
//! A CSpace is a per-process flat array of capability slots. Each process has
//! exactly one CSpace, and all capability operations (lookup, grant, revoke)
//! go through it. Userspace references capabilities by integer handles
//! (`CapHandle`), which the kernel resolves into the actual `Capability` via
//! the CSpace on every syscall.
//!
//! ## Design
//!
//! The CSpace is a `Vec<Slot>` where each slot is either empty (`None`) or
//! contains a capability. Each slot also has a **generation counter** that
//! increments every time the slot is freed. This generation is encoded into
//! the `CapHandle`, so stale handles (referencing a slot that has since been
//! reused) are detected and rejected.
//!
//! Slot 0 is permanently reserved as the Null capability — it can never hold
//! a real capability. This makes `CapHandle::NULL` (index 0, generation 0)
//! a reliable "no capability" sentinel.
//!
//! ## Capacity
//!
//! Maximum 4096 slots per CSpace (limited by the 12-bit index field in
//! `CapHandle`). The vector starts small and grows on demand up to this limit.
//!
//! ## Operations
//!
//! - **insert**: find the first free slot, place the capability, return a handle
//! - **lookup**: validate index + generation, return a reference to the capability
//! - **remove**: take the capability out, increment the slot's generation counter
//! - **grant**: derive a new capability from an existing one with equal or fewer rights
//! - **revoke**: invalidate a capability and all capabilities derived from it

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use super::{Capability, CapHandle, CapRights, CapType, MAX_CSPACE_SIZE};

/// Error type for CSpace operations.
///
/// Each variant describes a specific failure mode, making it easy to produce
/// clear error messages in syscall handlers and audit log entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// The handle's index is out of bounds (>= current slot count).
    InvalidIndex,

    /// The handle's generation doesn't match the slot's current generation.
    /// This means the capability that the handle referenced has been removed,
    /// and the slot may have been reused for a different capability.
    StaleGeneration,

    /// The slot at the handle's index is empty (no capability stored).
    SlotEmpty,

    /// The CSpace is full — all 4096 slots are occupied.
    SpaceFull,

    /// The requested rights are not a subset of the source capability's rights.
    /// Capabilities can only be derived with equal or fewer rights, never more.
    RightsEscalation,

    /// The source capability does not have the GRANT right, so it cannot be
    /// used to derive new capabilities.
    NoGrantRight,
}

/// A single slot in the CSpace.
///
/// Each slot tracks a generation counter independently. When a slot is freed
/// (capability removed), the generation increments. Handles encode the
/// generation at the time of creation, so a stale handle will have a
/// mismatched generation and be rejected on lookup.
#[derive(Debug, Clone)]
struct Slot {
    /// The capability in this slot, or `None` if the slot is free.
    capability: Option<Capability>,

    /// Monotonically increasing counter. Incremented each time this slot
    /// transitions from occupied to free. With 20 bits of generation in
    /// the handle, a slot must be reused over 1 million times before a
    /// stale handle could accidentally match.
    generation: u32,
}

/// A capability space — a flat array of capability slots owned by a single process.
///
/// The CSpace is the process's "keyring": it holds all the capabilities the
/// process currently possesses. The kernel resolves every capability handle
/// through the owning process's CSpace before performing any operation.
///
/// ## Slot 0 invariant
///
/// Slot 0 is always Null and cannot be overwritten. This ensures that
/// `CapHandle::NULL` (index 0, gen 0) always resolves to an invalid/null
/// capability, providing a safe sentinel value.
pub struct CSpace {
    /// The slot array. Starts with a single Null slot (index 0) and grows
    /// on demand up to `MAX_CSPACE_SIZE` (4096).
    slots: Vec<Slot>,
}

impl CSpace {
    /// Create a new CSpace with only the reserved Null slot at index 0.
    ///
    /// The Null slot has generation 0 and holds a Null capability. This
    /// matches `CapHandle::NULL` (index 0, generation 0), ensuring that
    /// the null handle always resolves to a Null capability.
    pub fn new() -> Self {
        let null_slot = Slot {
            capability: Some(Capability {
                cap_type: CapType::Null,
                rights: CapRights::NONE,
                parent: None,
            }),
            generation: 0,
        };
        Self {
            slots: vec![null_slot],
        }
    }

    /// Return the number of slots currently allocated (including empty ones).
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Return the number of occupied (non-empty) slots, excluding the Null slot.
    pub fn active_count(&self) -> usize {
        self.slots.iter()
            .skip(1) // skip the permanent Null slot
            .filter(|s| s.capability.is_some())
            .count()
    }

    /// Insert a capability into the first available free slot.
    ///
    /// Scans from index 1 (slot 0 is reserved) for an empty slot. If none
    /// exist and the CSpace hasn't reached its maximum size, a new slot is
    /// appended. Returns a `CapHandle` encoding the slot index and current
    /// generation, or `Err(CapError::SpaceFull)` if the CSpace is at capacity.
    pub fn insert(&mut self, cap: Capability) -> Result<CapHandle, CapError> {
        // Scan for the first free slot (skip index 0, which is always Null)
        for i in 1..self.slots.len() {
            if self.slots[i].capability.is_none() {
                let gen = self.slots[i].generation;
                self.slots[i].capability = Some(cap);
                return Ok(CapHandle::new(i, gen));
            }
        }

        // No free slot found — try to grow if under the size limit
        if self.slots.len() >= MAX_CSPACE_SIZE {
            return Err(CapError::SpaceFull);
        }

        let index = self.slots.len();
        self.slots.push(Slot {
            capability: Some(cap),
            generation: 0,
        });
        Ok(CapHandle::new(index, 0))
    }

    /// Look up a capability by handle.
    ///
    /// Validates that:
    /// 1. The handle's index is within bounds
    /// 2. The handle's generation matches the slot's current generation
    /// 3. The slot contains a capability (not empty)
    ///
    /// Returns a reference to the capability, or an error describing why
    /// the lookup failed. Stale handles (from removed capabilities) fail
    /// with `StaleGeneration`, which is the primary defense against
    /// use-after-free bugs in capability management.
    pub fn lookup(&self, handle: CapHandle) -> Result<&Capability, CapError> {
        let index = handle.index();
        let gen = handle.generation();

        let slot = self.slots.get(index).ok_or(CapError::InvalidIndex)?;

        if slot.generation != gen {
            return Err(CapError::StaleGeneration);
        }

        slot.capability.as_ref().ok_or(CapError::SlotEmpty)
    }

    /// Look up a capability by handle, returning a mutable reference.
    ///
    /// Same validation as `lookup()`, but returns `&mut Capability`.
    /// Used internally by operations that need to modify a capability in place.
    pub fn lookup_mut(&mut self, handle: CapHandle) -> Result<&mut Capability, CapError> {
        let index = handle.index();
        let gen = handle.generation();

        let slot = self.slots.get_mut(index).ok_or(CapError::InvalidIndex)?;

        if slot.generation != gen {
            return Err(CapError::StaleGeneration);
        }

        slot.capability.as_mut().ok_or(CapError::SlotEmpty)
    }

    /// Remove a capability from the CSpace and return it.
    ///
    /// After removal, the slot's generation counter is incremented so that
    /// any outstanding handles to this slot become stale. The slot becomes
    /// available for reuse by future `insert()` calls.
    ///
    /// Slot 0 (Null) cannot be removed — attempting to do so returns
    /// `Err(CapError::InvalidIndex)`.
    pub fn remove(&mut self, handle: CapHandle) -> Result<Capability, CapError> {
        let index = handle.index();
        let gen = handle.generation();

        // Prevent removal of the reserved Null slot
        if index == 0 {
            return Err(CapError::InvalidIndex);
        }

        let slot = self.slots.get_mut(index).ok_or(CapError::InvalidIndex)?;

        if slot.generation != gen {
            return Err(CapError::StaleGeneration);
        }

        let cap = slot.capability.take().ok_or(CapError::SlotEmpty)?;

        // Increment generation so stale handles are detected on future lookups.
        // The 20-bit generation field in CapHandle wraps after ~1 million
        // increments, which is acceptable — a slot would need to be freed
        // and reused over a million times for a collision.
        slot.generation = slot.generation.wrapping_add(1);

        Ok(cap)
    }

    /// Derive a new capability from an existing one with equal or reduced rights.
    ///
    /// This is the only way to create new capabilities from userspace. The
    /// source capability must have the GRANT right, and the requested rights
    /// must be a subset of (or equal to) the source's rights. The new
    /// capability records the source handle as its parent, enabling revocation
    /// to cascade from parent to child.
    ///
    /// ## Rights enforcement
    ///
    /// The `new_rights` parameter is intersected with the source's rights,
    /// so even if the caller requests rights the source doesn't have, the
    /// resulting capability will only have rights that both the source and
    /// the request agree on. However, if `new_rights` contains bits not in
    /// the source, we return `RightsEscalation` rather than silently masking
    /// — this makes rights violations explicit and auditable.
    pub fn grant(
        &mut self,
        source_handle: CapHandle,
        new_rights: CapRights,
    ) -> Result<CapHandle, CapError> {
        // Look up the source capability and verify it has GRANT permission.
        // We clone the relevant fields to avoid holding an immutable borrow
        // while we later call insert() (which needs &mut self).
        let (cap_type, source_rights) = {
            let source = self.lookup(source_handle)?;
            if !source.rights.contains(CapRights::GRANT) {
                return Err(CapError::NoGrantRight);
            }
            (source.cap_type, source.rights)
        };

        // Verify that the requested rights don't exceed the source's rights.
        // This check is the fundamental security invariant of the capability
        // system: authority can only be reduced, never amplified.
        if !source_rights.is_superset_of(new_rights) {
            return Err(CapError::RightsEscalation);
        }

        // Create the derived capability with the requested rights and a
        // parent link back to the source (for revocation).
        let derived = Capability {
            cap_type,
            rights: new_rights,
            parent: Some(source_handle),
        };

        self.insert(derived)
    }

    /// Revoke a capability and all capabilities derived from it.
    ///
    /// Walks the entire CSpace looking for capabilities whose parent chain
    /// includes the revoked handle. All such capabilities are removed, and
    /// their slots' generation counters are incremented. This is O(n) in
    /// CSpace size for each level of derivation — acceptable for Phase 2's
    /// small CSpaces (max 4096 entries).
    ///
    /// The revocation is recursive: if capability A was derived from B, and
    /// B was derived from C, revoking C will invalidate both B and A.
    ///
    /// The target capability itself is also removed.
    pub fn revoke(&mut self, handle: CapHandle) -> Result<(), CapError> {
        // First verify the handle is valid
        let index = handle.index();
        if index == 0 {
            return Err(CapError::InvalidIndex);
        }
        let slot = self.slots.get(index).ok_or(CapError::InvalidIndex)?;
        if slot.generation != handle.generation() {
            return Err(CapError::StaleGeneration);
        }
        if slot.capability.is_none() {
            return Err(CapError::SlotEmpty);
        }

        // Collect all handles that need to be revoked. We do this in rounds:
        // each round finds capabilities whose parent is in the "to revoke" set.
        // This continues until no new descendants are found.
        //
        // We use indices rather than handles here because we're walking slots
        // directly and need to match against parent handles.
        let mut to_revoke: Vec<CapHandle> = vec![handle];
        let mut found_new = true;

        while found_new {
            found_new = false;
            for i in 1..self.slots.len() {
                if let Some(ref cap) = self.slots[i].capability {
                    if let Some(parent) = cap.parent {
                        // Check if this capability's parent is in our revoke set
                        if to_revoke.iter().any(|h| h.index() == parent.index()
                            && h.generation() == parent.generation())
                        {
                            // Build the handle for this slot using the current generation
                            let child_handle = CapHandle::new(i, self.slots[i].generation);
                            // Only add if not already in the set
                            if !to_revoke.iter().any(|h| h.index() == child_handle.index()
                                && h.generation() == child_handle.generation())
                            {
                                to_revoke.push(child_handle);
                                found_new = true;
                            }
                        }
                    }
                }
            }
        }

        // Now remove all collected capabilities. Remove in reverse order of
        // discovery (children before parents) to avoid invalidating parent
        // references mid-walk, though with our approach this ordering isn't
        // strictly necessary since we've already collected all targets.
        for revoke_handle in to_revoke.iter().rev() {
            let idx = revoke_handle.index();
            if idx < self.slots.len() {
                self.slots[idx].capability = None;
                self.slots[idx].generation = self.slots[idx].generation.wrapping_add(1);
            }
        }

        Ok(())
    }

    /// Iterate over all occupied slots, yielding (handle, capability) pairs.
    ///
    /// Useful for shell commands that need to list all capabilities in a
    /// process's CSpace, and for debugging/audit purposes.
    pub fn iter(&self) -> impl Iterator<Item = (CapHandle, &Capability)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| {
            slot.capability.as_ref().map(|cap| {
                (CapHandle::new(i, slot.generation), cap)
            })
        })
    }
}
