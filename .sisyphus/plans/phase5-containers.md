# Phase 5 — OCI Containers & Linux Syscall Compatibility

**Status**: 5.0 IN PROGRESS (plan Momus-reviewed 2026-07-25 → REVISE fixes applied)
**Created**: 2026-07-25
**Reviewed**: 2026-07-25 (Momus: verified kernel gaps — per-task FS base, process
exit-status/wait, and an address-keyed futex queue are all net-new; 5.0 uses the
native ABI to isolate loader bugs; ET_EXEC-only, byte-source loader; image source
locked to a local `docker save` bundle)
**Phase**: 5
**Depends on**: Phase 2 (capabilities, processes, IPC), Phase 3 (VFS + overlay +
ext2), Phase 4 (TCP/IP + sockets) — all complete.

## Goal

Run **unmodified OCI/Docker container images** as capability-isolated ring-3
processes on ThemeliOS. Concretely, the off-ramp deliverable is:

```
> run busybox echo hello from a container      # busybox from a local docker-save bundle
hello from a container
```

(The off-ramp uses a **local image** — a `docker save` bundle staged on the data
disk — not `docker.io`, which needs the TLS registry path deferred past this
phase. See sub-phase 5.6.)

Getting there means four new capabilities layered on the existing kernel:

1. A **Linux syscall-compatibility personality** — ThemeliOS is not Linux, so a
   container's Linux syscalls (`write`, `openat`, `mmap`, `clone`, `futex`, …)
   must be recognised and serviced, either natively in the kernel or by
   delegating to the existing ring-3 VFS/net servers.
2. An **ELF64 loader + `exec`** — real Linux binaries are ELF, not the flat
   binaries the Phase 3/4 servers use. The kernel needs to load an ELF image,
   set up the Linux initial process stack (argv/envp/auxv), and enter it.
3. **OCI image handling** — pull an image, parse its manifest/config JSON, and
   unpack its layer tarballs into a root filesystem.
4. A **registry client** — fetch images over the Docker Registry HTTP API v2.

## The core thesis for this phase

**Capabilities are the container boundary. There are no Linux namespaces.**

A "container" on ThemeliOS is not a bundle of five namespaces plus cgroups. It is
a **process (tree) whose CSpace has been restricted to exactly**: its rootfs mount
capability, any explicitly granted socket/network capabilities, and its own
memory. That is *stronger* isolation than Linux namespaces (which share a kernel
and leak through dozens of `/proc`, `/sys`, and unnamespaced syscalls), and it
falls straight out of the existing capability system — no new isolation machinery
is required. The Linux syscall layer is a **translator that runs under the
caller's capabilities**; it can never grant a container access it was not given.

This is the payoff of Phases 2–4: the FS is already a capability-checked ring-3
service, the network is already a capability-checked ring-3 service, and processes
already have no ambient authority. Phase 5 mostly *reuses* those — it teaches the
kernel to speak Linux at the syscall boundary and to load ELF, then wires
container lifecycle on top.

## Key Architectural Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Linux ABI mechanism | **Per-process "Linux personality" flag; one `syscall` entry, two dispatch tables** | The `syscall` instruction and rax-numbered ABI are shared, but Linux numbers collide with native ones (Linux `write`=1 vs native `SYS_SEND`=1). A personality flag on the process selects the Linux dispatch table vs the native one — no second entry point, no renumbering. Native ThemeliOS servers keep their ABI; containers get Linux's. |
| Who services Linux syscalls | **Kernel fast-path for CPU/memory/process/signal; delegate FS→Phase 3 VFS, net→Phase 4 socket router** | Don't reimplement filesystems or TCP. A container's `openat`/`read` become VFS calls against its rootfs mount cap; `socket`/`connect` become Phase 4 socket-router calls. The Linux layer is glue, and the existing servers remain the isolation boundary and the untrusted-parser containment. |
| Binary format | **Statically-linked ELF (musl) first; dynamic linking (`ld.so`/glibc) deferred** | Static musl binaries need no dynamic loader, no shared objects, no `ld.so`, no `DT_NEEDED` resolution — an order of magnitude less loader surface, and they cover busybox and Alpine-static images (the canonical minimal containers). Dynamic linking is a later phase. |
| Image parsing location | **Ring-3 `oci-server` parses JSON + tar + gzip + HTTP; the kernel only loads the assembled rootfs and execs** | Image manifests, layer tarballs, and registry responses are **untrusted bytes off the network** — exactly the class Phase 3/4 keep out of the kernel. The oci-server unpacks into a rootfs via the VFS, then asks the kernel to `exec` the entrypoint with a restricted CSpace. The kernel never parses an image. |
| Isolation | **Capabilities, not namespaces** (see thesis above) | Core ThemeliOS design; no PID/mount/net/user/UTS/IPC namespace code, no ambient authority to strip. |
| Rootfs storage | **Layers unpacked onto the Phase 3 overlay, upper on the ext2 data volume** | Reuse the overlay + ext2 stack. OCI whiteouts (`.wh.*`) map to overlay deletions. Rootfs lives on disk, not RAM — image sizes exceed the 256 MiB budget. |
| Image transport | **OCI image spec (manifest + config + layer blobs); Registry HTTP API v2 over plain HTTP first, TLS deferred** | Decouple "run a container from a local image" from "pull from Docker Hub over HTTPS." TLS (rustls in ring 3) is a large dependency; a local `registry:2` over HTTP (or a `docker save` tarball staged on the data disk) proves the pipeline. TLS is a scoped follow-up. |
| Resource limits | **Memory bounded by the existing per-process address-space budget; CPU/pids cgroups deferred** | The address-space + heap-window mechanism already caps a process's memory. Fair-share CPU and pids limits are a later concern. |
| gzip/tar deps | **Reuse `miniz_oxide` (already vendored for SquashFS) for gzip; a small in-tree tar reader** | miniz_oxide is already accepted, ring-3-contained, and handles DEFLATE (gzip layers). Tar is a trivial 512-byte-header format — a purpose-built reader avoids a new dependency. |
| Target architecture | **amd64 only; arm64 in Phase 7** | Consistent with Phases 1–5; the ELF loader and syscall table are arch-specific by nature. |

