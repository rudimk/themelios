# The aarch64 Port

ThemeliOS targets x86_64 first and aarch64 second. This chapter describes what the ARM64 port *is*, what it deliberately is not, and the handful of architectural differences that shaped it.

> **Status**: Phase 8 in progress. The port has a **ring-0 kernel core**, **EL0 userspace**, a **working storage stack** (8.6) and, as of 8.7, **working networking**: the kernel mounts a SquashFS root under a RAM overlay plus an ext2 data volume, obtains a DHCP lease through a ring-3 smoltcp server, and answers `ls`/`cat`/`stat`/`write` and `ifconfig`/`ping`/`tcpconnect` at the shell. The Linux personality and containers on ARM are still deferred.

## Scope, stated plainly

The port covers boot, memory management on kernel-owned page tables, exceptions, interrupts, a preemptive scheduler, an interactive shell, VirtIO over the mmio transport (8.3, giving the block and network drivers), and — since 8.4/8.5 — EL0: user address spaces, the native syscall ABI, the process table, and the flat-binary userspace servers running at EL0. It does **not** yet cover the filesystem or socket servers above them, the Linux personality, or containers. "aarch64 support" here still does not mean "containers on ARM".

That boundary is visible in the test suite rather than left to prose: an aarch64 run reports **29 passed, 0 failed, 26 skipped**, and each skipped test names the subsystem that explains it. The total is 55 on both architectures, so the two runs are directly comparable.

## It runs on real Apple Silicon, with one caveat

Until recently the port had only ever run on this project's own QEMU invocations. It now
boots to the ThemeliOS console on **Apple Silicon under UTM**, which is the first time the
shipped image has started anywhere else.

Getting there took two fixes, and they were separate problems that looked like one:

* **A firmware-level fault before the kernel ran.** UTM showed EDK2's
  `Synchronous Exception at 0x47529074` and no ThemeliOS output at all. The faulting PC was
  low physical RAM while the kernel executes at `0xffffffff8…`, so the fault preceded the
  kernel's first instruction. Fixed by moving the bootloader from **Limine v8.7.0 to
  v10.8.5** — the only thing in the image that runs before the kernel.
* **A hang after the kernel ran.** Separately, on plain QEMU + HVF, the kernel booted fully
  and then hung forever: Apple Silicon under HVF delivered **no timer interrupts**, and two
  boot self-test loops were bounded only on a tick that consequently never advanced. Both
  now carry a `CNTVCT_EL0` bound, so a dead tick is reported rather than hung on.

**The caveat:** timer-interrupt delivery under HVF is still unexplained. The hang is gone,
but nothing has fixed delivery, so **do not assume preemption works on Apple Silicon**. A
boot there may well report `timer: FAIL — 0 IRQs` and run cooperatively. The next diagnostic
step is booting the same image with `-accel tcg` instead of `-accel hvf`: if the timer works
under TCG, the problem is interrupt injection rather than this port's GIC or timer setup.

The suite runs **42 of 55** here as of 8.7, with 13 written skips (16 when Phase 8 opened, 29 after 8.5, 35 after 8.6). amd64 runs 55/55.

(Phase 7 shipped this chapter saying 16 passed / 38 skipped. The passing count was right for the time; the skip count never was — 16 + 38 is 54, and the suite was 55. Corrected along with the 8.3 figures, and updated again at 8.5d and 8.6. These numbers are prose and go stale silently; the suite asserts its own arithmetic at startup, so trust a run over this paragraph.)

| Subsystem | x86_64 | aarch64 |
|---|---|---|
| Boot, serial console | ✅ | ✅ |
| Frame allocator, heap | ✅ | ✅ |
| Kernel page tables | ✅ | ✅ |
| Exceptions, interrupts, timer | ✅ | ✅ |
| Preemptive scheduler | ✅ | ✅ |
| Capability system, IPC, audit | ✅ | ✅ (and since 8.5d a real ring-3 *user* of them: a spawned server holds capabilities in its own CSpace) |
| Debug shell | ✅ | reduced (**24 of 28** commands — `procs`/`caps` at 8.5d, the six filesystem commands at 8.6, `ifconfig`/`sockets`/`ping`/`udpsend`/`tcpconnect` at 8.7) |
| Ring-3 / EL0 | ✅ | ✅ (8.4 address spaces + syscalls, 8.5 servers) |
| Process table, userspace servers | ✅ | ✅ (8.5d — `echo-server` spawns at EL0 and answers IPC) |
| VirtIO transport | ✅ virtio-PCI | ✅ virtio-mmio (8.3) |
| VirtIO block + network drivers | ✅ | ✅ (8.3 — the drivers themselves; the servers above them are ring-3) |
| Filesystems | ✅ | ✅ (8.6 — SquashFS root + RAM overlay + ext2 `/data`, ring-3 servers, FS syscalls from EL0) |
| Sockets, TCP/IP | ✅ | ✅ (8.7 — `net::socket` un-gated, ring-3 smoltcp server, DHCP, socket syscalls from EL0) |
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

