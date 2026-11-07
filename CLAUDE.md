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

Architecture is selected by **one** flag, `--arch`, on every command that produces or runs
a kernel. It accepts `amd64` | `x86_64` | `x86-64` and `arm64` | `aarch64`. There is no
`--amd64` / `--arm64` shorthand: two spellings of one selector drift, and a silently
ignored flag builds the default architecture while reporting success. Omitting it selects
amd64, but prefer naming it on both sides so the two commands read as a pair.

```bash
# Build the kernel
cargo xtask build --arch amd64        # x86_64-unknown-none
cargo xtask build --arch arm64        # aarch64-unknown-none-softfloat

# Build kernel and create bootable ISO (without launching QEMU)
cargo xtask iso --arch amd64          # target/themelios-amd64.iso
cargo xtask iso --arch arm64          # target/themelios-arm64.iso

# Build, create ISO, and run in QEMU (headless, serial output to terminal;
# lands at the debug shell — Ctrl+A, X to quit QEMU)
cargo xtask run --arch amd64
cargo xtask run --arch arm64

# Same, but with a QEMU graphical window
cargo xtask run --arch amd64 --display

# Run the test suite (same 55 tests on both; unported subsystems report [SKIP])
cargo xtask test --arch amd64
cargo xtask test --arch arm64

# Both suites bind a fixed host TCP port, so two cannot run concurrently.
THEMELIOS_TEST_PORT=15107 cargo xtask test --arch amd64

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
| **8** | aarch64 parity (EL0, storage, net, containers) | In progress (8.spike, 8.1–8.5 done) |
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
(arch-neutral discovery seam + `PlatformInfo`), ✅ 8.2 (`VirtioTransport` trait + PCI impl
extracted), ✅ 8.3 (virtio-mmio — the first sub-phase to move the ratchet, retiring
**seven** skips, not the eight the plan projected), ✅ 8.4 (EL0: 8.4a–e — entry/exit,
syscall dispatch, address spaces, the EL0 soak, FPSIMD context), and 8.5 (userspace
servers on aarch64) is **in progress**: ✅ 8.5a (arch-partitioned server staging + the blob
hash manifest), ✅ 8.5b (the native syscall ABI on aarch64 — one number list shared by
kernel and `libthemelios`, the IPC dispatcher, and `libthemelios`' 25 wrappers collapsed to
three positional primitives per arch), ✅ 8.5c (**the userspace servers build for
aarch64**: all seven flat servers plus `elf-smoke`, staged and hashed as
`servers-arm64.sha256`), ✅ 8.5d (**the servers actually run**) — which completes 8.5.
Next is **GICv3 support** (see below), then 8.6 (storage).

**8.5d — the process table and EL0 servers on aarch64.** `mod process` is un-gated on both
architectures, `embedded.rs` picks its staging directory through a `#[cfg]`'d
`server_blob!` macro (`concat!` needs literals, so a `const SERVER_ARCH` does not compose
with `include_bytes!`), and `spawn_server`'s ring-3 entry is arch-split: x86 keeps its
hand-built `iretq` frame, aarch64 calls `arch::aarch64::syscall::enter_el0`. The plan's
acceptance — "echo-server runs at EL0 and completes an IPC round trip" — is met and is now
a suite entry on both arches rather than a one-off: `test_server_spawn` passes on aarch64.

Three things were needed beyond un-gating, and each is where a reviewer should look:

  * **The scheduler resolves a task's address space differently per architecture.** x86
    looks `process_id` up in the process table; aarch64 reads `Task::ttbr0_root` directly,
    because 8.4d needed per-task EL0 spaces two sub-phases before `mod process` existed
    here. So `spawn_in_process` copies the process's root+ASID into the task — **outside**
    the scheduler lock, because taking `PROCESS_TABLE` under `SCHEDULER` inverts the order
    `create_process` → `spawn_server` → `spawn_in_process` already establishes. Mutation-
    tested: skip the copy and the arm64 boot dies with an instruction abort from a lower EL
    at `FAR_EL1 = 0x400000`, the init code page, because nothing installed `TTBR0_EL1`.
  * **`process::init()` had to join the aarch64 boot path**, right after `sched::init()`
    as on x86. Everything above it — `user_selftest`, `el0_ipc`, the soak — deliberately
    builds bare `AddressSpace`s and drops to EL0 with no process at all, and must keep
    doing so, so that a fault in `mod process` cannot masquerade as an EL0 fault.
  * **`process::init`'s payload is assembled, not hand-encoded, on aarch64.** The x86 blob
    is hand-assembled bytes with the endpoint patched in as a `mov r13, imm64`; the A64
    equivalent would need a `movz`/`movk` quartet with the value split across four
    instructions' bit 20:5 fields, and a slip there yields a blob that runs, sends to the
    wrong endpoint, and hangs. So it is a `global_asm!` blob (the `el0_ipc` mechanism) and
    the endpoint arrives in `x0` via `enter_el0_with_args`. Mutation-tested: read it from
    `x1` instead and the suite reports `[FAIL] test_userspace_init: init server never
    received a message from userspace` — a failure, not a hang.

