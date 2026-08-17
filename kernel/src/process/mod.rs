//! # Process abstraction
//!
//! A process is the unit of isolation in ThemeliOS. It bundles:
//! - An **address space** (PML4 page table) for memory isolation
//! - A **CSpace** (capability space) for authority control
//! - One or more **tasks** (kernel threads) that execute within the process
//! - **Metadata**: PID, name, state
//!
//! ## Kernel process (PID 0)
//!
//! PID 0 is special: it represents the kernel itself. All boot-time tasks
//! (main/bootstrap, idle, shell) belong to PID 0. The kernel process has
//! no user-mode address space (it runs entirely in ring 0) and no CSpace
//! (it has ambient authority over all kernel resources). The `address_space`
//! and `cspace` fields are `None` for PID 0.
//!
//! ## Process lifecycle
//!
//! 1. **Creation**: `create_process()` allocates a new address space
//!    (via `AddressSpace::new_user()`), creates an empty CSpace, and inserts
//!    a Process capability into the parent's CSpace.
//! 2. **Running**: tasks within the process are scheduled by the normal
//!    round-robin scheduler. The scheduler updates CR3 when switching between
//!    tasks in different processes.
//! 3. **Destruction**: `destroy_process()` kills all tasks, destroys the
//!    address space (freeing page table frames), drops the CSpace, and
//!    marks the process table slot as `None`.

extern crate alloc;

pub mod pid;
pub mod init;
pub mod server;
pub mod embedded;

use alloc::string::String;
use alloc::vec::Vec;
use crate::println;
use crate::sync::InterruptMutex;
use crate::mm::page_table::{self, AddressSpace};
use crate::cap::cspace::CSpace;
use crate::cap::{Capability, CapType, CapRights, CapHandle};
use crate::sched::task::TaskId;

pub use pid::ProcessId;

/// The possible states a process can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// The process is active — it has at least one runnable task.
    Running,

    /// The process has exited or been destroyed. Its resources have been
    /// freed and its slot in the process table is `None`.
    Exited,
}

/// A process — the unit of isolation in ThemeliOS.
///
/// Each process owns an address space and a capability space. Tasks within
/// the process share both. In Phase 2, each process has exactly one task
/// (multi-threading within a process comes later).
pub struct Process {
    /// Unique process identifier (index into the process table).
    pub pid: ProcessId,

    /// Human-readable name for debug output and the `procs` shell command.
    pub name: String,

    /// Per-process address space (PML4). `None` for the kernel process (PID 0),
    /// which runs on the kernel's own page tables.
    pub address_space: Option<AddressSpace>,

    /// Per-process capability space. `None` for the kernel process (PID 0),
    /// which has ambient authority (no capability restrictions).
    pub cspace: Option<CSpace>,

    /// Task IDs belonging to this process. When a task is spawned within
    /// this process, its ID is added here. When a task exits, it's removed.
    pub tasks: Vec<TaskId>,

    /// Current process state.
    pub state: ProcessState,

    /// Ring-3 entry point for a process launched from a loaded image (an ELF, as
    /// of Phase 5.0): `(entry_rip, entry_rsp)`. The generic ELF trampoline reads
    /// this to `iretq` into the program — unlike the fixed-address server
    /// trampoline, an ELF's entry and initial `%rsp` vary per image. `None` for
    /// the kernel process and flat-binary servers (which use the fixed layout).
    pub user_entry: Option<(u64, u64)>,

    /// Syscall personality (Phase 5.1). A `Linux` process has its `syscall`s
    /// routed through the Linux dispatch table (Linux numbers/ABI); a `Native`
    /// process uses ThemeliOS's own syscalls. The two number spaces collide
    /// (Linux `write`=1 vs native `SYS_SEND`=1), so the personality — not the
    /// number — decides the table.
    pub personality: Personality,

    /// Current program break for the Linux `brk` syscall (Phase 5.1). The heap
    /// grows upward from [`LINUX_BRK_BASE`]; 0 until the first `brk`.
    pub brk: u64,