### FPSIMD is saved per task, and FP is enabled at both ELs

Until Phase 8.4e there was **no FP save area**: the kernel is built for `aarch64-unknown-none-softfloat`, and `CPACR_EL1.FPEN` was *cleared* at boot so a stray SIMD instruction trapped loudly instead of corrupting another task's floating-point state. That backstop was verified rather than assumed — when the register was first *read* instead of asserted about, `FPEN` turned out to be `0b11`, so the net had never existed behind three comments claiming it did.

8.4e reverses it, because userspace must be hardfloat. There is no soft-float A-profile ABI (AAPCS64 defines one only for Armv8-R), glibc's *base* `strlen.S` opens with `ld1`, and the first lazy PLT binding in any dynamically-linked aarch64 binary executes SIMD. `FPEN` has four encodings and **none permits EL0 while trapping EL1** — `0b01` is the reverse — so enabling FP for userspace enables it at EL1 too.

Each task therefore carries 528 bytes of `v0`-`v31` + `FPCR`/`FPSR`, saved and restored **on the context switch and nowhere else**. The kernel emits no FP, so it cannot disturb a task's registers while handling that task's own exception; what can is another EL0 task running in between. That is why the exception frame has no vector fields — the same conclusion Linux reaches.

The removed trap is replaced by two guards: a **build-time assertion** that the target is softfloat (`target_feature = "neon"` is set for `aarch64-unknown-none` and unset for the softfloat target), which is stronger than the trap for the hazard that actually worried anyone; and a **boot self-test** that reads `CPACR_EL1` back, clears and re-enables `FPEN` to prove the enabling path works, and checks that a pattern in all 32 vector registers plus `FPCR`/`FPSR` survives `core::fmt`, the allocator and a context switch. `CPACR_EL1.ZEN` stays trapping and `HWCAP_SVE` is not advertised, because SVE's `z`/`p` state is not in the save area.

### A new task is entered by `ret`, not `eret`

The first switch to a task arrives out of the timer's IRQ handler, carrying the DAIF the CPU set on exception entry — where hardware masks all of `D`, `A`, `I` and `F`. Clearing only `I` would leave the others masked for the task's whole life, because the next preemption captures that DAIF into `SPSR_EL1` and `eret` faithfully restores it. The bootstrap therefore clears `A` as well as `I`; `F` and `D` stay masked deliberately, since nothing raises an FIQ and there is no debug-exception handling.

### Schedule after the EOI

A GICv2 CPU interface delivers nothing while an interrupt is active. Since `schedule()` switches stacks and does not return until the task runs again, scheduling before `GICC_EOIR` would leave the interrupt active for that whole period and the next tick would never arrive.

### Acknowledge the UART *before* draining it

The intuitive order — drain the FIFO, then clear the interrupt — can wedge the console permanently on QEMU. Its PL011 model (`hw/char/pl011.c`) hard-codes the receive trigger to 1 and raises RX with `if (read_count == read_trigger)`: an *equality*, so the interrupt fires only on the empty→non-empty transition. A byte landing between the last read and the `ICR` write therefore sets RX, our acknowledge clears it, and `read_count` never returns to zero — so the equality never holds again. QEMU never raises the receive *timeout* either (`INT_RT` is defined and masked but never asserted), so nothing rescues it. The console is dead for the rest of the boot.

Acknowledging first has no such window. `RTIM` is enabled anyway, because real PL011 parts trigger at a configurable FIFO level and need the timeout to deliver the tail of a short burst — but it is inert on the one platform this port currently runs on, and an earlier version of this chapter claimed the opposite as an observed fact.

### Two functions with the same name and different security guarantees (8.6)

The most instructive thing the storage port turned up was not about ARM at all.

