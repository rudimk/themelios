# Phase 1 — Kernel Basics (x86_64)

> **Status**: Ready for implementation
> **Created**: 2026-05-22
> **Reviewed by**: Momus (critical review agent) — 2026-05-26 second review, all findings addressed
> **Scope**: Take the kernel from "boot and halt" to a real kernel with memory management, interrupt handling, a preemptive scheduler, interrupt-driven debug shell, and automated test infrastructure.

---

## Requirements Summary

- Physical frame allocator (bitmap-based) using Limine memory map
- Kernel heap allocator (`linked_list_allocator` crate)
- Interrupt handling (GDT, IDT, 8259 PIC, exceptions, timer)
- Timer-driven preemptive scheduler (round-robin, stress-level testing)
- Interrupt-driven serial shell (help, mem, tasks, spawn, kill, peek)
- Automated test infrastructure (`isa-debug-exit`, `cargo xtask test`, GitHub Actions)
- Use Limine's page tables — no custom page table management (deferred to Phase 2)
- x86_64 only — aarch64 deferred to its own phase (new Phase 7)
- Fine-grained commits per logical change
- All code extensively commented

## Key Design Decisions

1. **Use Limine's page tables in Phase 1.** Limine provides identity map + higher-half kernel + HHDM. Phase 2 will build custom page tables for per-process address spaces.
2. **PIT + 8259 legacy PIC** for the timer. LAPIC timer deferred to SMP work (future).
3. **Do NOT reclaim bootloader-reclaimable memory** in Phase 1. Limine's GDT, stack, and page tables live there. Reclamation happens in Phase 2 when we own those structures.
4. **External crates**: `linked_list_allocator` (non-locked `Heap` variant), `spin` (mutex). Roll our own GDT/IDT.
5. **Interrupt-driven serial input** — IRQ4 handler wakes shell task via scheduler.
6. **Heap uses non-locked `Heap` wrapped in our own interrupt-disabling mutex** — prevents deadlock if timer interrupt fires mid-allocation. Do NOT use `LockedHeap` (its internal spinlock is not interrupt-safe).
7. **Global serial writer also uses interrupt-disabling mutex** — same deadlock risk as heap. If timer/scheduler handler prints via `println!` while the serial lock is held, the kernel hangs. Use an interrupt-disabling wrapper around the serial `Mutex`, not plain `spin::Mutex`.
8. **QEMU launched with `-m 256M`** for consistent memory size during development and testing.
9. **All physical memory access via HHDM** — every raw physical address is converted via `hhdm_offset + phys_addr`. This is a conscious architectural decision that must be consistent across all code.
10. **Heap is fixed at 1 MiB** for Phase 1. No dynamic growth. If 1 MiB is exhausted, allocation panics. Growth support deferred to Phase 2.
11. **Guard pages deferred to Phase 2.** Unmapping a page requires modifying page tables, which contradicts the "use Limine's page tables" decision. Stack overflow protection via guard pages will be added when custom page tables are built in Phase 2.

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| GDT CS reload dance (far return) wrong | GPF / triple fault | Test GDT load in isolation before adding IDT |
| TSS descriptor is 16 bytes in long mode | Silent selector misalignment | Explicit 16-byte entry handling in GDT code |
| Double fault without IST stack | Silent triple fault, QEMU reboots | Allocate static IST stack, wire into TSS before enabling interrupts |
| Bitmap allocator needs memory before allocator exists | Chicken-and-egg | Carve bitmap from first usable region in Limine memory map |
| Timer interrupt during context switch | Register corruption | Disable interrupts during context switch |
| `spin::Mutex` deadlock if interrupt handler allocates | Kernel hang | Use non-locked `Heap` with custom interrupt-disabling mutex |
| Limine HHDM offset varies per boot | Wrong physical-to-virtual translation | Request HHDM offset at boot, store globally |
| Limine stack in bootloader-reclaimable memory | Stack corruption if reclaimed | Never reclaim bootloader memory in Phase 1 |
| `naked_asm!` only accepts `sym`/`const` operands | Can't pass register operands | Design context switch to use calling convention registers (rdi/rsi) |
| Memory map entries may not be page-aligned | Bitmap off-by-one | Round start up, round end down to 4 KiB boundaries |
| 8259 PIC spurious IRQs (IRQ7/IRQ15) | PIC state corruption if EOI sent | Read ISR register to detect and discard spurious interrupts |
| Task stack overflow corrupts adjacent memory | Silent data corruption | Deferred to Phase 2 — guard pages require page table modification. In Phase 1, allocate extra padding between stacks to reduce (not eliminate) risk |
| Serial writer `spin::Mutex` deadlock on interrupt | Kernel hang if timer handler prints while serial lock held | Use interrupt-disabling mutex wrapper for serial writer (same pattern as heap) |
| Task exit without trampoline | Task returns to garbage address, crashes | Task entry wrapped in trampoline that calls scheduler to mark task Dead |
| `peek` command on unmapped address | Page fault halts kernel | Validate address against known mapped ranges before dereferencing |