    /// Next free virtual address for anonymous Linux `mmap` (Phase 5.1). A simple
    /// upward bump allocator from [`LINUX_MMAP_BASE`]; 0 until first use.
    pub mmap_next: u64,

    /// The VFS mount id serving this Linux process's root filesystem (Phase 5.2).
    /// Linux syscalls carry paths, not capabilities — so a container's isolation
    /// is that the kernel only ever resolves its paths against *this one* mount,
    /// and path resolution clamps `..` at the root. `None` for non-container
    /// processes.
    pub rootfs_mount: Option<u64>,

    /// Rootfs **base subdirectory** on `rootfs_mount` that this process is confined
    /// to (Phase 6.1b). `Some("/c/<id>")` means the container's `/` maps to
    /// `/c/<id>` on the shared mount: every path syscall prepends this base to the
    /// already-`..`-clamped container-relative path, so the container can name
    /// nothing outside its subtree (not sibling containers, not the mount root).
    /// `None` = rooted at the mount root (non-container processes, and the
    /// single-rootfs container path used before confinement).
    pub rootfs_base: Option<alloc::string::String>,

    /// Current working directory for cwd-relative Linux path resolution (Phase
    /// 5.2). An absolute, already-normalized **container-relative** rootfs path
    /// (the base is *not* stored here, so `getcwd` never leaks it); defaults to "/".
    pub cwd: alloc::string::String,

    /// Per-process Linux file-descriptor table (Phase 5.2). Integer fds ≥ 3 index
    /// here (0/1/2 are stdio, handled directly); a `Some` entry maps an fd to an
    /// open file on [`rootfs_mount`]. Linux fds are integers, not capabilities.
    pub fd_table: Vec<Option<LinuxFd>>,

    /// Exit status once the process has terminated (Phase 5.5). Set by
    /// `exit_group` (the whole-process exit); read by a waiter (`run`) to report
    /// the container's exit code. `None` while running.
    pub exit_code: Option<u64>,
}

/// An open Linux file descriptor (Phase 5.2): the mount + server-side fd it
/// resolves to, the current read/write offset, and whether it is a directory.
#[derive(Clone, Copy)]
pub struct LinuxFd {
    /// The VFS mount serving this fd (always the process's rootfs mount for now).
    pub mount: u64,
    /// The filesystem server's own fd handle (from `fs::kopen`).
    pub server_fd: u64,
    /// Current file offset for `read`/`getdents64`/`lseek`.
    pub offset: u64,
    /// File size at open time (for `SEEK_END` / `fstat`).
    pub size: u64,
    /// True if this fd refers to a directory (for `getdents64`/`fstat`).
    pub is_dir: bool,
}

/// A process's syscall personality — which ABI its `syscall`s speak.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Personality {
    /// Native ThemeliOS syscalls (servers, the ELF smoke test).
    Native,
    /// The Linux syscall ABI (OCI container binaries, Phase 5.1+).
    Linux,
}

/// Base of the Linux `brk` heap region (well clear of ELF segments at ~2 MiB and
/// the stack near 0x7fff_…). Grows upward.
pub const LINUX_BRK_BASE: u64 = 0x0000_0002_0000_0000; // 8 GiB
/// Base of the anonymous Linux `mmap` region. Grows upward, below the stack.
pub const LINUX_MMAP_BASE: u64 = 0x0000_0001_0000_0000; // 4 GiB

/// The global process table, protected by an interrupt-disabling mutex.
///
/// Indexed by PID (ProcessId::as_usize()). Slots are `None` for destroyed
/// processes. The table only grows — destroyed slots are never reused in
/// Phase 2 (simplicity over memory efficiency for now).
static PROCESS_TABLE: InterruptMutex<ProcessTable> =
    InterruptMutex::new(ProcessTable::new_empty());

/// Internal state of the process table.
struct ProcessTable {
    /// Process slots indexed by PID.
    processes: Vec<Option<Process>>,

