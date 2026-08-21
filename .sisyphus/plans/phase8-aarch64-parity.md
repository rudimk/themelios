# Phase 8 — aarch64 parity, then hyperscaler (plan)

**Deliverable:** bring aarch64 from a *ring-0 kernel core* (where Phase 7 left it) to
**full feature parity with amd64** — EL0 userspace, storage, networking, containers, the
management API — and then onto **real ARM server hardware** (Graviton and peers): device
discovery, GICv3, SMP, secure boot.

The single measurable definition of "parity" is already in the tree and already
checked by CI:

> `kernel/src/test_runner.rs` runs a **54-test** suite. On amd64, `SKIPPED` is empty and
> all 54 run. On aarch64, **16 run and 38 are skipped**, each with a written reason.
> **Parity is `SKIPPED.is_empty()` on both architectures.**

That is the ratchet this whole phase turns. Every sub-phase below states how many
`SKIPPED` entries it retires, and *which ones*. A sub-phase that lands without
shrinking `SKIPPED` (or without a stated reason why it can't yet) has not delivered.

## Scope note — how this relates to the roadmap's "Phase 8"

`CLAUDE.md` currently labels Phase 8 **"Hyperscaler support (AWS, GCP, Azure), secure
boot."** This plan **re-scopes** that entry rather than renumbering the roadmap: 8.0–8.7
are aarch64 parity, 8.8–8.10 are the hyperscaler/real-hardware work the label already
names. The re-scope is not a detour — Graviton is *why* parity matters, and every
hyperscaler item (GICv3, SMP, DT/ACPI, secure boot) is dead weight until an ARM node
can actually run a container. If you'd rather parity be its own phase number and push
hyperscaler out to 9+ (shifting 9–12), say so and this file gets renumbered; nothing
else changes.

## Grounding (verified against the tree at `4dcaa74`)

Every claim below was read out of the source, not recalled.

- **The arch seam is small and already built.** Only **14 files** carry
  `#[cfg(target_arch = "x86_64")]`, and five of them *are* the facade
  (`arch/{irq,time,serial,paging,context}.rs`). The rest is `main.rs`'s module ladder,
  `sched`, `shell/commands.rs`, `test_runner.rs`, `drivers/mod.rs`, `net/mod.rs`.
  **Phase 8 is mostly deleting `cfg` gates and supplying what falls out**, not
  re-architecting.
- **The whole ring-3 surface is one library and one linker script.** `libthemelios`
  holds **25 raw `syscall` asm blocks** in a single file (`servers/libthemelios/src/lib.rs`)
  and it is the *only* file in `servers/` containing inline `syscall`. Every server
  binary links through it. There are **27 kernel syscall numbers** (`SYS_NULL`=0 …
  `SYS_MGMT`=26, `arch/x86_64/syscall.rs:112-238`). The port is one file plus a
  linker script plus `xtask` target plumbing — not 25 scattered ports.
- **`AddressSpace::new_user` is a known, documented, *already-diagnosed* bug on
  aarch64** (`mm/page_table.rs:326`, and the skip reason in `test_runner.rs`): a fresh
  space copies `KERNEL_ROOT_START..512` and clears `0..KERNEL_ROOT_START`, but
  `KERNEL_ROOT_START` is **0** on aarch64 (the kernel lives in a separate TTBR1 tree),
  so the loop is empty and the new root *inherits Limine's low-half entries* — including
  the 1 GiB **block** QEMU `virt` maps at VA `0x4000_0000`. Mapping a 4 KiB page beneath
  a block panics `ensure_table` (`page_table.rs:702`). This is the single prerequisite
  defect carried forward from Phase 7.
- **`USER_ADDR_LIMIT = 0x0000_8000_0000_0000`** (`syscall.rs:723`) is *numerically*
  correct for aarch64 with `T1SZ`/`T0SZ` = 16 — but for the wrong reason (it is written
  as the x86 canonical-hole floor). It must be **derived from `T0SZ`**, not inherited,
  or it silently becomes wrong the moment the granule or address size changes.
- **The `sysret` non-canonical-RCX hazard does not exist on aarch64.** `sysretq` with a
  non-canonical RCX raises `#GP` **in ring 0** on Intel, which is why `syscall.rs:468`
  checks it. `eret` reads `ELR_EL1`; a bad `ELR` faults *in EL0*, attributed to the
  process. One x86-canonical assumption that gets *deleted*, not ported.
- **VirtIO is bound to PCI port-I/O.** `drivers/pci/mod.rs` drives `0xCF8`/`0xCFC` via
  `cpu::outl`/`cpu::inl` (lines 144-166). There is no transport abstraction — `virtio/`
  reaches into `pci` directly. QEMU `virt` has no port I/O at all.
- **The Linux personality is the x86_64 Linux ABI, hard-coded.** `linux/syscall.rs:32-61`
  pins `SYS_EXIT`=60, `SYS_CLONE`=56, `SYS_FUTEX`=202, `SYS_ARCH_PRCTL`=158 — the
  x86_64 table. aarch64 Linux uses `asm-generic/unistd.h`: `exit`=93, `clone`=220,
  `futex`=98, no `arch_prctl` at all (TLS is `TPIDR_EL0`, set directly), no `open`
  (only `openat`), no `fork`. `linux/elf.rs:172` rejects anything but `EM_X86_64`
  (0x3e); aarch64 is `EM_AARCH64` (0xB7). This is a **second syscall table**, not a
  tweak.
- **The `0x400` vector group is already populated and already fatal.**
  `arch/aarch64/exceptions.rs` installs all 16 slots including "lower EL, AArch64";
  today they report and halt. 8.1 turns slot `0x400` (sync, lower EL) into the SVC
  dispatcher. The scaffolding exists — the geometry constraint (128 bytes/slot, CPU
  branches *into* the slot) is documented in that file and must be respected.
- **`TPIDR_EL1` per-CPU is already live** (`arch/aarch64/percpu.rs`), rewritten on every
  context switch, carrying `kernel_stack_top`. That is exactly what SVC entry needs to
  find the kernel stack — the x86 `swapgs`+`gs:0x8` analog is **already paid for**.

## Cross-cutting invariants (non-negotiable, carried from Phase 7)

1. **amd64 stays fully green, every sub-phase.** The amd64 QEMU suite is the regression
   gate. Nothing in this phase is allowed to be "aarch64 progress, amd64 regression" —
   that happened once in 7.4 (the `usable_count` snapshot broke x86's `mem` command) and
   was caught only because the suite ran.
2. **aarch64 suite is now a gate too.** `cargo xtask test --arch aarch64` runs in CI as
   of `fd65f0a`. Both jobs must be green before merge, and **`SUITE_SIZE = 54` is
   asserted at the top of `run_tests()`** so a gated/un-gated test can't drift the
   totals silently.
3. **`SKIPPED` shrinks monotonically.** Each sub-phase names the entries it retires. An
   entry may only be *added* with a written reason and an explicit note here.
4. **Fresh branch + PR per sub-phase, off latest `main`. Adversarial Momus review to
   APPROVE. CI green. Never auto-merge.**
5. **Every new test must be demonstrated falsifiable.** Phase 7's recurring failure was
   *CI green on every broken version* — vacuous tests bound to `Ok(())` stubs, a
   console-wedging RX race, a 20%-flaky IPC race, an audit log with no timestamps, all
   passed. Each sub-phase PR must show a fault injection that makes its new assertions
   fail. "It passes" is not evidence; "it fails when I break it" is.
6. **Claims are checked before they are written.** The other Phase 7 failure was
   *branches making false claims about themselves* — in code comments, `CLAUDE.md`,
   docs, and PR bodies, three separate times. Before a PR body or doc change says a
   thing works, the thing gets run.
7. **Atomic commits.** One idea per commit.

## Pinned decisions

1. **Syscall ABI on aarch64 = the kernel's own ABI, transliterated — not the Linux
   aarch64 ABI.** Number in **`x8`**, arguments in **`x0`-`x5`**, return in **`x0`**.
   `x8` because it is the one register the AAPCS64 procedure-call standard leaves free
   at a call boundary (unlike x86 where RAX doubles as the return register), and
   because it matches what every aarch64 toolchain already expects to see around an
   `svc`. The *Linux personality* (8.3) is a separate table layered on top, exactly as
   on x86.
2. **`ESR_EL1.ISS` carries the `svc` immediate; we use `svc #0` and ignore it.** The
   syscall number lives in `x8`, uniformly with the rest of the ABI. Encoding it in the
   immediate would work but splits the ABI across two mechanisms for no gain.
3. **VirtIO transport = virtio-mmio, not PCIe ECAM.** QEMU `virt` exposes 32
   virtio-mmio slots at `0x0a00_0000` (0x200 stride, SPI 16-47 → INTID 48-79) *and* a
   PCIe ECAM window. mmio is dramatically simpler (no config-space enumeration, no BAR
   programming, no MSI-X) and is what the transport refactor should be validated
   against first. **ECAM is deferred to 8.8**, where real hardware makes it necessary —
   and by then the transport abstraction exists to receive it.
4. **`copy_from_user`/`copy_to_user` bound comes from `T0SZ`, computed once at paging
   init, not from a hard-coded constant.** See the `USER_ADDR_LIMIT` note above.
5. **EL0 gets its own `TTBR0_EL1` tree with a nonzero ASID; the kernel keeps `TTBR1_EL1`
   untouched.** This is the structural advantage aarch64 has over x86's single-CR3
   design: no kernel-half copying, so `new_user` produces a genuinely empty user space
   and the 8.0 bug cannot recur by construction. `TTBR0_EL1` is parked at 0 today
   (Phase 7.1) — 8.0 gives it a real tree.
6. **TLS = `TPIDR_EL0`, set directly on context switch.** No `arch_prctl` analog, and
   none is invented. `sched`'s `fs_base` field becomes `tls_base` behind the
   `arch::context` facade.
7. **Device discovery stays hard-coded until 8.8.** QEMU `virt`'s MMIO map is stable and
   documented; parsing a device tree to learn constants we already know is work that
   buys nothing until real firmware hands us a *different* map. 8.8 is where that
   becomes load-bearing, and it is scoped as its own sub-phase because it is.

## Sub-phases

Eleven sub-phases, each comparable in size to 7.1–7.4. Tiers 1–3 (8.0–8.7) deliver
parity; tier 4 (8.8–8.10) delivers real hardware.

---

### Tier 1 — EL0 / ring-3 (the keystone, and the long pole)

Everything else in the phase is gated on this. It is scheduled first *despite* being the
largest tier, because it holds the biggest unknown and a dead end here reshapes the rest.

#### 8.spike — throwaway EL0 round-trip spike

Mirrors `7.spike`: retire the highest-uncertainty item on a **throwaway branch** before
any merged work depends on it. Goals:

- (a) Build a `TTBR0_EL1` tree, map one page of hand-written EL0 code + one stack page,
  `eret` into it with `SPSR_EL1.M = 0b0000` (EL0t), and confirm it *executes*.
- (b) From EL0, `svc #0` → confirm the `0x400` sync slot fires with `ESR_EL1.EC = 0x15`,
  and that `SP_EL1` is the kernel stack (not EL0's).
- (c) `eret` back to EL0 and confirm the process continues — the full round trip.
- (d) Confirm `TPIDR_EL0` is readable/writable from EL0 and survives a round trip.
- (e) Measure what the vector-slot geometry costs: the SVC path needs a *fuller*
  register save than the fatal reporter's, in **128 bytes of slot**.

**Acceptance:** all five answered, findings written back into this file. Code is
throwaway and not committed.
**Riskiest unknown:** (e) — whether the SVC entry fits the slot budget, or needs the
common-body trampoline restructured.

#### 8.0 — User address spaces on aarch64 (`TTBR0_EL1`)

The prerequisite defect, fixed *before* anything depends on it. Deliver: a real
`AddressSpace::new_user` for aarch64 producing a genuinely empty low half (per pinned
decision 5, this is structural — nothing is copied); user leaf-descriptor encoding in
`arch::aarch64::paging` (`AP[1]` for EL0 access, `AP[2]` for read-only, `UXN`/`PXN`
set correctly — note aarch64 **inverts** x86's sense on both AP[2] and the XN bits);
ASID allocation and `TTBR0_EL1` activation behind the `arch::paging` facade; `TLBI
ASIDE1IS` for per-space invalidation with the 7.1 barrier discipline.

**Retires from `SKIPPED` (1):** `test_shared_memory`.
**Acceptance:** `test_shared_memory` runs and passes on aarch64; a new self-test maps
the same frame into two user spaces at different VAs and proves writes alias; amd64
unchanged. Falsifiability: corrupt the ASID and show the test fails.
**Riskiest unknown:** ASID rollover and the `TLBI` scope — an over-broad invalidate
hides a stale-TLB bug until SMP (8.9) exposes it, and an under-broad one corrupts
silently. Prefer over-broad *with a comment saying so*.

#### 8.1 — SVC entry, syscall dispatch, and the drop to EL0

The keystone. Deliver: the `0x400` sync slot decoding `EC = 0x15` into a syscall
dispatch (all other lower-EL syncs stay fatal); an aarch64 `SyscallFrame`; the kernel
stack found via `TPIDR_EL1.kernel_stack_top` (already live — 7.3); `copy_from_user`/
`copy_to_user` with the `T0SZ`-derived bound (decision 4); the EL0 drop via
`SPSR_EL1` + `ELR_EL1` + `SP_EL0`; `TPIDR_EL0` plumbed as the TLS base and rewritten on
every context switch; `sched`'s ring-3 fields (`kernel_stack_top`, `fs_base`→`tls_base`,
`clone_entry`, address-space swap) un-`cfg`'d and given aarch64 meanings.

**Explicitly audit the three Phase-4.5 race classes in `eret` form** — they are the
reason this sub-phase is not "just write the stub":
- *Syscall-exit double-fault (shared scratch slot).* x86 stashed user RSP in a single
  `gs:0x8` slot and read it back with interrupts **enabled**. aarch64's `SP_EL0` is a
  **banked register**, not a memory slot — the bug class is structurally absent. Confirm
  that by inspection and **write it down**, don't assume it.
- *Stale GS base.* Already fixed structurally in 7.3 (`TPIDR_EL1` rewritten on every
  switch). `TPIDR_EL0` must get the same treatment in this sub-phase — it is the same
  bug wearing a different register.
- *Exception-return atomicity.* The tail from "restore user state" to `eret` must run
  with interrupts masked, same as the x86 `cli; sysretq` fix.

**Retires from `SKIPPED` (1):** `test_syscall`.
**Acceptance:** a hand-written EL0 blob performs `SYS_DEBUG_PRINT` and `SYS_EXIT`;
`test_syscall` runs and passes on aarch64; a soak run of ≥1000 syscalls under
preemption is clean (the 4.5 races only appeared under load); amd64 unchanged.
Falsifiability: mask interrupts wrongly in the exit tail and show the soak breaks.
**Riskiest unknown:** the same per-CPU/exception-return race class that bit x86 in 4.5,
now with `eret` and banked registers instead of `sysretq` and `swapgs`. A soak run is
mandatory, not optional — the x86 versions of these bugs were 2-in-10 flakes.

#### 8.2 — `libthemelios` and the server toolchain on aarch64

Deliver: the 25 `syscall` asm blocks in `servers/libthemelios/src/lib.rs` given aarch64
counterparts (`svc #0`, `x8`/`x0`-`x5` per decision 1) behind `#[cfg(target_arch)]`;
`servers/linker-aarch64.ld`; `xtask::build_servers` parameterized by target (it hard-codes
`x86_64-unknown-none` in five places: lines 235, 247, 288, 298, 1759); the detached
smoke-test workspaces (`elf-smoke`, `linux-smoke`, `fs-smoke`, `threads-smoke`,
`isolation-smoke`, `confine-smoke`) built for both targets; the kernel's embedded-server
blobs selected per architecture.

**Retires from `SKIPPED` (3):** `test_process`, `test_userspace_init`,
`test_server_spawn`.
**Acceptance:** `echo-server` runs at EL0 on aarch64 and completes an IPC round trip;
the three tests run and pass; amd64 server binaries byte-identical to before (the
`#[cfg]` must not perturb the x86 codegen — check the built `.bin` hashes).
**Riskiest unknown:** the flat-binary link. The x86 servers are linked as raw flat
binaries with a custom script; aarch64 relocation types and the `.rodata`/GOT layout
differ enough that "the same script with `-melf64littleaarch64`" may produce something
that loads but jumps wrong. Budget for reading the link map.

#### 8.3 — The Linux personality on aarch64

A **second syscall table**, not a port. Deliver: `linux/syscall.rs` split into an
arch-neutral dispatcher plus per-arch number tables (aarch64 = `asm-generic/unistd.h`:
`exit`=93, `exit_group`=94, `clone`=220, `futex`=98, `openat`=56, `write`=64,
`writev`=66, `mmap`=222, `brk`=214, `ioctl`=29, `clock_gettime`=113, `getrandom`=278,
`set_tid_address`=96, `gettid`=178 …); **`arch_prctl` has no aarch64 analog** — TLS is
`TPIDR_EL0`, which 8.1 already plumbed, so the personality sets it directly; `openat`
is the only open (no `open`, no `stat`, no `fork`), so the `linux/fs.rs` path-clamping
entry points shift accordingly; `linux/elf.rs:172` accepting `EM_AARCH64` (0xB7) with
the machine type checked against the *build* target; `linux/thread.rs`'s clone/futex
paths un-`cfg`'d.

**Retires from `SKIPPED` (5):** `test_elf_exec`, `test_linux_exec`, `test_path_resolve`,
`test_linux_fs`, `test_linux_threads`. (`test_elf_exec` lands here rather than 8.2
because it needs the aarch64 ELF acceptance.)
**Acceptance:** the `linux-smoke`, `fs-smoke` and `threads-smoke` binaries, rebuilt for
aarch64, run to completion under the personality; the five tests run and pass; amd64's
table is provably unchanged (diff the constant list).
**Riskiest unknown:** `clone`/`futex`. The aarch64 `clone` argument *order* differs from
x86_64 (`clone(flags, stack, parent_tid, tls, child_tid)` vs x86's
`clone(flags, stack, parent_tid, child_tid, tls)` — **`tls` and `child_tid` are swapped**).
Getting this wrong produces a thread that runs with a garbage TLS pointer and fails far
from the cause.

---

### Tier 2 — VirtIO transport

#### 8.4 — virtio-mmio transport and a transport abstraction

Deliver: a `VirtioTransport` abstraction over the operations `virtio/` currently reaches
into `pci` for (feature negotiation, queue config, notify, ISR/interrupt ack, device
status, config space); the existing PCI path re-expressed as an implementation of it,
**with no behavior change on amd64**; a virtio-mmio implementation for the QEMU `virt`
map (32 slots at `0x0a00_0000`, 0x200 stride, INTID 48-79); MMIO region mapping through
`mm::mmio` (Device-`nGnRnE`, the path 7.2 established); SPI routing and enable in the
GICv2 distributor.

Note the mmio **version** split: legacy (v1) and modern (v2) differ in queue-address
programming — QEMU `virt` defaults to v2 (`QueueDescLow/High` triples), which is also
what real hardware does. Implement v2; reject v1 loudly rather than half-supporting it.

**Retires from `SKIPPED` (0 directly)** — but unblocks 11 entries in 8.5 and 10 in 8.6.
`test_pci_scan` stays skipped **permanently on aarch64** and its skip reason should be
rewritten from "aarch64 uses MMIO ECAM" (which this sub-phase makes false) to "PCI
config space is x86 port-I/O; aarch64 uses virtio-mmio."
**Acceptance:** the transport refactor lands amd64-green with byte-identical behavior
(the amd64 storage/net tests are the proof); on aarch64 a virtio-blk device is
discovered, negotiated, and reports its capacity; a new `test_virtio_mmio_transport`
runs on aarch64. Falsifiability: point the base address one slot off and show discovery
fails rather than silently finding nothing.
**Riskiest unknown:** the transport seam itself. If `virtio/` is more entangled with
`pci` than the file layout suggests, this becomes a larger refactor than a sub-phase —
and it is a refactor of *working amd64 storage and networking*, which is the highest
blast-radius change in the whole phase. **Scout the entanglement before committing to
the sub-phase boundary**; if it is deep, split into 8.4a (abstraction, amd64-only,
pure refactor, zero behavior change) and 8.4b (mmio implementation).

---

### Tier 3 — un-gate the arch-neutral stack

These three are the payoff sub-phases: the code is already written and already portable,
and each is mostly deleting `#[cfg]` gates and fixing what falls out.

#### 8.5 — Storage on aarch64

Un-gate `mod fs` and `mod drivers` for aarch64. Deliver: virtio-blk over the 8.4
transport; `block_server`, `squashfs-server`, `overlay-server`, `ext2-server` built and
spawned at EL0; the VFS capability path; `xtask` producing the SquashFS + ext2 images
for the aarch64 boot (it already does — the images are arch-neutral data).

**Retires from `SKIPPED` (10):** `test_virtio_transport`, `test_virtio_queue_failure`,
`test_virtio_blk`, `test_block_server_ipc`, `test_squashfs_server`, `test_overlay_server`,
`test_ext2_read`, `test_ext2_write`, `test_vfs_capability`, `test_fs_syscalls`.
**Acceptance:** all ten run and pass on aarch64; the aarch64 shell's `ls`/`cat` commands
are un-`cfg`'d and work interactively; amd64 unchanged.
**Riskiest unknown:** DMA coherence. x86 is cache-coherent for DMA by architecture;
aarch64 is **not guaranteed to be** — descriptor rings and buffers may need explicit
cache maintenance (`DC CVAC`/`DC IVAC`) unless the device is marked coherent. QEMU under
TCG will *not* reproduce a coherence bug, so this can pass in CI and fail on Graviton.
Get the memory attributes right (Normal Non-cacheable or Device for the rings) and
**write down which choice was made and why**, because 8.10 is where being wrong shows up.

#### 8.6 — Networking on aarch64

Un-gate `mod net`. Deliver: virtio-net over the 8.4 transport; smoltcp (already
portable — `servers/smoltcp-gate` has been compile-gating it for aarch64 since 7.0c);
`net-server` at EL0; the socket syscalls; DHCP.

**Retires from `SKIPPED` (9):** `test_virtio_net`, `test_net_service`,
`test_net_server_stack`, `test_net_icmp_echo`, `test_dhcp`, `test_socket_capability`,
`test_socket_list`, `test_udp_echo`, `test_tcp_client`.
**Acceptance:** all nine run and pass on aarch64; an aarch64 guest answers a host `ping`
and completes a TCP round trip; the shell's `ping`/`ifconfig`/`sockets` commands
un-`cfg`'d; amd64 unchanged.
**Riskiest unknown:** the RX recycling issue already documented as a Phase 6 deferral
("net-server RX recycling") — it is a latent amd64 defect, and a second architecture
exercising the same path is a plausible way to finally surface it. That would be a
*good* outcome, but budget for it landing in this sub-phase's lap.

#### 8.7 — Containers and the management API on aarch64 — **parity**

Un-gate `mod container`, `mod mgmt`, and the `api-server` spawn. Deliver: the container
runtime on aarch64; the registry client; the management ABI and its sentinel capability;
`api-server` at EL0 serving the Docker Engine API subset over the 8.6 network stack.
Note the container *images* must be aarch64 images — the embedded test payload needs an
ARM build.

**Retires from `SKIPPED` (8 — the last ones):** `test_container_run`,
`test_container_isolation`, `test_container_confinement`, `test_registry_pull`,
`test_container_registry`, `test_container_logs`, `test_management_capability`,
`test_api_server`. (`test_api_server` is listed under "the network stack rides on
VirtIO-PCI" but really needs the management ABI, so it lands here, not in 8.6.)
**Acceptance — this is the parity gate:** `SKIPPED` is **empty** on aarch64;
`test_runner` reports **54 running, 0 skipped** on *both* architectures; a container
runs on an ARM node and `GET /containers/json` returns it. Update `CLAUDE.md`,
`docs/src/milestones.md`, and `docs/src/aarch64.md` to say aarch64 is a first-class
target — **after** running it, per invariant 6.
**Riskiest unknown:** `test_pci_scan` is the one entry that can never be retired on
aarch64 (there is no port I/O), so "SKIPPED is empty" is achievable only if 8.4 also
converts it into an arch-neutral transport test or splits it per-arch. **Decide that in
8.4, not here** — discovering it at the parity gate would be discovering it too late.

---

### Tier 4 — real ARM hardware

Parity under QEMU is not parity on Graviton. These three make the difference.

#### 8.8 — Device discovery: device tree and ACPI

Every MMIO constant in the aarch64 port is hard-coded to QEMU `virt`: PL011 at
`0x0900_0000`, GICD at `0x0800_0000`, GICC at `0x0801_0000`, virtio-mmio at
`0x0a00_0000`. Real firmware hands over a different map. Deliver: a flattened-device-tree
parser (Limine passes the DTB pointer) *and* the ACPI tables path (Graviton/EC2 boots
ACPI, not DT — both are required, and which one is present is a runtime question);
discovery of UART, interrupt controller (including **which GIC version**), timer
frequency, memory map, and virtio/PCIe transports; PCIe **ECAM** as a second transport
implementation behind the 8.4 abstraction, since real ARM servers put NVMe and ENA on
PCIe, not virtio-mmio.

**Acceptance:** the aarch64 kernel boots QEMU `virt` with **zero hard-coded MMIO
addresses**, discovering everything; a deliberately relocated QEMU machine
(`-machine virt,highmem=on` or a shifted map) still boots. Falsifiability: that second
boot *is* the falsifiability test — a kernel still secretly using constants fails it.
**Riskiest unknown:** ACPI. A DT parser is a few hundred lines of well-specified
big-endian walking; ACPI is AML, and while the tables we need (MADT/GTDT/MCFG/SPCR) are
static and parseable without an interpreter, establishing *that* boundary confidently is
the work. Scope it to "static tables only, no AML" and hold that line.

#### 8.9 — GICv3 and SMP

Deliver: GICv3 — the system-register CPU interface (`ICC_SRE_EL1` to enable it,
`ICC_IAR1_EL1`/`ICC_EOIR1_EL1`/`ICC_PMR_EL1`/`ICC_IGRPEN1_EL1`), per-CPU
redistributors, `GICR_WAKER` wake protocol — behind the existing `arch::irq` facade
with runtime v2/v3 selection from 8.8's discovery; SMP bring-up via PSCI `CPU_ON`
(the PSCI conduit is already established in `arch/aarch64/psci.rs`); per-CPU
`TPIDR_EL1` blocks for secondaries (the structure is already there — 7.3); SGI-based
IPIs; a full audit of every `spin::Mutex` in the kernel for actual multi-core soundness.

**Acceptance:** GICv3 boots under `-machine virt,gic-version=3`; 4 CPUs come online and
the scheduler distributes work across them; a multi-core soak run is clean; amd64
unchanged (it stays UP — SMP on x86 is not in this phase's scope).
**Riskiest unknown:** the lock audit. The kernel was written UP and its `spin::Mutex`
uses have only ever been contended by interrupts, not by other cores. Every
`InterruptMutex` critical section and every `sched` lock ordering becomes a real
concurrency question at once. This is plausibly the second-largest item in the phase
after 8.1 and should be **split into 8.9a (GICv3, still UP) and 8.9b (SMP)** if the
audit turns out to be as broad as it looks.

#### 8.10 — Hyperscaler boot and secure boot

Deliver: boot on a real ARM instance (Graviton first — it is the most constrained and
therefore the most informative); ENA or virtio-net depending on instance type; NVMe or
EBS-over-virtio for storage; UEFI Secure Boot chain (signed `BOOTAA64.EFI` → signed
kernel), Measured Boot into a vTPM/NitroTPM where available; the same story on GCP Tau
T2A and Azure Cobalt.

**Acceptance:** a ThemeliOS ARM image boots on a real cloud instance, gets an address,
and runs a container reachable from outside. That is the phase's real deliverable and
everything above is scaffolding for it.
**Riskiest unknown:** the DMA-coherence decision made back in 8.5. TCG never reproduces
a coherence bug, so 8.5 through 8.9 can all be green while the attribute choice is
wrong, and it surfaces here as data corruption under load on real silicon — as far from
its cause as a bug can get. Mitigation: write the choice down in 8.5 *with its
reasoning*, so 8.10 has something to re-examine instead of something to rediscover.

---

## The ratchet — `SKIPPED` accounting

The 38 entries, assigned. This table is the phase's progress bar, and it must sum to 38
or something has been double-counted or forgotten.

| Sub-phase | Retires | Running total | Remaining |
|-----------|--------:|--------------:|----------:|
| (today)   |       — |            16 |        38 |
| 8.0       |       1 |            17 |        37 |
| 8.1       |       1 |            18 |        36 |
| 8.2       |       3 |            21 |        33 |
| 8.3       |       5 |            26 |        28 |
| 8.4       |       0 |            26 |        28 |
| 8.5       |      10 |            36 |        18 |
| 8.6       |       9 |            45 |         9 |
| 8.7       |       8 |            53 |         1 |
| **8.4 (revisited)** | 1 | **54** |     **0** |

`1 + 1 + 3 + 5 + 0 + 10 + 9 + 8 = 37`. The 38th is **`test_pci_scan`**, which no
sub-phase can retire by porting, because aarch64 has no port I/O and never will. It is
retired by 8.4 *reframing* it — either as an arch-neutral transport-discovery test with
a per-arch body, or as two tests. Until that decision is made, "SKIPPED is empty" is
unreachable, which is why it appears in this table as an explicit line item rather than
as a surprise at the parity gate.

## Sequencing and estimate

```
8.spike ─→ 8.0 ─→ 8.1 ─→ 8.2 ─→ 8.3 ──┐
                            │          ├─→ 8.5 ─→ 8.6 ─→ 8.7  ← PARITY
                            └─→ 8.4 ───┘
                                              8.8 ─→ 8.9 ─→ 8.10 ← HYPERSCALER
```

**8.4 can run in parallel with 8.3** — the transport refactor depends on 8.2's server
toolchain (the storage servers must build for aarch64) but not on the Linux personality.
Everything else is a chain.

**Eleven sub-phases**, each comparable to 7.1–7.4 in size, with three flagged as
possible splits (8.4 → a/b, 8.9 → a/b, and 8.1 is large enough that it may want its own
split between "SVC dispatch" and "EL0 drop"). Call it **11–14 sub-phases** delivered.
Parity (through 8.7) is **8–10 of them**; hyperscaler is the remaining 3–4.

## Deferred (documented — out of Phase 8 scope)

- **32-bit EL0 (AArch32).** The `0x600` vector group stays fatal. Nothing needs it.
- **SMP on amd64.** The x86 side stays UP throughout; 8.9's lock audit benefits it, but
  bringing up x86 APs is separate work.
- **Big-endian, 16 KiB/64 KiB granules, 52-bit VA (LPA2).** The port pins 4 KiB/48-bit
  and says so.
- **GICv4 / direct-injected virtual interrupts.** Only matters if ThemeliOS becomes a
  hypervisor.
- **Pointer authentication (PAC) and BTI.** Real ARM server security features, and a
  genuinely good fit for a capability kernel — but a hardening phase, not a parity one.
- **The Phase 6 deferrals** (TLS/mTLS on the management API, interactive `exec`
  streaming, Engine API breadth) remain deferred and are arch-neutral when they land.

## Notes carried forward

- **No IST/TSS analog on aarch64** (from Phase 7). A kernel-stack overflow re-faults on
  the same stack. This becomes materially worse at EL0 (8.1), because a user process can
  now *provoke* kernel stack depth. Consider a guard page below each kernel stack in
  8.1 — cheap, and it converts a silent re-fault into a reported one.
- **The kernel is softfloat** (`aarch64-unknown-none-softfloat`), and `CPACR_EL1.FPEN`
  is cleared *and verified* at boot (7.3). EL0 processes will want FP. 8.1 must decide:
  either enable FP for EL0 only and add a `v0`-`v31` save area to the context switch, or
  keep FP trapped and have userspace be softfloat too. **The second is much cheaper and
  is the recommendation** — but it is a real constraint on what container images can
  run, and 8.7 is where that bill comes due. Decide it in 8.1, out loud.
- **`test_pci_scan` cannot be retired on aarch64.** See 8.7's riskiest-unknown; the fix
  belongs in 8.4.
