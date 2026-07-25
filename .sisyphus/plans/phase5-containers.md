# Phase 5 — OCI Containers & Linux Syscall Compatibility

**Status**: NOT STARTED (draft plan)
**Created**: 2026-07-25
**Phase**: 5
**Depends on**: Phase 2 (capabilities, processes, IPC), Phase 3 (VFS + overlay +
ext2), Phase 4 (TCP/IP + sockets) — all complete.

## Goal

Run **unmodified OCI/Docker container images** as capability-isolated ring-3
processes on ThemeliOS. Concretely, the off-ramp deliverable is:

```
> run docker.io/library/busybox echo hello from a container
hello from a container
```

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

- **ELF64 loader**: parse `ET_EXEC` and static-`ET_DYN` (static-PIE) headers, map
  `PT_LOAD` segments with per-segment W^X, honour `PT_GNU_STACK`, set up TLS from
  `PT_TLS`, build the Linux initial stack (`argc`, `argv[]`, `envp[]`, `auxv[]`
  incl. `AT_PHDR/AT_PHENT/AT_PHNUM/AT_ENTRY/AT_PAGESZ/AT_RANDOM/AT_HWCAP`), and
  enter at `e_entry`.
- **Linux personality** flag on `Process`; the `syscall` entry routes by it.
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

**Goal**: Load a statically-linked ELF64 executable into a fresh ring-3 process
and run it — replacing the flat-binary path for Linux programs.

**What to build**:
- ELF64 header + program-header parsing (validate `EI_CLASS=64`,
  `EI_DATA=LSB`, `e_machine=x86-64`, `ET_EXEC` or static `ET_DYN`).
- Map each `PT_LOAD` at its `p_vaddr` (or base+offset for PIE) with W^X derived
  from `p_flags`; zero the `.bss` tail (`p_memsz > p_filesz`).
- Build the Linux initial stack: `argc`/`argv`/`envp`/`auxv` + a 16-byte-aligned
  entry `%rsp`; seed `AT_RANDOM` (16 bytes) from `getrandom` source.
- An `exec`-style entry that swaps a process's address space to the loaded image
  and enters at `e_entry` in ring 3.
- A test harness that embeds a tiny **native-built static ELF** (compiled from a
  no_std stub with a Linux-style `_start`) and runs it.

**Modules**: `kernel/src/linux/elf.rs`, `kernel/src/container/mod.rs`

**Acceptance**:
- [ ] A static ELF64 is parsed and its `PT_LOAD` segments mapped W^X
- [ ] Initial stack (argc/argv/envp/auxv) is correct; program reads its own argv
- [ ] The program runs in ring 3 and exits cleanly via a syscall
- [ ] Malformed ELF is rejected without a kernel fault

---

### Sub-phase 5.1 — Linux personality + minimal syscalls

**Goal**: A process marked "Linux" has its `syscall`s dispatched through a Linux
table; implement the minimum to run a static "hello world" (musl) binary.

**What to build**:
- A `personality` field on `Process`; the `syscall` entry branches on it.
- Linux syscall table with: `write`(→ serial/log for fd 1/2), `writev`, `read`
  (fd 0 stub), `brk`, `mmap`(anon)/`munmap`/`mprotect`, `arch_prctl`(SET_FS for
  TLS), `set_tid_address`, `exit`/`exit_group`, `rt_sigprocmask`(stub),
  `getrandom`, `clock_gettime`, `getpid`/`getuid`(0).
- Linux error-return convention (`-errno` in rax).
- Run a **statically-linked musl** hello-world (built off-tree, embedded like the
  test ELF) that prints to stdout and exits.

**Acceptance**:
- [ ] A static musl binary prints to stdout via `write` and exits via `exit_group`
- [ ] `brk` + anonymous `mmap` back musl's allocator
- [ ] `arch_prctl(SET_FS)` sets a working TLS base (musl `errno`/stdio work)
- [ ] Unimplemented syscalls return `-ENOSYS`, not a fault

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

**Acceptance**:
- [ ] `openat`+`read` returns file bytes from the rootfs mount
- [ ] `getdents64` lists a directory; `ls` works
- [ ] Paths cannot escape the rootfs mount (no `..`-escape, no host access)
- [ ] `fstat`/`newfstatat` return a Linux-shaped `stat`

---