    /// Next PID to assign. Monotonically increasing.
    next_pid: usize,
}

impl ProcessTable {
    /// Create an empty process table. Used for the static initializer.
    const fn new_empty() -> Self {
        Self {
            processes: Vec::new(),
            next_pid: 0,
        }
    }
}

// ========================================================================
// Public API
// ========================================================================

/// Initialize the process table and create the kernel process (PID 0).
///
/// The kernel process is the special ring-0 process that owns all existing
/// boot-time tasks. It has no user address space and no CSpace — the kernel
/// has ambient authority over all resources.
///
/// Must be called after the scheduler is initialized (so we know what tasks
/// exist) and before any process-related operations.
pub fn init() {
    let kernel_process = Process {
        pid: ProcessId::KERNEL,
        name: String::from("kernel"),
        address_space: None,
        cspace: None,
        tasks: Vec::new(), // Tasks will be assigned via assign_task_to_kernel()
        state: ProcessState::Running,
        user_entry: None,
        personality: Personality::Native,
        brk: 0,
        mmap_next: 0,
        rootfs_mount: None,
        rootfs_base: None,
        cwd: String::from("/"),
        fd_table: Vec::new(),
        exit_code: None,
    };

    let mut table = PROCESS_TABLE.lock();
    table.processes.push(Some(kernel_process));
    table.next_pid = 1;

    println!("Process table initialized (kernel process PID 0)");
}

/// Assign an existing task to the kernel process (PID 0).
///
/// Called during boot to retroactively associate existing scheduler tasks
/// (bootstrap, idle, shell) with the kernel process. These tasks were
/// created before the process system existed.
pub fn assign_task_to_kernel(task_id: TaskId) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(ref mut kernel)) = table.processes.get_mut(0) {
        kernel.tasks.push(task_id);
    }
}

/// Create a new user process.
///
/// Allocates a fresh address space (with shared kernel mappings), creates
/// an empty CSpace, and registers the process in the global table. If a
/// parent CSpace is provided, inserts a Process capability into it so the
/// parent can manage the new process.
///
/// Returns `(ProcessId, Option<CapHandle>)` — the new process's PID and
/// the capability handle in the parent's CSpace (if a parent was given).
pub fn create_process(name: &str, parent_cspace: Option<&mut CSpace>) -> (ProcessId, Option<CapHandle>) {
    let kernel_as = page_table::kernel_address_space();
    let user_as = AddressSpace::new_user(&kernel_as);
    // Don't drop the kernel AddressSpace handle (it's a global reference)
    core::mem::forget(kernel_as);

    let cspace = CSpace::new();

    let (pid, process) = {
        let mut table = PROCESS_TABLE.lock();
        let pid_val = table.next_pid;
        table.next_pid += 1;
        let pid = ProcessId::new(pid_val);

        let process = Process {
            pid,
            name: String::from(name),
            address_space: Some(user_as),
            cspace: Some(cspace),
            tasks: Vec::new(),
            state: ProcessState::Running,
            user_entry: None,
            personality: Personality::Native,
            brk: 0,
            mmap_next: 0,
            rootfs_mount: None,
            rootfs_base: None,
            cwd: String::from("/"),
            fd_table: Vec::new(),
            exit_code: None,
        };

        table.processes.push(Some(process));
        // We need to return just the pid — can't return a reference out of the lock
        (pid, ())
    };

    let _ = process; // unused binding from the block

    // If a parent CSpace was provided, insert a Process capability into it.
    let cap_handle = parent_cspace.map(|parent_cs| {
        let cap = Capability {
            cap_type: CapType::Process { pid: pid.as_usize() },
            rights: CapRights::ALL,
            parent: None,
        };
        parent_cs.insert(cap).expect("create_process: parent CSpace full")
    });

    println!("[process] Created process {} (\"{}\")", pid, name);

    (pid, cap_handle)
}

