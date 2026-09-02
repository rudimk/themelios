# The aarch64 Port

ThemeliOS targets x86_64 first and aarch64 second. This chapter describes what the ARM64 port *is*, what it deliberately is not, and the handful of architectural differences that shaped it.

> **Status**: Phase 7 complete — a **ring-0 kernel core** on QEMU `virt`. EL0/userspace, storage, networking and containers on ARM are a separate ABI surface and are deferred.

## Scope, stated plainly

The port covers boot, memory management on kernel-owned page tables, exceptions, interrupts, a preemptive scheduler, an interactive shell, and — since Phase 8.3 — VirtIO over the mmio transport, giving the block and network drivers. It does **not** cover ring-3/EL0, and therefore not filesystems, sockets or containers. "aarch64 support" here does not mean "containers on ARM".

That boundary is visible in the test suite rather than left to prose: an aarch64 run reports **23 passed, 0 failed, 32 skipped**, and each skipped test names the subsystem that explains it. The total is 55 on both architectures, so the two runs are directly comparable.

(Phase 7 shipped this chapter saying 16 passed / 38 skipped. The passing count was right for the time; the skip count never was — 16 + 38 is 54, and the suite was 55. Corrected here along with the 8.3 figures.)

| Subsystem | x86_64 | aarch64 |
|---|---|---|
| Boot, serial console | ✅ | ✅ |
| Frame allocator, heap | ✅ | ✅ |
| Kernel page tables | ✅ | ✅ |
| Exceptions, interrupts, timer | ✅ | ✅ |
| Preemptive scheduler | ✅ | ✅ |
| Capability system, IPC, audit | ✅ | ✅ (compiled and tested; no *user* of them yet on a non-test boot) |
| Debug shell | ✅ | reduced (11 of 28 commands) |
| Ring-3 / EL0 | ✅ | deferred (8.4/8.5) |
| VirtIO transport | ✅ virtio-PCI | ✅ virtio-mmio (8.3) |
| VirtIO block + network drivers | ✅ | ✅ (8.3 — the drivers themselves; the servers above them are ring-3) |
| Filesystems, sockets | ✅ | deferred (8.6/8.7 — both need ring 3) |
| Containers, management API | ✅ | deferred (8.10) |

## How the code is organised

Architecture-specific code lives under `kernel/src/arch/{x86_64,aarch64}/`. Shared code never names an architecture module directly; it goes through a small facade for each primitive:

| Facade | Provides | x86_64 | aarch64 |
|---|---|---|---|
| `arch::irq` | mask/unmask/halt | `cli`/`sti`/`hlt` | `DAIF`/`wfi` |
| `arch::time` | monotonic tick | PIT ISR | generic-timer ISR |
| `arch::serial` | console | 16550 (port I/O) | PL011 (MMIO) |
| `arch::paging` | descriptors, TLB, roots | PML4 / CR3 | VMSAv8-64 / TTBR1 |
| `arch::context` | task switch | System V, `rsp` | AAPCS64, `sp` |

`cap`, `audit`, `http` and `oci` compile unmodified on both. `mm` and `sched` are shared but not untouched: both carry `#[cfg(target_arch = "x86_64")]` for the ring-3 machinery they also hold — TSS stack staging, FS-base restore, CR3 swaps, the Linux thread fields.

## Differences that actually mattered

Most of the port was mechanical. These are the places where the architectures genuinely disagree, and each one cost real debugging.

### The vector table is code, not pointers

x86's IDT holds 256 descriptors naming handler addresses. `VBAR_EL1` instead points at **2 KiB of executable code**: sixteen slots of 128 bytes, and the CPU *branches into* the slot. A stub that does not fit silently overflows into the next slot, and because alignment then pushes that slot along, the whole table shifts and exceptions land in the wrong handler. The first version of ours saved all 31 registers inline, needed 188 bytes per slot, and delivered a data abort to the FIQ stub.

Each slot is therefore four instructions that branch to shared code.

### `SPSel`, and where exceptions land

Limine hands off with `SPSel = 0`, meaning EL1 runs on `SP_EL0` while `SP_EL1` holds whatever the bootloader left. That decides both which vector group fires *and* what stack the handler lands on — and an uninitialised `SP_EL1` means the entry stub faults inside itself, nests, and reports the nested syndrome. A `brk` presents as a data abort at a fixed address in the wrong slot. Early boot switches to `SP_EL1` before anything can fault.

### Adopt `MAIR`/`TCR`, never rewrite them

The kernel builds its own page tables but **inherits** Limine's memory-attribute configuration. The cloned entries carry Limine's `AttrIndx` values, so installing our own `MAIR_EL1` would silently reinterpret the cacheability of every inherited mapping. `TCR_EL1` is likewise verified rather than programmed — we are already executing on tables built for it, so rewriting it would fault instantly and undiagnosably.

### `TTBR1`, not `TTBR0`

