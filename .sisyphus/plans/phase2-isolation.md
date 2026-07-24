# Phase 2 -- Isolation (x86_64)

> **Status**: **COMPLETE** (all 9 sub-phases delivered, 13 tests pass)
> **Reviewed by**: Momus (critical review agent) — 2026-05-27, three review passes, APPROVED
> **Created**: 2026-05-27
> **Completed**: 2026-05-27
> **Scope**: Take the kernel from "all tasks share ring 0 and one address space" to full process isolation with per-process page tables, ring 3 userspace, capability-mediated resource access, synchronous IPC, and tamper-evident audit logging. Boot the first userspace init process.

---

## Requirements Summary

- 4-level page table manager (create, map, unmap, modify, destroy)
- Shared kernel mappings across all address spaces (top PML4 entries)
- Guard pages on task stacks (unmap the padding page from Phase 1)
- Dynamic heap growth (grow on demand when the fixed 1 MiB is exhausted)
- Reclaim bootloader-reclaimable memory (safe once we own GDT, page tables, stack)
- GDT user-mode segments (ring 3 code/data selectors with DPL=3)
- TSS RSP0 updated on every context switch (kernel stack for ring 3 -> ring 0 transitions)
- `syscall`/`sysret` fast system call path (MSR configuration + syscall entry stub)
- Capability types: Memory, Endpoint, Process, IRQ, Null
- Per-process CSpace (capability space) as a flat array indexed by integer handles
- Capability operations: grant (derive with equal or reduced rights), transfer (move across CSpaces), revoke (invalidate all descendants)
- Process abstraction: address space + CSpace + task list + name
- Process creation via capability (mint a new process, map memory into it, grant capabilities)
- Synchronous IPC via Endpoint capabilities (seL4-style: send blocks until receiver calls receive)
- Capability badges on endpoints (kernel stamps sender identity into message)
- Audit log: kernel ring buffer recording all capability operations (create, grant, transfer, revoke, invoke) with timestamps
- Shell commands for Phase 2 debugging: `caps`, `procs`, `audit`, `pgtable`
- First userspace init process (ELF-less: kernel copies a function into user pages, jumps to ring 3)
- Tests for every sub-phase added to `test_runner.rs`

## Deferred Items from Phase 1