/// Record a process's ring-3 entry point `(entry_rip, entry_rsp)`, as computed by
/// the ELF loader (Phase 5.0). The generic ELF trampoline reads it back via
/// [`user_entry`] to `iretq` into the loaded program.
pub fn set_user_entry(pid: ProcessId, entry_rip: u64, entry_rsp: u64) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.user_entry = Some((entry_rip, entry_rsp));
    }
}

/// Read back a process's ring-3 entry point, if one was recorded by the loader.
pub fn user_entry(pid: ProcessId) -> Option<(u64, u64)> {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .and_then(|proc| proc.user_entry)
}

/// Record a process's exit status and mark it `Exited` (Phase 5.5). Called by
/// `exit_group`; a waiter (`run`) reads it back via [`exit_status`].
pub fn set_exit_code(pid: ProcessId, code: u64) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.exit_code = Some(code);
        proc.state = ProcessState::Exited;
    }
}

/// The exit status of a process, or `None` if it is still running / not found.
pub fn exit_status(pid: ProcessId) -> Option<u64> {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .and_then(|proc| proc.exit_code)
}

/// The task ids currently belonging to a process (Phase 5.3, for `exit_group`).
pub fn task_ids(pid: ProcessId) -> Vec<TaskId> {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .map(|proc| proc.tasks.clone())
        .unwrap_or_default()
}

/// The syscall personality of a process (defaults to `Native` for any process
/// not found). Read on every syscall to pick the native vs Linux dispatch table.
pub fn personality(pid: ProcessId) -> Personality {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .map(|proc| proc.personality)
        .unwrap_or(Personality::Native)
}

/// Set a process's syscall personality (e.g. to `Linux` when launching a
/// container binary).
pub fn set_personality(pid: ProcessId, personality: Personality) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.personality = personality;
    }
}

/// Read a process's current program break (`brk`), or 0 if unset/not found.
pub fn get_brk(pid: ProcessId) -> u64 {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .map(|proc| proc.brk)
        .unwrap_or(0)
}

/// Set a process's current program break.
pub fn set_brk(pid: ProcessId, brk: u64) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.brk = brk;
    }
}

/// Read a process's next anonymous-`mmap` address, or 0 if unset/not found.
pub fn get_mmap_next(pid: ProcessId) -> u64 {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .map(|proc| proc.mmap_next)
        .unwrap_or(0)
}

/// Set a process's next anonymous-`mmap` address (the bump-allocator cursor).
pub fn set_mmap_next(pid: ProcessId, next: u64) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.mmap_next = next;
    }
}

// --- Linux rootfs / cwd / fd table (Phase 5.2) ---

/// Set the VFS mount serving a Linux process's root filesystem.
pub fn set_rootfs_mount(pid: ProcessId, mount: u64) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.rootfs_mount = Some(mount);
    }
}

/// The VFS mount serving a Linux process's root filesystem, if any.
pub fn rootfs_mount(pid: ProcessId) -> Option<u64> {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .and_then(|proc| proc.rootfs_mount)
}

/// Confine a Linux process to a rootfs base subdirectory on its mount (Phase
/// 6.1b). After this, every path syscall prepends `base` to the container's
/// `..`-clamped relative path. `base` must be an absolute, `..`-free path.
pub fn set_rootfs_base(pid: ProcessId, base: String) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.rootfs_base = Some(base);
    }
}

/// The rootfs base subdirectory a Linux process is confined to, if any. `None`
/// means it is rooted at the mount root.
pub fn rootfs_base(pid: ProcessId) -> Option<String> {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .and_then(|proc| proc.rootfs_base.clone())
}

/// A copy of a Linux process's current working directory (defaults to "/").
pub fn get_cwd(pid: ProcessId) -> String {
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .map(|proc| proc.cwd.clone())
        .unwrap_or_else(|| String::from("/"))
}

/// Set a Linux process's current working directory (already-normalized path).
pub fn set_cwd(pid: ProcessId, cwd: String) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.cwd = cwd;
    }
}