The kernel loads its root into `TTBR1_EL1` and parks `TTBR0_EL1` at zero. At EL1 with no userspace, `TTBR0` translates nothing, so switching it would prove nothing. `TTBR0` arrives with EL0.

### Returning into a task reads `x30`

x86's `switch_context` ends in `ret`, which pops a return address off the stack. aarch64's `ret` branches to whatever is in `x30`. A new task's initial frame therefore places the bootstrap trampoline in the `x30` slot and the entry function in `x19`.

There is **no FP save area**: the kernel is built for `aarch64-unknown-none-softfloat` and emits no vector instructions, and `CPACR_EL1.FPEN` is cleared at boot so a stray SIMD instruction traps loudly instead of corrupting another task's floating-point state. That backstop is verified at boot, not assumed — when it was first *read* rather than asserted, `FPEN` turned out to be `0b11`, so the safety net had never existed.

### A new task is entered by `ret`, not `eret`

The first switch to a task arrives out of the timer's IRQ handler, carrying the DAIF the CPU set on exception entry — where hardware masks all of `D`, `A`, `I` and `F`. Clearing only `I` would leave the others masked for the task's whole life, because the next preemption captures that DAIF into `SPSR_EL1` and `eret` faithfully restores it. The bootstrap therefore clears `A` as well as `I`; `F` and `D` stay masked deliberately, since nothing raises an FIQ and there is no debug-exception handling.

### Schedule after the EOI

A GICv2 CPU interface delivers nothing while an interrupt is active. Since `schedule()` switches stacks and does not return until the task runs again, scheduling before `GICC_EOIR` would leave the interrupt active for that whole period and the next tick would never arrive.

### Acknowledge the UART *before* draining it

The intuitive order — drain the FIFO, then clear the interrupt — can wedge the console permanently on QEMU. Its PL011 model (`hw/char/pl011.c`) hard-codes the receive trigger to 1 and raises RX with `if (read_count == read_trigger)`: an *equality*, so the interrupt fires only on the empty→non-empty transition. A byte landing between the last read and the `ICR` write therefore sets RX, our acknowledge clears it, and `read_count` never returns to zero — so the equality never holds again. QEMU never raises the receive *timeout* either (`INT_RT` is defined and masked but never asserted), so nothing rescues it. The console is dead for the rest of the boot.

Acknowledging first has no such window. `RTIM` is enabled anyway, because real PL011 parts trigger at a configurable FIFO level and need the timeout to deliver the tail of a short burst — but it is inert on the one platform this port currently runs on, and an earlier version of this chapter claimed the opposite as an observed fact.

## Stopping the machine

x86's test harness writes to QEMU's `isa-debug-exit` device and the verdict arrives as a process exit code. The `virt` machine has no such device, and aarch64 has no I/O ports for one to live behind.

The suite instead prints a sentinel and powers off through **PSCI `SYSTEM_OFF`**. The shutdown is what makes the contract four-valued rather than two:

| What happened | Verdict |
|---|---|
| PASS sentinel, QEMU exits | pass |
| FAIL sentinel, QEMU exits | fail — the `[FAIL]` lines say which |
| QEMU exits, no sentinel | fail — died mid-suite |
| no exit before the deadline | fail — hang |

Without the power-off the last two rows are the same timeout, and a kernel that panicked halfway through is indistinguishable from one that hung.

The same PSCI calls back the shell's `shutdown` and `reboot` (`SYSTEM_OFF` and `SYSTEM_RESET`). This is the one place where the aarch64 port is *better served than amd64* rather than catching up: PSCI's function IDs are fixed by the ARM specification, so there is nothing per-machine to discover. The x86 side has no equivalent — soft-off there needs `PM1a_CNT_BLK` out of the FADT, which needs an ACPI table parser this kernel does not have, so it writes to the addresses the emulators happen to end up with and reports plainly when none answers. See [Stopping and restarting a node](./architecture.md#stopping-and-restarting-a-node) for why the facade deliberately does not paper over that asymmetry.

## Running it

```bash
# Boot interactively (reduced shell over the PL011)
cargo xtask run --arch aarch64

# Run the kernel test suite on QEMU virt
cargo xtask test --arch aarch64

# Boot smokes — from a UEFI ESP, and from the shipped ISO
cargo xtask arm64-smoke
cargo xtask arm64-iso-smoke

# Build the arm64 ISO
cargo xtask iso --arch aarch64
```

The aarch64 ISO is UEFI-only (`BOOTAA64.EFI`); the amd64 one is a hybrid BIOS+UEFI image. Both are published by the release job.

## What comes next

Ring-3/EL0 is the next substantial piece, and it is what unlocks most of the deferred list. It brings `TTBR0_EL1` into use, gives `TPIDR_EL0` something to hold, turns `SVC` into the syscall path, and lets `Task::process_id` and the process table un-gate. The per-CPU block reached through `TPIDR_EL1` is already in place for the EL0 entry stub to read by offset.

After that: GICv3 (Graviton uses it), MMIO ECAM for PCI, and the VirtIO stack that storage, networking and containers all ride on.
