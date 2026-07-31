# Container Runtime

This document describes ThemeliOS's container runtime — how a container image
becomes a running, isolated process. It reflects the system as built in Phase 5.

> **Status**: Implemented in Phase 5 (amd64). Core pipeline complete; a real
> static-musl image over a live registry, container `exec`, and moving the image
> parser into a ring-3 server are documented deferrals (see the end of this
> chapter).

## What a container is here

A container is not a virtual machine and not a Linux namespace. It is an ordinary
ThemeliOS **process** with three things arranged so it *believes* it is running
on Linux, inside its own root filesystem, with no access to anything it wasn't
given:

1. a **Linux syscall personality** — its `syscall` instructions are answered by a
   Linux-ABI table, not the native ThemeliOS one;
2. a **rootfs mount** — every path it opens resolves inside one filesystem image,
   with no way to name anything outside it;
3. an **empty capability space** — it holds no capabilities at all, so every
   privileged operation (opening a socket, signalling another process) is denied
   by construction.

The isolation boundary is the **capability system**, not a namespace abstraction
layered on top of a shared kernel. A container can do exactly what its
capabilities permit — which, by default, is nothing beyond its own rootfs and
writing to its stdout/stderr. This is the whole reason the capability microkernel
exists: container isolation falls out of the capability model rather than being
bolted on.

## The pipeline: image → rootfs → process

Running a container (`container::create` then `container::start`) is a straight
line from image bytes to a ring-3 task:

```
 image bundle ─► unpack ─► assemble rootfs ─► load entrypoint ELF ─► ring-3 Linux process
   (OCI/tar)     (oci)     (VFS writes)        (elf loader)          (personality = Linux)
```