Four skips retired, as planned: `test_process`, `test_userspace_init`, `test_server_spawn`,
`test_registry_pull`. The last needed no porting at all — it was x86-gated only because its
mock layer payload was `LINUX_SMOKE`, staged for amd64 alone; it is `ELF_SMOKE` now, which
both arches stage, and the test never executed the payload in the first place. The shell
also gained `procs` and `caps` (11 → **13 of 28** commands), since there is finally a
process table to list.

The a–d split is this session's decomposition of the plan's single 8.5 entry, not something
the plan names. **One scope change is recorded in the plan**: 8.5 was to rewrite all six
`global_asm!` `_start` routines, and only `elf-smoke`'s was. The other five issue Linux
personality syscalls by their x86_64 numbers, and `mod linux` — which holds both the
personality *and* the ELF loader — is x86-gated until **8.9**, whose Deliver list now names
them. Writing them in 8.5 would have been assembly that could not execute against a table
that does not exist. Plan in
`.sisyphus/plans/phase8-aarch64-parity.md` (v2, after five adversarial review passes — nine
v1 claims and fourteen v2 claims were false, and the sub-phase order is reversed from v1).
**Ten sub-phases plus a spike**, taking aarch64 from the Phase 7 ring-0 core to full amd64
parity: VirtIO transport (8.1–8.3), EL0 (8.4–8.5), storage and networking (8.6–8.7), the
Linux personality (8.8–8.9), containers and the management API (8.10).

**GICv3 is NOT supported.** The aarch64 `PlatformInfo` (`platform.rs`) is a hard-coded
QEMU `virt` GICv2 descriptor; 8.1 delivered a discovery *seam*, not discovery. Every
aarch64 QEMU invocation in `xtask` pinned `gic-version=2`, so CI had never once run any
other configuration. Asked for `gic-version=3` explicitly, the kernel wrote `GICC_PMR` at
an address GICv3 does not implement and took a synchronous external abort. That is real and
is fixed: the kernel reads `ID_AA64PFR0_EL1.GIC` before touching any GIC MMIO and halts
with a diagnosis, pinned by `cargo xtask arm64-gicv3-smoke` — the "cheap genericity test"
this file had recommended for two phases without wiring anything to it. **Actually
supporting GICv3** (system-register CPU interface + redistributors) is its own sub-phase,
scheduled after 8.5 rather than deferred to the unplanned hardware phase — and 8.5 closed
at 8.5d, so **it is next in the queue**.

**But read the GICv3 note further down before acting on that ordering.** It was placed next
partly on the belief that it was the Apple Silicon unblocker, and a boot log from a real Mac
disproved that: `ID_AA64PFR0_EL1.GIC == 0` there, so that CPU has no GICv3 system-register
interface and GICv3 support would not help it at all. The sub-phase is still worth doing for
QEMU and server-class parity; the *reason* it was prioritised is gone. Whether it still
precedes 8.6 (storage) is an open call, not a settled one.

**Two claims that sat here were wrong — both measured false on 2026-09-24.** First, this
paragraph said `-M virt` *defaults* to GICv3. It does not, on QEMU 8.2.2 under TCG: `-M
virt` with `cortex-a72`, `max` and `neoverse-n1` all came up **GICv2** (the detection never
fired; `[gic] GICv2 up:` in every log). The default is "newest available", which depends on
QEMU version, CPU and accelerator — `cmd_arm64_gicv3_smoke`'s own comment says exactly
that, and this paragraph contradicted it.

Second, and the reason it matters: this was written as the diagnosis of a **UTM** boot
failure, and it is not that. UTM still fails on an ISO built from `719453b`, which contains
the fix. EDK2's `Synchronous Exception at 0x…` prints `ELR` — the faulting PC — and theirs
is `0x47529074`, low physical RAM. ThemeliOS executes at `0xffffffff8…`, so the fault is
**not in ThemeliOS at all**; it is in Limine or the firmware, before the kernel's first
instruction. It reproduces at the same address across two different kernel builds, which is
what one expects if the faulting code is one of the two parts that did not change.