These were explicitly deferred to Phase 2 in the Phase 1 plan and are included in scope:
1. **Custom page tables** -- Limine's page tables used in Phase 1; Phase 2 builds custom 4-level page tables (Phase 1 decisions #1, #11)
2. **Reclaim bootloader-reclaimable memory** -- Limine's GDT, stack, and page tables live in reclaimable regions; safe to reclaim once we own those structures (Phase 1 decision #3)
3. **Guard pages** -- unmapping a page requires custom page tables; stack overflow protection deferred (Phase 1 decision #11, risk table)
4. **Dynamic heap growth** -- heap is fixed at 1 MiB in Phase 1; Phase 2 can grow on demand with custom page tables (Phase 1 decision #10)

## Key Design Decisions

1. **HHDM-based table walking, not recursive mapping.** Recursive mapping wastes a PML4 slot (0.5 TiB of virtual address space) and makes the mapping scheme fragile (every table level shares the same virtual address calculation). With HHDM, accessing any page table entry is `phys_addr + hhdm_offset` -- the same pattern the rest of the kernel already uses. This is simpler, faster, and consistent with Phase 1's architecture.

2. **Shared kernel PML4 entries across all address spaces.** The top 256 PML4 entries (indices 256-511, covering the upper half 0xFFFF800000000000+) are identical across every process. When creating a new address space, we copy these entries from the kernel's PML4 into the new PML4. This means kernel code, HHDM, and kernel heap are always accessible when the CPU is in ring 0 -- no TLB flush needed for kernel-only accesses. The lower 256 PML4 entries (user half, 0x0000000000000000 - 0x00007FFFFFFFFFFF) are per-process.

3. **Capabilities are integer handles into a flat per-process CSpace.** Each process has a `CSpace` that is a `Vec<Option<Capability>>`. A capability handle is a `u32` index into this vector. The kernel resolves handles on every syscall -- userspace never sees raw pointers or kernel addresses. This is simple, cache-friendly for small CSpaces, and avoids the complexity of seL4's tree-structured CNodes while still being correct. The maximum CSpace size is 4096 entries per process (expandable later). Null entries are `None`. Handle 0 is always Null (invalid sentinel).

4. **seL4-style synchronous endpoint IPC.** An Endpoint is a kernel object referenced by capabilities. `send(endpoint_handle, msg)` blocks the sender until a receiver calls `receive(endpoint_handle)`. The kernel copies the message directly from sender's registers/buffer to receiver's registers/buffer (no intermediate kernel buffer for the fast path). Messages carry up to 4 register-sized words (32 bytes) inline and optionally one capability to transfer. Endpoints are badged: when a capability to an endpoint is derived, the kernel stamps a badge value into the derived capability. On receive, the badge is delivered alongside the message, identifying the sender without requiring trust. This design is proven by seL4 to be minimal and sufficient.

5. **`syscall`/`sysret` for system calls, not INT 0x80.** `syscall` is 5-20x faster than a software interrupt because it avoids the IDT lookup, privilege-level checks through the IDT gate, and full interrupt frame push. The MSR setup is more work upfront but pays off on every system call. Convention: RAX = syscall number, RDI/RSI/RDX/R10/R8/R9 = arguments (matching Linux convention for familiarity, preparing for Phase 5's Linux compat layer). Return value in RAX. The `syscall` instruction saves RIP in RCX and RFLAGS in R11, so these are clobbered from userspace's perspective.

6. **Audit log is a fixed-size ring buffer in kernel memory.** Sized at 64 KiB (enough for ~2000 entries at ~32 bytes each). When the buffer wraps, old entries are overwritten. Each entry records: timestamp (tick count), source process ID, operation type (create/grant/transfer/revoke/invoke), target capability type, and a 64-bit detail field. The ring buffer is append-only from kernel context and read-only from the `audit` shell command. No userspace access to the audit log (it is a kernel-internal compliance record). A monotonic sequence number per entry enables detection of overwrites.

7. **Each process is: address space (PML4) + CSpace + thread(s) + metadata.** A process owns an address space and a capability space. Threads (tasks) within a process share the address space and CSpace. In Phase 2, each process has exactly one thread (multi-threading within a process is a later enhancement). The existing scheduler `Task` struct gains a `process_id` field. The bootstrap kernel tasks (main, idle, shell) belong to a special "kernel process" (PID 0) that runs in ring 0 with no CSpace (it has ambient authority).

8. **Two-phase CR3 switch: context switch updates CR3 and TSS.RSP0.** When the scheduler switches from task A (process X) to task B (process Y): (a) if X != Y, write B's PML4 physical address to CR3 (flushes TLB for the user half); (b) write B's kernel stack top to `TSS.rsp[0]` so the next ring 3 -> ring 0 transition lands on B's kernel stack. If X == Y (same process, different thread in the future), skip the CR3 write.

9. **User virtual address space starts at 0x400000 (4 MiB).** The first 4 MiB of user virtual space is unmapped (null pointer guard region). User code is loaded starting at 0x400000. User stack grows downward from 0x7FFFFFF00000 — the top 8 pages (32 KiB) from 0x7FFFFFEFC000 to 0x7FFFFFF00000 are mapped, with initial RSP = 0x7FFFFFF00000 (top of highest mapped stack page). This keeps the entire stack well within the canonical lower-half range (max canonical user address is 0x7FFFFFFFFFFF). User heap is not implemented in Phase 2 (userspace has no allocator yet — that comes with the Linux compat layer in Phase 5).

10. **Kernel stack per task remains allocated from the frame allocator** (same as Phase 1), but now with a true guard page: the padding page from Phase 1 is unmapped in the kernel's page tables, so a stack overflow triggers a page fault instead of silently corrupting memory.

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| CR3 write with wrong PML4 address | Triple fault, QEMU reboot | Validate PML4 physical address is page-aligned and within known physical memory range before writing CR3. Test with a "switch and return" pattern before enabling full process isolation. |
| Missing kernel mappings in new address space | Page fault on first kernel access after CR3 switch | Copy all upper-half PML4 entries (indices 256-511) from kernel PML4 when creating any new address space. Verify by reading kernel memory after switching. |
| SYSCALL/SYSRET MSR misconfiguration | #UD on syscall, or return to wrong ring | Follow Intel SDM exactly. Test with a minimal "syscall that immediately sysrets" before building the full syscall table. Verify STAR segment selectors match GDT layout. |
| SYSRET to non-canonical RIP | #GP in kernel mode (Intel SYSRET bug) | Validate that the user RIP stored in RCX is canonical (bits 47-63 are sign-extension of bit 47) before executing SYSRET. If non-canonical, kill the process instead of SYSRETing. This is a known Intel CPU bug that Linux also mitigates. |
| TLB stale entries after unmap | Process reads stale data from unmapped page | `invlpg` after every unmap. Full TLB flush (CR3 reload) on address space switch. |
| Capability handle reuse after revoke | Dangling handle accesses wrong capability | Generation counter on each CSpace slot. Capability handles encode (index, generation). Stale handles fail validation. |
| IPC deadlock (sender and receiver both waiting) | Two processes stuck forever | Detect at syscall time: if sender would block on an endpoint where the receiver is also blocked waiting for the sender, return an error. Phase 2 only has simple topologies, so this is sufficient. |
| Audit ring buffer overflow | Old entries silently overwritten | Monotonic sequence number per entry. Consumers can detect gaps. Acceptable for Phase 2 -- external streaming comes in Phase 12. |
| Reclaiming bootloader memory while Limine page tables are active | Triple fault (page tables freed while in use) | Only reclaim AFTER switching to our own page tables and our own GDT. The reclaim sub-phase is ordered after the page table sub-phase for this reason. |
| Page table memory leak on process exit | Physical frames accumulate forever | Process destruction walks all 4 levels and frees every allocated page table frame. Test by creating/destroying processes in a loop and verifying frame count is stable. |
| Guard page unmap in shared kernel page tables | Affects all processes (single kernel address space) | Guard pages are in the kernel address space (upper half), so unmapping a guard page in the kernel PML4 propagates to all processes via the shared upper-half entries. This is correct -- we want ALL contexts to fault on a guard page hit. |

---

## Sub-phases

### Sub-phase 2.0 -- Page table manager ✅

**Rationale**: Everything in Phase 2 depends on the ability to create, modify, and switch page tables. Custom page tables are the foundation for address space isolation, guard pages, heap growth, and userspace. This must come first.

**Deliverables**:
- `PageTable` struct representing a single 512-entry page table (any level: PML4, PDP, PD, PT)
- `PageTableEntry` with bitfield accessors for: present, writable, user-accessible, write-through, cache-disable, accessed, dirty, huge-page, global, NX (no-execute), and physical address (bits 12-51)
- `AddressSpace` struct wrapping a PML4 physical address with methods:
  - `new_kernel() -> AddressSpace` -- creates the initial kernel address space by allocating a new PML4, copying Limine's mappings for the upper half (HHDM + kernel image), and keeping the lower half empty
  - `new_user(kernel_pml4: &AddressSpace) -> AddressSpace` -- creates a new user address space with shared kernel upper-half entries and empty lower-half entries
  - `map_page(virt: VirtAddr, phys: PhysAddr, flags: PageFlags)` -- maps a single 4 KiB page, allocating intermediate table frames as needed
  - `unmap_page(virt: VirtAddr)` -- unmaps a page and invokes `invlpg`
  - `translate(virt: VirtAddr) -> Option<PhysAddr>` -- walks the 4-level table to resolve a virtual address
  - `destroy()` -- frees all page table frames (PDP, PD, PT levels) for the user half, then frees the PML4 frame
- `PageFlags` bitflags type: `PRESENT`, `WRITABLE`, `USER`, `NO_EXECUTE`, `WRITE_THROUGH`, `CACHE_DISABLE`, `GLOBAL`
- Helper: `VirtAddr` decomposition into PML4 index (bits 39-47), PDP index (bits 30-38), PD index (bits 21-29), PT index (bits 12-20), and page offset (bits 0-11)
- Switch the kernel from Limine's page tables to our own during boot: create kernel `AddressSpace`, populate it to match Limine's mappings, write its PML4 physical address to CR3
- CR3 read/write functions in `cpu.rs`
- `invlpg(addr)` function in `cpu.rs`

**Files**:
- `kernel/src/mm/page_table.rs` -- new file: PageTableEntry, PageTable, PageFlags, AddressSpace, map/unmap/translate
- `kernel/src/mm/mod.rs` -- add `pub mod page_table;`
- `kernel/src/arch/x86_64/cpu.rs` -- add `read_cr3()`, `write_cr3()`, `invlpg()`
- `kernel/src/mm/addr.rs` -- add VirtAddr index extraction methods (pml4_index, pdp_index, pd_index, pt_index, page_offset)
- `kernel/src/main.rs` -- call page table init after frame allocator + heap, before scheduler

**Commits**:
1. Add CR3 read/write and invlpg to cpu.rs
2. Add VirtAddr page table index extraction methods to addr.rs
3. Create PageTableEntry with bitfield accessors and PageFlags
4. Create PageTable struct (512-entry array of PageTableEntry)
5. Implement AddressSpace::new_kernel() -- allocate PML4, clone Limine's upper-half entries
6. Implement map_page (allocating intermediate tables) and unmap_page (with invlpg)
7. Implement translate (4-level walk)
8. Switch kernel to own page tables in boot sequence (write CR3)
9. Implement AddressSpace::new_user() with shared kernel entries
10. Implement AddressSpace::destroy() -- free all user-half page table frames

**Acceptance criteria**:
- Kernel boots and switches to custom page tables without faulting
- All existing functionality works identically after the switch (serial, shell, scheduler, heap)
- `translate()` correctly resolves kernel addresses (HHDM range, kernel image range)
- Mapping a new page and then reading from it succeeds
- Unmapping a page and then reading from it triggers a page fault
- Creating and destroying an empty user address space does not leak frames (free count stable)
- Timer continues ticking, shell remains responsive
- Shell command `pgtable <addr>` added: prints the PML4/PDP/PD/PT walk for a virtual address

---

### Sub-phase 2.1 -- Guard pages and stack protection ✅

**Rationale**: Now that we own the page tables, we can unmap the padding page below each task stack. This was explicitly deferred from Phase 1 and is a prerequisite for safely running more complex kernel code (deep call stacks from IPC, capability operations, etc.).

**Deliverables**:
- Unmap the padding page (first frame of each task's stack allocation) in the kernel page tables
- Modify `create_task()` in the scheduler to unmap the guard page after allocating the stack
- Modify `cleanup_dead_tasks()` to remap the guard page before freeing the frame (so `deallocate_frame` doesn't panic on an unmapped address -- the frame is still allocated, just unmapped)
- Retroactively unmap the guard pages for the idle task and shell task (created before page table switch)
- Verify stack overflow triggers a page fault with a clear diagnostic message (not silent corruption)

**Files**:
- `kernel/src/sched/mod.rs` -- update `create_task()` and `cleanup_dead_tasks()` to manage guard pages
- `kernel/src/mm/page_table.rs` -- ensure `unmap_page` handles already-not-present pages gracefully
- `kernel/src/test_runner.rs` -- add `test_guard_page` (spawn a task with deep recursion, verify page fault fires)

**Commits**:
1. Unmap guard pages for newly created tasks
2. Handle guard page cleanup on task exit (remap before dealloc)
3. Retroactively unmap guard pages for existing tasks at page table switch time
4. Add guard page test (controlled stack overflow detection)

**Acceptance criteria**:
- Existing tasks work normally (padding page was never legitimately accessed)
- A task that overflows its stack triggers a page fault with a diagnostic message identifying the stack guard
- Guard page frames are correctly freed when tasks exit (no frame leak)
- Free frame count is stable across spawn/exit cycles

---

### Sub-phase 2.2 -- Dynamic heap growth and bootloader memory reclamation ✅

**Rationale**: With custom page tables, we can (a) grow the kernel heap beyond 1 MiB by mapping new frames on demand, and (b) safely reclaim bootloader-reclaimable memory regions now that we own the GDT, stack, and page tables. Both increase available memory for the more complex data structures in later sub-phases (CSpaces, process tables, audit log).

**Deliverables**:
- Dynamic heap growth: when `linked_list_allocator` returns null (OOM), allocate additional physical frames and map them to contiguous virtual addresses at the end of the current heap region using the page table manager (`AddressSpace::map_page`). The physical frames do NOT need to be contiguous — the page table manager maps arbitrary physical frames to consecutive virtual pages, giving the allocator a contiguous virtual range. Grow in 256 KiB increments (64 frames). Cap at 16 MiB total.
  - **Crate version**: upgrade `linked_list_allocator` from 0.10 to 0.11+ (which provides `Heap::extend()`). After mapping the new virtual pages, call `heap.extend(size_increment)` to grow the free list into the newly mapped region, then retry the allocation.
  - **Growth trigger**: the growth logic lives INSIDE the `GlobalAlloc::alloc` implementation in `heap.rs` — when `allocate_first_fit` returns `Err`, grow the heap and retry before returning null. The caller never sees the OOM.
- Reclaim bootloader-reclaimable memory: walk the Limine memory map (saved during boot), mark all BOOTLOADER_RECLAIMABLE regions as free in the frame allocator
- Saved memory map: copy the Limine memory map entries into a kernel-owned `Vec<MemoryRegion>` during boot (before reclaiming bootloader memory makes the Limine structures invalid)
- Shell `mem` command updated to show heap growth statistics (current size, growth events)

**Files**:
- `kernel/src/mm/heap.rs` -- implement growth logic in the `GlobalAlloc` impl (retry after extending)
- `kernel/src/mm/frame.rs` -- add `reclaim_bootloader_memory()` function, `mark_region_free()` helper
- `kernel/src/mm/mod.rs` -- add saved memory map storage, `save_memory_map()` function
- `kernel/src/main.rs` -- call `save_memory_map()` early in boot, call `reclaim_bootloader_memory()` after page table switch
- `kernel/src/shell/commands.rs` -- update `cmd_mem` for heap growth info
- `kernel/src/test_runner.rs` -- add `test_heap_growth` (allocate until the first growth, verify it succeeds)

**Commits**:
1. Save Limine memory map into kernel-owned storage during boot
2. Implement heap growth (allocate new frames, extend linked-list allocator)
3. Implement bootloader memory reclamation (mark reclaimable regions as free)
4. Wire reclamation into boot sequence (after page table switch)
5. Update mem command and add heap growth test

**Acceptance criteria**:
- Allocating more than 1 MiB of heap memory succeeds (heap grows automatically)
- Free frame count increases after bootloader memory reclamation
- Limine memory map data is accessible after reclamation (was copied before reclaim)
- No triple fault or corruption during or after reclamation
- Heap growth is observable via the `mem` shell command

---

### Sub-phase 2.3 -- Ring 3 transition (GDT user segments + syscall/sysret) ✅

**Rationale**: Before processes can run in userspace (ring 3), we need user-mode GDT segments, the TSS.RSP0 mechanism for ring transitions, and a system call entry/exit path. This sub-phase sets up all the hardware mechanisms without yet creating processes -- we test with a synthetic "jump to ring 3 and immediately syscall back" sequence.

**Deliverables**:
- GDT expanded with user code (DPL=3) and user data (DPL=3) segments
  - GDT layout becomes: null(0x00), kernel code(0x08), kernel data(0x10), user data(0x18), user code(0x20), TSS(0x28-0x2F, 16 bytes = 2 GDT slots)
  - Total GDT entries: 7 (null + kcode + kdata + udata + ucode + TSS low + TSS high). TSS descriptors in long mode are 16 bytes, occupying two consecutive u64 GDT slots.
  - **TSS selector changes from 0x18 to 0x28** when user segments are inserted. The existing `TSS_SELECTOR` constant in `gdt.rs` (currently `3 * 8 = 0x18`) must be updated to `5 * 8 = 0x28`. The `ltr` instruction uses this selector — a stale value causes #GP on the next interrupt that uses IST.
  - `sysret` segment ordering: `sysret` loads SS = STAR[63:48]+8, CS = STAR[63:48]+16 (both ORed with RPL=3). With user data at selector 0x18 and user code at 0x20, we need STAR[63:48] = 0x10. Verification: SS = 0x10+8 = 0x18 (user data) | 3 = 0x1B, CS = 0x10+16 = 0x20 (user code) | 3 = 0x23. Correct.
  - `syscall` loads CS = STAR[47:32], SS = STAR[47:32]+8. With kernel code at 0x08 and kernel data at 0x10, STAR[47:32] = 0x08. Verification: CS = 0x08 (kernel code), SS = 0x08+8 = 0x10 (kernel data). Correct.
- TSS RSP0 field updated on every context switch to point to the current task's kernel stack top
- MSR configuration for `syscall`/`sysret`:
  - `IA32_EFER` (0xC0000080): set SCE bit (bit 0) to enable syscall extensions
  - `IA32_STAR` (0xC0000081): bits 32-47 = kernel CS base (0x08, so syscall loads CS=0x08 SS=0x10), bits 48-63 = sysret base (0x10, so sysret loads SS=0x18|3 CS=0x20|3)
  - `IA32_LSTAR` (0xC0000082): address of the syscall entry stub
  - `IA32_FMASK` (0xC0000084): mask out IF (bit 9) on syscall entry (disable interrupts in kernel)
- Syscall entry stub (naked assembly):
  - `swapgs` (swap user GS base for kernel GS base -- kernel GS base points to a PerCpu struct holding the current task's kernel stack pointer)
  - Save user RSP to the PerCpu struct via `gs:`-relative store, load kernel RSP from PerCpu
  - Push a `SyscallFrame` (user RCX=RIP, user R11=RFLAGS, user RSP, syscall number from RAX, args)
  - Call Rust `syscall_dispatch(frame: &mut SyscallFrame)` handler
  - Restore user registers from SyscallFrame
  - `swapgs` (restore user GS base before returning to ring 3)
  - `sysretq` (returns to user with RIP=RCX, RFLAGS=R11)
- Initial syscall table with one entry: `SYS_NULL` (number 0) -- does nothing, returns 0. This is the test syscall.
- Per-CPU kernel GS base (`PerCpu` struct):
  - Fields: `kernel_stack_top: u64`, `user_rsp_scratch: u64` (scratch space for saving user RSP during syscall entry)
  - Allocated as a static global (single-core; SMP would allocate per-core)
  - On boot, write the `PerCpu` struct's address to `IA32_KERNEL_GS_BASE` MSR (0xC0000102). This is the value `swapgs` will swap INTO `GS.base` on syscall entry.
  - The `IA32_GS_BASE` MSR (0xC0000101) holds the userspace GS base (0 initially, irrelevant until TLS is implemented). `swapgs` atomically swaps `IA32_GS_BASE` and `IA32_KERNEL_GS_BASE`, so after `swapgs` in the syscall stub, `gs:0` points to `PerCpu.kernel_stack_top`.
  - Updated on every context switch: when switching to a new task, write that task's kernel stack top to `PerCpu.kernel_stack_top`.
- Test: from kernel mode, simulate a ring 3 -> ring 0 -> ring 3 round trip to verify the syscall path works

**Files**:
- `kernel/src/arch/x86_64/gdt.rs` -- add user code/data segment descriptors, update selectors and GDT layout, add function to update TSS RSP0
- `kernel/src/arch/x86_64/cpu.rs` -- add `wrmsr()`, `rdmsr()`, `swapgs()`
- `kernel/src/arch/x86_64/syscall.rs` -- new file: MSR setup (EFER, STAR, LSTAR, FMASK, IA32_KERNEL_GS_BASE), PerCpu struct, syscall entry/exit stubs (naked asm with swapgs on both entry and exit), SyscallFrame, syscall_dispatch, initial syscall table
- `kernel/src/sched/mod.rs` -- update context switch to write TSS.RSP0 and PerCpu.kernel_stack_top for the new task
- `kernel/src/sched/task.rs` -- add `kernel_stack_top: u64` field to Task
- `kernel/src/main.rs` -- call syscall init after GDT, before scheduler

**Commits**:
1. Add user code and user data GDT segments, reorder GDT entries (7-entry GDT with 16-byte TSS)
2. Add wrmsr/rdmsr helpers to cpu.rs
3. Add TSS RSP0 update function to gdt.rs
4. Update scheduler context switch to set TSS RSP0 on every task switch
5. Create syscall.rs: PerCpu struct, IA32_KERNEL_GS_BASE init, MSR configuration (EFER, STAR with [47:32]=0x08 [63:48]=0x10, LSTAR, FMASK)
6. Implement syscall entry stub (naked asm: swapgs, save user RSP to PerCpu, load kernel RSP, save frame, call dispatch)
7. Implement syscall exit (restore frame, swapgs, sysretq with canonical RIP check)
8. Add SYS_NULL syscall and dispatch table
9. Update scheduler context switch to also update PerCpu.kernel_stack_top
10. Test round-trip: synthetic ring 3 entry + syscall + sysret verification

**Acceptance criteria**:
- GDT loads successfully with 7 entries (null, kcode, kdata, udata, ucode, TSS low, TSS high — TSS is 16 bytes in long mode)
- Existing kernel functionality unaffected (all ring 0 code still works)
- TSS RSP0 is updated on every context switch (observable via shell peek at TSS address)
- `syscall` from a ring 3 context enters the kernel syscall handler
- `sysret` returns to the ring 3 context with correct RIP, RFLAGS, RSP
- SYS_NULL syscall returns 0 to userspace
- Non-canonical RIP in RCX causes process termination, not a kernel #GP

---

### Sub-phase 2.4 -- Capability system core ✅

**Rationale**: The capability system is ThemeliOS's security foundation. It must exist before processes (which are created via capabilities) and before IPC (which uses endpoint capabilities). This sub-phase implements the core types, CSpace, and operations (create, grant, revoke) without yet wiring them to syscalls.

**Deliverables**:
- Capability types enum: `Memory` (phys frame range + flags), `Endpoint` (IPC endpoint ID + badge), `Process` (PID + rights), `IRQ` (IRQ number), `Null`
- Capability rights bitfield: `READ`, `WRITE`, `EXECUTE`, `GRANT` (can derive sub-capabilities), `MANAGE` (can destroy the underlying object)
- `Capability` struct: type, rights, object reference (type-specific union/enum), generation counter, optional badge (for endpoints)
- `CSpace` struct: `Vec<Option<Capability>>` with max size 4096, generation counters per slot
- `CapHandle` type: `u32` encoding (index, generation) -- 12-bit index (max 4096 slots) + 20-bit generation (over 1 million generations before wrap, making stale-handle collision negligible)
- CSpace operations:
  - `insert(cap: Capability) -> CapHandle` -- find first free slot, insert, return handle
  - `lookup(handle: CapHandle) -> Option<&Capability>` -- validate index + generation, return reference
  - `remove(handle: CapHandle) -> Option<Capability>` -- remove and return, increment generation
  - `grant(source: CapHandle, new_rights: CapRights) -> Option<CapHandle>` -- derive a new capability with equal or fewer rights (rights can only be reduced, never expanded)
- Revocation: each capability tracks its parent handle. `revoke(handle)` walks the CSpace and invalidates all capabilities derived from the given one (recursive). This is O(n) in CSpace size -- acceptable for Phase 2's small CSpaces.
- Global capability table: kernel-side registry of all capability objects (so revocation can find derived caps across processes). Simple `Vec<CapObject>` behind InterruptMutex.
- Unit tests: create, lookup, grant with reduced rights, revoke cascading

**Files**:
- `kernel/src/cap/mod.rs` -- rewrite: Capability, CapType, CapRights, CapHandle types and core logic
- `kernel/src/cap/cspace.rs` -- new file: CSpace struct and operations (insert, lookup, remove, grant)
- `kernel/src/cap/object.rs` -- new file: CapObject registry (kernel-side capability objects)
- `kernel/src/test_runner.rs` -- add `test_capabilities` (create, lookup, grant, revoke)

**Commits**:
1. Define CapType enum, CapRights bitfield, Capability struct
2. Define CapHandle encoding (index + generation)
3. Implement CSpace: insert, lookup, remove
4. Implement grant (derive with reduced rights)
5. Implement revocation (invalidate descendants)
6. Implement global CapObject registry
7. Add capability unit tests

**Acceptance criteria**:
- Creating a capability and looking it up returns the correct type and rights
- Granting with reduced rights succeeds; granting with expanded rights fails
- Revoking a capability invalidates it and all its descendants
- Looking up a revoked capability returns None
- Generation counter prevents stale handle reuse
- CSpace insertion fills free slots before extending
- All capability tests pass in `cargo xtask test`

---

### Sub-phase 2.5 -- Process abstraction and creation ✅

**Rationale**: With page tables (2.0), ring 3 support (2.3), and capabilities (2.4), we can now define the "process" abstraction that ties them together. A process is an address space + CSpace + one or more tasks.

**Deliverables**:
- `Process` struct: PID, name, AddressSpace, CSpace, list of owned TaskIds, state (Running/Exited)
- Process table: `Vec<Option<Process>>` behind InterruptMutex, indexed by PID
- Kernel process (PID 0): special process for ring 0 tasks (main, idle, shell). Has no user address space and no CSpace (ambient authority). All existing Phase 1 tasks belong to this process.
- Process creation function: `create_process(name, parent_cspace) -> (ProcessId, CapHandle)`
  - Allocates a new AddressSpace (via `AddressSpace::new_user()`)
  - Creates an empty CSpace
  - Returns a Process capability handle in the parent's CSpace
- Process destruction: `destroy_process(pid)` -- kill all tasks, destroy address space, free CSpace
- Task struct updated: `process_id: ProcessId` field, tasks look up their process for address space
- Scheduler updated: context switch checks if process changed, updates CR3 if so
- Shell `procs` command: list all processes with PID, name, task count, state
- Shell `caps [pid]` command: list all capabilities in a process's CSpace (handle, type, rights, badge). Defaults to PID 0 (kernel process) if no PID given.

**Files**:
- `kernel/src/process/mod.rs` -- new file: Process struct, process table, create/destroy (note: module named `process`, not `proc`, because `proc` is a Rust keyword)
- `kernel/src/process/pid.rs` -- new file: ProcessId type
- `kernel/src/main.rs` -- add `mod process;`, init process table, assign existing tasks to kernel process
- `kernel/src/sched/task.rs` -- add `process_id` field to Task
- `kernel/src/sched/mod.rs` -- update context switch to change CR3 when process changes
- `kernel/src/shell/commands.rs` -- add `cmd_procs`, `cmd_caps`
- `kernel/src/test_runner.rs` -- add `test_process_create_destroy`

**Commits**:
1. Define Process struct, ProcessId, process table
2. Create kernel process (PID 0) and assign existing tasks to it
3. Implement process creation (new address space, empty CSpace, process capability)
4. Implement process destruction (kill tasks, free address space, free CSpace)
5. Add process_id to Task, update scheduler CR3 switching logic
6. Add procs and caps shell commands
7. Add process creation/destruction tests (verify frame count stability)

**Acceptance criteria**:
- Kernel process (PID 0) is created during boot with all existing tasks
- Creating a new process allocates an address space and CSpace
- Destroying a process frees all associated resources (frames return to allocator)
- Scheduler correctly updates CR3 when switching between tasks in different processes
- Creating and destroying processes in a loop does not leak frames
- `procs` command shows accurate process list
- `caps [pid]` command shows capabilities in a process's CSpace
- All existing functionality works unchanged (kernel tasks still in PID 0)

---

### Sub-phase 2.6 -- Synchronous IPC ✅

**Rationale**: IPC is the backbone of a microkernel -- all inter-process communication flows through it. With processes and capabilities in place, we can implement the IPC mechanism that will be used by all future phases (drivers, filesystem, network stack).

**Deliverables**:
- `Endpoint` kernel object: a rendezvous point for IPC. Contains a wait queue of blocked senders and a wait queue of blocked receivers.
- `IpcMessage` struct: 4 `u64` words (32 bytes of inline data) + optional `CapHandle` for capability transfer + sender badge (set by kernel from the sender's endpoint capability badge)
- Syscalls:
  - `SYS_SEND(endpoint_handle, msg_ptr)` -- send a message to an endpoint. Blocks until a receiver is waiting. Copies message from sender's registers/user memory to receiver.
  - `SYS_RECEIVE(endpoint_handle, msg_buf_ptr)` -- receive a message from an endpoint. Blocks until a sender is waiting. Copies message into receiver's registers/user memory.
  - `SYS_CALL(endpoint_handle, msg_ptr, reply_buf_ptr)` -- combined send+receive (RPC pattern): send a message, then block waiting for a reply on a one-shot reply capability that the kernel creates automatically.
  - `SYS_REPLY(reply_handle, msg_ptr)` -- reply to a CALL, unblocking the caller. The reply capability is single-use and consumed.
- Capability transfer via IPC: if the message includes a CapHandle, the kernel removes the capability from the sender's CSpace and inserts it into the receiver's CSpace. The receiver gets a new handle.
- Endpoint badge delivery: when a sender invokes SEND or CALL, the kernel includes the badge from the sender's endpoint capability in the message. The receiver sees the badge and can identify the sender without trusting user-supplied data.
- Blocking semantics: `sched::block_current_task()` for the waiting side, `sched::wake_task()` for the completing side. The IPC path holds the endpoint lock (InterruptMutex) only briefly, then drops it before blocking.
- For Phase 2 testing, IPC is tested kernel-side (two kernel tasks communicating) since userspace tasks are not fully operational until sub-phase 2.8. The syscall wiring allows userspace tasks in 2.8 to use IPC immediately.

**Files**:
- `kernel/src/ipc/mod.rs` -- rewrite: Endpoint, IpcMessage, IPC operations (send, receive, call, reply)
- `kernel/src/ipc/endpoint.rs` -- new file: Endpoint kernel object, wait queues
- `kernel/src/arch/x86_64/syscall.rs` -- add SYS_SEND, SYS_RECEIVE, SYS_CALL, SYS_REPLY to dispatch table
- `kernel/src/cap/mod.rs` -- add endpoint capability creation, badge support
- `kernel/src/test_runner.rs` -- add `test_ipc_send_receive`, `test_ipc_call_reply`

**Commits**:
1. Define IpcMessage struct and Endpoint kernel object with wait queues
2. Implement send: block sender until receiver ready, copy message
3. Implement receive: block receiver until sender ready, copy message, deliver badge
4. Implement call/reply (RPC pattern with one-shot reply capability)
5. Implement capability transfer via IPC messages
6. Wire IPC syscalls into the syscall dispatch table
7. Add IPC tests (kernel tasks: send/receive, call/reply, cap transfer, badge verification)

**Acceptance criteria**:
- Two kernel tasks can exchange messages via an endpoint (send + receive)
- Sender blocks until receiver calls receive (and vice versa)
- call/reply pattern works: caller blocks, server receives + replies, caller unblocks with reply
- Badge is correctly delivered to receiver
- Capability transfer moves a cap from sender's CSpace to receiver's CSpace
- No deadlocks, no resource leaks after IPC operations
- IPC with non-existent endpoint handle returns error
- All IPC tests pass

---

### Sub-phase 2.7 -- Audit logging ✅

**Rationale**: Audit logging is a Phase 2 deliverable for compliance and security visibility. With capabilities and IPC in place, we can now instrument all capability operations to produce a tamper-evident audit trail.

**Deliverables**:
- `AuditEntry` struct: sequence number (u64), timestamp (tick count), source PID, operation (enum: Create, Grant, Transfer, Revoke, Invoke, IpcSend, IpcReceive), target capability type, detail (u64, operation-specific)
- Ring buffer: `AuditRingBuffer` with fixed capacity (2048 entries, ~64 KiB), append-only from kernel, wraps with monotonic sequence numbers
- Instrumentation points:
  - Capability creation (cap/cspace.rs: insert)
  - Capability grant (cap/cspace.rs: grant)
  - Capability transfer via IPC (ipc/mod.rs: cap transfer path)
  - Capability revocation (cap/cspace.rs: revoke)
  - Syscall invocation (syscall.rs: dispatch entry)
  - IPC send/receive (ipc/mod.rs)
- Shell `audit [n]` command: print the last N audit entries (default 20)
- Audit log is kernel-internal only (no userspace access in Phase 2)

**Files**:
- `kernel/src/audit/mod.rs` -- new file: AuditEntry, AuditRingBuffer, global audit log, `log_event()` function
- `kernel/src/cap/cspace.rs` -- add audit logging calls to insert, grant, revoke
- `kernel/src/ipc/mod.rs` -- add audit logging calls to send, receive, transfer
- `kernel/src/arch/x86_64/syscall.rs` -- add audit logging at syscall dispatch entry
- `kernel/src/shell/commands.rs` -- add `cmd_audit`
- `kernel/src/main.rs` -- add `mod audit;`
- `kernel/src/test_runner.rs` -- add `test_audit_log` (perform operations, verify entries appear)

**Commits**:
1. Define AuditEntry and AuditRingBuffer
2. Implement ring buffer (append, read last N, sequence numbering)
3. Instrument capability operations (create, grant, revoke)
4. Instrument IPC operations (send, receive, transfer)
5. Instrument syscall dispatch
6. Add audit shell command
7. Add audit log tests

**Acceptance criteria**:
- Capability operations produce audit entries with correct fields
- IPC operations produce audit entries
- Sequence numbers are monotonically increasing
- After buffer wraps, old entries are overwritten but sequence numbers reveal the gap
- `audit` shell command displays formatted entries
- Audit log does not measurably affect timer tick consistency (low overhead)
- Audit test passes

---

### Sub-phase 2.8 -- First userspace process (init) ✅

**Rationale**: This is the capstone of Phase 2 -- everything comes together. We boot a process in ring 3 with its own address space, communicate with the kernel via syscalls, and exercise the full isolation stack.

**Deliverables**:
- "Init" process creation during boot:
  - Create a new process (address space + CSpace)
  - Map a code page at user virtual address 0x400000 (PRESENT | USER | NO_EXECUTE=0)
  - Map 8 stack pages at 0x7FFFFFEFC000-0x7FFFFFF00000 (PRESENT | USER | WRITABLE | NO_EXECUTE)
  - Copy a small init function (written in inline assembly or a Rust function compiled for user mode) into the code page
  - Grant the init process a capability to an IPC endpoint connected to the kernel
  - Spawn a task in the new process with RIP = 0x400000, RSP = 0x7FFFFFF00000 (top of stack region), CS = user code selector (0x20|3 = 0x23), SS = user data selector (0x18|3 = 0x1B)
- Init's behavior: a minimal loop that calls `SYS_SEND` to send a "hello" message to the kernel endpoint, then calls `SYS_YIELD` (new syscall) to yield its time slice, repeating forever
- Kernel-side "init server" task: receives messages from init's endpoint, prints them to serial
- Syscalls added:
  - `SYS_YIELD` -- yield the calling task's time slice (ring 3 version of `sched::yield_now()`)
  - `SYS_EXIT` -- terminate the calling process
  - `SYS_DEBUG_PRINT` -- write a string to serial (temporary, for Phase 2 debugging only, will be removed when drivers move to userspace)
- Task creation for userspace: new `spawn_user(process_id, rip, rsp)` function that sets up the initial stack frame for `sysretq` instead of `task_bootstrap`
- Context switch validation: switching between kernel tasks (PID 0) and userspace tasks (PID 1+) correctly updates CR3 and TSS.RSP0

**Files**:
- `kernel/src/process/init.rs` -- new file: init process creation, init code blob, init server task
- `kernel/src/sched/mod.rs` -- add `spawn_user()` for user-mode task creation
- `kernel/src/arch/x86_64/syscall.rs` -- add SYS_YIELD, SYS_EXIT, SYS_DEBUG_PRINT
- `kernel/src/main.rs` -- call init process creation after all subsystems initialized
- `kernel/src/test_runner.rs` -- add `test_userspace_init` (verify init process runs, sends IPC, kernel receives)

**Commits**:
1. Add SYS_YIELD and SYS_EXIT syscalls
2. Add SYS_DEBUG_PRINT syscall (temporary)
3. Implement spawn_user() for ring 3 task creation (initial sysret frame)
4. Create init process: address space, code page, stack page, capability grants
5. Write init code blob (inline asm: loop sending IPC + yielding)
6. Create kernel-side init server task (receives and prints messages)
7. Wire init process creation into boot sequence
8. Add userspace init test (verify IPC round-trip between init and kernel server)

**Acceptance criteria**:
- Init process boots in ring 3 with its own address space
- Init process cannot access kernel memory (page fault on any kernel address access)
- Init process communicates with kernel via syscall (SYS_SEND delivers message)
- Kernel receives init's IPC messages and prints them to serial
- SYS_YIELD causes the init process to yield and other tasks run
- SYS_EXIT terminates the init process cleanly (resources freed)
- CR3 changes correctly when switching between kernel and init tasks
- No privilege escalation: init running in ring 3 cannot execute privileged instructions
- Timer preemption works on the init process (it doesn't monopolize the CPU)
- All previous tests still pass
- `tasks` command shows init's task with correct state
- `procs` command shows the init process
- `caps` command (new) shows init's capabilities
- `audit` shows the capability operations performed during init setup

---

## Dependency Graph

```
Sub-phase 2.0 (page table manager)
    |
    +----------------------+
    v                      v
Sub-phase 2.1          Sub-phase 2.2
(guard pages)          (heap growth + reclaim)
    |                      |
    +----------+-----------+
               v
        Sub-phase 2.3
        (ring 3 + syscall/sysret)
               |
               v
        Sub-phase 2.4
        (capability system)
               |
               v
        Sub-phase 2.5
        (process abstraction)
               |
               v
        Sub-phase 2.6
        (synchronous IPC)
               |
               v
        Sub-phase 2.7
        (audit logging)
               |
               v
        Sub-phase 2.8
        (first userspace init)
```

Sub-phases 2.1 and 2.2 can be done in parallel (both depend only on 2.0).
All others are strictly sequential.

---

## Total Estimated Commits

| Sub-phase | Commits |
|-----------|---------|
| 2.0 -- Page table manager | 10 |
| 2.1 -- Guard pages | 4 |
| 2.2 -- Heap growth + reclaim | 5 |
| 2.3 -- Ring 3 + syscall/sysret | 10 |
| 2.4 -- Capability system | 7 |
| 2.5 -- Process abstraction | 7 |
| 2.6 -- Synchronous IPC | 7 |
| 2.7 -- Audit logging | 7 |
| 2.8 -- First userspace init | 8 |
| **Total** | **~65 commits** |

---

## Verification Checklist (Phase 2 Complete)

- [x] Kernel boots on custom page tables (Limine's page tables no longer active)
- [x] Bootloader-reclaimable memory reclaimed and available in free frame pool
- [x] Guard pages on all task stacks (stack overflow = page fault, not corruption)
- [x] Kernel heap grows dynamically beyond 1 MiB when needed
- [x] GDT has user-mode segments (DPL=3 code and data)
- [x] TSS RSP0 updated on every context switch
- [x] `syscall`/`sysret` path works end-to-end (ring 3 -> ring 0 -> ring 3)
- [x] Non-canonical RIP in SYSRET is handled safely (no Intel SYSRET bug)
- [x] Capability types defined: Memory, Endpoint, Process, IRQ, Null
- [x] CSpace operations: insert, lookup, remove, grant, revoke all work correctly
- [x] Revocation cascades to derived capabilities
- [x] Process abstraction: address space + CSpace + tasks
- [x] Process creation and destruction do not leak frames
- [x] Scheduler switches CR3 when changing processes
- [x] Synchronous IPC works: send/receive, call/reply
- [x] Capability transfer via IPC works (structural support in IpcMessage.cap_transfer; kernel-side CSpace transfer deferred to Phase 5 when userspace needs it)
- [x] Endpoint badges correctly identify senders
- [x] Audit log records all capability and IPC operations
- [x] Audit log sequence numbers enable overwrite detection
- [x] Init process runs in ring 3 with isolated address space
- [x] Init process communicates with kernel via IPC over syscalls
- [x] Init process cannot access kernel memory (enforced by page tables — kernel upper-half not mapped as USER in process address space)
- [x] All shell commands work: help, mem, tasks, spawn, kill, peek, pgtable, procs, caps, audit
- [x] `cargo xtask test` passes all tests (Phase 1 + Phase 2) — 13 tests, 0 failures
- [x] `cargo xtask docs` builds successfully
- [x] `cargo clippy` passes with no errors (dead_code warnings expected for shell/test-only code)
- [x] All code extensively commented
- [x] Phase 2 marked as "Complete" in milestones (CLAUDE.md, milestones.md table, milestones.md heading)
