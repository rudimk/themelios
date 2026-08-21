# Phase 8 — aarch64 parity, then any SystemReady ARM64 node (plan, Momus-reviewed)

**Deliverable:** bring aarch64 from a *ring-0 kernel core* (where Phase 7 left it) to
**full feature parity with amd64** — EL0 userspace, storage, networking, containers, the
management API — and then onto **real ARM server hardware**, targeting a conformance
class rather than a vendor.

The single measurable definition of "parity" is already in the tree and already
checked by CI:

> `kernel/src/test_runner.rs` runs a **54-test** suite. On amd64, `SKIPPED` is empty and
> all 54 run. On aarch64, **16 run and 38 are skipped**, each with a written reason.
> **Parity is `SKIPPED.is_empty()` on both architectures.**

Every sub-phase below states how many `SKIPPED` entries it retires, and *which ones*.
A sub-phase that lands without shrinking `SKIPPED` (or without a stated reason why it
can't yet — three of them legitimately can't) has not delivered.

> ### Revision note — v2, after three adversarial review passes
>
> v1 of this plan was reviewed along three lenses (architectural claims, codebase
> grounding, ARM genericity + sequencing + falsifiability). All three returned REVISE.
> **Nine v1 claims were false**, including two that were the stated justification for
> *not* auditing the hardest part of the EL0 work, and one that would have made the
> aarch64 CI job reject every device QEMU hands it. The order of the whole phase is
> reversed from v1. Everything corrected is recorded in "Corrections from v1" at the
> foot of this file rather than quietly overwritten — the plan's own invariant 6 says
> claims get checked before they get written, and a plan that silently repairs itself
> teaches nobody which kind of claim to distrust.

## Scope note — the target is a conformance class, not a vendor

v1 said "Graviton first — it is the most constrained and therefore the most
informative." That is backwards. Graviton is the *least* informative single ARM64
target: UEFI + ACPI + GICv3 + PCIe, the textbook conformant case, with exactly one
vendor's device set. The genuinely constrained targets are the ones *without* ACPI,
*without* UEFI, or *without* PSCI.

**The target class is a SystemReady SR / SBBR-conformant ARM64 node:** UEFI + ACPI
static tables + GICv3 + PCIe + PSCI. AWS Graviton, Ampere Altra/AmpereOne, NVIDIA
Grace, and the Arm VM offerings of GCP and Azure are all *instances* of that class.
The plan is written against the class. **Any sub-phase that names a vendor must say
which class property it is exercising.** SystemReady **IR** (UEFI + devicetree) is a
second, explicitly scoped class. Non-UEFI device-tree boot — stock Raspberry Pi
firmware, raw U-Boot `booti` — is **out of scope by name** (see 8.11's note).

Genericity is not asserted in prose here. It is a CI matrix and a boot line — see
**decision 9** and **8.11**.

### How this relates to the roadmap's "Phase 8"

`CLAUDE.md` labels Phase 8 **"Hyperscaler support (AWS, GCP, Azure), secure boot."**
This plan **re-scopes** that entry rather than renumbering the roadmap: 8.1–8.10 are
aarch64 parity, 8.11–8.17 are the real-hardware work the label names. Parity is not a
detour — the hyperscaler items are dead weight until an ARM node can run a container.

## Grounding (verified against the tree at `4dcaa74`, plus primary sources)

Every claim here was read out of the source or fetched from a primary reference. Claims
that could not be verified are marked **UNVERIFIED** and written as checks the kernel
should perform, not assumptions the plan may make.

### The seam

- **The `#[cfg(target_arch = "x86_64")]` gate appears in 14 files — but that is a count
  of gate *sites*, not of gated *code*, and it is the wrong size signal.** `main.rs:155-236`
  is a module ladder gating out `process`, `drivers`, `fs`, `net`, `linux`, `container`,
  `mgmt`: **24 files, ~8,355 lines, 26% of the 31,670-line kernel**, none carrying a
  `cfg` of its own. Size estimates must be against the 8.4 kLoC, not the 14 files.
- **There is no `arch::syscall` and no `arch::cpu` facade.** `arch/` contains exactly
  `context.rs, irq.rs, mod.rs, paging.rs, serial.rs, time.rs`. `linux/thread.rs:24` is an
  *unconditional* `use crate::arch::x86_64::syscall::{copy_from_user, copy_to_user,
  SyscallFrame}`, and `:184`/`:204` call `arch::x86_64::cpu::swapgs()` directly — same in
  `linux/fs.rs` and `linux/syscall.rs`. **These are not `cfg`'d; there is nothing to
  un-gate.** A facade of ~15 items across 5 consumer files must be *created*
  (`SyscallFrame`, `copy_from_user`, `copy_to_user`, `user_range_ok`, `write_fs_base`,
  `set_kernel_stack`, `refresh_kernel_gs_base`, `swapgs`, `write_cr3`, `set_tss_rsp0`,
  `int3`, `exit_qemu`, `SYS_MGMT`, `init`, `test_syscall_round_trip`).

### Ring-3

- **The ring-3 surface is 53 `syscall` sites across 7 files, not 25 across 1.**
  `servers/libthemelios/src/lib.rs` holds 25 in `asm!` blocks. Six *more* files hold 28
  in `global_asm!` blocks of hand-written x86-64 assembly — entire `_start` routines:
  `isolation-smoke` (7), `linux-smoke` (6), `fs-smoke` (5), `threads-smoke` (5),
  `confine-smoke` (4), `elf-smoke` (1). `elf-smoke/src/main.rs:37-53` is representative:
  `mov r8, [rsp]` / `movzx r10, byte ptr [r9]` / a magic result value, with an argc/argv
  stack-ABI contract and a fixed result-page address. **These must be rewritten in
  aarch64 assembly, not rebuilt with `--target`.**
- There are **27 kernel syscall numbers** (`SYS_NULL`=0 … `SYS_MGMT`=26,
  `arch/x86_64/syscall.rs:112-238`).
- **`.cargo/config.toml` has no entry for the servers' target at all.** Whether EL0 is
  softfloat or hardfloat is an unmade decision, not a plumbing detail — and decision 8
  settles it.

### Memory

- **`AddressSpace::new_user` is broken on aarch64 — but not for the reason v1 gave.**
  `paging::KERNEL_ROOT_START` is 0 (`arch/aarch64/paging.rs:82`), so the clear loop
  `for i in 0..KERNEL_ROOT_START` (`page_table.rs:340`) is empty — **but fixing it is a
  no-op**, because the next loop (`:345`) is `for i in KERNEL_ROOT_START..512` and
  overwrites all 512 entries anyway. **The defect is in the copy loop.** And the tree
  copied is the kernel's **TTBR1** tree (`new_kernel` clones `paging::current_root()`,
  which is `read_ttbr1() & ADDR_MASK`, `paging.rs:601-603`) — a TTBR1 tree has no low
  half. What actually happens: a TTBR1-rooted tree loaded into TTBR0 re-presents the
  HHDM's 1 GiB blocks at low VAs, because L0/L1 index arithmetic is identical across the
  two translation regimes. HHDM base `0xffff_0000_0000_0000` + RAM base `0x4000_0000`
  lands at L0[0]/L1[1] — exactly where user VA `0x4000_0000` lands, which is precisely
  where `test_shared_memory` maps (`test_runner.rs:1709`). Mapping a 4 KiB page beneath a
  block then panics `ensure_table` (`page_table.rs:702`).
- **The teardown loop is the mirror-image bug and nothing has ever mentioned it.**
  `page_table.rs:594-597` walks `for l0_idx in 0..KERNEL_ROOT_START` to free user frames.
  On aarch64 that is empty, so once TTBR0 trees own frames, **a user address space frees
  nothing** — a silent per-process frame leak.
- **`USER_ADDR_LIMIT = 0x0000_8000_0000_0000` (`syscall.rs:723`) is wrong for aarch64 by
  a factor of two** — v1 called it "numerically correct for the wrong reason". T0SZ=16 ⟹
  48-bit TTBR0 input ⟹ TTBR0 owns `0x0` … `0x0000_FFFF_FFFF_FFFF`, exclusive limit
  **2^48**. The constant is 2^47, correct on x86 only because a *single* address space is
  split by the canonical hole. aarch64 has no hole: TTBR0 owns the low 2^48 and TTBR1 the
  top 2^48. Linux agrees (`TASK_SIZE_64 = 1 << vabits_actual` = 1<<48). Safe (conservative)
  but wrong, and it bites: Linux arm64 places high anonymous mmaps near 2^48, so a
  container binary mmapping high gets valid addresses rejected.
- **`verify_tcr` checks `T1SZ` only** (`paging.rs:331`) — "T0SZ = 16" is an assumption,
  not a verified fact. And Phase 7.1 disabled the low half via **`TCR_EL1.EPD0`**
  (`paging.rs:551-578`), which must be cleared when TTBR0 gets a real tree.
