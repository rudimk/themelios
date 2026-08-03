# Phase 7 — aarch64 port (plan, Momus-reviewed)

**Deliverable (renamed for honesty — Momus):** a **ring-0 kernel-core port to aarch64
with a reduced in-kernel serial shell**. Bring the kernel up on QEMU `virt` (ARM64) to
an interactive in-kernel shell with preemptive multitasking, and run the portable,
alloc-only test suite on aarch64 in CI. This is **not** "Docker on ARM": ring-3/EL0
userspace, storage, networking, and containers on aarch64 are a separate, deferred ABI
surface. The milestone label must not imply userspace/containers.

## Grounding (from the porting-surface map + Momus verification)

- **No x86-only third-party crates.** `limine`, `spin`, `linked_list_allocator`,
  `miniz_oxide` are all arch-neutral; every HW primitive (UART, PIC, PIT, GDT/IDT,
  MSRs, PTE format, `PhysAddr`/`VirtAddr`) is hand-rolled in-tree. **The port is
  writing aarch64 implementations, not replacing dependencies.**
- **Two ABI surfaces, sequenced.** The *kernel* boot is independent of *ring-3
  servers*: `libthemelios` wraps the syscall ABI in ~30 inline `syscall` blocks and
  every server binary is x86. The in-kernel shell is a **ring-0 `sched::spawn` task**
  (`shell/mod.rs:41`, confirmed) draining the serial-RX ring buffer — it needs no EL0.
- **Limine hands off "warm," not bare-metal (Momus MUST-FIX 4).** limine 0.5 supports
  aarch64 (`lib.rs:126-129`). The kernel is entered at **EL1 with the MMU already
  enabled, caches on, stack set, BSS zeroed** — the exact analog of the x86 handoff
  where `kmain` prints its banner on Limine's tables *before* `mm::page_table::init()`
  (`main.rs:230-386`). We do **not** reset `SCTLR_EL1`/stack/BSS in early boot; we
  inherit Limine's state and only *verify* it.
- **The kernel does not compile for aarch64 today, and the seam is bigger than
  "main.rs + CLASS-B" (Momus MUST-FIX 1).** `arch/mod.rs:26-31` declares `pub mod
  x86_64` only under `cfg(target_arch="x86_64")`, so `crate::arch::x86_64` **does not
  exist** on aarch64 — yet these unconditionally-compiled modules reference it at
  module scope with no cfg gate:
  - *Tick reads (facade-able):* `net/net_service.rs:46`, `container/registry.rs:25`,
    `audit/mod.rs:226`, `shell/commands.rs:1038` → `arch::x86_64::idt::tick_count()`.
  - *Ring-3/Linux (must be cfg-partitioned out of aarch64):* `linux/fs.rs:13`,
    `linux/thread.rs:24,184,204`, `linux/syscall.rs:24,113,140,320,343` —
    `copy_from_user`/`SyscallFrame`/`swapgs`/`sti`.
- **kmain's boot tail stays cfg-gated (Momus MUST-FIX 2).** `main.rs:486-527` calls
  `shell::init()`, `process::init::start()` (which **drops to ring 3** —
  `process/init.rs:73`), `fs::boot_storage()`, `net::boot_net()`, and the api-server
  spawn — none arch-gated today. On aarch64 the boot ladder must *stay* `#[cfg]`'d for
  every deferred subsystem; "convert the cfg ladder to unconditional facade calls"
  applies only to the arch-core primitives (serial/idt/gdt/cpu/pic/pit/syscall init),
  not the deferred subsystems.

## Cross-cutting invariants (non-negotiable)

1. **amd64 stays fully green** every sub-phase. The facade re-exports x86 impls
   unchanged; aarch64 code is additive behind `cfg(target_arch="aarch64")`. The amd64
   QEMU suite is the regression gate.
2. **aarch64 compile gate per PR from 7.0b on (Momus SHOULD-FIX 5).** Extend the CI
   `arm64-gate` job (`build.yml:52`) to `cargo build --target aarch64-unknown-none`
   the kernel — "amd64 green" is insufficient once the seam touches shared files.
3. Each sub-phase is a fresh branch + PR off latest `main`, Momus-reviewed, CI-green
   before merge.