## Net-new kernel primitives (verified gaps)

A code audit for this plan confirmed the following do **not** exist today and are
net-new work, not "translations" of existing primitives. They are called out here
so no sub-phase treats them as free:

- **Per-task FS-base save/restore.** The context switch handles the GS base (the
  Phase 4 fix in `kernel/src/arch/x86_64/syscall.rs`) but never sets
  `IA32_FS_BASE` (`0xC000_0100`). Linux TLS via `arch_prctl(SET_FS)` requires
  adding a per-`Task` FS base that the context switch loads on every switch —
  modelled on the Phase 4 GS-base handling. (5.1)
- **Process exit-status + `wait`/reaping.** `ProcessState` is only
  `Running`/`Exited` (`kernel/src/process/mod.rs`); there is no exit code and no
  wait/reap. `wait4` and `run`'s exit-status propagation need a new exit-status
  field and a parent-wait/reap primitive. (5.5)
- **Address-keyed wait queue for `futex`.** Kernel blocking today is
  endpoint-based (`kernel/src/ipc/`); there is no wait queue keyed by a memory
  address. `futex` needs one, scoped to **private WAIT/WAKE** (no PI, no requeue,
  no cross-process shared futex initially). (5.3)

*Good news from the same audit*: multiple tasks per process/address space is
**already** supported (`Process.tasks: Vec<TaskId>`, `add_task_to_process`), so
`clone(CLONE_THREAD)` has structural support — the threading half of 5.3 is a fit,
and the risk concentrates in TLS + futex.

## Security Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                          Ring 0 (Kernel)                          │
│                                                                   │
│   syscall entry ─▶ personality?                                   │
│       ├── native  ─▶ native dispatch (SYS_SEND, SYS_OPEN, …)       │
│       └── linux   ─▶ Linux dispatch ─┬─ CPU/mem/proc: native prims │
│                                       ├─ FS: Phase 3 VFS router    │
│                                       └─ net: Phase 4 socket router│
│   ELF loader + exec (maps segments W^X, builds argv/envp/auxv)    │
│                                       ▲                            │
├───────────────────────────────────────┼────────────────────────────┤
│                    Ring 3 (Userspace) │  (all capability-checked)   │
│   ┌─────────────────┐   ┌─────────────┴─────┐   ┌───────────────┐  │
│   │  oci-server     │   │  container:       │   │ VFS + net     │  │
│   │  pull/unpack:   │──▶│  a Linux-ABI      │──▶│ servers       │  │
│   │  JSON+tar+gzip  │   │  process, CSpace  │   │ (Phase 3/4)   │  │
│   │  +registry HTTP │   │  = rootfs cap +   │   │               │  │
│   │  → rootfs (VFS) │   │  granted sockets  │   │               │  │
│   └─────────────────┘   └───────────────────┘   └───────────────┘  │
└──────────────────────────────────────────────────────────────────┘
```

**What a container can reach**: only its rootfs mount capability, the socket/net
capabilities explicitly granted at launch, and its own address space. It cannot
enumerate other processes, touch the host root, or open ungranted network
sockets — because it never holds those capabilities. A malicious image binary is
contained by the ring-3 boundary (like any process) *and* the capability boundary
(unlike a Linux container, which shares the host kernel's full syscall surface).

**What parses untrusted bytes**: the ring-3 `oci-server` (JSON, tar, gzip, HTTP)
and the existing ring-3 VFS/net servers. The kernel's new surface is the ELF
loader and the Linux syscall translators — which operate on already-mapped,
capability-gated resources, never on raw image/network bytes.

## Deliverables

- **ELF64 loader**: parse `ET_EXEC` headers (static-PIE `ET_DYN` deferred — needs
  `R_X86_64_RELATIVE` relocation), map `PT_LOAD` segments with per-segment W^X,
  honour `PT_GNU_STACK`, set up TLS from `PT_TLS`, build the Linux initial stack
  (`argc`, `argv[]`, `envp[]`, `auxv[]` incl. `AT_PHDR/AT_PHENT/AT_PHNUM/AT_ENTRY/
  AT_PAGESZ/AT_RANDOM/AT_HWCAP`), and enter at `e_entry`. Reads from a
  **byte-source** (embedded or VFS file), not a fixed `&[u8]`.
- **Linux personality** flag on `Process`; the `syscall` entry routes by it.
- **Per-task FS base** (`Task.fs_base` + context-switch load) for `arch_prctl`
  TLS, and **process exit-status + wait/reap** — both confirmed net-new.
- **Linux syscall table** (x86-64 numbers), with a growing set of translators:
  - *Process/thread*: `exit`, `exit_group`, `clone`(thread), `set_tid_address`,
    `gettid`, `getpid`, `getppid`, `wait4`, `futex`(WAIT/WAKE), `sched_yield`,
    `set_robust_list`(stub), `prctl`(min), `arch_prctl`(SET_FS/SET_GS).
  - *Memory*: `brk`, `mmap`/`munmap`/`mprotect`/`mremap`(min), anonymous +
    file-backed (via VFS), `madvise`(stub).
  - *I/O*: `read`, `write`, `readv`, `writev`, `openat`, `close`, `lseek`,
    `fstat`, `newfstatat`, `statx`, `getdents64`, `readlinkat`, `ioctl`(min:
    `TCGETS`/`TIOCGWINSZ` → ENOTTY/stub), `fcntl`(min), `dup`/`dup3`, `pipe2`.
  - *fd table*: Linux integer fds → VFS `FileDescriptor` caps or socket caps;
    `0/1/2` (stdin/stdout/stderr) → serial/log ring buffer.
  - *Signals* (minimal): `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`,
    `kill`/`tgkill` (deliver `SIGKILL`/`SIGTERM` to the container).
  - *Net*: `socket`/`bind`/`connect`/`listen`/`accept4`/`sendto`/`recvfrom`/
    `sendmsg`/`recvmsg`(min)/`setsockopt`(min)/`getsockname` → Phase 4 router.
  - *Misc*: `clock_gettime`, `nanosleep`/`clock_nanosleep`, `getrandom`, `uname`,
    `getcwd`, `chdir`, `getuid`/`geteuid`/`getgid`/`getegid`(return 0/root),
    `sysinfo`(min), `poll`/`ppoll`(min for a single fd).
- **OCI image handling** (ring-3 `oci-server`): image manifest + config JSON
  parser (no serde — a small hand-rolled JSON reader or a `no_std` crate),
  gzip-decompress + tar-extract each layer in order onto an overlay, apply
  whiteouts, and expose the assembled rootfs as a VFS mount.
- **Registry client**: Docker Registry HTTP API v2 pull (`GET /v2/<name>/
  manifests/<ref>`, `GET /v2/<name>/blobs/<digest>`), content-digest (`sha256`)
  verification, over plain HTTP first.
- **Container runtime**: read the image config (entrypoint, cmd, env, workdir,
  user), assemble the rootfs, create a Linux-ABI process with a CSpace restricted
  to the rootfs (+ granted sockets), `exec` the entrypoint, and `wait` for exit.
- **Management surface**: `run <image> [cmd…]`, `ps` (list containers), `kill
  <id>` shell commands (a stand-in for the Phase 6 Docker-compatible API).
- **Tests**: static hello-world ELF; busybox `echo`/`cat`/`ls`; layer unpack +
  whiteout; registry pull against a local HTTP `registry:2` (via xtask); a
  container-isolation test (a container cannot open a path outside its rootfs or a
  socket it wasn't granted).
- **Post-phase**: containers architecture documented in mdbook.

## Workspace Structure (new)

```
themelios/
├── kernel/
│   └── src/
│       ├── linux/               # NEW: Linux syscall personality
│       │   ├── mod.rs           #   personality flag + dispatch entry
│       │   ├── syscall.rs       #   Linux syscall number table + translators
│       │   ├── elf.rs           #   ELF64 loader + initial-stack builder
│       │   ├── fd.rs            #   Linux fd table ↔ VFS/socket caps
│       │   ├── mem.rs           #   brk/mmap/mprotect over the address space
│       │   ├── thread.rs        #   clone(thread)/futex/set_tid_address
│       │   └── signal.rs        #   minimal signal delivery
│       └── container/           # NEW: kernel-side runtime glue
│           └── mod.rs           #   create restricted process, exec, wait
├── servers/
│   └── oci-server/              # NEW: ring-3 image pull + unpack
│       ├── src/main.rs          #   JSON/tar/gzip/HTTP; assemble rootfs
│       └── Cargo.toml           #   no_std; miniz_oxide (gzip), no serde
└── ...
```

Whether the ELF loader lives in `kernel/src/linux/elf.rs` or a shared
`kernel/src/exec/` is an open call for 5.0; native ThemeliOS processes may later
want ELF too, so keep the loader personality-agnostic and let the Linux layer own
only the *Linux initial-stack* shape.

## Sub-phase Dependency Graph

```
5.0 (ELF64 loader + exec a native-built static ELF)
             │
             ▼
