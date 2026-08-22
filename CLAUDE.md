# ThemeliOS

An experimental capability-based microkernel OS written in Rust, designed as a secure foundation for running container workloads in cloud environments. "Themelio" (θεμέλιο) is Greek for "foundation."

## Project Overview

ThemeliOS is a from-scratch kernel — it does not use or build on top of Linux. It implements its own scheduler, memory manager, IPC, and isolation primitives. The long-term goal is a minimal, immutable OS that boots, runs OCI-compatible containers, and serves as a Kubernetes/K3s worker node.

## Architecture

- **Microkernel**: minimal kernel providing memory management, scheduling, IPC, and capability enforcement. Everything else (drivers, filesystems, networking) runs in userspace.
- **Capability-based security**: all resource access is mediated by unforgeable capability tokens. No ambient authority — processes can only access resources they've been explicitly granted.
- **Immutable root**: the root filesystem is read-only. Nodes are cattle, not pets. Updates are whole-image swaps.
- **No SSH, no shell**: all management is via an external API. There is no interactive login.
- **Logging**: RAM-backed ring buffer with real-time streaming to external collectors. No persistent log storage on-node.

## Target Platforms

- **Primary**: x86_64 (amd64)
- **Secondary**: aarch64 (arm64)
- **Dev/test**: QEMU/KVM
- **Future**: bare metal (headless), AWS, GCP, Azure

## Tech Stack

- **Language**: Rust (nightly, `#![no_std]`)
- **Build system**: `cargo` workspace with `xtask` pattern (no Makefile)
- **Testing**: QEMU-based boot tests via `cargo xtask run`
- **Docs**: `rustdoc` for code, `mdbook` for architecture and usage documentation
- **CI**: GitHub Actions (future)

## Project Structure

```
themelios/
├── kernel/              # The kernel crate (no_std)
│   ├── src/
│   │   ├── main.rs      # Kernel entry point
│   │   ├── arch/        # Architecture-specific code (x86_64, aarch64)
│   │   ├── mm/          # Memory management (physical, virtual, allocator)
│   │   ├── sched/       # Scheduler
│   │   ├── cap/         # Capability system
│   │   ├── ipc/         # Inter-process communication
│   │   ├── drivers/     # VirtIO and platform drivers
│   │   ├── fs/          # Filesystem (read-only root, ephemeral layers)
│   │   └── net/         # Network stack
│   └── Cargo.toml
├── xtask/               # Build/run/test tooling (runs on host)
│   ├── src/
│   │   └── main.rs
│   └── Cargo.toml
├── docs/                # mdbook documentation
│   ├── book.toml
│   └── src/
│       ├── SUMMARY.md
│       └── ...
├── Cargo.toml           # Workspace root
├── rust-toolchain.toml  # Pins nightly toolchain
├── CLAUDE.md            # This file
├── LICENSE              # MIT
└── .cargo/
    └── config.toml      # Per-target build flags, linker scripts
```

## Code Style

### Comments and Documentation

**All code must be extensively commented.** This is a learning project and a complex system — comments are mandatory, not optional.

- Every public function, struct, enum, trait, and module gets a `///` doc comment explaining what it does, why it exists, and how it fits into the larger system.
- Non-obvious logic, Rust idioms, and OS/hardware concepts get inline `//` comments.
- Each module (`mod.rs` or top-level file) gets a `//!` module-level doc comment explaining the module's purpose and design.
- When implementing OS concepts (page tables, scheduling algorithms, capability systems), explain the concept, not just the code.

### Rust Style

- Follow standard Rust conventions (`rustfmt` defaults).
- Use `clippy` with default lints.
- Prefer safe Rust. Use `unsafe` only where hardware interaction or performance demands it, and always document why it's safe with a `// SAFETY:` comment.
- All `unsafe` blocks must have a corresponding safety comment.

## Build Commands