## Pinned decisions (Momus-adjusted)

1. **Arch seam = module facade, not a trait object.** `arch::{irq,time,cpu,paging,
   context,serial,intc,timer,syscall,exceptions}` `pub use` the active arch's impl.
   Monomorphized, zero-cost, matches the existing `cfg`-module style. *(Confirmed
   correct — Momus.)*
2. **GICv2**, via `-machine virt,gic-version=2`. Simpler bring-up (MMIO GICD+GICC; no
   redistributors, no `ICC_*_EL1`/`ICC_SRE_EL1` sysreg interface). GICv3 is **real
   Phase-8 work** for Graviton, not a drop-in. *(Confirmed correct — Momus.)*
3. **Timer = virtual generic timer `CNTV`** (`CNTV_TVAL_EL0`/`CNTV_CTL_EL0`, GIC
   **PPI 27**) — the trap-free guest choice (Momus SHOULD-FIX 2). `CNTP` (PPI 30) can
   trap via `CNTHCTL_EL2.EL1PCEN` when EL2 is present (Graviton). The 7.spike confirms
   whichever we pick actually fires at EL1 before we commit.
4. **CI pass/fail = serial-sentinel parsing (Momus MUST-FIX 3), arch-uniform.**
   Semihosting `SYS_EXIT` returns host exit **0/1**, never the **3** that
   `xtask:1118-1130` hardcodes for isa-debug-exit "pass" — so semihosting does *not*
   preserve the contract. The test harness prints a unique end sentinel to serial;
   `cmd_test` greps for pass/fail in captured serial (works identically on both
   arches). Machine stop via PSCI `SYSTEM_OFF` (QEMU `virt` implements PSCI) or
   semihosting `SYS_EXIT` — used only to halt, not to signal pass/fail. (Optionally
   migrate x86 to the same sentinel scheme so the harness is one path.)
5. **Firmware = UEFI (AAVMF/edk2-aarch64) via the `pflash` CODE+VARS pair**, not
   `-bios CODE.fd` alone (more reliable across edk2 versions — Momus CONSIDER). The
   firmware path is discovered at runtime (like `find_mkfs_ext2`, `xtask:515`), not
   hardcoded. `virt` is UEFI-only (no BIOS); the ISO carries `BOOTAA64.EFI`.
6. **FP/SIMD trap must be enabled in early boot (Momus MUST-FIX 5).**
   `aarch64-unknown-none` is **hardfloat** (fp+neon on), unlike soft-float
   `x86_64-unknown-none` — the compiler can lower `memcpy`/`memset`/struct-moves/
   formatting to SIMD, and any FP/SIMD access at EL1 **traps** unless
   `CPACR_EL1.FPEN = 0b11`, which Limine does not guarantee. Set `CPACR_EL1.FPEN`
   before any non-trivial Rust runs, **or** build the kernel with `+soft-float`/`-neon`.
   Decide in the 7.spike (measure which the codegen actually needs).
7. **Scheduler proven with kernel (ring-0) tasks**, independent of EL0. *(Confirmed —
   Momus: shell + scheduler are ring-0.)*

## Sub-phases

### 7.spike — throwaway boot+GIC+timer spike — DONE ✅
Ran locally (QEMU 8.2.2 `virt`, AAVMF firmware, Limine v8 `BOOTAA64.EFI`), throwaway
code in scratchpad (not committed). All four goals retired; findings below.

**Results:**
- **(a) PL011 + (c) FP-trap + (d) GICv2/CNTV** — validated via a `-kernel` direct boot
  (MMU off, EL1):
  - PL011 TX at `0x0900_0000` works; boots at **EL1**.
  - **FP-trap CONFIRMED (Momus MUST-FIX 5 is real):** ordinary `f64` math traps at EL1
    with `ESR_EL1.EC=0x07`; `CPACR_EL1` is `0` on entry. Setting `CPACR_EL1.FPEN=0b11`
    (→ `0x30_0000`) and retrying makes it succeed. **The kernel must enable FP in early
    boot.**
  - **`VBAR_EL1` exception vectors work:** `brk #0` → sync `EC=0x3C`, handler resumes.
  - **GICv2 + CNTV CONFIRMED (decisions 2 & 3):** GICD+GICC init + `CNTV_CTL/TVAL_EL0`
    deliver **INTID 27** IRQs through the full vector→`GICC_IAR`→`GICC_EOIR` path;
    `CNTFRQ_EL0 = 62.5 MHz`. Re-arm + mask work.