5.1 (Linux personality + minimal syscalls → static "hello world")
             │
     ┌───────┼─────────────────┐
     ▼       ▼                 ▼
5.2 (FS    5.3 (mm +          (net syscalls fold in with 5.2/5.5,
syscalls   threads/futex →     reusing Phase 4 router)
over VFS)  pthread/malloc)
     │       │
     └───┬───┘
         ▼
5.4 (OCI image: ring-3 oci-server, JSON+tar+gzip → rootfs on overlay,
     from a local `docker save` bundle on the data disk)
         │
         ▼
5.5 (Container runtime + `run` → assemble rootfs, exec entrypoint with
     restricted caps, wait; busybox container end-to-end)
         │
         ▼
5.6 (Registry client: HTTP v2 pull over TCP, digest-verified)
         │
         ▼
5.7 (exec/signals/process-mgmt polish + isolation test + docs → Phase 5 done)
```

**Off-ramps**: after **5.3** the node "runs a static Linux binary" — a real,
demonstrable milestone. After **5.5** it "runs a container from a local image."
**5.6** adds registry pull. Dynamic linking, TLS/Docker Hub, and cgroups are
explicitly beyond this phase's off-ramp.

## Sub-phases

### Sub-phase 5.0 — ELF64 loader and `exec`

**Goal**: Load a statically-linked `ET_EXEC` ELF64 into a fresh ring-3 process and
run it — replacing the flat-binary path with a real loader.

**Sequencing note (verified)**: the Linux syscall personality does not exist until
5.1, but a loaded program still needs *some* syscall ABI to exit. So **5.0's test
binary uses the native ThemeliOS ABI** — reuse `libthemelios` (its raw `syscall`
wrappers), but link it as a **normal ELF** (drop the servers' `--oformat=binary`)
instead of a flat image. This proves segment mapping + entry + initial stack
*independent of* Linux-ABI correctness (which 5.1 validates by swapping in a
musl binary). It also means 5.0 needs no external toolchain — it reuses the
existing server build with one linker-flag change.

**What to build**:
- ELF64 header + program-header parsing (validate `EI_CLASS=64`, `EI_DATA=LSB`,
  `e_machine=x86-64`, **`ET_EXEC`**). *Static-PIE (`ET_DYN`) is deferred* — it
  requires applying `R_X86_64_RELATIVE` relocations at load; build musl test
  binaries `-no-pie -static` to stay `ET_EXEC`.
- Map each `PT_LOAD` at its `p_vaddr` with W^X derived from `p_flags`; zero the
  `.bss` tail (`p_memsz > p_filesz`).
- **Byte-source abstraction**: the loader reads the ELF via an "read `n` bytes at
  offset" trait, so it works from an embedded `&[u8]` (5.0 test) *and* from a VFS
  file on the rootfs (5.5) without a rewrite. Do **not** hardcode `&'static [u8]`.
- Build the initial stack: `argc`/`argv`/`envp`/`auxv` (`AT_PHDR`/`AT_PHENT`/
  `AT_PHNUM`/`AT_ENTRY`/`AT_PAGESZ`/`AT_RANDOM`/`AT_HWCAP`) + a 16-byte-aligned
  entry `%rsp` per the SysV/Linux entry contract; seed `AT_RANDOM` (16 bytes).
