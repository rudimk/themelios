# Memory Management

This document describes ThemeliOS's memory management subsystem design.

> **Status**: Design phase. Implementation begins in Phase 1.

## Overview

The memory management (MM) subsystem is responsible for:

1. **Physical frame allocation** — tracking which 4 KiB pages of physical RAM are free or in use
2. **Virtual memory** — creating and managing page tables for each process
3. **Kernel heap** — providing dynamic allocation (`alloc`-style) for kernel data structures

## Physical memory

### Boot-time discovery

The bootloader provides a memory map describing which physical address ranges are usable RAM, reserved by firmware, or used for MMIO. The frame allocator uses this map to initialize its free list.

### Frame allocator

The frame allocator hands out 4 KiB physical memory frames. Initial implementation will use a **bitmap allocator**:

- One bit per physical frame (1 = allocated, 0 = free)
- Simple, predictable, easy to implement
- For 4 GiB of RAM: bitmap is 128 KiB (manageable)

Later optimization: replace with a **buddy allocator** for efficient allocation of contiguous multi-frame regions (needed for DMA buffers, large pages).

### Capability integration

Physical frames are resources protected by capabilities. When a process requests memory:

1. Kernel allocates a frame from the free pool
2. Kernel creates a `MemoryCap` for that frame
3. Kernel inserts the capability into the process's CSpace
4. Process can now map the frame into its address space using the capability

A process cannot access physical memory it doesn't have a capability for — the page tables are configured to reflect capability permissions.

## Virtual memory

### Address space layout (x86_64)

```
 Lower half (user space, per-process):
   0x0000_0000_0000_0000 - 0x0000_7FFF_FFFF_FFFF

 Upper half (kernel space, shared across all processes):
   0xFFFF_8000_0000_0000 - 0xFFFF_FFFF_FFFF_FFFF
     ├── Physical memory direct map
     ├── Kernel code and data
     ├── Kernel heap
     └── Per-CPU data
```

### Page tables

x86_64 uses 4-level page tables (PML4 → PDPT → PD → PT), each with 512 entries. Each entry is 8 bytes and can point to:

- The next level table
- A large page (2 MiB at PD level, 1 GiB at PDPT level)
- A 4 KiB page (at PT level)

The kernel manages page tables for each process. When a context switch occurs, the CPU's CR3 register is loaded with the new process's PML4 physical address, instantly switching the entire address space.

### aarch64 differences

aarch64 uses a similar 4-level translation table scheme but with different register names (TTBR0/TTBR1 instead of CR3) and different table entry formats. The architecture abstraction layer hides these differences from the rest of the kernel.

## Kernel heap

The kernel needs dynamic allocation for data structures like:

- Process control blocks
- Capability tables
- IPC message buffers
- Driver state

We'll use the `linked_list_allocator` crate initially (a simple free-list allocator suitable for `#![no_std]` kernels), backed by physical frames allocated from the frame allocator.

The kernel heap lives in the upper-half virtual address space and is shared across all contexts (but only accessible from kernel mode).

## Memory safety

Rust's ownership model provides compile-time guarantees against:

- **Use-after-free**: The compiler prevents using a frame after it's been freed
- **Double-free**: The compiler prevents freeing a frame twice
- **Data races**: Shared mutable access requires synchronization (`Mutex`, `RefCell`)

The `unsafe` keyword is required for raw pointer operations (hardware register access, page table manipulation) — these are confined to small, well-documented blocks.