- **(b) Limine UEFI warm-handoff** — validated via the real chain (AAVMF `pflash` →
  `BOOTAA64.EFI` → higher-half kernel):
  - **Chain works:** UEFI finds `BOOTAA64.EFI`, Limine loads the higher-half ELF and
    **hands off at EL1 in the higher half** (kernel ran at `0xffffffff8000_01f0`, MMU
    ON). Limine warm-handoff confirmed — do NOT reset `SCTLR`/stack/BSS.
  - **HHDM present**, offset `= 0xffff_0000_0000_0000`.
  - **⚠️ KEY FINDING — Limine's HHDM maps RAM, NOT device MMIO.** Both raw-phys
    `0x0900_0000` **and** HHDM+`0x0900_0000` data-abort (translation fault, `FAR` =
    `0xffff_0000_0900_0018`). Unlike x86 (port-I/O serial works instantly), **on
    aarch64 the kernel must establish a mapping for the PL011 MMIO before it can print**
    — early serial is entangled with a minimal page-table edit. This reshapes 7.0b (see
    below).

**Pinned-decision confirmations:** decision 3 (CNTV) ✓, decision 6 (FP trap real) ✓,
GICv2 (decision 2) ✓, Limine warm handoff ✓.

### 7.spike — throwaway boot+GIC+timer spike (original scope, now retired)
Momus SHOULD-FIX 1: the two highest-uncertainty items (Limine-UEFI-on-ARM handoff,
GIC/timer) were spiked FIRST on a throwaway branch so a dead-end couldn't strand merged
work. Done — see results above.

### 7.0 — Arch seam + boot-to-UART (split — Momus)
- **7.0a-i — `irq`/`time` facade (amd64-only, mechanical).** Add `arch::irq::{disable,
  enable,are_enabled,halt}` + `arch::time::tick_count()`; re-export x86 impls unchanged;
  route the CLASS-B interrupt sites (`ipc`, `sched`, `sync.rs` `InterruptMutex`) and
  **all** tick reads (`audit`, `shell`, `net/net_service`, `container/registry`,
  `linux` — the portable ones) through the facade. **Must not disturb the three known
  GS/per-CPU race fixes** (CLAUDE.md): `sync.rs:154-166` and the sched critical sections
  keep identical ordering. amd64 suite green = the whole acceptance.
- **7.0a-ii — cfg-partition the ring-3/Linux subtree.** *(Sequencing refinement: this
  MOVES INTO 7.0b.)* `#[cfg]`-gate the EL0-dependent module tree out of the aarch64
  build: `linux/*` (`SyscallFrame`, `copy_*_user`, `swapgs`), ring-3
  `process::init::start`, and the deferred boot-tail calls in `kmain`
  (`main.rs:486-527`). **Rationale for moving it:** a dangling `arch::x86_64` ref is a
  *compile error* that only surfaces when compiling aarch64, which first happens in
  7.0b — so the partition is done there, driven by real build errors (the honest,
  verifiable way), rather than blind in an amd64-only PR where its aarch64-correctness
  can't be checked. 7.0a therefore ships the **facade alone** (7.0a-i): a clean,
  amd64-green, independently-reviewable refactor.