---

## Pre-work: Milestone Documentation Updates — ✓ DONE

**Already completed.** Commits `969f9a9` and `d72f394` performed the aarch64 renumbering and CLAUDE.md updates. No action needed — skip to Sub-phase 1.0.

---

## Sub-phases

### Sub-phase 1.0 — Global serial output and println! macros ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `e5c6277`, `e83d2d6`, `14ef38e`, `bbcb820`

**Summary**:
- Added `spin` crate (v0.9, spin_mutex feature) for no_std spinlock primitives
- Added `cli()`, `sti()`, `interrupts_enabled()` wrappers in `cpu.rs`
- Created `kernel/src/sync.rs` with `InterruptMutex<T>` — a reusable spinlock wrapper that disables interrupts while held, with correct drop ordering (unlock before re-enabling interrupts) and nested locking support
- Added global `SERIAL_WRITER` behind `InterruptMutex<Option<SerialPort>>` with `init()`, `_print()`, and `print!`/`println!` macros in `serial.rs`
- Switched `kmain` and panic handler to use `println!` macros, removed stale `use core::fmt::Write`
- Panic handler now calls `cli` before printing
- `hcf()` now disables interrupts before the halt loop; doc comment fixed to match
- Zero clippy warnings, kernel boots with identical output to Phase 0

**All acceptance criteria verified** ✓

---

### Sub-phase 1.1 — GDT and TSS ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `2c9ec07`, `a87d99f`, `9ef9525`

**Summary**:
- Added `GdtRegister` struct, `lgdt()`, `ltr()`, `reload_segments()` (CS far-return dance + DS/ES/SS mov) to `cpu.rs`
- Created `gdt.rs` with: GDT layout (null + kernel code 0x08 + kernel data 0x10 + TSS 0x18), packed `Tss` struct (104 bytes), 16-byte TSS descriptor builder, 20 KiB double-fault IST stack, `DOUBLE_FAULT_IST_INDEX` constant for IDT use
- `init()` fills TSS IST[0], builds GDT entries, loads via `lgdt`, reloads all segment registers, loads TSS via `ltr`
- Kernel boots, prints "GDT loaded", no triple faults or GPFs
- Clippy clean (one `identity_op` suppressed on `1 * 8` for readability)

**All acceptance criteria verified** ✓

---

### Sub-phase 1.2 — IDT and exception handlers ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `91f0f38`, `a57f67a`, `0abc22c`