/// Allocate a Linux file descriptor for `entry`, returning the integer fd (≥ 3),
/// or -1 if the process is not found. Reuses freed slots before growing.
pub fn linux_fd_alloc(pid: ProcessId, entry: LinuxFd) -> i32 {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        // Reuse a freed slot if one exists, else append.
        let idx = match proc.fd_table.iter().position(|e| e.is_none()) {
            Some(i) => {
                proc.fd_table[i] = Some(entry);
                i
            }
            None => {
                proc.fd_table.push(Some(entry));
                proc.fd_table.len() - 1
            }
        };
        (idx + 3) as i32 // fds 0/1/2 are stdio
    } else {
        -1
    }
}

/// Look up a Linux fd (≥ 3). Returns a copy of the entry, or `None`.
pub fn linux_fd_get(pid: ProcessId, fd: i32) -> Option<LinuxFd> {
    if fd < 3 {
        return None;
    }
    let idx = (fd - 3) as usize;
    let table = PROCESS_TABLE.lock();
    table
        .processes
        .get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .and_then(|proc| proc.fd_table.get(idx).copied().flatten())
}

/// Update the stored offset of an open Linux fd (for `read`/`lseek`/`getdents64`).
pub fn linux_fd_set_offset(pid: ProcessId, fd: i32, offset: u64) {
    if fd < 3 {
        return;
    }
    let idx = (fd - 3) as usize;
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        if let Some(Some(e)) = proc.fd_table.get_mut(idx) {
            e.offset = offset;
        }
    }
}

/// Close a Linux fd, returning the entry that was there (so the caller can close
/// the underlying server fd), or `None` if it wasn't open.
pub fn linux_fd_close(pid: ProcessId, fd: i32) -> Option<LinuxFd> {
    if fd < 3 {
        return None;
    }
    let idx = (fd - 3) as usize;
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(proc)) = table.processes.get_mut(pid.as_usize()) {
        if let Some(slot) = proc.fd_table.get_mut(idx) {
            return slot.take();
        }
    }
    None
}

/// Destroy a process by PID.
///
/// Kills all tasks belonging to the process, destroys its address space
/// (freeing all page table frames), drops its CSpace, and marks the
/// process table slot as `None`.
///
/// Cannot destroy the kernel process (PID 0). Returns `true` if the
/// process was found and destroyed, `false` otherwise.
pub fn destroy_process(pid: ProcessId) -> bool {
    if pid == ProcessId::KERNEL {
        return false;
    }

    let mut table = PROCESS_TABLE.lock();
    let slot = match table.processes.get_mut(pid.as_usize()) {
        Some(slot) => slot,
        None => return false,
    };

    let process = match slot.take() {
        Some(p) => p,
        None => return false,
    };

    // We've already taken the process out of the table, so we can drop the table
    // lock before calling kill_task (which acquires the scheduler lock).
    let task_ids = process.tasks.clone();
    drop(table);

    // ORDER IS SECURITY-CRITICAL (Phase 5.7): mark every task Dead *before* freeing
    // the address space. `kill_task` removes each task from the run queue, so once
    // this loop finishes no task can be scheduled with a CR3 pointing into the page
    // tables we're about to free. The previous order (free address space, then kill
    // tasks — with the lock dropped and interrupts re-enabled in between) left a
    // window in which a timer tick could context-switch INTO a still-`Runnable`
    // task whose saved CR3 referenced a just-freed (and possibly recycled) PML4
    // frame: a use-after-free / triple fault. Exit-path callers pass already-Dead
    // tasks, so this is a no-op for them; `container::terminate` on a *live*
    // container is the first caller that depends on the reorder.
    for task_id in &task_ids {
        crate::sched::kill_task(*task_id);
    }

    // Now that no task can run in this address space, free its page-table frames.
    if let Some(address_space) = process.address_space {
        address_space.destroy();
    }
    // CSpace is dropped automatically when `process` goes out of scope.

    println!("[process] Destroyed process {} (\"{}\")", pid, process.name);
    true
}