x86 and aarch64 had each grown a `user_range_ok` / `copy_from_user` / `copy_to_user`
triple. Same three names; different arity, argument order, return type and safety — and,
far worse, a different guarantee. x86's range check walked the page tables and proved every
page of the range was mapped. aarch64's checked that the address was numerically inside the
EL0 regime and nothing more.

That difference is not a style question. An in-range-but-unmapped pointer passes a bounds
check, and the read then takes a data abort at EL1, which the Phase 7.2 handler treats as
fatal. `SYS_STAT` takes a user pointer from ring 3. So porting the filesystem dispatcher to
the aarch64 triple — which compiles, because the names match, and passes, because no test
hands it a bad pointer — would have given any ring-3 process a one-syscall node halt.

What prevented it was documentation that refused to round the difference off. `arch::syscall`
re-exported the x86 triple *only*, and said why: an earlier version had re-exported the
aarch64 pair under the same names and claimed the facade "flattens that difference away",
which it did not — "the two are unrelated functions that happen to share a spelling". It
also said unification was real work that "wants a live consumer to design against".

The filesystem syscalls were that consumer, one sub-phase later than predicted. There is now
one `arch::uaccess` with the stronger guarantee on both architectures, using the page-table
walk from x86 and the volatile byte-wise copies from aarch64 — the latter being the better
half too, since a `&[u8]` over user memory asserts that bytes another task may write will not
change. What remains of the old aarch64 pair is renamed `*_bounds_only`, because it cannot be
deleted: the strong check resolves through the process table, and the bare-EL0 self-tests drop
to EL0 with no process on purpose, so for them the strong check is not stricter — it is wrong.

Still missing on **both** architectures: the fixup. There is no exception table, so a user
copy that faults is fatal. The merge gave both the check; neither has the recovery.

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

# Boot smokes — from a UEFI ESP, and from the shipped ISO. Both boot a *diskless*
# machine on purpose, which is why the filesystem smoke below is separate.
cargo xtask arm64-smoke
cargo xtask arm64-iso-smoke

# The storage stack driven from the shell: boots with the real SquashFS root and ext2
# data volume attached, types ls/cat/stat/write over the serial console, and asserts
# the output (the Phase 8.6 acceptance test)
cargo xtask arm64-fs-smoke

# Does the kernel survive the GIC it was not written for? Asserts it detects a GICv3
# system-register interface and says so, rather than aborting on an absent GICC
cargo xtask arm64-gicv3-smoke

# Build the arm64 ISO
cargo xtask iso --arch aarch64
```

The aarch64 ISO is UEFI-only (`BOOTAA64.EFI`); the amd64 one is a hybrid BIOS+UEFI image. Both are published by the release job.

## What comes next

This section said for two sub-phases that "ring-3/EL0 is the next substantial piece" and
listed "the VirtIO stack that storage, networking and containers all ride on" as still to
come. Both were done by the time anyone read it — EL0 in 8.4/8.5, virtio-mmio in 8.3 — and
the paragraph is reproduced in the previous sentence rather than quietly replaced, because
the pattern is the point: prose about what is unfinished is exactly the prose that nothing
checks.

As of 8.7 the remaining work on this architecture is two sub-phase pairs and one open bug:

- **8.8–8.9 — the Linux personality.** `mod linux` is x86-gated in its entirety and holds
  both the ELF loader and the syscall personality. 8.8 makes it arch-neutral; 8.9 builds
  the aarch64 number table, which is `asm-generic/unistd.h` and a genuinely second table
  rather than a tweak. Five of the staged servers' `_start` routines also still issue Linux
  syscalls by their x86_64 numbers and are rewritten here.
- **8.10 — containers and the management API.** The parity gate. `mod container` cannot
  un-gate before `mod linux` does, because a container's whole job is to execute an
  unmodified Linux binary.

The open bug is **timer-interrupt delivery under HVF on Apple Silicon** — see the caveat
near the top. It is not on the parity path (QEMU/TCG and UTM both tick), and it is not
fixed, only made non-fatal.

Deliberately *not* on this list: real ARM server hardware. GICv3 proper, PCIe ECAM with
MSI-X, SMP, cloud NICs and NVMe get their own phase, written when parity lands. GICv3 was
briefly scheduled right after 8.5 on the belief that it was the Apple Silicon unblocker; a
boot log from a real Mac disproved that (`ID_AA64PFR0_EL1.GIC == 0` there, so that CPU
exposes no GICv3 system-register interface at all), and the reason for the priority went
with it. It is still worth doing for QEMU and server-class parity, which is a weaker claim
than the one that put it there.