### Sub-phase 5.3 — Linux memory, threads, and futex

**Goal**: Support the mmap/thread/futex surface musl needs for pthreads and a
real allocator.

**What to build**:
- Full `mmap`/`munmap`/`mprotect`/`mremap`(min); file-backed `mmap` (via VFS
  read into mapped pages; shared/private semantics as feasible).
- `clone`(CLONE_THREAD|CLONE_VM|…): a new task sharing the address space, with its
  own TLS and stack; `gettid`.
- `futex`(WAIT/WAKE) on a shared address — the primitive pthreads mutexes/condvars
  need. Reuse the kernel's block/wake machinery keyed by physical address.
- `set_robust_list`/`rseq`(stub), `sched_yield`.

**Acceptance**:
- [ ] A multi-threaded static binary spawns threads via `clone` and joins
- [ ] `futex` WAIT/WAKE correctly blocks/wakes threads (a mutex round-trips)
- [ ] File-backed `mmap` reads file contents into the mapping
- [ ] No leaks/faults across thread create/exit

---

### Sub-phase 5.4 — OCI image unpack (ring-3 oci-server)

**Goal**: Turn an OCI image (staged locally) into a rootfs on disk.

**What to build**:
- `servers/oci-server`: parse the image **manifest** and **config** JSON
  (hand-rolled `no_std` reader; extract layer digests, entrypoint/cmd/env/cwd).
- gzip-decompress (miniz_oxide) + tar-extract each layer in order onto the Phase 3
  **overlay** (upper on ext2); apply OCI whiteouts (`.wh.<name>`, `.wh..wh..opq`).
- Stage input from a **`docker save` tarball** placed on the ext2 data volume by
  xtask (no network yet).
- Expose the assembled rootfs as a VFS mount id.

**Acceptance**:
- [ ] A multi-layer `docker save` bundle unpacks to a correct rootfs
- [ ] Whiteouts delete lower-layer entries in the overlay
- [ ] Layer digests verify (`sha256`) before extraction
- [ ] Malformed tar/gzip/JSON crashes only the oci-server, not the kernel

---

### Sub-phase 5.5 — Container runtime + `run`

**Goal**: Assemble a rootfs, launch its entrypoint as a capability-restricted
Linux process, and wait for it.

**What to build**:
- Kernel `container` glue: create a Linux-ABI process whose CSpace holds **only**
  the rootfs mount cap (+ optionally a socket-factory cap), apply the image
  config (entrypoint+cmd → argv, env → envp, workdir → cwd), `exec`, and `wait4`.
- `run <image-ref> [cmd…]` shell command driving oci-server unpack → runtime exec.
- `ps` (list running containers) and `kill <id>`.

**Acceptance**:
- [ ] `run busybox echo hello` prints from inside a container and exits 0
- [ ] The container's CSpace excludes the host root and ungranted sockets
- [ ] Exit status propagates to `run`/`ps`
- [ ] A crashing container does not affect the kernel or other containers

---

### Sub-phase 5.6 — Registry client (HTTP v2 pull)

**Goal**: Pull an image from a registry over TCP instead of a local bundle.

**What to build**:
- Docker Registry HTTP API v2 client in oci-server: `GET .../manifests/<ref>`
  (handle manifest lists → pick amd64), `GET .../blobs/<digest>` for config +
  layers, streamed to disk with `sha256` verification.
- A minimal HTTP/1.1 client over the Phase 4 TCP socket API.
- **Plain HTTP** target: a local `registry:2` served by xtask (host) reachable via
  QEMU slirp; **TLS/Docker Hub deferred** (documented) — rustls-in-ring-3 is a
  scoped follow-up.

**Acceptance**:
- [ ] `run localhost:5000/busybox …` pulls + runs over HTTP
- [ ] Config and every layer blob digest-verify before use
- [ ] A registry/network error surfaces as a clean error, not a hang/fault
- [ ] Manifest-list images resolve to the amd64 manifest

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
| ELF initial-stack / auxv / TLS subtleties | High | High | 5.0/5.1 validate incrementally against a known static musl binary; `AT_RANDOM`, `AT_PHDR`, 16-byte `%rsp` alignment are the usual traps — test early. |
| `futex` correctness (pthreads) | High | High | 5.3 keys a wait-queue by physical address, reusing the kernel's block/wake; test with a real pthread mutex round-trip. |
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