**Summary**:
- Added `IdtRegister`, `lidt()`, `int3()` to `cpu.rs`
- Created `idt.rs` with: 256-entry IDT, `IdtEntry` struct (16 bytes, `repr(C)`), `InterruptStackFrame` struct matching exact stack layout after all pushes
- ISR stub macros (`isr_stub_no_error!` / `isr_stub_error!`) using `#[unsafe(naked)]` + `naked_asm!` — stubs push dummy/real error code + vector number, then `jmp isr_common`
- `isr_common`: naked function that saves all 15 GPRs, calls Rust handler via System V ABI, restores, `add rsp, 16`, `iretq`
- Rust `exception_handler` prints vector name, error code, RIP, RSP, CS, RFLAGS; page fault also prints CR2 + decoded error flags
- Breakpoint (#BP) returns and continues; all others halt (fatal in kernel)
- Double fault (#DF, vector 8) wired to IST1 for dedicated stack
- INT3 breakpoint test in kmain: fires, prints diagnostics, resumes — IDT pipeline verified
- Zero clippy warnings

**All acceptance criteria verified** ✓

---

### Sub-phase 1.3 — 8259 PIC and timer interrupt ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `85875bd`, `d6cfc70`, `60ee0bf`, `d6b32c4`

**Summary**:
- Created `pic.rs`: full 8259 PIC driver with ICW1-4 initialization sequence, IRQ remap to vectors 32-47, I/O wait delays (port 0x80), EOI handling (master-only for IRQ0-7, both PICs for IRQ8-15), mask/unmask with automatic IRQ2 cascade unmasking for slave IRQs, spurious IRQ detection via ISR register reads for IRQ7/IRQ15
- Created `pit.rs`: 8254 PIT channel 0 configured in mode 3 (square wave) with divisor 11931 for ~100 Hz timer
- Updated `idt.rs`: added 16 IRQ stub functions (vectors 32-47) using existing `isr_stub_no_error!` macro, global `AtomicU64` tick counter, unified interrupt dispatcher routing vectors 32-47 to `irq_handler()` with spurious check → device handler → EOI, periodic tick printing every 100 ticks (~1 second)
- Updated `main.rs`: boot sequence now calls `pic::init()` → `pit::init()` → `pic::unmask(0)` → `sti()`, idle loop uses `hlt` (not `cli+hlt`) so timer interrupts keep firing
- Verified in QEMU: timer ticks print at ~1 second intervals, no triple-faults, no spurious IRQ spam, clean halt/resume cycle

**All acceptance criteria verified** ✓

---

### Sub-phase 1.4 — Physical frame allocator ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `f6afb55`, `f1a394e`, `c063dcf`, `80f6ef9`, `cf1452a`, `936da52`

**Summary**:
- Added Limine `MemoryMapRequest`, `HhdmRequest`, `ExecutableAddressRequest` to main.rs with documented request statics in `.requests` section
- Added `-m 256M` to QEMU invocation in xtask for consistent 256 MiB guest memory
- Created `mm/addr.rs`: type-safe `PhysAddr` and `VirtAddr` wrappers with HHDM-based conversion (`to_virt()`, `to_phys()`, `as_ptr()`, `as_mut_ptr()`), plus `align_up`/`align_down` utilities
- Rewrote `mm/mod.rs`: HHDM offset global (`AtomicU64`), `PAGE_SIZE` constant, `init_hhdm()` and `hhdm_offset()` functions
- Created `mm/frame.rs`: `BitmapFrameAllocator` with bootstrap sequence — finds highest phys addr, sizes bitmap, carves from first USABLE region, zeros (all allocated), marks USABLE frames free, re-marks bitmap frames allocated. Asserts kernel phys base not in USABLE region. Linear-scan `allocate_frame()`, `deallocate_frame()` with double-free protection
- Wrapped allocator in `InterruptMutex<Option<BitmapFrameAllocator>>` with public free functions
- Wired into kmain: prints full 16-entry memory map with types/sizes, shows 56099 free frames (~219 MiB usable), alloc/dealloc smoke test passes
- Kernel frames correctly classified as `Executable & Modules` (verified by assertion)
- Timer continues ticking after frame allocator init — no corruption or hangs

**All acceptance criteria verified** ✓

---

### Sub-phase 1.5 — Kernel heap allocator ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `4d5105a`, `5b50f1b`, `044de9f`, `2d71b63`

**Summary**:
- Added `linked_list_allocator = { version = "0.10", default-features = false }` to Cargo.toml — non-locked `Heap` variant, wrapped in our own InterruptMutex
- Updated xtask `-Zbuild-std` from `core` to `core,alloc` in both `cmd_build()` and `cmd_docs()`, extracted to `BUILD_STD` / `BUILD_STD_FEATURES` constants to prevent divergence
- Created `mm/heap.rs`: `KernelHeap` wrapper implementing `GlobalAlloc` via `InterruptMutex<Heap>`, `#[global_allocator]` static, `init()` function that allocates 256 contiguous physical frames (1 MiB) and initializes the linked-list heap with the HHDM-mapped virtual region
- Added `allocate_contiguous_frames(count)` to frame allocator for contiguous physical region allocation
- Added `extern crate alloc;` to main.rs, heap init called after frame allocator, smoke test verifies Vec, Box, and String all allocate correctly
- `#[alloc_error_handler]` is no longer available on nightly — the default behavior (panic on OOM) is used, which matches our requirements
- `cargo xtask docs` verified to still build correctly with `core,alloc`
- Heap stats at boot: 1024 KiB total, 104 bytes used after smoke test, timer continues ticking with no deadlocks

**All acceptance criteria verified** ✓

---

### Sub-phase 1.6 — Preemptive round-robin scheduler ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `c9e5da1`

**Summary**:
- Created `sched/task.rs`: Task struct (ID, name, state, context, stack info), TaskId type, TaskState enum (Ready/Running/Blocked/Dead), TaskContext (rsp only), stack allocation constants (4 pages usable + 1 page padding)
- Created `sched/context.rs`: `switch_context` naked_asm (save/restore rbx, rbp, r12-r15 via push/pop, rsp swap via mov), `task_bootstrap` naked fn (sti → call r12 → call task_exit), `task_exit` trampoline
- Rewrote `sched/mod.rs`: Scheduler struct with tasks Vec + VecDeque ready queue, `init()` creates bootstrap (task 0, uses boot stack) + idle task (halt loop, never queued), `spawn()` allocates 5 contiguous frames + sets up initial stack frame with entry fn in r12 slot, `schedule()` does round-robin pick + releases lock before switch_context (raw pointers safe with interrupts disabled on single core), `yield_now()` for cooperative yielding, `exit_current_task()` marks Dead + reschedules, dead task cleanup frees stack frames
- Updated `idt.rs`: IRQ0 sends EOI before calling schedule() (prevents PIC from blocking timer to switched-to task), gated by `sched::is_initialized()`
- Updated `main.rs`: scheduler init after PIC/PIT, spawns 24 stress test tasks (print ID + counter, yield between iterations)
- Verified in QEMU: 24 tasks interleave output correctly, all complete 5 iterations, tasks exit cleanly via trampoline, timer continues ticking after all tasks finish, no triple faults or corruption

**All acceptance criteria verified** ✓

**Why before shell**: The shell runs as a task managed by the scheduler. The scheduler is also the final "big" kernel subsystem for Phase 1.

**Deliverables**:
- `Task` struct: ID, state (Ready/Running/Blocked/Dead), name, saved register context, kernel stack pointer, entry point
- Task states: `Ready`, `Running`, `Blocked`, `Dead`
- Per-task kernel stack allocated from frame allocator (4 pages = 16 KiB per task)
- **No guard pages in Phase 1** — deferred to Phase 2 (requires page table modification). Allocate one extra frame of padding below each stack as a buffer, but this is NOT a true guard page.
- Context switch via `naked_asm!`:
  - Save/restore: rbx, rbp, r12-r15, rsp, rflags (rflags via pushfq/popfq — needed to preserve interrupt enable state)
  - Function signature: `extern "C" fn switch_context(old: *mut TaskContext, new: *const TaskContext)`
  - Parameters arrive in rdi/rsi per System V ABI (no register operands in `naked_asm!`)
  - Interrupts disabled for the duration of the switch
- **Task exit trampoline**: each new task's initial stack is set up with a return address pointing to a `task_exit_trampoline()` function. When a task's entry function returns, it returns into the trampoline, which calls the scheduler to mark the task as Dead and switch to the next task. This avoids returning to a garbage address.
- Round-robin scheduler: ready queue (VecDeque), pick next task, switch context
- Timer interrupt (IRQ0) calls the scheduler for preemption
- Task creation: `spawn(name, entry_fn) -> TaskId`
- Idle task: runs `hlt` in a loop when no other tasks are ready
- Initial kernel stack set up to look like a saved context (first-task bootstrap)
- Stack alignment: 16-byte aligned at function entry per System V ABI
- **Stress test**: spawn 20+ tasks that each print their ID and a counter, verify interleaving on serial output

**Files**:
- `kernel/src/sched/mod.rs` — rewrite: scheduler logic, ready queue, task switching, spawn, exit
- `kernel/src/sched/task.rs` — new file: Task struct, TaskId, task states, TaskContext
- `kernel/src/sched/context.rs` — new file: context switch assembly (`naked_asm!`), task exit trampoline
- `kernel/src/arch/x86_64/idt.rs` — update IRQ0 handler to call scheduler
- `kernel/src/main.rs` — init scheduler, spawn initial tasks

**Commits**:
1. Define Task struct, TaskId, TaskContext, and task states
2. Implement per-task kernel stack allocation (with padding frame, no guard page)
3. Implement context switch with `naked_asm!` (save/restore rbx, rbp, r12-r15, rsp, rflags)
4. Implement task exit trampoline (return address on initial stack)
5. Implement round-robin ready queue and scheduler loop
6. Wire timer interrupt to scheduler for preemption
7. Implement task spawn (with initial stack setup) and idle task
8. Stress test: spawn 20+ tasks, verify round-robin interleaving on serial

**Acceptance criteria**:
- Multiple tasks run concurrently, preempted by timer
- Serial output shows interleaved output from different tasks
- Tasks can be spawned dynamically
- Tasks that return are cleaned up via trampoline (stack freed, removed from queue)
- Idle task runs when no other tasks are ready
- No register corruption, stack corruption, or triple faults
- rflags preserved across context switches (interrupt state correct)
- 20+ tasks run simultaneously without issues

---

### Sub-phase 1.7 — Interrupt-driven serial shell ✓ DONE

> **Completed**: 2026-05-26
> **Commits**: `09a5ecb`

**Summary**:
- Created `shell/mod.rs`: shell task entry with line editing (backspace/echo), command parser, dispatch table, `init()` enables IRQ4 receive interrupts + unmasks IRQ4 in PIC + spawns shell task
- Created `shell/input.rs`: 256-byte ring buffer with `push_byte()`/`pop_byte()`, shell task ID stored in AtomicUsize for IRQ4 handler to wake via `sched::wake_task()`
- Created `shell/commands.rs`: 6 commands — `help` (list commands), `mem` (frame/heap stats), `tasks` (task table with ID/state/name/current marker), `spawn` (test task with 10 iterations), `kill` (marks Dead, cleaned up by scheduler), `peek` (hex+ASCII dump with address validation against HHDM and kernel image ranges)
- Updated `serial.rs`: added `enable_receive_interrupt()`, `receive_byte()`, `data_ready()` for IRQ4-driven input
- Updated `idt.rs`: IRQ4 handler drains UART receive buffer into ring buffer, wakes shell task
- Updated `sched/mod.rs`: added `task_list()` (copies task info outside lock), `kill_task()` (marks Dead, removes from ready queue), `block_current_task()` (sets Blocked, yields), `wake_task()` (sets Ready, enqueues)
- Updated `main.rs`: replaced stress test with `shell::init()`, added `mod shell`
- Verified in QEMU: all 6 commands work correctly, shell blocks until input (no busy-wait), kill stops running task mid-execution, peek on invalid address prints error (no page fault), echo and backspace work, timer continues running alongside shell

**All acceptance criteria verified** ✓

**Why now**: All infrastructure (interrupts, allocator, scheduler) is in place. The shell provides interactive debugging for everything built so far and all future phases.

**Deliverables**:
- Serial receive interrupt: enable IRQ4 in PIC, configure COM1 to generate interrupts on data received
- Ring buffer for received characters (e.g., 256 bytes)
- Shell task: blocked until input arrives, woken by IRQ4 handler
- Line-editing: backspace support, enter to submit
- Command parser: split input into command + arguments
- Built-in commands:
  - `help` — list available commands
  - `mem` — print free frames, heap usage, total/used/free memory
  - `tasks` — list all tasks (ID, state, name)
  - `spawn <name>` — spawn a test task (e.g., a counter-printing task)
  - `kill <id>` — mark a task as Dead, clean up on next schedule
  - `peek <addr> [count]` — hex dump `count` bytes (default 64) starting at virtual address `addr`. **Validates address**: checks that the address falls within known mapped ranges (HHDM, kernel image, heap) before dereferencing. Prints an error for unmapped addresses instead of page-faulting.

**Files**:
- `kernel/src/arch/x86_64/serial.rs` — add receive interrupt enable, data-ready read
- `kernel/src/shell/mod.rs` — new file: shell task, command parser, command dispatch
- `kernel/src/shell/commands.rs` — new file: individual command implementations
- `kernel/src/shell/input.rs` — new file: ring buffer, line editing
- `kernel/src/main.rs` — add `mod shell;`, spawn shell task after scheduler init
- `kernel/src/arch/x86_64/idt.rs` — add IRQ4 handler
- `kernel/src/arch/x86_64/pic.rs` — unmask IRQ4

**Commits**:
1. Enable COM1 receive interrupts and add IRQ4 handler
2. Implement input ring buffer
3. Implement shell task with line-editing and command parser
4. Implement `help` and `mem` commands
5. Implement `tasks` and `spawn` commands
6. Implement `kill` command
7. Implement `peek` command with address validation
8. Wire shell task into boot sequence

**Acceptance criteria**:
- Typing in the serial console (QEMU terminal) produces echoed characters
- `help` lists all commands
- `mem` shows accurate memory statistics
- `tasks` shows all running tasks with correct states
- `spawn` creates a new task visible in `tasks` output
- `kill` terminates a task
- `peek` shows hex dump of valid kernel memory
- `peek` on an unmapped address prints an error message (does NOT page fault)
- Shell task does not busy-wait — sleeps until IRQ4 fires

---

### Sub-phase 1.8 — Automated test infrastructure

**Why last**: Tests verify all the subsystems built above. Having them last means we can write comprehensive tests for everything.

**Deliverables**:
- QEMU `isa-debug-exit` device support:
  - I/O port: `0xf4`, iosize: `0x04`
  - Exit code mapping: `(value << 1) | 1`. Writing `0x00` → QEMU exits with code `1`. Writing `0x01` → QEMU exits with code `3`.
  - Convention: write `0x01` for test success (QEMU exit code `3`), write `0x00` for test failure (QEMU exit code `1`)
- `cargo xtask test` command:
  - Builds the kernel with `--features test` flag
  - Boots QEMU with `-device isa-debug-exit,iobase=0xf4,iosize=0x04` and `-display none -serial stdio`
  - Interprets QEMU exit code `3` as success, anything else (including `1` and timeout) as failure
  - Timeout: 30 seconds, kills hung kernels
  - Prints captured serial output on failure
- Test kernel mode: `#[cfg(feature = "test")]` branch in kmain runs test functions instead of spawning the shell
- Test functions:
  - `test_boot` — kernel boots and reaches init complete (smoke test)
  - `test_frame_allocator` — allocate/deallocate frames, verify no overlap, verify free count changes
  - `test_heap` — allocate `Vec`, `Box`, `String`, verify values are correct
  - `test_scheduler` — spawn tasks, verify they all run to completion (via shared atomic counter)
  - `test_interrupts` — verify tick counter increments after a short busy-wait (timer working)
- Test output: each test prints `[PASS] test_name` or `[FAIL] test_name: reason` to serial
- GitHub Actions workflow (`.github/workflows/test.yml`):
  - Runs on: push to main, pull requests
  - Runner: `ubuntu-latest`
  - Steps: checkout, install Rust (via `rust-toolchain.toml`), `apt-get install qemu-system-x86 xorriso`, `cargo xtask test`
  - Cache: `~/.cargo` and `target/limine` (Limine git clone is expensive)
  - Timeout: 10 minutes for the whole job

**Files**:
- `kernel/Cargo.toml` — add `test` feature (empty feature, just a cfg flag)
- `kernel/src/test_runner.rs` — new file: test harness (run tests, report results, exit QEMU)
- `kernel/src/main.rs` — `#[cfg(feature = "test")]` branch in kmain
- `kernel/src/arch/x86_64/cpu.rs` — add `exit_qemu(code: u8)` function (write to port 0xf4)
- `xtask/src/main.rs` — implement `cmd_test` (build with --features test, QEMU with isa-debug-exit, timeout, exit code mapping)
- `.github/workflows/test.yml` — new file: CI workflow

**Commits**:
1. Add `exit_qemu()` function and `isa-debug-exit` port write to cpu.rs
2. Add `test` feature flag to kernel Cargo.toml
3. Implement test runner harness (run functions, report pass/fail, exit QEMU)
4. Add boot smoke test
5. Add frame allocator tests
6. Add heap allocator tests
7. Add scheduler tests (spawn tasks, verify completion via atomic counter)
8. Add interrupt/timer tests
9. Implement `cargo xtask test` in xtask (build with --features test, QEMU flags, timeout, exit code)
10. Add GitHub Actions CI workflow (install deps, cache Limine, run tests)

**Acceptance criteria**:
- `cargo xtask test` runs all tests in QEMU and reports pass/fail
- Each test prints `[PASS]` or `[FAIL]` with test name to serial
- Successful run: QEMU exits with code 3, xtask reports success
- Failed test: QEMU exits with code 1, xtask reports failure and prints serial output
- Hung kernel: killed after 30 seconds, xtask reports timeout
- GitHub Actions workflow runs on push to main and on PRs
- All tests pass in CI
- `cargo xtask docs` still works (not broken by test feature)

---

## Dependency Graph

```
Pre-work (milestone docs update)
    │
    ▼
Sub-phase 1.0 (println! macros)
    │
    ▼
Sub-phase 1.1 (GDT + TSS)
    │
    ▼
Sub-phase 1.2 (IDT + exceptions)
    │
    ▼
Sub-phase 1.3 (PIC + timer)
    │
    ▼
Sub-phase 1.4 (frame allocator)
    │
    ▼
Sub-phase 1.5 (kernel heap)
    │
    ▼
Sub-phase 1.6 (scheduler)
    │
    ▼
Sub-phase 1.7 (serial shell)
    │
    ▼
Sub-phase 1.8 (test infrastructure)
```

Strictly linear — each sub-phase depends on all previous ones.

---

## Total Estimated Commits

| Sub-phase | Commits |
|-----------|---------|
| Pre-work — milestone docs | 3 |
| 1.0 — println! macros | 4 |
| 1.1 — GDT + TSS | 4 |
| 1.2 — IDT + exceptions | 5 |
| 1.3 — PIC + timer | 4 |
| 1.4 — Frame allocator | 6 |
| 1.5 — Kernel heap | 5 |
| 1.6 — Scheduler | 8 |
| 1.7 — Serial shell | 8 |
| 1.8 — Test infrastructure | 10 |
| **Total** | **~57 commits** |

---

## Verification Checklist (Phase 1 Complete)

- [ ] Kernel boots on QEMU x86_64 with no warnings or errors
- [ ] `cargo clippy` passes with no warnings
- [ ] GDT and IDT loaded, all CPU exceptions handled with informative output
- [ ] Timer interrupt fires at ~100 Hz, spurious IRQs handled
- [ ] Physical frame allocator tracks all usable memory correctly
- [ ] Kernel heap supports `Vec`, `Box`, `String` without deadlocks
- [ ] 20+ tasks run concurrently with round-robin preemption
- [ ] Task exit trampoline works (tasks that return are cleaned up)
- [ ] Interactive serial shell with all 7 commands working (help, mem, tasks, spawn, kill, peek)
- [ ] `peek` validates addresses (no page fault on bad input)
- [ ] `cargo xtask test` passes all automated tests
- [ ] `cargo xtask docs` still builds correctly
- [ ] GitHub Actions CI workflow runs tests on push and PRs
- [ ] All code extensively commented
- [ ] Documentation updated (milestones, CLAUDE.md) — aarch64 as Phase 7, phases renumbered
- [ ] Phase 1 marked as "In progress" → "Complete" in milestones