**That conclusion is specific to UTM and does not generalise** — see the Apple Silicon
entries below. On the same Mac, under plain QEMU + HVF, the kernel boots all the way; the
"kernel never runs" inference was drawn from the UTM console alone and was wrong there.

**Limine is pinned to v10.x (was v8.x), and the pin is now enforced.** Two separate
things, both landed 2026-09-24 while investigating the UTM boot failure.

*The bump.* We shipped v8.7.0; upstream is v11.4.1. v10.8.5 accepts the boot-protocol base
revision the `limine` crate (0.5) asks for, needs no kernel change, and passes both suites
and all three arm64 boot smokes (`arm64-smoke`, `arm64-iso-smoke`, `arm64-gicv3-smoke`),
plus a by-hand OVMF boot of the amd64 ISO. v11 refuses outright — `Base revision 3 is no longer supported
for aarch64 (minimum: 6)` — so going further means bumping the crate too and reading what
revisions 4-6 changed about the aarch64 handoff. That is the blocker, and it is the
bootloader's minimum rather than a crate build error.

*The pin.* `ensure_limine` returned early the moment `target/limine/limine` existed, which
made `LIMINE_BRANCH` a comment: editing it changed nothing wherever a checkout already sat.
CI caches `target/limine` under a key of `hashFiles('**/Cargo.lock', 'rust-toolchain.toml')`
— the branch is not in the key, and `restore-keys` falls back to any earlier entry — so the
bump would have been silently ignored on every warm runner and the published ISO would have
kept shipping v8 while the source said v10. The checkout is stamped with its branch now and
re-cloned on a mismatch.

**CI now boots the aarch64 ISO with a framebuffer** (`-device ramfb` in `arm64-iso-smoke`)
and Limine runs with `verbose: yes`. Every aarch64 QEMU invocation in `xtask` had been
display-less, so the project had never once run the configuration a desktop VM gives you —
and it is not cosmetic. Limine probes each framebuffer with `AT S1E1W`, reads its attribute
out of `PAR_EL1`, and installs it as **MAIR_EL1 index 1**: headless, MAIR arrives `0x…00ff`
and Device-nGnRnE is index 1; with a framebuffer it arrives `0x…ffff` and Device moves to
**index 2**. `paging::device_attr_index` searches rather than assumes, so it was already
right — but nothing had demonstrated that, and the hard-coded index its comment records as
the tempting shortcut would have mapped the PL011 **Normal write-back** for every desktop
user: speculative register reads and coalesced writes, no fault, no message. Measured:
`ramfb` exercises this path, `virtio-gpu-pci` does not (MAIR stays at the headless value
under this firmware). Note Limine writes its own output to the framebuffer when one exists,
so `verbose: yes` shows up on a desktop VM's display, not in a serial capture.

**Still untested in CI: the amd64 ISO's UEFI path.** `cargo xtask test --arch amd64` boots
the ISO, but with `-cdrom` and no pflash — that is SeaBIOS, so `BOOTX64.EFI` is published
without ever being executed. It was verified by hand under OVMF for the Limine bump (boots
to the shell), which is not the same as covered. Adding the smoke needs an OVMF install
step in `build.yml`.

**The UTM failure is FIXED, and the Limine bump is what fixed it.** Confirmed on the
machine that failed: the arm64 ISO from `e77a480` boots to the ThemeliOS console in UTM on
Apple Silicon. It had previously died with EDK2's `Synchronous Exception at 0x47529074` and
no kernel output at all, across two different kernel builds.

The attribution is sound by elimination rather than by guess. The fault was *before the
kernel's first instruction* — the faulting PC was low physical RAM while ThemeliOS executes
at `0xffffffff8…` — so only something running before the kernel could be responsible, and
the only such thing that changed is the bootloader: **Limine v8.7.0 → v10.8.5**. The kernel
changes in the same ISO cannot explain it, because the old ISO never reached kernel code.

**How it was found matters more than the fix, because the fix was a guess that happened to
be right.** Eighteen QEMU configurations were tried and **none reproduced it** — GICv2 and
v3, `cortex-a72`/`max`/`neoverse-n1`, 512M/4G/8G, 1 and 4 CPUs, virtio-blk and USB CD,
headless and two framebuffers, `virtualization=on`, Limine v8 and v10. Apple Silicon under
HVF with UTM's own EDK2 build was simply not reachable from this environment. So the bump
shipped explicitly labelled *"the best-supported candidate, not a confirmed fix"*, and was
only confirmed when the user booted it. Had it not worked, that labelling is what would
have kept the search honest instead of closing the issue on a plausible story.

