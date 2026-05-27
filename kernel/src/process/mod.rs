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
}

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

    // Kill all tasks belonging to this process.
    // We need to drop the table lock before calling kill_task (which acquires
    // the scheduler lock). But since we've already taken the process out of
    // the table, we can safely drop the lock.
    let task_ids = process.tasks.clone();

    // Destroy the address space (frees all user-half page table frames).
    if let Some(address_space) = process.address_space {
        address_space.destroy();
    }

    // CSpace is dropped automatically when process goes out of scope.

    // Drop the lock before killing tasks (which acquires scheduler lock)
    drop(table);

    for task_id in &task_ids {
        crate::sched::kill_task(*task_id);
    }

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
        .map(|as_ref| as_ref.pml4_phys().as_u64())
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
