# Capability System

This document details the design of ThemeliOS's capability system — the core security mechanism of the kernel.

> **Status**: Design phase. Implementation begins in Phase 2.

## Overview

In ThemeliOS, every resource is accessed through capabilities. A capability is a kernel-managed, unforgeable token that encodes:

1. **Which resource** (identified by a kernel object ID)
2. **What operations** are permitted (a bitmask of rights)

## Capability types

| Capability type | Resource | Example rights |
|----------------|----------|---------------|
| `MemoryCap` | Physical memory region | Read, Write, Execute, Map |
| `EndpointCap` | IPC endpoint | Send, Receive |
| `ThreadCap` | Thread/process | Start, Stop, Suspend, Resume |
| `DeviceCap` | Hardware device (MMIO region) | Read, Write |
| `IRQCap` | Interrupt line | Acknowledge, Bind |

## Capability spaces

Each process has a **capability space** (CSpace) — a table mapping local capability slots to kernel objects. A process refers to its capabilities by slot index, not by object ID. The kernel translates slot indices to objects on each syscall.

```
Process A's CSpace:
  Slot 0 → MemoryCap(region=0x1000, rights=RW)
  Slot 1 → EndpointCap(endpoint=#7, rights=Send)
  Slot 2 → (empty)
  Slot 3 → ThreadCap(thread=#12, rights=Start|Stop)

Process B's CSpace:
  Slot 0 → EndpointCap(endpoint=#7, rights=Receive)
  Slot 1 → MemoryCap(region=0x2000, rights=R)
```

Process A can send to endpoint #7 (slot 1), and Process B can receive from it (slot 0). Neither can access the other's memory — they'd need explicit capabilities for that.

## Capability operations

### Grant

A parent process can grant a capability to a child process, optionally with reduced rights:

```
Parent has: MemoryCap(region=X, rights=RWX)
Parent grants child: MemoryCap(region=X, rights=R)
```

The child gets read-only access. Rights can only be reduced, never elevated.

### Transfer via IPC

Capabilities can be attached to IPC messages. This is how services delegate access:

```
FileServer receives "open /config" request
FileServer replies with MemoryCap(region=file_data, rights=R)
Client now has read access to the file's memory region
```

### Revoke

The kernel (or a process with the appropriate meta-capability) can revoke a capability, immediately invalidating it. Any future use of the revoked slot returns an error.

## Container mapping

In ThemeliOS, a "container" is a group of processes sharing a common set of capabilities. The container's capability set defines its sandbox:

- **Memory**: Only the memory regions granted to it
- **Network**: Only the network endpoints it has capabilities for
- **Filesystem**: Only the filesystem views it's been granted
- **IPC**: Only the services it has endpoint capabilities for

A container cannot discover or access anything outside its capability set. Unlike Linux containers (where a kernel exploit can escape the namespace), escaping a capability sandbox requires forging a kernel object — which is impossible without a kernel memory corruption bug.

## Comparison with Linux isolation

| Aspect | Linux (namespaces/cgroups) | ThemeliOS (capabilities) |
|--------|---------------------------|--------------------------|
| Default | Access everything, restrict selectively | Access nothing, grant explicitly |
| Enforcement | Kernel checks on each syscall | No syscall exists without capability |
| Escape risk | Kernel bugs can bypass namespaces | Requires kernel memory corruption |
| Resource discovery | Can probe for resources | Can't even address unknown resources |
| Granularity | Per-namespace | Per-object, per-right |