Two earlier diagnoses of this same symptom were **wrong**, and both are worth remembering:
GICv3 (the ISO under test already contained that fix and still failed), and "the kernel
never runs" generalised from the UTM console to all of Apple Silicon (a plain QEMU + HVF
boot log showed the kernel running fine).

**But GICv3 is NOT the Apple Silicon unblocker, and it was queued partly on the belief
that it was.** A boot log from a real Mac (QEMU + HVF, `-cpu host`) shows our GICv3
detector **not firing** — so `ID_AA64PFR0_EL1.GIC == 0` and that CPU exposes no GICv3
system-register interface at all. GICv2 comes up and reads back sanely there. Whatever is
wrong on Apple Silicon, adding GICv3 support would not touch it. Re-prioritise on that
basis rather than on the earlier assumption.

**Apple Silicon under HVF delivers no timer interrupts, and that used to hang the boot.**
Measured on the same log: GIC up (`1024 INTIDs, TYPER=0x0478001f` — our QEMU reports
`288 / 0x00000008`), `CNTVCT_EL0` advancing at 24 MHz, and `timer: FAIL — 0 IRQs, 0
spurious`. Everything downstream needs the tick, so the scheduler self-test scored
`slices [0, 0, 0]`, and then boot **hung forever** at `Spawned task 6 ("el0-payload")`.

The hang was ours and is fixed. `boot.rs` already states the principle — *"if delivery is
broken the tick never moves and a loop waiting on it would hang the boot rather than fail
it"* — and `sched_selftest`/`timer_selftest` pair every tick bound with a `CNTVCT_EL0`
one. **Two loops did not**: the 8.4b EL0 wait and `el0_ipc`'s, each carrying a comment
asserting the tick "is actually running", which is an assumption about the hardware rather
than something the code establishes. Both now carry the counter bound, slack at 2x the
tick bound so a healthy machine is unaffected. Reproduced locally by disabling the timer
PPI: without the fix the last line is `Spawned task 6 ("el0-payload")`, matching the Mac
exactly; with it, boot reports 8.4b/8.4d/8.5b FAILED and reaches the shell.

**They were two problems, and one is now closed.** UTM's firmware-level exception is fixed
by the Limine bump (above). The no-timer-IRQ behaviour under plain QEMU + HVF is a separate
issue and is **still open**: the hang it caused is gone, but nothing has been done about
delivery itself.

Still unknown, and worth stating as unknown rather than inferred:

  * **Why HVF delivers no timer IRQ.** The next datum is the same QEMU one-liner with
    `-accel tcg` in place of `-accel hvf` — if the timer works under TCG and not HVF, it is
    interrupt injection rather than our GIC or timer setup.
  * **Whether the timer works in UTM.** UTM now reaches the console, but the boot output
    was not captured, so we do not know whether `timer: FAIL — 0 IRQs` appeared there or
    whether the tick is live. Both are consistent with reaching a prompt: a dead tick now
    *reports and continues* rather than hanging. Reading one line of that boot log would
    settle it, and until someone does, do not assume preemption works on Apple Silicon.

**Real ARM server hardware is deliberately NOT planned.** The roadmap's Phase 8 label
originally read "hyperscaler"; that work — platform discovery, GICv3 + ITS, PCIe ECAM +
MSI-X, SMP, cloud NICs and NVMe, secure boot, measured boot — gets its own phase, written
when parity lands. It was cut because review found the error density concentrated exactly
there, and a plan that is wrong is worse than a stub saying "not planned". What survived
verification is kept as a seed list in the plan's Tier 5 stub — including that **none of
the three clouds runs virtio** (AWS is ENA, GCP gVNIC, Azure MANA, all with NVMe), so none
of Phase 8's virtio work runs on any of them, and that `qemu-system-aarch64 -M sbsa-ref` is
the cheap genericity test.

**Filed for that phase, and wired so it cannot be missed: the virtio drivers trust the
device.** `Virtqueue::poll_used` returns the used ring's `id` and `len` unvalidated, and
the async RX path (`virtio/net.rs::receive`) feeds `id` into pointer arithmetic *and* into
`write_desc(id, …)` — an out-of-bounds **write** past the descriptor table for any
`id >= size` — while clamping the frame length only against the caller's buffer, never
against `RX_BUF_SIZE - VIRTIO_NET_HDR_LEN`. The **synchronous** path already validates
(`submit_and_wait` rejects a completion id that is not the chain it submitted), so this is
an asymmetry between two paths rather than a policy.

