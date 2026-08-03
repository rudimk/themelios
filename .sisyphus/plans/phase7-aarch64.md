# Phase 7 — aarch64 port (plan)

**Roadmap goal**: aarch64 port — **boot, memory, scheduler, shell**. Bring the kernel
up on ARM64 (QEMU `virt`) to an interactive in-kernel serial shell with preemptive
multitasking, and run the portable test suite on aarch64 in CI. Ring-3/EL0 userspace,
storage, and networking on ARM are **explicitly out of Phase 7 scope** (see Deferred).

## Grounding (from the porting-surface map)

- **No x86-only third-party crates.** The kernel takes `limine`, `spin`,
  `linked_list_allocator`, `miniz_oxide` — all arch-neutral. Every hardware primitive
  (UART, PIC, PIT, GDT/IDT, MSRs, PTE format, `PhysAddr`/`VirtAddr`) is hand-rolled
  in-tree. **The port is writing aarch64 implementations, not replacing dependencies.**
- **Two ABI surfaces, sequenced.** Booting the *kernel* on aarch64 is independent of
  running *ring-3 servers*: `servers/libthemelios` wraps the syscall ABI in ~30 inline
  `syscall` blocks and every server binary is x86. The in-kernel shell needs neither,
  so Phase 7 reaches "boot → shell" without touching the userspace ABI.
- **Facade win.** ~35 of 66 `arch::x86_64::*` sites are CLASS B (interrupt
  enable/disable/halt + monotonic tick) — pure critical-section/counter primitives that
  collapse behind a thin `arch::{irq,time}` facade with the x86 impls **unchanged**.
  The rest are CLASS A (page tables, context switch, syscall entry, interrupt
  controller, timer, UART, exception vectors, per-CPU) — they need a *second
  implementation*, and the seam just makes `main.rs`/`sched` call `arch::foo::init()`
  unconditionally instead of the current `#[cfg]` ladder.
- **Already portable as-is**: frame allocator, heap, capability system, IPC *logic*,
  audit ring, VFS, containers/OCI, HTTP/JSON, and the `smoltcp` stack (already
  CI-proven for `aarch64-unknown-none`). `mm::PAGE_SIZE = 4096` matches the 4 KiB
  granule; `PhysAddr`/`VirtAddr` are hand-rolled but their canonical-address math
  assumes x86 sign-extension and must be re-audited for the aarch64 TTBR0/1 split.

## Cross-cutting invariant (non-negotiable)

**Every sub-phase PR keeps amd64 fully green.** The facade re-exports the x86 impls
unchanged; all aarch64 code is additive behind `cfg(target_arch = "aarch64")`. The
amd64 QEMU suite remains the regression gate throughout; a port must never regress the
working architecture. Each sub-phase is a fresh branch + PR off latest `main`,
Momus-reviewed, CI-green before merge — the established workflow.

## Key up-front decisions (pin these; Momus to challenge)

1. **Arch seam = module facade, not a trait object.** `arch::{irq,time,cpu,paging,
   context,serial,intc,timer,syscall,exceptions}` are plain modules that
   `pub use` the active arch's impl. No `dyn`, no vtables in the kernel hot path —
   monomorphized, zero-cost, matches the existing `cfg`-module style.
2. **GIC version = GICv2**, pinned via QEMU `-machine virt,gic-version=2`. GICv2 is the
   simpler bring-up (MMIO distributor + CPU interface; no per-CPU redistributor, no
   `ICC_*_EL1` sysreg interface). Real cloud ARM (Graviton) is GICv3 — noted as a
   follow-up when Phase 8 hyperscaler work needs it, not a Phase 7 blocker.
3. **Timer = physical generic timer** (`CNTP_TVAL_EL0`/`CNTP_CTL_EL0`, GIC **PPI 30**).
   Limine hands off in EL1 where CNTP is accessible; no EL2 trap handling needed.
4. **QEMU exit = ARM semihosting** (`-semihosting`, `SYS_EXIT` via `HLT #0xF000`) to
   preserve the CI pass/fail **exit-code** contract that `isa-debug-exit` gives on
   x86 (`virt` has no `isa-debug-exit`). This replaces `cpu::exit_qemu`'s port write.
5. **Firmware = UEFI (AAVMF/edk2-aarch64).** `virt` has no BIOS; boot is UEFI-only via
   `-bios AAVMF_CODE.fd` (apt `qemu-efi-aarch64`). The ISO carries `BOOTAA64.EFI`, not
   the BIOS El-Torito bits.
6. **Scheduler proven with kernel tasks.** Context-switch/preemption is exercised with
   in-kernel tasks (ring-0), independent of EL0/ring-3 — so 7.3 needs no userspace ABI.

## Sub-phases

### 7.0 — Arch seam + boot-to-UART
Split for reviewability:
- **7.0a — Seam refactor (amd64-only, pure refactor, no behavior change).** Introduce
  the `arch::{irq,time,cpu,serial,paging,context,intc,timer,syscall,exceptions}`
  facade; re-export the x86 impls unchanged; convert `main.rs`'s ~15 `#[cfg]` call
  sites + the CLASS-B interrupt/tick sites (`ipc`, `sched`, `sync.rs`,
  `audit`/`registry`/`shell`/`linux` tick reads) to unconditional facade calls. Big
  but mechanical diff; **amd64 suite must stay green** (this is the whole acceptance).