```bash
# Build the kernel (defaults to x86_64)
cargo xtask build

# Build for a specific architecture
cargo xtask build --arch aarch64

# Build kernel and create bootable ISO (without launching QEMU)
cargo xtask iso

# Build, create ISO, and run in QEMU (headless, serial output to terminal)
cargo xtask run

# Same, but with a QEMU graphical window
cargo xtask run --display

# Build and run on arm64 QEMU
cargo xtask run --arch aarch64

# Run tests
cargo xtask test

# Build documentation
cargo xtask docs
```

## Development Setup

### Prerequisites

Host tools the build/test pipeline shells out to (Rust itself is managed by
rustup via `rust-toolchain.toml`, not the OS package manager):

- Rust nightly toolchain (pinned via `rust-toolchain.toml`)
- QEMU: `qemu-system-x86_64` and `qemu-system-aarch64`
- `xorriso` (bootable ISO creation)
- `mksquashfs` from `squashfs` / `squashfs-tools` (SquashFS root image, Phase 3)
- `mkfs.ext2` from `e2fsprogs` (ext2 data volume image, Phase 3; keg-only on
  macOS — `xtask` resolves the keg path automatically)
- mdbook (for documentation)

On macOS these are declared in the repo's `Brewfile` — run `brew bundle` to
install them all. See the [development setup guide](docs/src/dev-setup.md) in
the mdbook for per-OS instructions.

## Milestone Roadmap

**Important**: Milestone status is tracked in two places that must be kept in sync:
1. This table below
2. `docs/src/milestones.md` — both the summary table at the top and the inline status label on each phase heading

When starting or completing a phase, update all three locations (this table, the docs summary table, and the docs phase heading).

| Phase | Goal | Status |
|-------|------|--------|
| **0** | Boot on QEMU, serial output | Complete |
| **1** | Memory allocator, scheduler, interrupts (x86_64) | Complete |
| **2** | Capability system, process isolation, IPC, audit logging | Complete |
| **3** | VirtIO block driver, read-only FS, ephemeral layers | Complete |
| **4** | VirtIO net driver, TCP/IP stack | Complete |
| **5** | OCI containers, Linux syscall compat, exec, registries | Complete (core; real-image busybox, live registry transport, ring-3 oci-server deferred) |
| **6** | Docker-compatible management API | Complete (core; TLS/mTLS, exec/streaming, live docker CLI, networks/images deferred) |
| **7** | aarch64 port (boot, memory, scheduler, shell) | Complete (ring-0 core; EL0/storage/net/containers deferred) |
| **8** | aarch64 parity (EL0, storage, net, containers) | Planned |
| **9** | Testing and benchmarks | Not started |
| **10** | Kubernetes worker node (full parity) | Not started |
| **11** | GPU support across clouds | Not started |
| **12** | Production operations (observability, updates) | Not started |

## Current Status — resume here

_Single source of truth for "where are we / what's next". Update the relevant
line when finishing a sub-phase. Detailed per-sub-phase checklists live in
`.sisyphus/plans/` (tracked in git); the git commit history has the full
narrative per commit._

**Phase 8 — aarch64 parity: IN PROGRESS.** ✅ 8.spike (EL0 round trip proven), ✅ 8.1
(arch-neutral discovery seam + `PlatformInfo`); next is 8.2 (transport trait). Plan in
`.sisyphus/plans/phase8-aarch64-parity.md` (v2, after five adversarial review passes — nine
v1 claims and fourteen v2 claims were false, and the sub-phase order is reversed from v1).
**Ten sub-phases plus a spike**, taking aarch64 from the Phase 7 ring-0 core to full amd64
parity: VirtIO transport (8.1–8.3), EL0 (8.4–8.5), storage and networking (8.6–8.7), the
Linux personality (8.8–8.9), containers and the management API (8.10).