1. **Unpack** (`oci::unpack` / `oci::unpack_registry`). The image — either a local
   `docker save` bundle or a registry manifest + blobs — is parsed into a flat
   file list plus a runtime config (entrypoint, cmd, env, workdir). Layers are
   applied in order, with OCI whiteouts (`.wh.*`) resolved. See
   [Images and layers](#images-and-layers) below.
2. **Assemble the rootfs.** The unpacked files are written onto a writable mount
   (the ext2 data volume from the [storage stack](./storage.md)) via the ordinary
   VFS syscalls — the container runtime has no special filesystem access.
3. **Load the entrypoint.** The entrypoint ELF is read *out of the assembled
   rootfs* (a `VfsByteSource` feeding the ELF loader) and mapped into a fresh
   address space with `W^X` segment permissions and a System V initial stack
   (argc/argv/envp/auxv).
4. **Enter ring 3.** The process is marked `Personality::Linux`, given its rootfs
   mount and initial cwd, and spawned. From its first instruction it is a Linux
   program that cannot tell it isn't on Linux.

## The Linux syscall personality

A process flagged `Personality::Linux` has its `syscall` entries routed to
`linux::syscall::dispatch` instead of the native table. This matters because the
two ABIs collide — native `SYS_SEND` is 1, but Linux `write` is also 1 — so the
personality flag, checked on every syscall, is what keeps them apart.

The implemented subset is what a small static binary needs to start, run, and
exit: `write`/`writev`, `openat`/`read`/`close`/`lseek`/`fstat`/`getdents64`/
`getcwd`/`chdir`/`readlinkat` (the filesystem set, Phase 5.2), `brk`/`mmap`
(anonymous), `arch_prctl` (TLS via `%fs`), `clone`(`CLONE_THREAD`)/`futex`/
`set_tid_address` (threads, Phase 5.3), `clock_gettime`, `getrandom`,
`exit`/`exit_group`, and the process-control calls below. Unimplemented numbers
return `-ENOSYS`.

Per-thread TLS is real: `arch_prctl(SET_FS)` records an `%fs` base that the
scheduler restores on every context switch, so thread-local storage works across
preemption.

## Capability isolation — how a container is contained

Two boundaries make a container safe, and Phase 5.7 made both **enforced and
tested** rather than incidental.

### Filesystem: one mount, `..` clamped at the root

Every filesystem syscall resolves its path against the process's single rootfs
mount. The path resolver **clamps `..` at the root**: `../../../../etc/passwd`
normalizes back to `/etc/passwd` *inside the container's own mount* — there is no
host root for it to escape to. A container therefore cannot name, let alone open,
any file outside its image.

This is verified positively, not vacuously. The `test_container_isolation`
integration test runs a probe (`servers/isolation-smoke`) as a container `/init`
that opens `/only`, then opens `../../../../only`, and asserts the second call
**succeeds and returns bytes identical to the first** — proving the clamp is live
on the real syscall path, not merely that some out-of-tree path happens to miss.
(A bare "escape returns `-ENOENT`" assertion would prove nothing: with a single
mount and no host root, the miss happens whether the clamp works or not.)

**Per-container confinement (Phase 6.1b).** Multiple containers share one writable
mount (a per-container *mount* is infeasible here — mounts need a physical disk and
are never freed), so each container is instead confined to a `/c/<id>`
**subdirectory**. Its `rootfs_base` is prepended to every already-`..`-clamped
path at a single choke point (`linux::fs::host_path`), so the container's `/` *is*
`/c/<id>` and it can name nothing outside that subtree — not a sibling container's
files, not the mount root. Because untrusted **image** paths are just as dangerous
(the ext2 server honors `..`, so a layer member `../../host_secret` would escape at
*assembly* time), every image path is run through the same clamp before it is
written. `test_container_confinement` proves both halves: a malicious `../../evil`
is clamped into the base (never reaching the mount root, and a root `/host_secret`
is left intact), and a confined probe reads its own file but cannot open that root
`/host_secret`. (One caveat carried forward: the guarantee is proven for a single
running container; serializing the kernel↔fs-server forwarding region is a
prerequisite before the management API runs *multiple* containers concurrently.)

### Everything else: no capability, no access

A container is created with an **empty capability space**. Ambient authority does
not exist in ThemeliOS, so holding no capability means being able to do nothing
privileged. The sharpest example is the network: a container that calls
`socket(AF_INET, SOCK_DGRAM, 0)` receives `-EPERM`. It holds no `SOCKET_FACTORY`
capability, and the Linux `socket()` ABI carries no handle by which it could
present one — so the denial is a checked, real Linux errno. The isolation probe
asserts exactly this `-EPERM`.

The result: opening a network socket, signalling another process, or touching
another container's filesystem are all denied at the capability layer, uniformly,
by the same mechanism that governs every other resource in the system.

## Images and layers

Two on-disk formats are understood, both parsed by the dependency-light `oci`
module (`alloc`-only, no `serde`):

- **`docker save` bundles** (Phase 5.4): an outer tar containing `manifest.json`,
  an image config JSON, and one or more **uncompressed** layer tars.
- **Registry images** (Phase 5.6): a Docker Registry HTTP API v2 manifest naming
  a config blob and **gzip-compressed** layer blobs by `sha256:` digest. Every
  blob is **digest-verified before use** — a blob whose contents don't hash to
  the digest the manifest names is rejected (`DigestMismatch`) before it is ever
  parsed or inflated.

Because these parsers consume untrusted image bytes, they fail closed on hostile
input: bounded gzip inflation (a decompression bomb is capped, not allowed to
exhaust the heap), bounded JSON nesting (a deeply-nested manifest cannot overflow
the kernel stack), and no arithmetic panics on adversarial lengths.

## Lifecycle

- **`run`** (shell) launches the demo container: unpack → assemble → load → run,
  then waits for the exit status and prints it.
- **`stop <pid>`** (shell) force-terminates a running container —
  `container::terminate`, the minimal `SIGKILL` equivalent. It verifies the
  target actually *is* a container before tearing it down, so it cannot be used
  to destroy a kernel service. Teardown marks all of the container's tasks dead
  **before** freeing its address space, closing a use-after-free window in which a
  timer tick could otherwise switch into a task whose page tables had just been
  freed.
- **exit status** is captured by `exit_group` and readable by the launcher; this
  is the "wait" primitive the runtime uses.

`kill(2)` from inside a container may only signal *itself* (there is no
cross-process signal capability): a fatal self-signal routes to `exit_group` with
the conventional `128 + signo` status; signalling any other pid returns `-EPERM`.
`wait4(2)` returns `-ECHILD` — there is no parent/child process linkage.

## Testing

The runtime is exercised deterministically, with no external toolchain, by
hand-crafted Linux-ABI probe ELFs run as container entrypoints, each reporting a
result code to a kernel-mapped page:

- `test_container_run` — a full unpack → assemble → load → run → exit round-trip.
- `test_container_isolation` — the enforced-isolation test described above
  (positive read, live `..` clamp, absent-path miss, `socket()` → `-EPERM`).
- `test_oci_unpack`, `test_sha256`, `test_registry_pull`, `test_registry_hardening`
  — the image/registry pipeline, including digest verification and the
  fail-closed hardening for bombs, deep JSON, and bad lengths.

## Deferred

The following are documented, deliberate deferrals — the core pipeline and its
isolation guarantees do not depend on them:

- **A real static-musl image over a live registry.** The runtime has been driven
  with synthetic probe ELFs as `/init` and a mock registry transport; the live
  TCP `Connection` (through a slirp `guestfwd` to a host `registry:2`) and a real
  `busybox` image are a thin, well-scoped follow-up.
- **`exec` into a running container** — a second process sharing an existing
  container's rootfs. Needs process-group semantics not yet required.
- **Real `wait4` and signal-handler delivery.** These need parent/child PID
  tracking and a per-process signal-disposition table respectively; `rt_sigaction`
  is currently an accepted no-op.
- **Moving the image parser into a ring-3 `oci-server`.** The `oci` module (tar/
  JSON/gzip/sha256) currently runs in the kernel for the deterministic tests.
  Relocating it — so untrusted image bytes never parse in ring 0, exactly as the
  [filesystem](./storage.md) and [network](./networking.md) stacks already do —
  is the standing containment hardening for this subsystem.