- **7.0b — aarch64 boot-to-UART.** `kernel/linker-aarch64.ld`, `.cargo/config.toml`
  `[target.aarch64-unknown-none]`, xtask UEFI-ISO + `qemu-system-aarch64 -M virt
  -cpu cortex-a72 -bios AAVMF` path (replace the `run --arch aarch64` stub), EL1 setup
  (`SCTLR_EL1`, stack, BSS zero), PL011 UART driver, arch-neutral boot banner.
  **Acceptance**: `cargo xtask run --arch aarch64` prints the banner over PL011.
  **Riskiest unknown**: Limine UEFI-on-ARM handoff + AAVMF wiring + higher-half linker
  layout / `.requests` sections.

### 7.1 — MMU / paging
ARM descriptor `PageFlags` (valid/table/AP/AttrIndx/AF/UXN/PXN), `TCR_EL1`
(T0SZ/T1SZ=16, 4 KiB TG0/TG1), `MAIR_EL1` attributes, 4-level walk reusing the
HHDM-based `AddressSpace`, `write_cr3` → `TTBR0_EL1` + `TLBI VMALLE1IS`/`DSB`/`ISB`.
Re-audit `mm/addr.rs` canonical math for the TTBR0/1 split (aarch64 top-VA is all-ones,
not sign-extended).
**Acceptance**: paging enabled, kernel runs on its own tables, a map/unmap/translate
self-test passes on aarch64; **amd64 unchanged**.
**Riskiest unknown**: MAIR/AttrIndx cacheability (wrong attrs = silent corruption or
DMA-only faults later).

### 7.2 — Exceptions + GIC + timer
`VBAR_EL1` 16-entry vector table; sync handler decodes `ESR_EL1.EC` (SVC vs data/
instruction abort vs `BRK`); GICv2 distributor + CPU-interface init; physical generic
timer → `arch::time` tick + preemptive `schedule()`. `brk #0` self-test (the `int3`
analog).
**Acceptance**: a timer IRQ fires at a fixed rate and increments the tick; a
synchronous exception (`brk`) is caught and handled; **amd64 unchanged**.
**Riskiest unknown** (highest in the port): GIC EOI mode + PPI routing; getting
distributor/CPU-interface enable + priority-mask right.

### 7.3 — Scheduler context switch
aarch64 `switch_context` (save/restore x19–x30 + SP) and `task_bootstrap`
(`msr daifclr`; `blr`); `TPIDR_EL1` as the PerCpu pointer written **on every switch**
(the `gs`-base analog). Explicitly replay the three known GS/per-CPU race classes
(CLAUDE.md) in `TPIDR` form: per-switch `TPIDR_EL1` write, atomic exception-return
tail.
**Acceptance**: ≥2 in-kernel tasks preempt and round-robin on aarch64 under the timer;
a soak run is clean; **amd64 unchanged**.
**Riskiest unknown**: per-CPU/`TPIDR` races (the exact bug class that bit x86 in 4.5).

### 7.4 — Shell + tests + CI + finalize
PL011 RX-interrupt-driven in-kernel serial shell (the roadmap's "shell"); semihosting
`exit_qemu`; un-gate the portable `test_runner` tests for aarch64 + add aarch64-specific
smoke tests (paging, timer, context switch); add a `cargo xtask test --arch aarch64` CI
job (runner installs `qemu-system-arm` + `qemu-efi-aarch64`); mdbook aarch64 chapter;
reconcile the three trackers to Phase 7 = Complete (core; ring-3/storage/net deferred).
**Acceptance**: `cargo xtask run --arch aarch64` gives an interactive shell; aarch64
CI job green; amd64 suite still green; mdbook builds.
**Riskiest unknown**: the QEMU `virt` exit-code contract (semihosting) for `cmd_test`.

## Deferred (documented — out of Phase 7 scope)

- **Ring-3 / EL0 userspace on aarch64** — the SVC syscall entry + aarch64 register ABI,
  `copy_*_user`/`SyscallFrame` aarch64 impls, EL0 drop (`SPSR_EL1`/`ELR_EL1`), and
  porting `servers/libthemelios` (~30 `syscall` sites) + an aarch64 server linker script
  + aarch64 server builds in xtask. This is the "two binary sets" milestone; it gates
  containers/`api-server` on ARM. Revisit when Phase 8 (hyperscaler ARM) needs it.
- **Storage + networking on aarch64** — `drivers::pci` is port-I/O based; `virt`
  exposes VirtIO as **virtio-mmio** (device-tree) or PCIe **ECAM**. The smoltcp stack
  is already portable but has no device to bind to until a virtio-mmio/ECAM transport
  is written. Post-Phase-7.
- **GICv3** — for real cloud ARM; GICv2 suffices in QEMU `virt`.
- **Secure boot / real firmware** — Phase 8.

## Open questions for review

- Is scoping Phase 7 to kernel-boot-to-shell (no ring-3) the right cut, or should the
  EL0/userspace ABI (7.x) be pulled into Phase 7 to match "Docker on ARM" expectations?
- 7.0a seam refactor as a standalone amd64-only PR: acceptable big-but-mechanical diff,
  or split further?
- GICv2 vs GICv3 for the first bring-up — is deferring GICv3 acceptable given Graviton
  is GICv3?
- Semihosting vs PSCI `SYSTEM_OFF`+serial-parse for the CI exit contract.
- Anything in `mm/addr.rs` canonical-address handling that will silently break under
  the TTBR0/1 split rather than fail loudly.