**Real ARM server hardware is deliberately NOT planned.** The roadmap's Phase 8 label
originally read "hyperscaler"; that work — platform discovery, GICv3 + ITS, PCIe ECAM +
MSI-X, SMP, cloud NICs and NVMe, secure boot, measured boot — gets its own phase, written
when parity lands. It was cut because review found the error density concentrated exactly
there, and a plan that is wrong is worse than a stub saying "not planned". What survived
verification is kept as a seed list in the plan's Tier 5 stub — including that **none of
the three clouds runs virtio** (AWS is ENA, GCP gVNIC, Azure MANA, all with NVMe), so none
of Phase 8's virtio work runs on any of them, and that `qemu-system-aarch64 -M sbsa-ref` is
the cheap genericity test.

Parity has one measurable definition — `test_runner`'s `SKIPPED` list empty on both
architectures, i.e. 54/54 running on each. It is **16 running / 38 skipped** on aarch64
today. Three of the 38 cannot be retired by porting (`test_pci_scan`, `test_syscall`,
`test_linux_exec`'s TLS assertion) and are retired by reframing, each decided in the
sub-phase that owns it.

**Phase 7 — aarch64 port: COMPLETE (ring-0 core).** A ring-0 kernel-core port to QEMU `virt`
(ARM64); EL0/userspace, storage, networking and containers on ARM are a separate,
deferred ABI surface. amd64 stays green every sub-phase — the QEMU suite is the gate.
Detailed plan in `.sisyphus/plans/phase7-aarch64.md`. **Phase 8 continues this work to
parity — see above.**

- ✅ **7.0a** Arch-neutral `arch::{irq,time}` facade; all tick reads and interrupt
  sites routed through it, x86 impls re-exported unchanged.
- ✅ **7.0b** Boot to banner on QEMU `virt`: `linker-aarch64.ld`, aarch64 arch module
  (`boot`/`serial`/`irq`/`time`), per-arch `kmain` dispatch, `CPACR_EL1.FPEN` enabled
  in early boot (hardfloat target — compiler-emitted SIMD traps otherwise; **reversed
  in 7.3**, which targets softfloat and *clears* FPEN so stray SIMD traps), and the
  PL011 mapped by editing Limine's live TTBR1 tables (its HHDM maps RAM, not MMIO).
- ✅ **7.0c** Separate `themelios-{amd64,arm64}.iso` (amd64 hybrid BIOS+UEFI, arm64
  UEFI-only with `BOOTAA64.EFI`); `arm64-iso-smoke` boots the shipped image in CI.
- ✅ **7.1** **MMU/paging on kernel-owned tables.** `mm` is now arch-neutral: the
  4-level walker drives per-arch descriptor formats through a new `arch::paging`
  facade (`arch/{x86_64,aarch64}/paging.rs`). aarch64 clones Limine's kernel-half L0
  entries into its own root and loads **`TTBR1_EL1`** (not TTBR0 — at EL1 with no
  userspace TTBR0 translates nothing, so switching it would prove nothing);
  `TTBR0_EL1` is parked at 0. Barrier discipline at every map/unmap
  (`DSB ISHST` → `TLBI` → `DSB ISH` + `ISB`). **`MAIR_EL1` and `TCR_EL1` are adopted
  and *verified*, never rewritten** — the cloned entries carry Limine's `AttrIndx`
  values, so installing our own MAIR would silently reinterpret the cacheability of
  every inherited mapping. Proven by an arch-neutral
  `mm::page_table::selftest()` (map → translate → write/read → verify the physical
  frame via HHDM → unmap → translate-is-None) whose sentinel the CI arm64 smokes now
  assert. Bootloader memory is deliberately **not** reclaimed on aarch64 until 7.2
  provides fault reporting.
- ✅ **7.2** **Exceptions + GIC + timer tick.** `VBAR_EL1` vector table (16 slots of
  *code*, not pointers — the CPU branches into them, so all sixteen are populated and
  each stub is small enough to fit 128 bytes); `ESR_EL1.EC` decoding with `FAR_EL1` and
  the fault-status code broken out; `BRK` trapped and resumed (`ELR` points *at* the
  trapping instruction, unlike `int3`). Early boot now **switches to `SP_EL1`**
  (`use_sp_el1`) — Limine hands off with `SPSel = 0`, which routes exceptions to the
  `0x000` vector group *and* lands them on an uninitialised `SP_EL1`, so the entry stub
  faults, nests, and reports the nested syndrome instead of the real one. GICv2
  (GICD+GICC) mapped via `mm::mmio`, which is the first real exercise of the
  Device-`nGnRnE` `AttrIndx` path 7.1 could only verify by inspecting a descriptor.
  `CNTV` virtual timer (trap-free under EL2, unlike `CNTP`) at 100 Hz on PPI 27 driving
  `arch::time::tick_count`. Self-tests assert a `brk` is caught and that **five** ticks
  arrive — one tick would pass even if the timer were never re-armed or the GIC never
  EOI'd, which are the two failure modes here. Verified in CI: `tick 0 -> 5, 5 IRQs, 0
  spurious`, `CNTFRQ = 62.5 MHz`, `GICD TYPER` reads 288 INTIDs.
- ✅ **7.3** **Scheduler context switch + preemption.** `sched` is now arch-neutral: the
  ready queue, task lifecycle and round-robin policy are shared, with context switching
  behind a new `arch::context` facade (`arch/{x86_64,aarch64}/context.rs`). aarch64
  saves the AAPCS64 callee-saved set (x19-x30, 96 bytes — **no FP save area**, sound
  only because the kernel is softfloat) and `ret`s through the restored **`x30`**, not
  off the stack as x86 does, so a new task's initial frame puts `task_bootstrap` in the
  `x30` slot and the entry fn in `x19`. The timer IRQ calls `schedule()` **after**
  `GICC_EOIR` — a GICv2 CPU interface delivers nothing while an interrupt is active, so
  scheduling first would stop the next tick ever arriving. What is *not* ported is the
  ring-3 machinery `sched` also carries (`kernel_stack_top`→TSS.RSP0, `fs_base`,
  `clone_entry`, CR3 swaps): each is EL0-era state whose aarch64 analog (`SP_EL1`,
  `TPIDR_EL0`, `TTBR0_EL1`) has nothing to hold in a ring-0-only kernel, so it is
  `#[cfg]`'d off rather than guessed at. **`TPIDR_EL1` *is* plumbed** as the GS-base
  analog: a `PerCpu` block rewritten on **every** switch (the structural form of the
  4.5 stale-GS fix), read back *through the register* so it cannot be decorative, and
  used by the fatal-exception reporter to name the faulting task — which must not take
  the scheduler lock, since `schedule()` holds it. Also fixed `arch::irq` calls in
  `sched` that 7.0a had left `#[cfg]`-gated to x86 — live on aarch64, they would have
  entered `schedule()` with interrupts unmasked; `schedule()` now `debug_assert!`s that
  contract. Proven by boot self-tests the CI smokes assert: three non-yielding workers
  score **13 tick-slices each** (equal share is 12 across 4 runnable tasks) inside a
  band of [6, 24] — a floor alone would pass `[60,6,6]`, the monopoly it claims to
  reject — and all three return through `task_exit`. Fairness is measured in *ticks
  resident*, not loop iterations: under TCG the latter varied 4.5x between identical
  boots and says nothing about the scheduler.
  Two defects the adversarial review caught, both silent: `task_bootstrap` cleared only
  `DAIF.I`, so every spawned task inherited **masked SError** for life (a new task is
  entered by `ret` out of the IRQ handler, not `eret`, and the mask then propagated
  through `SPSR`) — disabling 7.2's abort vector and misattributing any pending SError
  to a later task; and `CPACR_EL1.FPEN` was **`0b11`** — Limine leaves FP enabled, so
  the "stray SIMD traps loudly" backstop justifying the absent `v8`-`v15` save area had
  never existed. FPEN is now cleared *and verified* at boot, `verify_fp_trapped` gates
  the sentinel, and the kernel passes every self-test with FP trapping.
- ✅ **7.4** **Shell, portable tests, CI, finalize.** `mod shell` and `mod test_runner`
  un-gated; `cap`/`audit`/`ipc`/`http`/`oci`/`mm::shared` un-gated for aarch64 (all
  `alloc`-only; `cap`+`audit` needed only `ProcessId`, so aarch64 gets the 41-line
  identity newtype without the 712-line process table and `current_process_id` answers
  `ProcessId::KERNEL`). **aarch64 suite: 16 passed / 0 failed / 38 skipped of 54** —
  skipped tests name the deferred subsystem, and all 39 `Ok(())` stubs — which would
  have reported a vacuous "54 passed" — are deleted (Momus caught three still live: the
  `TESTS` entry un-gated but the impl still x86-gated, so the table bound the stub). **Exit contract** (the plan's riskiest
  unknown): no `isa-debug-exit` on `virt`, so a serial sentinel carries the verdict and
  **PSCI `SYSTEM_OFF`** (HVC conduit) stops the machine — which is what distinguishes
  "died mid-suite" from "hung"; all four rows verified by hand during development, by injecting the matching fault. **Shell**:
  PL011 RX via `IMSC` `RXIM`+`RTIM` (the timeout matters on real parts; QEMU hard-codes
  `read_trigger=1` and never raises `INT_RT`, so `RXIM` alone carries it there), acked
  **before** draining — the reverse order can wedge the console permanently, since QEMU
  raises RX only on the FIFO's 0→1 transition, UART on **SPI 1 = INTID 33**, 8 of 25 commands with the
  rest `#[cfg]`'d out of dispatcher, impls *and* help text. Three defects the tests
  caught: `test_page_tables` assumed 1 MiB is RAM (true on a PC, an unbacked hole on
  `virt`); the suite ran with interrupts in whatever state boot left them, enabled only
  as a side effect of the first `yield_now` (an order-dependent environment — real, but
  **not** the cause of the `test_ipc` flake I attributed to it: that was `ipc`'s
  lost-wakeup guard, `#[cfg]`'d to x86 by the same 7.0a sweep 7.3 fixed in `sched`,
  reproducing at 2-in-10 until un-gated); and the boot self-tests left dead-task stacks for
  `cleanup_dead_tasks` to reclaim mid-test, breaking `test_frame_allocator`'s exact
  frame-count equality 5 runs of 5. Plus one in the shell: the RX handler buffered
  bytes but never called `wake_shell`, so every register read correctly and the console
  was silent. mdbook `aarch64.md` documents the port.

**Phase 6 — Management API: COMPLETE (core).** The node is driven entirely
through an external HTTP API (no SSH, no shell). A ring-3 **`api-server`** holds a
`Management` **sentinel capability**, opens an inbound-TCP listener via a
**kernel-accept shim**, and serves a Docker Engine API subset behind **two
authorization layers**: the kernel cap (which *process* may drive the ABI) + an
app-layer **bearer token** (which *client* may call the API). Untrusted HTTP/JSON
parsing stays in ring 3, **fail-closed against a node-halting fault**; every
container mutation crosses into the kernel through the capability-checked, audited
**`SYS_MGMT`** ABI. Full design in mdbook `management-api.md`. `cargo xtask test`
before pushing. **Phase 7 (aarch64 port) is now in progress — see above.**

- ✅ **6.0** HTTP/1.1 request parser + minimal JSON serializer (`http`, `oci::json`),
  `alloc`-only, fail-closed (bounded sizes, `checked_add`, `None`-on-malformed), lifted
  unchanged into ring 3. `test_http_*`.
- ✅ **6.1b** Container **rootfs confinement** — each container confined to a `/c/<id>`
  subtree via a single `host_path` choke point; `create_confined`. Isolation probe +
  `test_container_confinement`.
- ✅ **6.3** Kernel **management ABI** (`mgmt`) + `CapType::Management` sentinel cap —
  cap-checked, audited (`ApiAccess`) list/inspect/create/start/stop/logs/node_info/
  listen ops returning owned JSON. `test_management_capability`.
- ✅ **6.4** Ring-3 **inbound TCP** + sentinel-cap grant — `ServerConfig::grant_management`
  mints the cap into a kernel-spawned server; the kernel-accept shim (`mgmt::listen`
  mints a listener `Socket` cap) + fail-closed control. Host-coordinated echo test.
- ✅ **6.5** api-server **read pipeline** — `SYS_MGMT` JSON read verbs + wrappers;
  `http`+`json` single-sourced into ring 3 via `#[path]`; GET routing (`_ping`/`version`/
  `info`/`containers/json`/`{id}/json`).
- ✅ **6.5b** **Write verbs** — `SYS_MGMT` create/start/stop/logs; `POST /containers/create`
  (untrusted body JSON → `Image` extraction → NUL-join), start/stop → 204/404/409.
  Deterministic in-process **self-test** (routing + JSON content, no flaky wire path) +
  a single live inbound smoke.
- ✅ **6.6** **Bearer-token auth** — token provisioned via boot-info to the api-server
  only; all routes except `/_ping`/`/version` require `Authorization: Bearer`; missing/
  wrong → **401 before any op** (incl. unknown paths); auth outcomes audited
  (`ApiAccess` / `ApiAuthReject`). Self-test asserts `[200,401,401,200,400,500,409]`.
- ✅ **6.7** Finalize — mdbook `management-api.md`; trackers reconciled; **Momus
  hardening audit** of the untrusted-input surface: **no reachable kernel panic, no
  auth bypass** (fail-closed confirmed end-to-end); landed the F1 empty-token guard;
  F2 (dev secret→Phase 8), F3 (bounded slowloris), F4 (container-count cap), F5 (json
  permissiveness — NUL guard load-bearing) tracked.

Per-milestone branches: each Phase 6 sub-phase was a fresh branch + PR off the
latest `main` (6.3 #17 … 6.6 #21); 6.7 lives on `claude/phase-6.7-finalize`.
**Deferred (documented):** TLS/mTLS, interactive `exec`/websocket streaming, a live
`docker` CLI / multi-request `curl` mutation sequence (net-server RX recycling +
`/data`-at-boot), and broader Engine API breadth (networks, volumes, images).
**Phases 4 (Networking) and 5 (Containers) are complete** — design + sub-phase detail
in mdbook `networking.md` / `containers.md`.

Notable — three long-standing concurrency bugs in the syscall/IPC core were
root-caused and fixed during Phase 4.5 (they grew from rare to majority-of-runs
as the suite added tasks/syscalls; `b1b7cea` had only partially masked the
first):
- **Syscall-exit double-fault.** `PerCpu.user_rsp_scratch` (`gs:0x8`) is a single
  shared slot; the syscall exit re-stashed the user RSP there and read it back
  with interrupts *enabled*, so a preempting syscall clobbered it and the task
  `sysretq`'d onto another task's stack. Fixed by making the exit tail atomic
  (`cli`; `sysretq` restores IF).
- **Stale GS base.** GS base is a global register not saved per task, and the
  interrupt stubs never `swapgs`, so an interrupt in ring 3 could context-switch
  with the user GS base. Fixed by writing `IA32_GS_BASE = &PER_CPU` on every
  context switch (not just `KERNEL_GS_BASE`).
- **Spurious IPC "call failed".** `ipc_receive` woke a popped caller-sender
  before its reply was delivered (when `ipc_call` raced ahead of the server
  parking). Fixed by only waking plain sends (`reply_token == 0`); callers wait
  for `ipc_reply`. Same guard as `ipc_try_receive`.

## License

MIT — Copyright (c) 2026 Rudi MK