- An `exec`-style entry that installs the loaded image into a process address
  space and enters at `e_entry` in ring 3.
- Test harness: embed the native-ABI ELF (built as above) and run it.

**Modules**: `kernel/src/linux/elf.rs`, `kernel/src/container/mod.rs`,
`servers/linker.ld` / xtask (emit an ELF for the test binary)

**Acceptance** (all met — `test_elf_exec`, 36 tests green, 4/4 soak):
- [x] A static `ET_EXEC` ELF64 is parsed and its `PT_LOAD` segments mapped W^X
- [x] Initial stack (argc/argv/envp/auxv, 16-byte `%rsp`) is correct; the program
      reads its own argv[0] (verified: argc==2, argv[0][0]=='e')
- [x] The program runs in ring 3 and exits cleanly (native-ABI `SYS_EXIT`)
- [x] The loader reads from a byte-source (`SliceSource` now; VFS-ready trait)
- [x] Malformed/truncated ELF and non-`ET_EXEC` are rejected without a kernel fault

**Implementation notes** (branch `claude/phase-5.0-elf-loader`): `kernel/src/linux/
elf.rs` (parser + `ByteSource` trait + `SliceSource`, `load_into`, `map_segment`
W^X, `build_initial_stack`), `kernel/src/linux/mod.rs` (`exec_elf`/`spawn_loaded`
+ a per-process `elf_trampoline` that `iretq`s to the recorded `(entry, rsp)` —
`Process` gained a `user_entry` field + `set_user_entry`/`user_entry`).
`servers/elf-smoke` is a detached crate built as a real ELF by a new
`build_elf_smoke` xtask step (static, `-no-pie` → `ET_EXEC`); it writes proof
words (magic, argc, argv[0][0]) to a kernel-mapped result page and exits via the
native `SYS_EXIT`. Deferred as planned: static-PIE relocations, and reclaiming
mapped data frames on `destroy_process` (`AddressSpace::destroy` frees only
page-table frames today — a small per-run leak, fine for the test; proper teardown
is later-phase work).

---

### Sub-phase 5.1 — Linux personality + minimal syscalls

**Goal**: A process marked "Linux" has its `syscall`s dispatched through a Linux
table; implement the minimum to run a static "hello world" (musl) binary.

**What to build**:
- A `personality` field on `Process`; the `syscall` entry branches on it (native
  dispatch vs Linux dispatch) — Linux `write`=1 collides with native `SYS_SEND`=1,
  so the branch is mandatory, not cosmetic.
- **Per-task FS-base save/restore** (net-new, see the primitives section): add an
  `fs_base` to `Task`, load it into `IA32_FS_BASE` on every context switch, and
  set it from `arch_prctl(SET_FS)`. Without this, musl TLS (`errno`, stdio locks)
  is broken. Model on the Phase 4 GS-base handling.