Not a vulnerability today: the device is the hypervisor, which a guest necessarily trusts.
It becomes one with passthrough, SR-IOV or a real NIC — i.e. in this phase. Same class as
**CVE-2026-99999** (Linux `skb_checksum_help`, CWE-20, CVSS 9.3), whose own text argues for
the check anyway: *"as a defence in depth measure, let's also check."*

The reason it is not merely written here: this file has a track record of recording a
precaution and never wiring it to anything — `-M sbsa-ref` sat in this very paragraph for
two phases unconnected to any test, `LIMINE_BRANCH` read as a pin and was a comment, and
`boot.rs` stated the tick-bound rule in one place while two loops ignored it. So the
obligation is attached to the thing that changes: `virtio::DEVICE_IN_TCB` is `true` with a
`const _: () = assert!(…)` beside it, and setting it to `false` — which is what declaring
a device untrusted *means* — fails the build with a message naming both fixes.
Mutation-tested: flipping it produces `error[E0080]` carrying those instructions.

Parity has one measurable definition — `test_runner`'s `SKIPPED` list empty on both
architectures, i.e. 55/55 running on each. It is **29 running / 26 skipped** on aarch64
today (16/39 before 8.3, 23/32 after it, 25/30 after 8.4c). Three of the 26 cannot be
retired by porting (`test_pci_scan`, `test_syscall`, `test_linux_exec`'s TLS assertion) and
are retired by reframing, each decided in the sub-phase that owns it.

Note the denominator, and note that this line goes stale silently. It previously read
"54/54" and "16 running / 38 skipped", wrong on both counts — `SUITE_SIZE` has been 55, and
16 + 38 does not reach it — and then sat at "23 running / 32 skipped" through 8.4c, which
had un-gated two. The suite itself asserts `TESTS.len() + SKIPPED.len() == SUITE_SIZE`
before running anything, so the *kernel* cannot drift; this sentence can, and only reading
it against a run catches that.

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
  never existed. FPEN was then cleared *and verified* at boot and the kernel passed
  every self-test with FP trapping. **Superseded by 8.4e**, which had to enable FP again
  for userspace's sake — `FPEN` has no encoding permitting EL0 while trapping EL1 — and
  replaced the trap with a build-time softfloat assertion plus a per-task FPSIMD save
  area; `verify_fp_trapped` no longer exists.
- ✅ **7.4** **Shell, portable tests, CI, finalize.** `mod shell` and `mod test_runner`
  un-gated; `cap`/`audit`/`ipc`/`http`/`oci`/`mm::shared` un-gated for aarch64 (all
  `alloc`-only; `cap`+`audit` needed only `ProcessId`, so aarch64 gets the 41-line
  identity newtype without the 712-line process table and `current_process_id` answers
  `ProcessId::KERNEL`). **aarch64 suite at the close of 7.4: 16 passed / 0 failed /
  39 skipped of 55** —
  skipped tests name the deferred subsystem, and all 39 `Ok(())` stubs — which would
  have reported a vacuous "54 passed" — are deleted (Momus caught three still live: the
  `TESTS` entry un-gated but the impl still x86-gated, so the table bound the stub). **Exit contract** (the plan's riskiest
  unknown): no `isa-debug-exit` on `virt`, so a serial sentinel carries the verdict and
  **PSCI `SYSTEM_OFF`** (HVC conduit) stops the machine — which is what distinguishes
  "died mid-suite" from "hung"; all four rows verified by hand during development, by injecting the matching fault. **Shell**:
  PL011 RX via `IMSC` `RXIM`+`RTIM` (the timeout matters on real parts; QEMU hard-codes
  `read_trigger=1` and never raises `INT_RT`, so `RXIM` alone carries it there), acked
  **before** draining — the reverse order can wedge the console permanently, since QEMU
  raises RX only on the FIFO's 0→1 transition, UART on **SPI 1 = INTID 33**, and 8 of the
  then-25 shell commands available, with the rest `#[cfg]`'d out of dispatcher, impls
  *and* help text. (Now **13 of 28** — `shutdown`, `reboot` and `exit` were portable at
  7.4; `procs` and `caps` joined at 8.5d with the process table.) Three defects the tests
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