- **7.0b — aarch64 boot-to-UART — DONE ✅** (boots on QEMU `virt`; banner over PL011).
  Delivered: `kernel/linker-aarch64.ld` + `.cargo` target; the aarch64 arch module
  (`boot`/`serial`/`irq`/`time`) + an `arch::serial` facade that moved the
  `print!`/`println!` macros arch-neutral; a per-arch `kmain` dispatcher (`kmain_x86_64`
  unchanged); the not-yet-ported modules cfg-gated to x86_64 (mm/sched/process/linux/
  net/drivers/fs/… — un-gated as each later sub-phase lands) + a panic-on-alloc
  placeholder allocator; early boot enables `CPACR_EL1.FPEN` and **maps the PL011 by
  editing Limine's live TTBR1 tables via the HHDM** (the spike finding — allocates
  intermediate tables from a static `.bss` pool, Device-`nGnRnE` via a MAIR scan, then
  `TLBI`); xtask `run --arch aarch64` (UEFI ESP + `qemu-system-aarch64 -M
  virt,gic-version=2` + runtime AAVMF discovery); CI arm64 gate extended to build the
  aarch64 kernel. Verified locally: `cargo xtask run --arch aarch64` prints the banner;
  amd64 builds + suite stay green. **Original detail below.**

  `kernel/linker-aarch64.ld` (higher-half
  `0xffffffff80000000`, `elf64-littleaarch64`, `.requests*` markers KEEP'd — validated
  in the spike), `.cargo/config.toml` `[target.aarch64-unknown-none]`, xtask UEFI-boot
  (`BOOTAA64.EFI` from `target/limine`; `-M virt,gic-version=2 -cpu cortex-a72` +
  `-pflash AAVMF_CODE/VARS` discovered at runtime; **no BIOS El-Torito** — replace the
  `run --arch aarch64` stub at `main.rs:971`). Early aarch64 entry: **verify** EL1 via
  `CurrentEL` (do NOT reset `SCTLR`/stack/BSS — inherit Limine's warm handoff), **enable
  `CPACR_EL1.FPEN`** (spike-confirmed mandatory), then the PL011 banner.
  **⚠️ Early-serial requires a UART mapping (spike finding):** Limine's HHDM does not
  map device MMIO, so before the first `println!` the kernel must map the PL011
  (`0x0900_0000`) — the pragmatic path is to edit Limine's live page tables via the HHDM
  (page-table frames are RAM, hence HHDM-reachable) to add one Device-`nGnRnE` entry for
  the UART. This pulls a *minimal* page-table-edit primitive into 7.0b (a precursor to
  the full 7.1 MMU work); keep it to "map one device page," not the whole allocator.
  **Acceptance:** `cargo xtask run --arch aarch64` prints the banner; kernel
  `cargo build --target aarch64-unknown-none` added to CI. **Riskiest unknown — RETIRED
  by 7.spike** (Limine handoff, AAVMF wiring, higher-half layout all confirmed working).

### 7.1 — MMU / paging
ARM descriptor `PageFlags` (valid/table/AP/AttrIndx/AF/UXN/PXN), `TCR_EL1`
(T0SZ/T1SZ=16, 4 KiB TG0/TG1), `MAIR_EL1`, 4-level walk reusing the HHDM-based
`AddressSpace`, `write_cr3` → `TTBR0_EL1` write. **Barrier discipline as the map/unmap
contract (Momus SHOULD-FIX 3):** `DSB ISHST` before `TLBI`, `TLBI VMALLE1IS`, then
`DSB ISH` + `ISB` after — at every map/unmap, not just the TTBR load. **`mm/addr.rs`
needs NO Phase-7 change (Momus correction):** its index extraction (bits 39/30/21/12)
and HHDM `to_virt`/`to_phys` are arch-neutral for 48-bit/4-level/4KiB; the only
x86-canonical assumptions (`USER_ADDR_LIMIT`, the `sysret` non-canonical-RCX check) are
in the **deferred ring-3 path** (`syscall.rs:466-486,723`) — audit those when EL0 is
picked up, not now.
**Acceptance**: paging on our own tables; map/unmap/translate self-test passes on
aarch64; amd64 unchanged.
**Riskiest unknown**: `MAIR`/`AttrIndx` cacheability (wrong attrs = silent corruption
or DMA-only faults later).

### 7.2 — Exceptions + GIC + timer (tick only)
`VBAR_EL1` 16-entry vector table; sync handler decodes `ESR_EL1.EC` (SVC / data+instr
abort / `BRK`); GICv2 GICD+GICC init; `CNTV` timer → `arch::time` tick. **Tick only —
`schedule()` is wired into the timer handler in 7.3 (Momus CONSIDER: matches the x86
`idt.rs:442` split).** `brk #0` self-test (the `int3` analog).
**Acceptance**: timer IRQ fires at a fixed rate and increments the tick; a `brk`
synchronous exception is caught; amd64 unchanged.
**Riskiest unknown (highest in the port, pre-spiked in 7.spike)**: GICv2 EOI discipline
+ PPI routing + distributor/CPU-interface enable + priority mask.

### 7.3 — Scheduler context switch
aarch64 `switch_context` (save/restore x19–x30 + SP) and `task_bootstrap`
(`msr daifclr`; `blr`); `TPIDR_EL1` as the PerCpu pointer written **on every switch**
(the `gs`-base analog); wire `schedule()` into the 7.2 timer handler. Explicitly replay
the three known GS/per-CPU race classes (CLAUDE.md) in `TPIDR` form: per-switch
`TPIDR_EL1` write, atomic exception-return tail.
**Acceptance**: ≥2 in-kernel tasks preempt and round-robin under the timer; a soak run
is clean; amd64 unchanged.
**Riskiest unknown**: per-CPU/`TPIDR` races (the exact class that bit x86 in 4.5).

### 7.4 — Shell + tests + CI + finalize
PL011 RX-IRQ-driven in-kernel serial shell — **reduced command set on aarch64 (Momus
CONSIDER):** the ~18/30 commands that depend on deferred fs/net/container (`ls`/`cat`/
`ping`/`run`/`ps`/`stop`/`mount`/`ifconfig`/`sockets`/…) are `#[cfg]`'d out; aarch64
ships the arch/mem/sched/help commands. Serial-sentinel `cmd_test` exit contract
(decision 4); **un-gate the genuinely portable, alloc-only tests for aarch64 (Momus
SHOULD-FIX 4):** frame allocator, heap, capability system, IPC *logic*, VFS logic,
http/json parsers — so aarch64 CI proves real assertions, not just "it boots." Add
aarch64-specific smokes (paging, timer, context switch). aarch64 `cargo xtask test` CI
job (runner installs `qemu-system-arm` + `qemu-efi-aarch64`). mdbook aarch64 chapter;
reconcile the three trackers to Phase 7 = Complete (ring-0 core; ring-3/storage/net
deferred).
**Acceptance**: `cargo xtask run --arch aarch64` gives an interactive (reduced) shell;
aarch64 CI job green on real tests; amd64 suite still green; mdbook builds.
**Riskiest unknown**: the serial-sentinel exit contract wiring for `cmd_test`.

## Deferred (documented — out of Phase 7 scope)

- **Ring-3 / EL0 userspace on aarch64** — SVC syscall entry + aarch64 register ABI,
  `copy_*_user`/`SyscallFrame` aarch64 impls, EL0 drop (`SPSR_EL1`/`ELR_EL1`), porting
  `servers/libthemelios` (~30 `syscall` sites) + an aarch64 server linker script +
  aarch64 server builds. **This is where the x86-canonical assumptions
  (`USER_ADDR_LIMIT`, `sysret` RCX check) get their aarch64 audit.** Gates
  containers/`api-server` on ARM. Revisit when Phase 8 (hyperscaler ARM) needs it.
- **Storage + networking on aarch64** — `drivers::pci` is port-I/O based; `virt`
  exposes VirtIO as **virtio-mmio** (device-tree) or PCIe **ECAM**. smoltcp is already
  portable but has no device to bind to until a virtio-mmio/ECAM transport exists.
- **GICv3** — for Graviton; GICv2 suffices in QEMU `virt`.
- **SMP / secondary CPUs** (PSCI `CPU_ON`) — the kernel is UP for bring-up.
- **Secure boot / real firmware** — Phase 8.

## Notes (Momus CONSIDER, carried forward)

- **No IST/TSS analog.** Same-EL aarch64 exceptions reuse `SP_EL1`, so a kernel-stack
  overflow re-faults on the same stack (x86 caught double-faults on an IST stack).
  Acceptable for bring-up; note it, don't solve it in Phase 7.
- **7.spike is throwaway** — its purpose is to retire unknowns and confirm decisions
  3 (CNTV) and 6 (FP trap), not to produce merged code.