/// Add a task to a process's task list.
///
/// Called when spawning a task within a process so the process tracks
/// which tasks it owns (for cleanup on destruction).
pub fn add_task_to_process(pid: ProcessId, task_id: TaskId) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(ref mut proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.tasks.push(task_id);
    }
}

/// Remove a task from a process's task list.
///
/// Called when a task exits so the process no longer tracks it.
pub fn remove_task_from_process(pid: ProcessId, task_id: TaskId) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(Some(ref mut proc)) = table.processes.get_mut(pid.as_usize()) {
        proc.tasks.retain(|&id| id != task_id);
    }
}

/// Get the PML4 physical address for a process.
///
/// Returns `None` for the kernel process (PID 0, which uses the kernel's
/// own page tables) or if the PID is invalid.
pub fn process_pml4(pid: ProcessId) -> Option<u64> {
    let table = PROCESS_TABLE.lock();
    table.processes.get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .and_then(|proc| proc.address_space.as_ref())
        .map(|as_ref| as_ref.root_phys().as_u64())
}

/// Info about a process, returned by `process_list()` for display purposes.
/// Copied out of the process table lock so callers can format at leisure.
pub struct ProcessInfo {
    pub pid: ProcessId,
    pub name: String,
    pub task_count: usize,
    pub state: ProcessState,
    pub cap_count: usize,
}

/// Get a snapshot of all live processes.
///
/// Returns a Vec of `ProcessInfo` structs that can be printed outside the
/// process table lock. Used by the shell's `procs` command.
pub fn process_list() -> Vec<ProcessInfo> {
    let table = PROCESS_TABLE.lock();
    table.processes.iter()
        .filter_map(|slot| {
            slot.as_ref().map(|proc| ProcessInfo {
                pid: proc.pid,
                name: proc.name.clone(),
                task_count: proc.tasks.len(),
                state: proc.state,
                cap_count: proc.cspace.as_ref().map_or(0, |cs| cs.active_count()),
            })
        })
        .collect()
}

/// Get the capabilities in a process's CSpace.
///
/// Returns a Vec of (handle, type, rights) tuples for display. Returns
/// an empty Vec for the kernel process (which has no CSpace) or invalid PIDs.
pub fn process_caps(pid: ProcessId) -> Vec<(CapHandle, CapType, CapRights)> {
    let table = PROCESS_TABLE.lock();
    let proc = match table.processes.get(pid.as_usize()).and_then(|s| s.as_ref()) {
        Some(p) => p,
        None => return Vec::new(),
    };
    match &proc.cspace {
        Some(cs) => cs.iter().map(|(h, cap)| (h, cap.cap_type, cap.rights)).collect(),
        None => Vec::new(),
    }
}

/// Get the total number of active processes (non-None slots).
pub fn process_count() -> usize {
    let table = PROCESS_TABLE.lock();
    table.processes.iter().filter(|s| s.is_some()).count()
}

/// Execute a closure with access to a process's address space.
///
/// Returns `None` if the PID is invalid, the process is destroyed, or the
/// process has no address space (kernel process). The closure receives the
/// AddressSpace reference and can map/unmap pages.
pub fn with_address_space<F, R>(pid: ProcessId, f: F) -> Option<R>
where
    F: FnOnce(&AddressSpace) -> R,
{
    let table = PROCESS_TABLE.lock();
    table.processes.get(pid.as_usize())
        .and_then(|slot| slot.as_ref())
        .and_then(|proc| proc.address_space.as_ref())
        .map(f)
}

/// Execute a closure with mutable access to a process's CSpace.
///
/// Returns `None` if the PID is invalid, the process is destroyed, or the
/// process has no CSpace (kernel process).
pub fn with_cspace_mut<F, R>(pid: ProcessId, f: F) -> Option<R>
where
    F: FnOnce(&mut CSpace) -> R,
{
    let mut table = PROCESS_TABLE.lock();
    table.processes.get_mut(pid.as_usize())
        .and_then(|slot| slot.as_mut())
        .and_then(|proc| proc.cspace.as_mut())
        .map(f)
}