- **v1's claim that "aarch64 inverts x86's sense on both AP[2] and the XN bits" is half
  wrong, and the tree says so.** Only `AP[2]` is inverted: `DESC_AP_RO = 1 << 7`
  (read-only *when set*, vs x86's positive `WRITABLE`). But `DESC_AP_EL0 = 1 << 6` is
  *positive* (EL0 permitted when set), same polarity as x86's `USER`; and
  `DESC_PXN = 1 << 53` / `DESC_UXN = 1 << 54` are execute-never-when-set — **the same
  polarity as x86's `NO_EXECUTE`**. Inverting XN produces a silently executable user page.

### VirtIO

- **`VirtioTransport` already exists** — `drivers/virtio/mod.rs:616`, a concrete struct.
  The work is turning it into a trait with two impls, not inventing an abstraction.
- **Driver-level PCI entanglement is shallow: 3 sites, 2 types.** `virtio/blk.rs:35,101`
  and `virtio/net.rs:33,105` (`use pci::PciDevice`, `init_from_pci`), `virtio/mod.rs:642,651`.
  After `VirtioTransport::init`, both drivers are fully transport-agnostic.
- **The blast radius is the callers: ~35 sites, 31 of them in `test_runner.rs`** — using
  `pci::devices_by_vendor(VIRTIO_VENDOR_ID)` + a PCI class filter. Production sites:
  `net/mod.rs:90-95`, `fs/mod.rs:430-434`, `main.rs:497`. **virtio-mmio has no vendor ID
  and no class code** (it has a `DeviceID` register), so the discovery *predicate*
  changes, not just the enumeration. Those 31 test bodies are exactly the tests 8.6–8.10
  must retire.
- **`notify` is not a transport method.** It lives in `Virtqueue` (`mod.rs:263` field,
  write sites `:365`, `:483`) as a PCI-shaped per-queue doorbell computed at `:802-808`
  from `notify_base + notify_off * multiplier`. virtio-mmio has one shared `QueueNotify`.
- **`mod.rs:84-97` defines 14 `COMMON_*` offsets** — the virtio-PCI modern common-config
  layout. Every one of `status`/`set_status`/`read_device_features`/`write_driver_features`/
  `num_queues`/`setup_queue` is written against them. This is the bulk of the port.
- **The virtio stack has no interrupt path at all — it polls.** `mod.rs:817` writes
  `COMMON_QUEUE_MSIX_VECTOR = 0xFFFF` (NO_VECTOR) with the comment "we poll";
  `net/net_service.rs:33-34` confirms it. `VirtioTransport.isr` is `#[allow(dead_code)]`
  (`mod.rs:624`). **v1 invented GIC/SPI work for 8.4 that is not needed.**

### QEMU `virt` (verified against `hw/arm/virt.c`, `include/hw/arm/virt.h`, `hw/virtio/virtio-mmio.c`)

- virtio-mmio geometry: **`[VIRT_MMIO] = { 0x0a000000, 0x00000200 }`**,
  **`NUM_VIRTIO_TRANSPORTS 32`**, irqmap `[VIRT_MMIO] = 16` → SPI 16-47 → **INTID 48-79**.
  All four numbers correct. GICD `0x0800_0000`, GICC `0x0801_0000`, PL011 `0x0900_0000`,
  ECAM `[VIRT_PCIE_ECAM] = { 0x3f000000, 0x01000000 }`, redistributors
  `[VIRT_GIC_REDIST] = { 0x080A0000, 0x00F60000 }`.
- **QEMU `virt` defaults virtio-mmio to LEGACY (version 1), not modern.**
  `hw/virtio/virtio-mmio.c`: `DEFINE_PROP_BOOL("force-legacy", VirtIOMMIOProxy, legacy,
  **true**)`, and `hw/arm/virt.c`'s `create_virtio_devices` never overrides it. Known open
  issue (qemu-project/qemu #1342). **v1 had this exactly inverted and would have shipped a
  kernel that rejects every device QEMU offers.** The invocation needs
  `-global virtio-mmio.force-legacy=false`.
- **Device→slot mapping is REVERSED.** Verbatim from `create_virtio_devices`:
  *"qbus_realize() prepends (not appends) new child buses … `-device` options in
  increasing command line order are mapped to virtio-mmio buses with **decreasing** base
  addresses."* So the port **must scan all 32 slots and dispatch on `DeviceID`
  (0 = unpopulated)**, never on slot index.
- **The `virt` GIC version flips silently at 8 CPUs.** `finalize_gic_version_do` with no
  explicit `gic-version=`: GICv2 if `max_cpus <= 8`, else GICv3. Graviton instances are
  16-64 vCPU, so GICv3 is on the critical path, not an alternative.

### Linux personality

- `linux/syscall.rs:32-61` pins the x86_64 table: `SYS_EXIT`=60 (:51), `SYS_CLONE`=56
  (:48), `SYS_FUTEX`=202 (:49), `SYS_ARCH_PRCTL`=158 (:56). 27 constants, 22 match arms.
- **There is a SECOND, undeclared syscall table** at `linux/fs.rs:126-141`, written in
  **bare numeric literals** with only trailing comments: `0=>read, 1=>write, 2=>open,
  3=>close, 5=>fstat, 8=>lseek, 79=>getcwd, 80=>chdir, 217=>getdents64, 257=>openat,
  262=>newfstatat, 267=>readlinkat`. Twelve more hard-coded x86_64 numbers, 12 more arms.
  Magic numbers are strictly worse than named constants here — aarch64 `openat`=56 collides
  with x86 `SYS_CLONE`=56, and a missed arm falls through to `_ => return None`.
- **The dispatcher is written in x86 register names**: 77 `frame.r{ax,di,si,dx,10,8}`
  references across `linux/`.
- `linux/elf.rs:172` rejects anything but `EM_X86_64` (0x3e). `EM_AARCH64` = 183 = **0xB7**
  (`include/uapi/linux/elf-em.h`).
- **aarch64 Linux syscall numbers verified** against `include/uapi/asm-generic/unistd.h`:
  exit=93, exit_group=94, clone=220, futex=98, openat=56, write=64, writev=66, mmap=222
  (`__NR3264_mmap`), brk=214, ioctl=29, clock_gettime=113, getrandom=278,
  set_tid_address=96, gettid=178. All 14 correct; `open`/`fork`/`arch_prctl` absent, and
  there is no bare `stat` (only `fstat`=80, `fstatat`=79).
- **The absence list is much longer than open/stat/fork**, and every one is a
  path-clamping entry point: no `select`, `poll`, `pipe`, `dup2`, `rename`, `link`,
  `unlink`, `mkdir`, `readlink`, `chmod`, `access`, `getdents`, `epoll_create`,
  `epoll_wait` — only `pselect6`, `ppoll`, `pipe2`, `dup3`, `renameat2`, `linkat`,
  `unlinkat`, `mkdirat`, `readlinkat`, `fchmodat`, `faccessat`, `getdents64`,
  `epoll_create1`, `epoll_pwait`.
- **`clone`'s argument order differs, and the mechanism is invisible in the headers.**
  `arch/arm64/Kconfig` selects **`CLONE_BACKWARDS`**, which means "tls is the 4th argument
  of clone(2), not the 5th"; x86_64 does not select it. So aarch64 is
  `clone(flags, stack, parent_tid, tls, child_tid)` and x86_64 is
  `clone(flags, stack, parent_tid, child_tid, tls)`. **It is not the syscall number (220
  either way) and not anything in the asm-generic table** — it is a Kconfig symbol
  consumed by `kernel/fork.c`'s `SYSCALL_DEFINE5`.
  **The risk is worse than "garbage TLS pointer."** musl and glibc both set
  `CLONE_CHILD_SETTID`/`CLONE_PARENT_SETTID`, so the kernel *writes* the child TID through
  the `child_tid` pointer. Swap the arguments and the kernel performs **an
  attacker-influenced store into user memory** — a memory-corruption bug with a
  capability-kernel blast radius.

### The exception path

- **`ESR_EL1.EC = 0x15` is SVC-from-AArch64 — but it is not EL0-specific.** The encoding
  names the *instruction and execution state*, not the originating EL. An `svc` executed
  at **EL1** is also `EC = 0x15`, taken through the **`0x200`** (current EL, SP_ELx) group.
  A dispatcher keyed on EC alone in a shared common body would service a kernel-originated
  `svc` as a user syscall. Key on the **slot first**, then EC; keep `EC=0x15` fatal at 0x200.
- Phase 7 populates all 16 `VBAR_EL1` slots (`arch/aarch64/exceptions.rs`), 128 bytes each,
  CPU branching *into* the slot. Slot geometry is a hard constraint on the SVC entry.

## Cross-cutting invariants (non-negotiable, carried from Phase 7)

1. **amd64 stays fully green, every sub-phase.** The amd64 QEMU suite is the regression
   gate. Phase 7 shipped an aarch64 fix that regressed x86's `mem` command; only the suite
   caught it.
2. **The aarch64 suite is a gate too** (`cargo xtask test --arch aarch64`, live since
   `fd65f0a`), and **`SUITE_SIZE = 54` is asserted at the top of `run_tests()`** so gate
   drift is a loud failure, not a wrong total.
3. **`SKIPPED` shrinks monotonically**, per the accounting table below.
4. **Fresh branch + PR per sub-phase off latest `main`. Adversarial Momus review to
   APPROVE. CI green. Never auto-merge.**
5. **Every new test must be demonstrated falsifiable — mechanically, not by promise.**
   See decision 10. Phase 7's recurring failure was CI green on every broken version:
   vacuous `Ok(())` stubs, a console-wedging RX race, a 20%-flaky IPC race, and a
   timestamp-less audit log all passed.
6. **Claims are checked before they are written.** Phase 7's other failure was branches
   making false claims about themselves in code comments, `CLAUDE.md`, docs and PR bodies,
   three separate times. **v1 of this plan then did it nine more times.** See decision 10
   for the mechanical half.
7. **Atomic commits.** One idea per commit.
8. **Amd64-only refactors land alone.** Three sub-phases (8.1, 8.2, 8.8) change *working
   amd64 code* with zero intended behavior change. Each ships by itself so "amd64 green"
   is a checkable claim about that change and nothing else.

## Pinned decisions

1. **Syscall ABI on aarch64 = the kernel's own ABI, transliterated.** Number in **`x8`**,
   arguments in **`x0`-`x5`**, return in **`x0`**. *Not* because AAPCS64 leaves `x8` free —
   it does not; AAPCS64 gives `x8` the **indirect result location register** role, and
   `x9`-`x15` are equally free. The actual reason: **`x8` is what Linux and `asm-generic`
   use for the syscall number on aarch64**, so every toolchain, debugger, strace and libc
   already agrees. The Linux personality is a separate table layered on top.
2. **`svc #0`; the `ESR_EL1.ISS` immediate is ignored.** The number lives in `x8`,
   uniformly. Dispatch keys on the **vector slot** first, then `EC`.
3. **VirtIO transport = virtio-mmio first, PCIe ECAM later.** mmio needs no config-space
   enumeration, no BAR programming, no MSI-X — and MSI-X on real hardware needs the GICv3
   ITS, which is four sub-phases away (see 8.12/8.13). **The QEMU invocation must pass
   `-global virtio-mmio.force-legacy=false`**; without it QEMU offers version 1.
   Implement modern (v2) and reject v1 with a named message *that points at the missing
   flag*, so the failure is self-diagnosing.
4. **`copy_from_user`/`copy_to_user` bound = `2^(64 - T0SZ)`, computed once at paging init
   and *verified* against `TCR_EL1`,** not a hard-coded constant. `verify_tcr` grows a
   T0SZ check in the same style as its existing T1SZ one.
5. **EL0 gets its own `TTBR0_EL1` tree with a nonzero ASID; `TTBR1_EL1` is untouched.**
   Nothing is copied, so `new_user`'s bug cannot recur by construction. `TCR_EL1.EPD0` is
   cleared as part of this. `new_user`'s `kernel: &AddressSpace` parameter is `#[cfg]`'d
   away on aarch64 rather than left present-and-ignored — an unused kernel-root argument is
   exactly how this bug class returns.
6. **TLS = `TPIDR_EL0`, saved and restored per task.** No `arch_prctl` analog is invented.
7. **`SP_EL0`, `TPIDR_EL0`, `SPSR_EL1` and `ELR_EL1` are per-task context saved in the
   exception frame.** See 8.4 — this replaces v1's false "structurally absent" claim.
8. **EL0 is hardfloat; the kernel stays `-mgeneral-regs-only`; `v0`-`v31` + `FPCR`/`FPSR`
   are saved per task (520 bytes).** v1 recommended keeping FP trapped and making
   userspace softfloat. **That is unworkable and would have deleted the deliverable:**
   there is no soft-float aarch64 A-profile ABI (AAPCS64 defines one only for Armv8-**R**);
   glibc's *base* `sysdeps/aarch64/strlen.S` opens with `ld1 {v0.16b}, [src]`; musl ships
   SIMD `memcpy.S`/`memset.S`; and `sysdeps/aarch64/dl-trampoline.S` unconditionally
   saves `q0`-`q7`, so **the first lazy PLT binding in any dynamically-linked aarch64
   binary executes SIMD**. This is Linux's own design (kernel general-regs-only, userspace
   hardfloat, per-thread FPSIMD save) and it is a normal-sized piece of 8.4, not a research
   problem. **Do not advertise `HWCAP_SVE`, and leave `CPACR_EL1.ZEN` trapping** — Neoverse
   V1/V2 (Graviton 3/4) implement SVE, and a glibc ifunc resolver that selects SVE string
   routines would use register state we do not save.
9. **Genericity is a CI matrix and a boot line, not a prose claim.** Every boot prints one
   `platform:` line — firmware source (ACPI|DT), GIC version, UART type + base, CPU count,
   PA bits, timer frequency. CI boots the **same unmodified image** across the matrix in
   8.11 and asserts both that the sentinel matches *and* that the `platform:` lines from
   different machines **differ**. A kernel still secretly using hard-coded constants prints
   identical lines on two different machines, and that check catches it.
10. **Falsifiability is mechanical.** A `--features mutate` table in `test_runner.rs`, one
    entry per test un-skipped during Phase 8, each naming the mutation and the test it must
    break; `MUTATIONS.len() == MUTATIONS_REQUIRED` asserted at startup exactly as
    `SUITE_SIZE` is; and a CI job that runs each mutation and **fails if the run is green**.
    "Demonstrated falsifiable" becomes a compile-time count plus a CI matrix. Scoped to
    Phase 8 un-skips; not retrofitted.
12. **Virtqueue rings and buffers are Normal memory; only the MMIO register window is
    Device.** v1 said "Normal Non-cacheable **or Device** for the rings." **Device is wrong
    and would be a bug TCG also hides:** Device memory permits only naturally-aligned
    accesses (unaligned → Alignment fault) and has no defined exclusives/atomics, while
    rings are ordinary Rust structs touched by `ldp`/`stp`, `memcpy`, unaligned field
    access, and potentially LSE atomics on the avail/used indices. The pinned answer:
    **Device-`nGnRnE` for the MMIO registers only** (7.2's established path), **Normal
    Cacheable Inner-Shareable** for rings and buffers when the device is declared coherent,
    **Normal Non-Cacheable Inner-Shareable** when it is not. The coherent flag is
    *discoverable* — QEMU emits DT `dma-coherent` on every `virtio_mmio@` node, and ACPI
    `_CCA` carries it on real hardware — so this is a value to read (8.11), not a decision
    to guess, and 8.3 states its interim assumption explicitly.
11. **Device *discovery* is deferred to 8.11; device *description* is not.** v1's decision
    was "hard-code until 8.8, the constants are stable and known." **The constants were
    never the debt — the shape is.** Discovery yields `(compatible, base, size, irq)` tuples
    and a device *list*; hard-coded init functions take no arguments and know their own
    IRQ. Deferring the shape means 8.11 rewrites every driver signature *after* 19+ tests
    have frozen it. So: **8.3 lands `PlatformInfo { uart, gic, virtio_slots: &[PlatformDevice] }`
    with exactly one provider — a hard-coded QEMU-`virt` table — and every driver takes its
    base and IRQ from it.** 8.11 adds providers instead of performing a cross-cutting
    refactor. Cost today: one struct and one call site per driver.

## Sub-phases

Seventeen sub-phases plus a spike. **The order is reversed from v1**: the VirtIO transport
work goes first, EL0 second. Rationale in the sequencing section.

---

### 8.spike — throwaway EL0 round-trip spike

Mirrors `7.spike`: retire the highest-uncertainty item on a **throwaway branch** before
merged work depends on it. This is what discharges "do the biggest unknown first" — *not*
the ordering of the merged sub-phases.

- (a) Build a `TTBR0_EL1` tree, clear `EPD0`, map one page of hand-written EL0 code + one
  stack page, `eret` into it with `SPSR_EL1.M = 0b0000` (EL0t); confirm it executes.
- (b) From EL0, `svc #0` → confirm the `0x400` sync slot fires with `ESR_EL1.EC = 0x15`
  and `SP_EL1` is the kernel stack.
- (c) `eret` back and confirm the process continues — the full round trip.
- (d) `TPIDR_EL0` readable/writable from EL0 and surviving a round trip.
- (e) **Measure the vector-slot budget.** The SVC path needs a fuller register save than
  the fatal reporter's, in 128 bytes of slot.
- (f) **Confirm the `SPSR_EL1.M` hazard is real:** deliberately `eret` with `M = 0b0100`
  and confirm it returns to **EL1**; deliberately `eret` with `M = 0b0001` (Reserved) and
  confirm `PSTATE.IL` fires and the node halts. Both are the justification for 8.4's
  validation step; confirm them before writing the code that assumes them.
- (g) Confirm whether PAN prevents EL1 from touching EL0-accessible pages (this decides
  whether 8.4's alias test must run from EL0).

**Acceptance:** all seven answered, findings written back into this file. Throwaway.
**Riskiest unknown:** (e) — whether SVC entry fits the slot budget or needs the common-body
trampoline restructured.

---

### Tier 1 — VirtIO transport (first, not fifth)

#### 8.1 — Arch-neutral device discovery + `PlatformInfo`

Amd64-only, **zero intended behavior change**. Deliver: `virtio::devices()` returning
`(transport handle, device type)` replacing the `pci::devices_by_vendor` + class-filter
idiom at ~35 call sites (31 in `test_runner.rs`, plus `net/mod.rs:90-95`,
`fs/mod.rs:430-434`, `main.rs:497`); and `PlatformInfo` per decision 11 with one
hard-coded provider each for x86 and QEMU `virt`.

Wide but shallow, and it must land alone: a PR that moves 35 call sites *and* rewrites
the register layer is not reviewable, and "amd64 green" would be a claim about two
changes at once.

**Retires (0).**
**Acceptance:** the amd64 suite is green and **the built kernel's behavior is unchanged
by construction** — assert the same device set is discovered in the same order (print it
and diff against a committed baseline). Falsifiability: reorder the returned device list
and show the baseline diff fails.
**Riskiest unknown:** whether any of the 31 test bodies depend on PCI-specific fields
(BAR values, class codes) beyond identity. If so they need per-arch bodies, which is
work 8.3 inherits.

#### 8.2 — `VirtioTransport` → trait, PCI impl extracted

Amd64-only, **zero intended behavior change**. Deliver: the existing concrete
`VirtioTransport` (`virtio/mod.rs:616`) becomes a trait; the PCI implementation is
extracted behind it; the 14 `COMMON_*` offsets (`mod.rs:84-97`) move into the PCI impl;
**`notify` is hoisted out of `Virtqueue`** (`mod.rs:263`, `:365`, `:483`, doorbell computed
`:802-808`) into the transport, since mmio has one shared `QueueNotify` where PCI has a
per-queue doorbell. Trait surface: discovery + device-type predicate, reset, status,
feature negotiation, queue configuration, notify, device config, `set_driver_ok`.

**Not in the surface:** ISR/interrupt acknowledgement. `VirtioTransport.isr` is
`#[allow(dead_code)]` and the stack polls (`mod.rs:817` sets MSI-X to NO_VECTOR).

**Retires (0).**
**Acceptance:** amd64 suite green; `blk.rs`/`net.rs` contain zero references to `pci`.
Falsifiability: stub one trait method to return a wrong queue size and show storage tests
go red — proving the trait is actually on the path and not shadowed.
**Riskiest unknown:** the `Virtqueue` notify hoist. It touches the hot path of working
storage and networking.

#### 8.3 — virtio-mmio on aarch64

Deliver: the mmio transport implementation (modern/v2 register layout); MMIO mapping via
`mm::mmio` (Device-`nGnRnE`, the 7.2 path); **scan all 32 slots, dispatch on `DeviceID`,
never on slot index** — QEMU maps `-device` options to *decreasing* base addresses;
`-global virtio-mmio.force-legacy=false` added to the aarch64 invocation, with v1 rejected
by a message naming the missing flag; the aarch64 `PlatformInfo` provider populated with
the `virt` table; **the disk and NIC that the aarch64 QEMU invocation does not currently
have** (see 8.6's xtask note — `virtio-blk-device`/`virtio-net-device`, not the `-pci`
variants the amd64 path uses).

Also: **memory attributes for rings and buffers, per decision 12 below.** Rings are
**Normal** memory — Normal Cacheable Inner-Shareable if the device is declared coherent,
Normal **Non-Cacheable** Inner-Shareable if not. **Never Device.** Device memory permits
only naturally-aligned accesses and has no defined exclusives/atomics; rings are ordinary
Rust structs touched by `ldp`/`stp` and unaligned field access. Under TCG the wrong choice
appears to work.

And: **explicit barriers at the four virtqueue publish/consume points.** A missing
`dmb ishst` before publishing `avail->idx` or before the MMIO `QueueNotify` is invisible
under TCG (effectively sequentially consistent) and is a classic silent corruptor on
out-of-order Neoverse. This is a larger TCG blind spot than coherence and v1 did not
mention it at all.

**Retires (7):** `test_virtio_transport`, `test_virtio_queue_failure`, `test_virtio_blk`,
`test_block_server_ipc`, `test_virtio_net`, `test_net_service`, `test_pci_scan` (reframed).
The first six need **no EL0 whatsoever** — `drivers::block_server` is an in-kernel
`sched::spawn` task (`block_server.rs:120,138`), not a ring-3 server, and none of the six
references `spawn_server`/`embedded::`.
**`test_pci_scan` cannot be ported** (no port I/O on aarch64) and is retired by
*reframing*: an arch-neutral transport-discovery test with per-arch bodies. Its skip reason
was also wrong — aarch64 *does* have PCI config space, via ECAM at `0x3f000000`, which 8.13
implements. The accurate statement is "PCI configuration via the `0xCF8`/`0xCFC` port-I/O
mechanism is x86-only."
**Acceptance:** the seven run and pass on aarch64. Plus, per the Phase 7.2 lesson that one
tick would pass a dead timer: **(a)** a test reads back the leaf descriptor covering the
virtqueue region and asserts `AttrIndx` equals the intended MAIR index, printing both —
this fails the moment the attribute drifts, TCG or not; **(b)** boot emits a
`dma: coherent|maintained` line so 8.15 has something to grep rather than rediscover.
Falsifiability: point the base at a **hole** (`0x0a00_0000 - 0x1000`) and assert discovery
reports zero devices and fails closed with a named message. *Not* "one slot off" — on
`virt` that lands on another populated slot whose magic still reads `virt`, so the mutation
would pass and prove nothing. Separately assert the magic/version/device-id triple is
genuinely checked, by counting the empty slots traversed during a normal boot.
**Riskiest unknown:** the coherence attribute is decided here but the flag that determines
it (DT `dma-coherent`, which QEMU *does* emit on every `virtio_mmio@` node; ACPI `_CCA` on
real hardware) is not read until 8.11. Either pull a minimal coherency probe forward, or
state the hard-coded assumption **and** name the 8.11 task that replaces it. Do not let it
be a silent guess.

---

### Tier 2 — EL0 / ring-3

#### 8.4 — User address spaces, SVC entry, and the drop to EL0

v1 split this into 8.0 (address spaces) and 8.1 (SVC). **They merge**, because ASID
allocation and `TTBR0_EL1` activation cannot be *exercised* until something installs a
user address space — `test_shared_memory` calls `translate()` and `core::mem::forget`s
both spaces without ever loading `TTBR0`. A sub-phase whose central deliverable cannot be
tested is not a sub-phase.

Deliver: `AddressSpace::new_user` for aarch64 per decision 5 (empty low half by
construction; **fix the copy loop, not the clear loop**; `#[cfg]` the kernel-root
parameter away); **the teardown loop** (`page_table.rs:594-597`) so user spaces free their
frames instead of leaking them; user leaf encoding (`AP[2]` inverted, `AP[1]` positive,
`PXN`/`UXN` **same polarity as x86** — see grounding); ASID allocation, `TTBR0_EL1`
activation, `TCR_EL1.EPD0` cleared, `TLBI ASIDE1IS` with the 7.1 barrier discipline;
`verify_tcr` extended to T0SZ; the `arch::syscall` / `arch::cpu` **facades that do not
exist** (~15 items, 5 consumers); an aarch64 `SyscallFrame`; `copy_from_user`/`copy_to_user`
bounded by `2^(64 - T0SZ)` per decision 4; the `0x400` sync slot decoding `EC = 0x15` into
syscall dispatch, **keyed on slot first** so an EL1-originated `svc` stays fatal;
`TPIDR_EL0` TLS; the `v0`-`v31` + `FPCR`/`FPSR` save area per decision 8; `sched`'s ring-3
fields un-`cfg`'d with aarch64 meanings.

**The four race classes, audited — v1's reasoning here was wrong twice:**

- **`SP_EL0` — the hazard is PRESENT and RELOCATED, not absent.** v1 claimed the x86
  shared-scratch-slot bug was "structurally absent" because `SP_EL0` is a banked register
  rather than a memory slot. The premise is true (`AArch64_TakeException` sets
  `PSTATE.SP = '1'` and does not write `SP_EL0`) and the conclusion is false: **`SP_EL0` is
  banked by exception level, not by task.** It is exactly as CPU-global as `gs:0x8` was.
  The 4.5 bug was never about EL transitions — it was about *preemption*. Leave the user SP
  live in `SP_EL0` across a syscall and: task A takes `svc`; timer IRQ; `schedule()` to
  task B, also mid-syscall; B's exit writes `msr SP_EL0, <B's user SP>` and `eret`s; A is
  rescheduled, reaches its exit tail, and `eret`s onto **B's user stack**. Byte-for-byte
  the same bug, one register substituted for one memory slot.
  **The invariant:** `SP_EL0` is per-task context, saved into the task's exception frame on
  entry and restored from it with interrupts masked immediately before `eret`, never read
  back live after any window where preemption could occur. The tree already reasons this
  way about neighbouring registers — `exceptions.rs:204` says of `ELR_EL1`/`SPSR_EL1` that
  they are *"single system registers, not per-task storage."* The identical logic applies
  to `SP_EL0`, `ESR_EL1` and `FAR_EL1`.
- **`TPIDR_EL0`** — the same fix applied to a second register. `TPIDR_EL1` was already
  structurally fixed in 7.3 (rewritten on every switch); `TPIDR_EL0` needs the same, and
  the current context switch (`context.rs`) saves x19-x30 only, 96 bytes, with neither
  register in it.
- **Exception-return atomicity, and it is a *stronger* requirement than x86's.** v1 said
  the `sysretq` non-canonical-RCX hazard "gets deleted, not ported." **False.** The narrow
  point about a bad `ELR` faulting in EL0 is fine, but two hazards reach EL1 through a
  corrupted `SPSR_EL1` — the register the tree already calls CPU-global:
  **(a) privilege escalation.** `M[3:0]` = `0b0000` is EL0t, `0b0100` is EL1t.
  `IllegalExceptionReturn` rejects only returns to a *higher* EL, so EL1→EL1 is **legal**.
  **One bit** makes `eret` return to EL1 at whatever `ELR_EL1` holds. On x86, `sysretq`
  forces ring 3 by construction; on aarch64 the target EL is a **data field in a
  clobberable system register**. `eret` is *more* dangerous here, not less.
  **(b) a user-reachable node halt.** `M = 0b0001` is Reserved; `SetPSTATEFromPSR` takes
  the `illegal_psr_state` branch, setting `PSTATE.IL = 1` and **skipping** the assignment
  of `PSTATE.EL`/`PSTATE.SP`. The PE stays at **EL1 on `SP_EL1`**, and the next fetch
  raises Illegal Execution State from EL1 to EL1 — the `0x200` group, which ThemeliOS
  treats as fatal. That is the exact shape `syscall.rs:468` exists to prevent on x86.
  **Requirement:** construct `SPSR_EL1` from a constant for the EL0 drop, **validate
  `M == 0b0000` immediately before `eret`**, and restore `SPSR_EL1`/`ELR_EL1`/`SP_EL0`
  under masked interrupts.
- **SError in the exit window.** `exceptions.rs:491` clears `DAIF.A` at boot and 7.3 made
  every task inherit unmasked SError deliberately. Today that is safe because an SError at
  EL1 is fatal, so clobbered `ELR`/`SPSR` never matter. Once this sub-phase makes the
  lower-EL sync path *resumable*, an SError between restoring the three registers and the
  `eret` clobbers all three. Either mask the full set (`msr DAIFSet, #0xf`) across the exit
  tail, or state explicitly that SError-at-EL1 stays fatal by design so the clobber is
  unreachable. Decide it here, with the other three.

**Retires (3):** `test_shared_memory`, `test_syscall`, `test_path_resolve`.
**`test_syscall` is the second test that cannot be retired by porting**, and v1 did not
notice. Its body (`arch/x86_64/syscall.rs:1406-1446`, called from `test_runner.rs:770`) is
entirely x86 MSR verification — `EFER.SCE`, `STAR[47:32]`, `STAR[63:48]`, `LSTAR`,
`FMASK & RFLAGS_IF` — then an in-kernel `syscall_dispatch`. **It never enters ring 3.**
There is no aarch64 counterpart to any of it. Retiring it means writing a *different test
under the same name* (`VBAR_EL1` installed, `ESR_EL1.EC == 0x15` decoded, dispatch on an
aarch64 `SyscallFrame`), which needs its own mutation entry per decision 10.
`test_path_resolve` (`test_runner.rs:3860-3883`) is pure string logic over
`linux::fs::resolve_path` with a 10-case table — no ring-3, no ELF, no storage. A one-line
`#[cfg]` split exposing `resolve_path` alone retires it here, and since it is a
*container-escape* test, having it green early has real value.
**Acceptance — v1's was impossible to perform.** v1 said "corrupt the ASID and show the
test fails", but `test_shared_memory` never loads `TTBR0_EL1`, so no ASID is on that path
and the mutation cannot fail. Instead: **install the user space in `TTBR0_EL1`; write a
sentinel through user VA `A`; read it back through user VA `B` (same frame, different VA)
and through the HHDM — three views, one frame, all agreeing. Then install a second space
with a different frame at VA `A` and prove the read changes.** Falsifiability: (i) omit the
`TLBI ASIDE1IS` on switch and show the second read returns the stale frame; (ii) give both
spaces the same ASID and show the same. Per 8.spike(g), if PAN blocks EL1 from touching
EL0 pages, this runs from EL0.
Plus: an EL0 blob performing `SYS_DEBUG_PRINT` and `SYS_EXIT`; and a **soak with a
predicate** — v1 said "≥1000 syscalls under preemption is clean", which 1000 syscalls with
interrupts accidentally masked throughout would satisfy, the exact opposite of the point.
Instead: **N syscalls each returning a value derived from its arguments, every return
asserted; the tick count must advance by ≥K across the soak; per-task residency must be
non-degenerate** — proving preemption actually happened. Name which assertion goes red for
each injected fault.
**Riskiest unknown:** the exception-return race class. The x86 versions of these were
2-in-10 flakes that grew from rare to majority-of-runs as the suite added tasks. A soak with
a real predicate is mandatory.

#### 8.5 — `libthemelios`, six hand-written `_start`s, and the server toolchain

**v1 scoped this as "one file plus a linker script plus xtask plumbing." It is seven files
and 53 syscall sites.** Deliver: aarch64 counterparts for the 25 `asm!` blocks in
`servers/libthemelios/src/lib.rs` (`svc #0`, `x8`/`x0`-`x5`); **aarch64 rewrites of the six
`global_asm!` `_start` routines** in `elf-smoke`, `linux-smoke`, `fs-smoke`,
`threads-smoke`, `isolation-smoke`, `confine-smoke` (28 more sites), including their
argc/argv stack-ABI contracts, fixed result-page addresses and kernel-side result-code
mappings; a `.cargo/config.toml` entry for the servers' aarch64 target (hardfloat, per
decision 8); `xtask` target parameterization — noting v1's line references were wrong:
`x86_64-unknown-none` appears at `main.rs:235,247` (in `build_servers`), `:288,298` (in
`build_detached_elf`), and `:1759` (in `cmd_docs`, unrelated to servers).

**And the thing that will silently produce a broken kernel if missed:** `build_servers`
stages to `target/servers/*.bin` and `build_detached_elf` to `target/servers/*.elf` —
**not arch-qualified** — and `kernel/src/process/embedded.rs:18-73` does **13 unconditional
`include_bytes!`** against those fixed paths. Parameterize the target without partitioning
the staging directory and an arm64 kernel built after an amd64 build embeds **x86 blobs**,
failing as an undefined-instruction abort at EL0, arbitrarily far from the cause. Deliver
per-arch staging dirs, `#[cfg]`'d `include_bytes!`, **and a build-time assertion that the
staged blob's ELF machine matches the kernel's target.**
Also: `cmd_build` calls `build_servers` unconditionally even for `--arch aarch64`
(`main.rs:1394-1403`), and `cmd_test --arch aarch64` never builds servers at all
(`:1586-1589` returns into `cmd_test_aarch64` before the call). Both need restructuring.

**Retires (4):** `test_process`, `test_userspace_init`, `test_server_spawn`,
`test_registry_pull`. v1 put `test_registry_pull` in the container sub-phase; it belongs
here. `test_runner.rs:4540-4589` uses `crate::oci::{registry, sha256}` (`mod oci` is
already un-gated), an in-process `MockConn`, and `process::embedded::LINUX_SMOKE` (`:4548`)
purely as **tar payload bytes, never executed**. Its only blocker is `mod process`/`embedded`
existing on aarch64 — no container runtime, no storage, no network, no management ABI.
**Acceptance:** `echo-server` runs at EL0 on aarch64 and completes an IPC round trip; the
four tests pass; **amd64 server binaries byte-identical to before** — and the `.bin` hashes
are **committed and checked in CI**, not diffed once by hand. (This was v1's strongest
acceptance line and it was squandered as a manual check.)
**Riskiest unknown:** the flat-binary link — but aimed correctly. `servers/linker.ld`
contains **no `OUTPUT_FORMAT`, no `OUTPUT_ARCH`, nothing arch-specific**: just
`ENTRY(_start)`, `. = 0x200000`, four section placements and a `/DISCARD/`. It is very
likely arch-neutral as-is, so try it before writing a sibling. The real hazard is
`--oformat=binary` + aarch64 relocations + `.rodata`/GOT placement under
`-Crelocation-model=static`, which the script does not control. Budget link-map reading for
the relocation behavior.

---

### Tier 3 — un-gate the arch-neutral stack

#### 8.6 — Storage on aarch64

Un-gate `mod fs`. Deliver: `squashfs-server`, `overlay-server`, `ext2-server` at EL0; the
VFS capability path; the shell's `ls`/`cat`/`mount`/`stat`/`write`/`mkdir`.

**And the xtask/CI work v1 asserted was already done.** v1 said xtask "already produces the
SquashFS + ext2 images for the aarch64 boot — the images are arch-neutral data." The
*content* is arch-neutral; **the plumbing does not exist.** `ensure_images` is called at
`main.rs:1528` and `:1661` only, both inside amd64-only paths; `run_aarch64` (`:869-890`)
and `cmd_test_aarch64` (`:381-484`) call neither it nor `ensure_scratch_disk` nor any disk
`-drive`. `qemu_aarch64_base` (`:933-941`) attaches two `pflash` drives and one ESP.
Also: **the arm64 CI job installs `qemu-system-arm qemu-efi-aarch64 xorriso`
(`build.yml:101-104`) but not `squashfs-tools e2fsprogs`** (which the amd64 job does,
`:48-51`) — the moment `ensure_images` runs on the aarch64 path, the job dies on a missing
binary.
Note also that the ESP is already on the virtio-mmio bus (`if=virtio` binds to mmio on
`virt`), so slot 0 of the window 8.3 enumerates is the boot FAT image — discovery must
survive that, and the amd64 path's documented "order fixes PCI slot assignment" idiom
(`:1523-1530`) has no mmio equivalent.

**Retires (6):** `test_squashfs_server`, `test_overlay_server`, `test_ext2_read`,
`test_ext2_write`, `test_vfs_capability`, `test_fs_syscalls`.
**Acceptance:** the six pass; **`ls /` and `cat` driven from `cmd_test` over the serial
path with asserted output** — v1 said the shell commands "work interactively," which is
precisely the shape of claim invariant 6 forbids, since nothing checks it.
**Riskiest unknown:** the memory-ordering half. 8.3 got the attributes right; the barrier
discipline is only *exercised* here, under load, and TCG will not expose a missing one.

#### 8.7 — Networking on aarch64

Un-gate `mod net`. Deliver: `net-server` at EL0 (smoltcp is already portable —
`servers/smoltcp-gate` has compile-gated it since 7.0c); the socket syscalls; DHCP; the
shell's `ping`/`ifconfig`/`sockets`/`udpsend`/`tcpconnect`; the `virtio-net-device` and
`hostfwd` plumbing on the aarch64 QEMU path plus the host-side peer thread the amd64 path
has (`main.rs:1659-1672`).

**Retires (7):** `test_net_server_stack`, `test_net_icmp_echo`, `test_dhcp`,
`test_socket_capability`, `test_socket_list`, `test_udp_echo`, `test_tcp_client`.
**Acceptance:** the seven pass; an aarch64 guest answers a host `ping` and completes a TCP
round trip.
**Riskiest unknown:** the RX-recycling issue already documented as a Phase 6 deferral. It is
a latent *amd64* defect, and a second architecture exercising the path is a plausible way to
finally surface it. That would be a good outcome; budget for it landing here.

---

### Tier 4 — Linux personality and containers

#### 8.8 — Arch-neutral Linux dispatcher

Amd64-only, **zero intended behavior change**, landing alone per invariant 8. Deliver: the
77 `frame.r{ax,di,si,dx,10,8}` references across `linux/` renamed to `arg0..arg5`/`ret`;
**both** syscall tables extracted — the 22 named constants in `linux/syscall.rs:32-61` and
the **12 bare numeric literals in `linux/fs.rs:126-141`** that v1 did not know existed;
`linux/elf.rs`'s machine check parameterized.

**Retires (0).**
**Acceptance:** amd64 suite green; the extracted x86_64 table is provably identical to the
constants it replaced (diff the list); `linux/` contains zero `r`-prefixed register names.
**Riskiest unknown:** the `fs.rs` table's magic numbers. A missed arm falls through to
`_ => return None` rather than failing loudly, so extraction must be mechanical and
reviewed against the trailing comments, not retyped.

#### 8.9 — The aarch64 Linux table

Deliver: the `asm-generic/unistd.h` numbers (verified list in grounding); **the full `*at`
migration** — no `open`/`select`/`poll`/`pipe`/`dup2`/`rename`/`link`/`unlink`/`mkdir`/
`readlink`/`chmod`/`access`/`getdents`/`epoll_create`/`epoll_wait`, only the `*at`/`*2`/`*1`
forms, and **every one is a path-clamping entry point**, so enumerate them rather than
discovering them one `ENOSYS` at a time; `arch_prctl` has no analog and TLS is set directly
via `TPIDR_EL0` (8.4 plumbed it); `EM_AARCH64` (0xB7) accepted, checked against the build
target; the `clone` argument order per `CLONE_BACKWARDS`.

**Retires (4):** `test_elf_exec`, `test_linux_exec`, `test_linux_fs`, `test_linux_threads`.
Note `test_linux_exec`'s result mapping (`test_runner.rs:3850`) asserts
`"linux-smoke: arch_prctl(SET_FS)/TLS check failed"` — a check with **no aarch64 analog**.
The aarch64 `linux-smoke` needs a different assertion set and the kernel-side mapping must
change with it.
**Acceptance:** the aarch64 `linux-smoke`/`fs-smoke`/`threads-smoke` binaries run to
completion; the four tests pass; the amd64 table is provably unchanged.
Falsifiability for `clone`: **assert the child TID landed at the address the caller passed
as `child_tid`** — not merely that a thread started. With the arguments swapped and
`CLONE_CHILD_SETTID` set (musl and glibc both set it), the kernel writes a TID through
whatever was passed as `tls`: an attacker-influenced kernel store into user memory. The test
must fail on the swap, and a test that only checks "a thread ran" will not.
**Riskiest unknown:** the `clone` order, precisely because the mechanism is invisible in
every header the implementation will consult — it is a Kconfig symbol, not a number.

#### 8.10 — Containers, management API — **PARITY**

Un-gate `mod container`, `mod mgmt`, the `api-server` spawn. Deliver: the container runtime;
the registry client; the management ABI and its sentinel capability; `api-server` at EL0.
Container images must be **aarch64** images. **And the shell commands nothing else claims**
— `shell/mod.rs:77-110` gates 17 commands; 8.6 and 8.7 take eleven between them, leaving
`run`, `ps`, `logs`, `stop`, `procs`, `caps`, plus the arch-split `cmd_help` body
(`commands.rs:47/68`) to reconcile. **Without this, `SKIPPED.is_empty()` is true with six
commands still `#[cfg]`'d out, and the parity gate does not check parity.**

**Retires (7 — the last):** `test_container_run`, `test_container_isolation`,
`test_container_confinement`, `test_container_registry`, `test_container_logs`,
`test_management_capability`, `test_api_server`. (`test_api_server` is listed under the
network group but needs the management ABI *and* — its phase 3 is a live inbound smoke over
`hostfwd 127.0.0.1:15007 → guest:7` with a host-side peer, `test_runner.rs:5399-5463` —
8.7's xtask plumbing. Phases 1-2 are deterministic and in-process.)
**Acceptance — the parity gate:** `SKIPPED` is **empty** on aarch64; **54 running, 0
skipped on both architectures**; zero `#[cfg(target_arch)]` remaining in `shell/mod.rs`; a
container runs on an ARM node and `GET /containers/json` returns it. Update `CLAUDE.md`,
`docs/src/milestones.md`, `docs/src/aarch64.md` **after** running it.
**Loophole to close:** a skip can be retired by *weakening* the test — an early
`return Ok(())` or a `#[cfg]` branch inside the body. Mechanical guard: a CI lint over
`test_runner.rs` asserting no test named in a committed `PORTED_IN_PHASE8` list contains a
`#[cfg]` or an arch-conditional early return within its function body.

---

### Tier 5 — any SystemReady ARM64 node

#### 8.11 — Platform discovery: ACPI, device tree, and the genericity matrix

**Three cases, not two.** (1) **SR/SBBR** — UEFI + ACPI, and **no DT exists at all**
(Graviton, Ampere servers, Grace, cloud Arm VMs). (2) **IR/EBBR** — UEFI + DT, the DTB
arriving through the **EFI configuration table**, which is where Limine's DTB request gets
it. (3) **non-UEFI** — DTB in `x0`, no boot services, no Limine. Code shaped as "DTB always
present, ACPI optional" inverts on case 1.
**UNVERIFIED:** whether Limine's DTB request is ever populated on an ACPI-only machine.
Assume not; fail closed.

Deliver: a `Platform` provider interface with providers `Acpi` (RSDP from Limine's RSDP
request), `DtFromEfi` (DTB from Limine's DTB request), selection ACPI-then-DT decided once
at boot, and a node presenting neither **halting with a named message rather than falling
back to constants**. Static ACPI tables consumed: RSDP/XSDT, **FADT** (PSCI conduit +
reset), **MADT** (GIC version, distributor/redistributor bases, CPU list), **GTDT** (timer
PPIs, `CNTFRQ` override), **MCFG** (ECAM), **SPCR** (UART **type** *and* base — SPCR
distinguishes PL011 / 16550 / SBSA Generic UART, so "UART" is at least two drivers),
**IORT** (PCIe RequesterID → ITS DeviceID and SMMU StreamID — without it, MSI writes go to
an ITS that has never heard of the device and **silently vanish**), **PPTT** (CPU topology,
needed by 8.14), **TPM2** (for 8.17).
**"Static tables only, no AML" holds** for SR-class servers — reset and poweroff come from
PSCI declared in FADT's ARM boot flags, not AML, which is the usual reason a static-only OS
gets dragged into an interpreter. A *required* device describable only from the DSDT
namespace is a hard failure with a named message, not a silent degradation.

Also, three things v1 pinned that must instead be **read**:
- **`TCR_EL1.IPS` from `ID_AA64MMFR0_EL1.PARange`**, not a constant. Not hypothetical:
  `-machine virt,highmem=on` puts the high PCIe window above 512 GiB, and a pinned IPS
  truncates or faults there.
- **Verify `ID_AA64MMFR0_EL1.TGran4`** and halt with a named message otherwise. Two
  instructions, converting "4 KiB is available everywhere, probably" into a checked fact.
  **UNVERIFIED** whether any target of interest lacks it — which is the reason to check.
- **Cache line size from `CTR_EL0`, minimum across CPUs.** This is the real
  heterogeneous-core hazard and it lands in 8.6's maintenance loops: a loop written against
  the boot CPU's `DminLine` under-maintains on another core. Print `MIDR_EL1` per CPU so an
  erratum report has something to name.

**Boot chain: UEFI only.** ThemeliOS targets SystemReady **SR** and **IR**. Limine on
aarch64 is UEFI-only; that covers SR and — via U-Boot's UEFI implementation, which
chainloads `BOOTAA64.EFI` and passes the DTB as an EFI config table — most EBBR boards.
Platforms that boot only the raw Linux `Image` protocol (stock Raspberry Pi firmware,
`booti` without U-Boot's EFI loader) are **out of scope for Phase 8** and would need a
dedicated sub-phase: `ARM\x64` header, MMU-off entry, DTB in `x0`. Saying this is the
difference between a scoped port and an unbounded one.

**Retires (0).**
**Acceptance — decision 9's matrix, booting the SAME unmodified image:**

| Invocation | What it proves |
|---|---|
| `-M virt,gic-version=2,acpi=off` | the DT path is real (QEMU emits DT, no ACPI) |
| `-M virt,gic-version=3` | ACPI path, GICv3, ECAM |
| `-M virt,gic-version=3,highmem=on -m 16G` | `PARange`-derived IPS; high MMIO window |
| `-M virt,virtualization=on` | EL2 present — the condition that made 7.2 choose `CNTV` |
| `-cpu cortex-a53 / neoverse-n1 / max` | no implicit CPU-feature assumptions |
| **`-M sbsa-ref`** | **the genericity test** |

**`sbsa-ref` is the cheap, concrete answer to "runs on an arbitrary SystemReady node"**:
GICv3, ACPI, TF-A + EDK2 firmware, PCIe with E1000E/AHCI, **no virtio-mmio at all**, an
entirely different memory map — and it costs a firmware image, not hardware. Its
expectation is scoped honestly: boot + discovery + UART + timer + GIC + the device-free
portion of the suite. It has no virtio, so storage and networking stay skipped **there**
with a written reason (a per-machine skip list, not a per-arch one).
CI asserts the sentinel matches on every row **and that the `platform:` lines from `virt`
and `sbsa-ref` differ.** A kernel still secretly using constants prints identical lines on
two different machines.
**v1's falsification test did not test what it claimed:** `-machine virt,highmem=on` does
**not** relocate `virt`'s low map — PL011 stays at `0x0900_0000`, GICD at `0x0800_0000`,
virtio-mmio at `0x0a00_0000`. It adds a high window. A kernel hard-coding every low-map
constant passes it unchanged. Keep it as the `PARange`/IPS row, which is what it actually
exercises; the positive test is a *different machine model*.
**Riskiest unknown:** ACPI scope discipline. A DT parser is a few hundred lines of
well-specified big-endian walking; the risk is scope creep into AML. Hold the line at static
tables and fail loudly.
**UNVERIFIED:** whether any target's firmware leaves an SMMU enabled rather than in bypass
(IORT names it) — worth a boot-time check rather than an assumption.

#### 8.12 — GICv3 + ITS (still uniprocessor)

Deliver: the system-register CPU interface (`ICC_SRE_EL1` — **written and read back to
confirm it stuck**, since it may be RAZ/WI or trap unless a higher EL enabled
`ICC_SRE_EL2/EL3`; `ICC_IAR1_EL1`, `ICC_EOIR1_EL1`, `ICC_PMR_EL1`, `ICC_IGRPEN1_EL1`),
behind the existing `arch::irq` facade with runtime v2/v3 selection from 8.11.

**Three things v1's register list omitted, without which no interrupt ever arrives:**
- **`GICD_CTLR.ARE_NS`** — Affinity Routing must be enabled in the distributor before the
  `ICC_*` interface routes anything. This is the missing step between `ICC_SRE_EL1.SRE=1`
  and the first interrupt.
- **`GICR_ISENABLER0` / `GICR_IGROUPR0`** at the redistributor's SGI frame
  (`RD_base + 64 KiB`). In GICv3, SGI and **PPI** enable/group/priority/config moved off
  `GICD_*` onto the per-CPU redistributor. **Phase 7.2's `CNTV` tick is PPI 27** — a
  GICv2-shaped driver that keeps enabling INTID 27 through `GICD_ISENABLER` does nothing,
  and the failure looks like "the timer stopped," not "the GIC is misprogrammed."
- **`GICD_IROUTER<n>`** (offset 0x6000, 8 bytes per SPI) — with ARE=1, SPI targeting is by
  affinity, not `GICD_ITARGETSR`.

**And the ITS, which v1 did not mention at all.** On every Tier-5 platform the devices are
PCIe and the interrupts are **MSI-X**, which on GICv3 means **LPIs via the ITS**: command
queue, device table, collection table, `MAPD`/`MAPTI`/`INV`/`SYNC`, LPI property + pending
tables, `GITS_TRANSLATER` doorbell, DeviceID from IORT. This is not an optimization —
several PCIe endpoints do not wire legacy INTx at all, so "INTx for now" may not be
available. GICv2m MSI frames are out of scope; a GICv2 platform with PCIe is unsupported
and says so. A non-GIC interrupt controller (bare-metal Apple silicon uses AIC) fails loudly
rather than mis-detecting.
**GICv4's deferral is safe** and the plan should say *why* rather than just listing it: v4
adds direct injection of virtual interrupts for a hypervisor, and is v3-compatible for
everything a non-hypervisor OS does.

**Retires (0).**
**Acceptance:** boots under `-M virt,gic-version=3`; the 7.2 tick self-test (five ticks, not
one) passes **through the redistributor path**; an ITS-delivered MSI is counted.
**Riskiest unknown:** silent GIC-version flips. `virt` selects GICv3 automatically at >8
CPUs, so `-smp 16` changes the interrupt controller with no diagnostic — which is also why
GICv3 is on the critical path for Graviton-class (16-64 vCPU) instances rather than being
an alternative to GICv2.

#### 8.13 — PCIe ECAM + MSI-X

Deliver: ECAM enumeration (MCFG from 8.11; `0x3f000000` on `virt`), BAR programming, MSI-X
capability programming with DeviceIDs mapped through IORT, and a third `VirtioTransport`
impl for virtio-PCI-on-ECAM.

**This sub-phase is why 8.12 precedes it.** v1 put ECAM in the discovery sub-phase, *before*
GICv3 — but ECAM gets you config space and BARs, not interrupts, and those devices are
MSI-X, which needs the ITS. As v1 drew it, the sub-phase depended on its successor.

**Retires (0).**
**Acceptance:** a virtio-PCI device on `-M virt` (ECAM path) passes the same transport tests
as the mmio one; an MSI-X interrupt is delivered and counted.
**Riskiest unknown:** IORT DeviceID mapping. A wrong mapping produces MSI writes that vanish
silently — no fault, no interrupt, just a device that never completes.

#### 8.14 — SMP

Deliver: secondary bring-up via PSCI `CPU_ON` (the conduit is established —
`arch/aarch64/psci.rs`); per-CPU `TPIDR_EL1` blocks for secondaries (the structure exists
from 7.3); per-CPU redistributor init; SGI-based IPIs; **and a full audit of every
`spin::Mutex` for multi-core soundness.** The kernel was written UP and its locks have only
ever been contended by interrupts.

**Retires (0).**
**Acceptance — v1's repeated 7.3's exact mistake.** v1 said "4 CPUs come online and the
scheduler distributes work across them; a soak run is clean" — no unit on "distributes," no
predicate on "clean," the same shape as the loop-iteration fairness metric 7.3 had to throw
away and re-measure in ticks of residency. Instead:
(i) **per-CPU tick residency** in a stated band that **rejects `[all,0,0,0]`**;
(ii) a task records `MPIDR_EL1` each timeslice and must be observed on ≥2 distinct CPUs;
(iii) an SGI IPI counted at a **named** target CPU;
(iv) a contended counter — 4 CPUs × 100k increments under `InterruptMutex` totalling
**exactly 400,000**, the only assertion in the set a lock bug can actually fail;
(v) **the harness asserts it launched with `-accel tcg,thread=multi`** — without it all
vCPUs are time-sliced in one host thread and *no lock bug can appear at all*, so a kernel
with zero SMP soundness passes.
**Riskiest unknown:** the memory-ordering half of the lock audit is **not verifiable under
TCG**. A missing acquire/release or `dmb` is invisible in a sequentially-consistent emulator
and is a classic silent corruptor on out-of-order Neoverse. **UNVERIFIED** how faithfully
MTTCG models ARM's weak ordering on an x86 host; treat as "it does not." This makes a
**KVM-on-real-ARM run a gate in 8.15, not a bonus** — and it is worth pulling a KVM run
forward as soon as any ARM runner is available, since it also covers 8.3's coherence choice
and 8.6's barriers.

#### 8.15 — One cloud, end to end

**v1's 8.10 was five sub-phases wearing one number**, and its device list was wrong on every
clause. Verified: **GCP Arm VMs** (T2A, C4A/Axion) support **gVNIC + NVMe only** — "Virtio-Net
and SCSI interfaces are not supported on Arm VMs." **Azure Cobalt 100** uses **MANA**,
falling back to **NetVSC over VMBus** — a whole paravirtual bus, not a PCI device — plus
NVMe. **AWS Graviton** is ENA + NVMe (all Nitro); there is no "EBS-over-virtio" on Graviton,
that was the Xen-era x86 path. **So none of the three clouds runs a single line of the
8.1-8.7 virtio work.**

| Platform | NIC | Block | Discovery | Interrupts |
|---|---|---|---|---|
| QEMU `virt` | virtio-net-mmio/pci | virtio-blk | DT or ACPI | GICv2/v3 |
| QEMU `sbsa-ref` | E1000E | AHCI | ACPI | GICv3 |
| AWS Graviton | **ENA** (PCIe) | **NVMe** | ACPI | GICv3 + ITS |
| GCP Axion/T2A | **gVNIC** (PCIe) | **NVMe** | ACPI | GICv3 + ITS |
| Azure Cobalt | **MANA**, fallback **NetVSC/VMBus** | NVMe | ACPI | GICv3 + ITS |
| Bare-metal SBSA | vendor PCIe NIC | NVMe | ACPI | GICv3 + ITS |

**The minimum device set for "runs on any ARM node" is NVMe + the platform's UART + the
generic timer.** Networking is per-platform and cannot be promised generically.

Deliver: an **NVMe driver** (block), **one** cloud's NIC, and boot on a real instance of
that cloud. **The other two clouds are deferred by name, each as one NIC-driver sub-phase.**
Each of ENA / gVNIC / MANA+VMBus is comparable to Phase 3's whole storage bring-up; folding
four drivers into one sub-phase is how an estimate becomes fiction.

**Retires (0) — but this is where the phase's real deliverable lands.**
**Acceptance — v1 had no verdict here, only a human saying it worked.**
(a) The shipped ARM image runs the **same suite** and prints the **same sentinel** over the
platform's serial console (EC2 serial console / cloud console output), so "it booted"
becomes "54 passed, 0 skipped" — machine-checkable by the same grep the QEMU harness uses.
(b) `cargo xtask cloud-smoke` provisions one instance, boots, greps the sentinel, destroys —
nightly or manual, **never per-PR**.
(c) A **committed hardware matrix table**: every platform ever booted, the date, the
`platform:` line it printed, and the sentinel. That table is the honest substitute for CI
coverage on hardware, and its staleness is visible.
(d) **The KVM-on-ARM run from 8.14 is a gate here**, covering the memory-ordering and
coherence questions TCG cannot answer.
**Riskiest unknown:** the DMA-coherence and barrier decisions made back in 8.3/8.6. TCG
never reproduces either, so 8.3-8.14 can all be green while both are wrong, surfacing here
as corruption under load on real silicon — as far from its cause as a bug gets. Mitigations
are (a) the `AttrIndx` assertion and `dma:` boot line from 8.3, and (b) pulling the KVM run
as early as an ARM runner allows.

#### 8.16 — UEFI Secure Boot and image signing

Signed `BOOTAA64.EFI` → signed kernel; key generation, `db`/KEK enrolment, and the image
packaging chain. Largely an image and key-management problem rather than kernel work, which
is why it is its own sub-phase rather than a bullet.
**Acceptance:** a signed image boots with Secure Boot enabled, and an image with a
deliberately corrupted signature is **rejected** — the second half is the test.

#### 8.17 — Measured boot

Consume the UEFI TCG2 event log; discover a TPM 2.0 via the ACPI **TPM2** table (CRB
interface); extend PCRs. **Fail closed to "unmeasured" where no TPM is present.** vTPM and
NitroTPM are *instances* of this, not the design — and v1's "vTPM/NitroTPM where available"
was covering an unchecked gap: **NitroTPM is not supported on Graviton1/2 instances.**
**UNVERIFIED:** the ACPI TPM2 `StartMethod` values for the ARM SMC-based CRB.
**Acceptance:** PCR values are reproducible across two boots of the same image and **differ**
after a deliberate one-byte image change.

---

## The ratchet — `SKIPPED` accounting

The 38 entries, assigned. This table is the phase's progress bar and must sum to 38.

| Sub-phase | Retires | Running | Remaining |
|-----------|--------:|--------:|----------:|
| (today)   |       — |      16 |        38 |
| 8.1 discovery seam (amd64) | 0 | 16 | 38 |
| 8.2 transport trait (amd64) | 0 | 16 | 38 |
| **8.3 virtio-mmio** | **7** | 23 | 31 |
| 8.4 user AS + SVC + EL0 | 3 | 26 | 28 |
| 8.5 libthemelios + smokes | 4 | 30 | 24 |
| 8.6 storage | 6 | 36 | 18 |
| 8.7 networking | 7 | 43 | 11 |
| 8.8 Linux dispatcher (amd64) | 0 | 43 | 11 |
| 8.9 aarch64 Linux table | 4 | 47 | 7 |
| **8.10 containers + mgmt** | **7** | **54** | **0** |

`7 + 3 + 4 + 6 + 7 + 4 + 7 = 38`. ✓

**Three of the 38 cannot be retired by porting** and are retired by *reframing*, each with
the decision made in the sub-phase that owns it rather than discovered at the parity gate:

- **`test_pci_scan`** (8.3) — no port I/O on aarch64, ever. Becomes an arch-neutral
  transport-discovery test with per-arch bodies.
- **`test_syscall`** (8.4) — pure x86 MSR verification (`EFER`/`STAR`/`LSTAR`/`FMASK`),
  never enters ring 3. Becomes a different test under the same name.
- **`test_linux_exec`**'s TLS assertion (8.9) — asserts `arch_prctl(SET_FS)`, which has no
  aarch64 analog.

**The most consequential correction v1's ratchet needed:** v1 credited all 19 storage and
network skips to the post-EL0 sub-phases. **Six of them need no EL0 at all** —
`drivers::block_server` is an in-kernel `sched::spawn` task (`block_server.rs:120,138`),
and none of the six references `spawn_server`/`embedded::`. v1 therefore materially
overstated how much of the phase is gated on userspace, which was the load-bearing number
in its sequencing argument.

## Sequencing

```
8.spike ─→ 8.1 ─→ 8.2 ─→ 8.3 ─→ 8.4 ─→ 8.5 ─┬─→ 8.6 ─┐
                        (+7)         (+3)   (+4)     ├─→ 8.8 ─→ 8.9 ─→ 8.10  ← PARITY
                                                └─→ 8.7 ─┘         (+4)   (+7)
                                                    (+6/+7)

PARITY ─→ 8.11 ─→ 8.12 ─→ 8.13 ─→ 8.14 ─→ 8.15 ─→ 8.16 ─→ 8.17   ← ANY SR NODE
        discovery  GICv3   ECAM    SMP    cloud   secure  measured
                   + ITS   + MSI-X                 boot     boot
```

8.6 and 8.7 are independent of each other after 8.5. 8.8 is an amd64-only refactor that can
land any time after 8.5 and is drawn here only because 8.9 needs it.

### Why the VirtIO tier goes first — the v1 order is reversed

v1 put EL0 first, arguing it "holds the biggest unknown and a dead end there reshapes the
rest." That argument is wrong in a specific way:

1. **The unknown is retired by `8.spike`.** That is what a spike is *for*. "Do the biggest
   unknown first" is discharged by a throwaway branch, not by sequencing five merged
   sub-phases ahead of everything else. v1 proved the spike, then reused the argument to
   order the tier.
2. **8.1-8.3 are the highest-blast-radius change in the phase and they refactor *working
   amd64 storage and networking*.** Landing them first means they land against a small
   aarch64 tree, with the amd64 suite as a clean bisect target, and with nobody
   simultaneously mid-EL0-debug. Landing them fifth means an amd64 storage regression
   arriving interleaved with four sub-phases of new EL0 code.
3. **v1's own escape hatch was unreachable where it sat.** v1's 8.4 said "if it is deep,
   split into 8.4a/8.4b" — but by then 8.0-8.3 would be merged on a schedule that assumed
   it wasn't. Discovering the entanglement is only cheap if you discover it first. (This
   review did the scouting, which is why the split is now three named sub-phases.)
4. **It moves the ratchet immediately:** 7 skips retired in the third merged sub-phase
   instead of 0 in the fifth. The plan's own definition of a sub-phase that has delivered
   is one that shrinks `SKIPPED`.
5. **The dead ends are asymmetric.** EL0 has one — true. 8.1-8.3 have none: the amd64 path
   already works and virtio-mmio is the simplest transport in existence. The risk there is
   *cost*, and cost discovered early is schedulable; cost discovered late is not.

What EL0-first genuinely bought: 8.5's flat-binary link risk and 8.4's `eret` race class are
the two items most likely to exceed budget, and they gate 24 of the 38 skips. That argues
for not deferring them *far* — which the order above respects, putting them immediately
after the transport work — but not for putting them ahead of a refactor that is independent
of them.

## Estimate

**Seventeen sub-phases plus a spike**, each comparable to 7.1-7.4. Parity (8.1-8.10) is
**ten**; any-SR-node (8.11-8.17) is **seven**. v1 said "11-14 sub-phases"; that was low
because it folded four device drivers, a signing chain and a TPM path into one number, and
because it did not know about the six hand-written `_start` routines or the second Linux
syscall table.

## Deferred (documented — out of Phase 8 scope)

- **The second and third clouds** — one NIC-driver sub-phase each (gVNIC, MANA/VMBus).
- **Non-UEFI boot** — the raw Linux `Image` protocol for stock Raspberry Pi firmware and
  `booti`. A dedicated sub-phase if ever wanted.
- **GICv2m MSI frames** — a GICv2 platform with PCIe is unsupported and says so.
- **32-bit EL0 (AArch32).** The `0x600` vector group stays fatal.
- **SMP on amd64.** x86 stays UP; 8.14's lock audit benefits it, but AP bring-up is separate.
- **Big-endian, 16 KiB/64 KiB granules, 52-bit VA (LPA2).** 4 KiB/48-bit is pinned — but
  *verified* at boot (8.11), not assumed.
- **SVE/SME.** Not advertised in `HWCAP`; `CPACR_EL1.ZEN` left trapping.
- **GICv4** — adds direct injection of virtual interrupts for a hypervisor; v3-compatible
  for everything a non-hypervisor OS does.
- **The Phase 6 deferrals** (TLS/mTLS, interactive `exec` streaming, Engine API breadth) —
  arch-neutral when they land.

## Notes carried forward

- **No IST/TSS analog on aarch64** (Phase 7). A kernel-stack overflow re-faults on the same
  stack. This becomes materially worse at EL0, because a user process can now *provoke*
  kernel stack depth. **Add a guard page below each kernel stack in 8.4** — cheap, and it
  converts a silent re-fault into a reported one.
- **`test_runner.rs:162`'s doc comment says "thirty-nine" skipped. It is 38.** Fix when
  next touching the file.

## Corrections from v1

Recorded rather than silently overwritten, because which *kind* of claim proved unreliable
is the useful information.

| # | v1 claimed | Actually |
|---|---|---|
| 1 | `libthemelios` is the only file in `servers/` with inline `syscall`; the port is "one file plus a linker script" | **53 sites across 7 files.** Six hand-written x86 `_start` routines in `global_asm!` must be rewritten in aarch64 asm |
| 2 | `new_user` inherits **Limine's** low-half entries; the empty **clear** loop is the bug | The tree copied is the kernel's **TTBR1** tree; the bug is in the **copy** loop. Fixing the clear loop is a no-op. Teardown leaks frames — unmentioned |
| 3 | The `SP_EL0` banking makes the x86 scratch-slot race **structurally absent** | `SP_EL0` is banked **by EL, not by task** — as CPU-global as `gs:0x8`. The same preemption bug reproduces exactly |
| 4 | The `sysret` non-canonical-RCX hazard "gets **deleted**, not ported" | Two worse hazards: `SPSR_EL1.M` one bit → **return to EL1**; a Reserved `M` → `PSTATE.IL` → **user-reachable node halt**. `eret` needs a *stronger* check than x86 |
| 5 | Keep FP trapped; make EL0 **softfloat** — "much cheaper and is the recommendation" | **No soft-float aarch64 A-profile ABI exists.** glibc's base `strlen.S` opens with `ld1`; `dl-trampoline.S` saves `q0`-`q7`. Would have deleted 8.10's deliverable |
| 6 | QEMU `virt` defaults virtio-mmio to **v2**; "reject v1 loudly" | Defaults to **legacy v1** (`force-legacy=true`). v1's remedy would fail CI on the default command line |
| 7 | `USER_ADDR_LIMIT` is "numerically correct" for aarch64 | **Wrong by 2×.** TTBR0 owns 2^48; the constant is 2^47 |
| 8 | aarch64 **inverts** x86's sense on AP[2] **and the XN bits** | Only `AP[2]`. `PXN`/`UXN` are execute-never-when-set, **same polarity as x86**. Inverting them yields a silently executable user page |
| 9 | 8.4 needs "SPI routing and enable in the GICv2 distributor"; the transport needs "ISR/interrupt ack" | **The virtio stack has no interrupt path — it polls** (MSI-X set to NO_VECTOR; `isr` is `dead_code`). Invented work |
| 10 | xtask "already produces the images for the aarch64 boot" | `ensure_images` is never called on any aarch64 path; the aarch64 VM has **no disk and no NIC**; arm64 CI lacks `squashfs-tools`/`e2fsprogs` |
| 11 | `x8` "is the one register AAPCS64 leaves free at a call boundary" | AAPCS64 gives `x8` the **indirect result location** role, and `x9`-`x15` are equally free. Right decision, wrong reason |
| 12 | Graviton "is the most constrained and therefore the most informative" | It is the textbook conformant case. **None of the three named clouds runs virtio at all** — all are ENA/gVNIC/MANA + NVMe |

Two more findings had no v1 counterpart because v1 did not mention the subject: **the GICv3
ITS** (without which PCIe MSI-X cannot be delivered, making v1's ECAM sub-phase depend on
its successor) and **virtqueue barriers** (a larger TCG blind spot than the coherence issue
v1 did flag).