- Linux syscall table with the *actual* static-musl `_start` set (verified —
  musl's stdio probes the tty and uses `writev`, so these belong in 5.1, not 5.2):
  `write`, **`writev`**, `read`(fd 0 → 0/EOF), `brk`, `mmap`(anon)/`munmap`/
  `mprotect`, `arch_prctl`(SET_FS), `set_tid_address`, **`ioctl`(TCGETS/
  TIOCGWINSZ → `-ENOTTY`)** so `isatty` resolves, `exit`/`exit_group`,
  `rt_sigprocmask`(stub), `getrandom`, `clock_gettime`, `getpid`/`getuid`(0).
- **stdio routing**: container fd `1`/`2` → the kernel serial writer (the same
  sink as `println!`, so container output reaches the console); fd `0` → EOF.
  (Output interleaves with kernel logs until 5.5 adds per-container capture.)
- Linux error-return convention (`-errno` in rax); a syscall trace so unimplemented
  numbers surface as "add this next," not silent failures.
- Run a **statically-linked musl** (`-no-pie -static`) hello-world (embedded like
  the 5.0 test ELF) that prints to stdout and exits.

**Acceptance** (met via a hand-crafted Linux-ABI probe — `test_linux_exec`, 37
tests green, 5/5 soak):
- [x] A Linux-ABI binary writes to stdout via `write` (console shows
      "linux-smoke ok") and exits via `exit_group`
- [x] `brk` growth maps a new heap page that is writable; anonymous `mmap`
      returns a writable mapping (both self-checked by the probe)
- [x] `arch_prctl(SET_FS)` + per-task FS-base restore give working TLS — the probe
      writes/reads via `%fs` and it survives scheduling (soak-verified)
- [x] `ioctl` returns `-ENOTTY`; unimplemented syscalls return `-ENOSYS` (logged),
      not a fault
- [~] **Real static-musl binary deferred**: no musl toolchain is guaranteed in
      this environment, so 5.1 validates the surface with a deterministic
      hand-crafted probe instead. Running an actual musl binary end-to-end waits
      on a checked-in toolchain fixture (or Rust's `x86_64-unknown-linux-musl`),
      layered on 5.2's FS syscalls + 5.3's threads/futex which a real libc needs.

**Implementation notes** (branch `claude/phase-5.1-linux-personality`):
`Process` gained a `personality` (`Native`/`Linux`) plus `brk`/`mmap_next`; the
shared `syscall_dispatch` branches to `linux::syscall::dispatch` for Linux
processes (Linux `write`=1 collides with native `SYS_SEND`=1). `Task` gained an
`fs_base` restored on every context switch (next to the Phase 4 GS-base refresh)
and set by `arch_prctl(SET_FS)` via `sched::set_current_fs_base`.
`kernel/src/linux/syscall.rs` implements write/writev (fd 1/2 → serial), read (fd
0 → EOF), brk, anonymous mmap (bump allocator from `LINUX_MMAP_BASE`), mprotect/
munmap (no-op), arch_prctl(SET/GET_FS), ioctl→ENOTTY, set_tid_address/gettid/
getpid/get[e][ug]id, rt_sig*(stub), clock_gettime, getrandom (splitmix64),
sched_yield, exit/exit_group (mirrors the native swapgs). `servers/linux-smoke` is
a detached real-ELF probe that self-checks TLS/brk/mmap and reports to a result
page. `exec_elf` now marks its process `Linux`.

---

### Sub-phase 5.2 — Linux filesystem syscalls over the VFS

**Goal**: A Linux process reads and writes its rootfs through the Phase 3 VFS.

**What to build**:
- Linux fd table: integer fds → VFS `FileDescriptor` caps (and `0/1/2` → console).
- `openat`/`close`/`read`/`write`/`lseek`/`fstat`/`newfstatat`/`statx`/
  `getdents64`/`readlinkat`/`getcwd`/`chdir`, translated to the VFS router against
  the process's **rootfs mount capability** (path resolution rooted at the
  container root — no escape above it).
- Linux `struct stat`/`dirent64` layout marshalling.
- Run static **busybox** applets that touch the FS: `echo`, `cat <file>`, `ls`.

**Acceptance** (met — `test_linux_fs` + `test_path_resolve`, 39 tests green, 4/4 soak):
- [x] `openat`+`read` returns file bytes from the rootfs mount (fs-smoke reads a
      staged `/hello.txt` and the bytes match)
- [x] Paths cannot escape the rootfs — `test_path_resolve` asserts `..` clamps at
      root (incl. `../../../../../etc/passwd` → `/etc/passwd`); the kernel only ever
      passes the process's single `rootfs_mount` to the VFS
- [x] `fstat`/`newfstatat` return a Linux-shaped `stat` (144-byte layout, st_mode/
      st_size); `getdents64` emits `linux_dirent64` records
- [~] A `getdents64`-driven `ls` and interactive shell use are deferred to the
      container runtime (5.5) — 5.2 validates the syscall via the probe

**Implementation notes** (branch `claude/phase-5.2-linux-fs-syscalls`): `Process`
gained `rootfs_mount`, `cwd`, and a **Linux fd table** (`LinuxFd`: mount +
server_fd + offset + size + is_dir); fds 0/1/2 are stdio, ≥3 index the table.
`kernel/src/linux/fs.rs` translates `openat`/`open`/`read`/`write`/`close`/
`lseek`/`fstat`/`newfstatat`/`getdents64`/`getcwd`/`chdir` onto the mount-keyed
`fs::k*` VFS calls against `rootfs_mount`. The security boundary is the factored,
unit-tested [`resolve_path`], which clamps `..` at the root — a container path can
never reach above its rootfs, and since Linux syscalls carry no capabilities, the
kernel enforces isolation by only ever resolving against the one `rootfs_mount` the
process holds. `servers/fs-smoke` opens/reads a staged file and checks clamping.
Deferred: `statx`, `readlinkat`/symlinks, real `dirfd` (non-`AT_FDCWD`) resolution,
and a streaming `getdents64` cursor (5.2 requires the batch fit the caller buffer).

---

### Sub-phase 5.3 — Linux memory, threads, and futex

**Goal**: Support the mmap/thread/futex surface musl needs for pthreads and a
real allocator.

**What to build**:
- Full `mmap`/`munmap`/`mprotect`/`mremap`(min); file-backed `mmap` (via VFS
  read into mapped pages; shared/private semantics as feasible).
- `clone`(CLONE_THREAD|CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SETTLS|…): a new task in
  the **same** process/address space (already supported — `Process.tasks`), with
  its own user stack and TLS (`CLONE_SETTLS` → the new task's FS base); `gettid`.
- `futex`(WAIT/WAKE) — **net-new primitive** (the kernel has no address-keyed wait
  queue; IPC blocking is endpoint-based). Add a wait queue keyed by
  `(address_space, virtual address)`; scope to **private** WAIT/WAKE only — no PI,
  no `FUTEX_REQUEUE`, no cross-process shared futex. This is the primitive pthreads
  mutexes/condvars block on.
- `set_robust_list`/`rseq`(stub), `sched_yield`.

**Acceptance** (met — `test_linux_threads`, 40 tests green, 6/6 soak):
- [x] A binary `clone`s a thread (shared address space, own TLS + stack) and joins
      it — `threads-smoke` clones, the child writes a magic and `exit`s, the parent
      `futex`-waits on the CLEARTID word until the kernel clears + wakes it
- [x] `futex` WAIT/WAKE blocks/wakes correctly (the join is a real futex wait
      released by the child's exit; the WAIT value-recheck handles the exit race)
- [~] **File-backed `mmap` deferred** — separable from the threads/futex core and
      not needed for a static busybox; a static-musl binary uses anonymous mmap
      (5.1) + brk. Layer file-backed mmap when a dynamic loader needs it.
- [~] A strict global free-frame leak check can't hold yet because
      `AddressSpace::destroy` reclaims only page-table frames (the 5.0-noted
      limitation), so process data frames leak regardless of threads. Thread
      **kernel stacks** are reclaimed on exit via `cleanup_dead_tasks`; reliability
      is covered by the 6/6 soak. Full teardown reclamation is later-phase work.

**Implementation notes** (branch `claude/phase-5.3-threads-futex`): `Task` gained
`clone_entry` + `clear_child_tid`. `kernel/src/linux/thread.rs`: `sys_clone`
(CLONE_THREAD|CLONE_VM only) spawns a sibling task whose ring-3 entry (via a new
`thread_trampoline`, `rax=0` on the child stack) is the parent's post-`syscall`
RIP; CLONE_SETTLS seeds its FS base, CLONE_CHILD_SETTID publishes the tid at clone
time (no join race), CLONE_CHILD_CLEARTID is honoured on exit. `sys_futex`
(private WAIT/WAKE) uses a global `(pid, uaddr)` wait queue — the value re-check +
enqueue + `block_current_task` is non-preemptible on single-core, so no wakeup is
lost. `exit`(60) is now thread exit (clear the join word + futex-wake + kill the
task); `exit_group`(231) kills sibling tasks then exits. `set_tid_address` records
the CLEARTID word. `servers/threads-smoke` is the clone+join probe. Deferred:
file-backed mmap, PI/requeue/timeout futexes, `fork`/`vfork`.

---

### Sub-phase 5.4 — OCI image unpack (ring-3 oci-server)

**Goal**: Turn an OCI image (staged locally) into a rootfs on disk.

**What to build**:
- `servers/oci-server`: parse the image **manifest** and **config** JSON
  (hand-rolled `no_std` reader; extract layer digests, entrypoint/cmd/env/cwd).
- gzip-decompress (miniz_oxide) + tar-extract each layer in order onto the Phase 3
  **overlay** (upper on ext2); apply OCI whiteouts (`.wh.<name>`, `.wh..wh..opq`).
- Stage input from a **`docker save` tarball** placed on the ext2 data volume by
  xtask (**locked decision** — no network enters until 5.6; a local HTTP registry
  is explicitly *not* used here). xtask produces the bundle at image-build time
  (`docker save busybox -o …`, unpacked into the data-disk image), so tests are
  deterministic and offline.
- Expose the assembled rootfs as a VFS mount id.

**Acceptance** (unpack library met — `test_oci_unpack`, 41 tests green, 3/3 soak):
- [x] A multi-layer `docker save` bundle unpacks to a correct rootfs (2-layer
      synthesized bundle → correct file set + contents)
- [x] Whiteouts delete lower-layer entries (`.wh.hello` removes `/bin/hello`;
      `.wh..wh..opq` opaque-dir handling implemented)
- [x] Malformed input is rejected, never a panic (garbage bundle → `Err`)
- [x] Image config parsed (Entrypoint/Cmd/Env/WorkingDir)
- [~] **Deferred to 5.5/5.6:** writing the assembled rootfs to disk via the
      **ring-3 `oci-server`** (folded into 5.5 assemble-and-run, where the
      untrusted-parser-in-ring-3 containment applies); layer **`sha256`** digest
      verification and **gzip** layers (the registry wire format) → **5.6**.

**Corrections vs the original plan (verified):** (1) `docker save` layers are
**uncompressed** tar, so 5.4 needs no gzip — gzip is a registry concern (5.6).
(2) No usable `docker` daemon in the dev sandbox, so the test **synthesizes** a
minimal docker-save bundle (manifest + config + two layer tars) in-memory rather
than shelling out — deterministic and offline.

**Implementation notes** (branch `claude/phase-5.4-oci-unpack`): `kernel/src/oci/`
— `tar.rs` (USTAR reader: names+prefix+GNU-longname, files/dirs), `json.rs` (a
small recursive-descent JSON parser, no serde), `mod.rs` (`unpack(bundle) →
Image{files, config}`: outer tar → `manifest.json` → config + layers, applies
layers in order with whiteout/opaque handling, parses Entrypoint/Cmd/Env/
WorkingDir). Written `alloc`-only with **no kernel deps** so it lifts into the
ring-3 `oci-server` unchanged in 5.5 (test-gated in the kernel for now). Precedent
confirmed: `squashfs-server` already runs `miniz_oxide` in ring 3 (gzip path for
5.6), and `libthemelios` has FS client wrappers (`open`/`read_file`/`write_file`)
for the 5.5 server to write the rootfs.

---

### Sub-phase 5.5 — Container runtime + `run`

**Goal**: Assemble a rootfs, launch its entrypoint as a capability-restricted
Linux process, and wait for it.

**What to build**:
- **Process exit-status + wait/reap** (net-new — `ProcessState` is only
  `Running`/`Exited`, no code, no wait): add an exit-status field set by
  `exit_group`, and a parent-side wait/reap so `run` can block for the container's
  exit code and `wait4` inside the container works.
- Kernel `container` glue: create a Linux-ABI process whose CSpace holds **only**
  the rootfs mount cap (+ optionally a socket-factory cap), apply the image
  config (entrypoint+cmd → argv, env → envp, workdir → cwd), load the entrypoint
  ELF **from the rootfs via the byte-source loader** (5.0), `exec`, and `wait`.
- **Per-container stdout capture**: route the container's fd 1/2 to a buffer/stream
  the `run` command prints, rather than raw-interleaving with kernel logs (the
  5.1 direct-to-serial routing was the bootstrap).
- `run <image-ref> [cmd…]` shell command driving oci-server unpack → runtime exec.
- `ps` (list running containers) and `kill <id>`.

**Acceptance** (met — `test_container_run`, 42 tests green, 4/4 soak):
- [x] A container runs from an image and exits 0 — `test_container_run` builds an
      image whose `/init` is a real Linux ELF (`linux-smoke`), assembles the rootfs
      on an ext2 mount, launches it, and the entrypoint runs (prints "linux-smoke
      ok") and exits cleanly. `run` shell command does the same live on `/data`.
- [x] The container is rooted at its own rootfs mount; Linux path syscalls resolve
      there and cannot escape (the 5.2 clamp) — no host-root FS access
- [x] Exit status propagates — `exit_group` records it on the process; `run` and
      the test read it back (`process::exit_status`)
- [~] A dedicated crash-isolation test is deferred (ring-3 containment is
      structural, as noted throughout Phase 4/5); a crashing container's fault is
      confined to its ring-3 process.

**Implementation notes** (branch `claude/phase-5.5-container-runtime`): the payoff
integrating 5.0–5.4. `kernel/src/container/mod.rs`: `create(bundle, mount)`
unpacks the image (`oci::unpack`), writes the rootfs onto the mount
(`kmkdir`/`kcreate`/`kwrite`, creating parent dirs), loads the entrypoint ELF
**from that rootfs** via a new `VfsByteSource` (the loader's `ByteSource` over
`fs::kread` with filling reads), and creates a Linux process (rootfs_mount, argv =
entrypoint++cmd, env, cwd, `Personality::Linux`) — so the entrypoint round-trips
bundle → ext2 → loaded-from-rootfs → run. `start(pid)` launches it. Process gained
`exit_code`; `exit_group` records it + marks `Exited`; `process::exit_status`
reads it. `run` shell command runs an embedded demo image on `/data`. Bumped
`block_server` `MAX_INSTANCES` 8→16 (the suite now spins up more FS servers).
**Containment (flagged):** `oci::unpack` (untrusted parsing) runs in the kernel
for now — it is safe `alloc`-only Rust returning `Result` (no `unsafe`, no panic
on bad input), so a bug is contained; relocating it into a dedicated ring-3
`oci-server` is documented hardening (it lifts unchanged). Deferred: `ps`/`kill`,
ring-3 oci-server, real-image staging (5.6).

---

### Sub-phase 5.6 — Registry client (HTTP v2 pull) — ✅ DONE

**Goal**: Assemble an image from the **registry wire format** (Docker Registry
HTTP API v2): a manifest naming a config blob and **gzipped** layer blobs by
`sha256:` digest, each **digest-verified** before use.

**What was built**:
- `oci/sha256.rs` — a small `alloc`-only SHA-256 (`sha256`, `hex`, `verify`);
  `verify(data, "sha256:<hex>")` gates every blob. Tested against FIPS 180-4
  vectors (`""`, `"abc"`, the 56-byte multi-block message).
- `oci/gzip.rs` — `decompress(&[u8]) -> Option<Vec<u8>>`: parses the RFC-1952
  gzip wrapper (magic/CM/FLG + optional FEXTRA/FNAME/FCOMMENT/FHCRC + 8-byte
  trailer) and inflates the DEFLATE body via `miniz_oxide::inflate` (which does
  raw DEFLATE/zlib, not gzip — hence the hand-rolled wrapper). Fails closed
  (`None`) on bad magic/header/DEFLATE; never panics.
- `oci::unpack_registry(manifest_json, get_blob)` — parses a manifest v2, pulls
  the config + layer blobs through the `get_blob` closure, **digest-verifies**
  each, gunzips layers, and applies them (reusing the shared `parse_image_config`
  + `apply_layer` extracted from `unpack`).
- `oci/registry.rs` — HTTP/1.1 pull over a `Connection` trait (`request(&[u8]) ->
  Option<Vec<u8>>`): `http_body` parses status + `Content-Length` body;
  `get` issues `GET /v2/<name>/manifests|blobs/...`; `pull` fetches the manifest,
  then every blob it names, then hands them to `unpack_registry`.
- `miniz_oxide` added to `kernel/Cargo.toml` (`with-alloc`, no default features).

**Deferred (documented)**: the **live** `Connection` over the kernel TCP client
through a slirp `guestfwd` registry, plus a host `registry:2` in xtask — the full
transport is a thin, well-scoped follow-up; the pull *pipeline* (HTTP parse →
digest-verify → gunzip → assemble) is what carries the risk and is fully tested
here offline. Manifest-list → amd64 selection also deferred (single-manifest
images only for now). TLS/Docker Hub remains a separate gated follow-up.

**Acceptance**:
- [x] `test_registry_pull` pulls + assembles an image end-to-end over a mock
      `Connection` (manifest v2 + gzipped, digest-verified layer → `/init`)
- [x] Config and every layer blob digest-verify before use (`sha256::verify`)
- [x] A tampered (wrong-digest) blob surfaces `OciError::DigestMismatch`, not a
      hang/fault
- [x] `test_sha256` (FIPS vectors) + `test_registry_pull` green; all 44 tests
      pass, 3× soak clean

**Momus review — hardening fixes applied** (untrusted registry input; the
attacker controls *both* the manifest and the blobs that hash to the digests in
it, so digest verification does **not** bound content):
- **gzip decompression bomb** → `gzip::decompress` now inflates with
  `decompress_to_vec_with_limit(_, 8 MiB)` (was the unbounded `decompress_to_vec`);
  a bomb fails closed instead of exhausting the 16 MiB heap → abort.
- **Unbounded JSON recursion** → `json` threads a depth counter (`MAX_DEPTH = 64`)
  through `value`/`object`/`array`; deep nesting (`[[[[…`) returns `None` instead
  of overflowing the kernel stack.
- **Content-Length overflow** → `http_body` uses `body_start.checked_add(n)` so a
  huge `Content-Length` returns `None` instead of panicking under debug-build
  overflow checks.
- **Compat limitations documented** (not safety): `http_body` handles only
  `Content-Length` framing (not chunked); `gzip::decompress` handles a single
  member. Both matter for the live puller, noted in the doc comments.
- `test_registry_hardening` locks in the JSON-depth and Content-Length fixes.
  Momus confirmed digest-verify-before-use ordering is correct everywhere and
  found no memory-safety/UB bugs.

**Containment note**: the OCI parser (tar/json) and now `miniz_oxide` run
**in-kernel** for the deterministic tests. Relocating the whole `oci` module +
miniz into the ring-3 `oci-server` (so untrusted image bytes never parse in ring
0) is the documented hardening, carried into 5.7/later.

**Note on `make_gzip` (test helper)**: it emits **stored (uncompressed) DEFLATE
blocks** rather than calling `miniz_oxide::deflate` — `CompressorOxide` is
~100 KiB and overflows the 16 KiB kernel test stack. Stored blocks are valid
DEFLATE, so the production `gzip::decompress` (header parse + miniz inflate) path
is still fully exercised; only the compressor (never used in production) is
avoided.

---

### Sub-phase 5.7 — Process management, signals, isolation test, docs

**Goal**: Round out process lifecycle and finalize the phase.

**What to build**:
- `exec` into a running container (a second process sharing its rootfs);
  `wait4`/exit-status plumbing; minimal signal delivery (`SIGTERM`/`SIGKILL`).
- A deterministic **isolation test**: a container binary tries to `openat` a host
  path and to open an ungranted socket, and gets `-EACCES`/`-EPERM`.
- mdbook `containers.md`; update `milestones.md` + `CLAUDE.md` to Phase 5 status.

**Acceptance**:
- [ ] `kill` terminates a container; exit status is observable
- [ ] Isolation test: no host-FS/ungranted-socket access from inside a container
- [ ] All new tests pass; Phases 1–4 tests still pass; arm64 gate still green
- [ ] Containers architecture documented in mdbook

---

## Dependencies

| Crate | Used by | Purpose | no_std? |
|-------|---------|---------|---------|
| miniz_oxide | oci-server | gzip (DEFLATE) layer decompression | Yes — **already vendored** (SquashFS) |
| (hand-rolled) | oci-server | tar reader, JSON reader, HTTP/1.1 client | in-tree, no new dep |
| (deferred) rustls / a no_std TLS | oci-server | HTTPS registries (Docker Hub) | **Phase 5.x/6 follow-up — verify bare-metal build first, per the smoltcp/miniz precedent** |

**Build-time (host) tooling**: a static musl toolchain (or prebuilt static test
binaries checked into `tests/fixtures/`), and `docker`/`skopeo` to produce the
`docker save` bundles and run the local `registry:2` for 5.6. Any new host tool
goes in the `Brewfile` + `dev-setup.md` + `CLAUDE.md`, per project convention.

## Error Types (sketch)

Linux syscalls return `-errno`. A `LinuxError` enum maps the internal
`SockError`/VFS errors/`NoResources` onto the Linux errno space (`ENOSYS`,
`EBADF`, `EACCES`, `ENOENT`, `EFAULT`, `EINVAL`, `EAGAIN`, `ENOMEM`, `EPERM`,
`ESRCH`, `EMFILE`, …). The oci-server keeps its own `OciError` (bad manifest,
digest mismatch, unsupported media type, registry status).

## Memory Budget

Phase 5 keeps the 256 MiB QEMU RAM. Rootfs layers live on the **ext2 data disk**,
not RAM. A container process gets a bounded address-space budget (the existing
mechanism); mmap/heap growth is capped per process. The oci-server needs a larger
heap (layer decompression buffers) — budget ~16–32 MiB, streamed rather than
whole-image-in-RAM where possible.

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| ELF initial-stack / auxv / TLS subtleties | High | High | 5.0/5.1 validate incrementally; 5.0 uses the native ABI so loader bugs are isolated from Linux-ABI bugs. `AT_RANDOM`, `AT_PHDR`, 16-byte `%rsp` alignment are the usual traps — test early. |
| Per-task FS-base save/restore is net-new (TLS) | High | High | Confirmed missing (only GS base is switched today). Add `Task.fs_base` + a context-switch load, modelled on the Phase 4 GS-base fix; 5.1 gates on musl TLS working across a switch. |
| `futex` correctness — net-new address-keyed wait queue | High | High | No such primitive exists (IPC blocking is endpoint-based). 5.3 adds a queue keyed by `(address_space, vaddr)`, scoped to private WAIT/WAKE; test with a real pthread mutex round-trip. |
| Process exit-status / wait / reaping is net-new | Medium | Medium | Confirmed missing (`ProcessState` = Running/Exited only). 5.5 adds an exit-status field + parent wait/reap before `run` can report exit codes. |
| `mmap` semantics (MAP_FIXED, file-backed, shared) | Medium | High | Start anon-only (5.1), add file-backed private (5.3); defer shared/MAP_FIXED-overmap edge cases with explicit `-ENOSYS`. |
| Static-musl still hits unimplemented syscalls | Medium | Medium | `-ENOSYS` + a syscall trace so gaps surface as clear "add this next," not faults; grow the table driven by real binaries. |
| Image parsing surface (tar/gzip/JSON) | Medium | Medium | All in ring-3 oci-server — a bug is a server crash, not a kernel compromise (Phase 3/4 precedent). |
| Registry TLS scope creep | Medium | Medium | Explicitly plain-HTTP for 5.6; TLS is a separate, gated follow-up with its own bare-metal-build check. |
| Phase size (7 sub-phases, whole new subsystem) | High | Medium | Hard off-ramps at 5.3 (static binary) and 5.5 (local-image container); registry and dynamic linking are additive. |
| Dynamic linking / glibc images out of scope | Certain | Medium | Static-musl only this phase; documented. Most minimal images (busybox/alpine-static) are covered; glibc/dynamic is a later phase. |

## Estimated Effort

| Sub-phase | Est. commits | Complexity |
|-----------|-------------|------------|
| 5.0 ELF loader + exec | 4 | High |
| 5.1 Linux personality + minimal syscalls | 5 | High |
| 5.2 FS syscalls over VFS | 4 | Medium-High |
| 5.3 mm + threads + futex | 6 | Very High |
| 5.4 OCI image unpack (oci-server) | 5 | High |
| 5.5 Container runtime + `run` | 5 | High |
| 5.6 Registry client (HTTP v2) | 4 | Medium-High |
| 5.7 process mgmt + signals + isolation + docs | 4 | Medium |
| **Total** | **~37** | |

## Verification Checklist

- [ ] ELF64 static binaries load and run in ring 3 (W^X segments, correct auxv)
- [ ] Linux personality routes `syscall` correctly; native processes unaffected
- [ ] A static musl "hello world" prints and exits
- [ ] FS syscalls map onto the VFS against a rootfs mount cap; no path escape
- [ ] mmap/brk back a real allocator; file-backed mmap works
- [ ] clone(thread) + futex support a pthread mutex round-trip
- [ ] OCI image (local bundle) unpacks to a correct rootfs, whiteouts applied
- [ ] Layer/config digests verify before use
- [ ] `run <image>` launches a capability-restricted container and waits
- [ ] Container cannot reach the host FS or ungranted sockets (isolation test)
- [ ] Registry pull over HTTP works and digest-verifies
- [ ] Image parsing bugs crash only the oci-server, never the kernel
- [ ] All new tests pass; no regressions in Phases 1–4; arm64 gate green
- [ ] milestones.md and CLAUDE.md updated
- [ ] **Post-phase**: containers architecture documented in mdbook

## Explicit Non-Goals (this phase)

- Dynamic linking / `ld.so` / glibc images (static-musl only)
- HTTPS registries / Docker Hub TLS (plain HTTP; TLS is a scoped follow-up)
- cgroups CPU/pids limits (memory bounded by address-space budget only)
- Linux namespaces (capabilities are the isolation model)
- `/proc`, `/sys`, `/dev` beyond the minimum a static binary touches
- Full signal semantics (only `SIGTERM`/`SIGKILL` delivery)
- arm64 (Phase 7)
