//! # Linux threads and futex (Phase 5.3)
//!
//! Implements the pieces a real libc's pthreads need: `clone(CLONE_THREAD)` to
//! create a thread sharing the address space, `futex` WAIT/WAKE for the
//! mutex/condvar/join primitives, and the `set_tid_address` /
//! `CLONE_CHILD_CLEARTID` mechanism that makes `pthread_join` work.
//!
//! ## Threads
//!
//! A ThemeliOS process already supports multiple tasks in one address space
//! (`Process.tasks`), so `clone(CLONE_THREAD)` maps onto a new scheduler task in
//! the same process. The child resumes at the parent's post-`syscall` RIP with
//! `rax = 0` on its own stack and its own `%fs` base (`CLONE_SETTLS`) — the raw
//! Linux clone contract that libc's `__clone` wrapper builds on.
//!
//! ## futex
//!
//! The kernel had no address-keyed wait queue (IPC blocking is endpoint-based),
//! so this adds one, keyed by `(pid, uaddr)` — sufficient for **private** futexes,
//! which is what threads sharing an address space use. WAIT re-checks the word
//! under the non-preemptible single-core syscall before blocking, so there is no
//! lost-wakeup window. No priority inheritance, requeue, or timeout (scoped out).

use crate::arch::x86_64::syscall::{copy_from_user, copy_to_user, SyscallFrame};
use crate::process::{self, ProcessId};
use crate::sched::{self, task::TaskId};
use crate::sync::InterruptMutex;
use alloc::vec::Vec;

extern crate alloc;

// --- clone flags ---
const CLONE_VM: u64 = 0x100;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

// --- futex ops ---
const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_CMD_MASK: u64 = !(128 | 256); // strip FUTEX_PRIVATE_FLAG / CLOCK_REALTIME

// --- errno (negated on return) ---
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;

fn err(e: i64) -> u64 {
    (-e) as u64
}

/// One thread parked in a `FUTEX_WAIT`, keyed by `(pid, uaddr)`.
struct Waiter {
    pid: usize,
    uaddr: u64,
    task: TaskId,
}

/// The global futex wait queue. Small and linear — the thread counts a container
/// runs are modest; a hashed queue is a later optimisation.
static FUTEX_WAITERS: InterruptMutex<Vec<Waiter>> = InterruptMutex::new(Vec::new());

/// Wake up to `max` threads waiting on `(pid, uaddr)`. Returns the count woken.
/// Collects the matching task ids under the futex lock, releases it, then wakes —
/// never holding the futex lock across the scheduler lock.
fn futex_wake(pid: usize, uaddr: u64, max: u32) -> u32 {
    let mut to_wake: Vec<TaskId> = Vec::new();
    {
        let mut q = FUTEX_WAITERS.lock();
        let mut i = 0;
        while i < q.len() && (to_wake.len() as u32) < max {
            if q[i].pid == pid && q[i].uaddr == uaddr {
                to_wake.push(q[i].task);
                q.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
    for tid in &to_wake {
        sched::wake_task(*tid);
    }
    to_wake.len() as u32
}

/// `clone(flags, child_stack, ptid, ctid, tls)` — thread creation only (5.3).
///
/// x86-64 raw ABI: `rdi=flags, rsi=child_stack, rdx=ptid, r10=ctid, r8=tls`.
/// Returns the new thread id to the parent; the child enters ring 3 separately
/// (rax=0) via the thread trampoline.
pub fn sys_clone(frame: &SyscallFrame) -> u64 {
    let flags = frame.rdi;
    let child_stack = frame.rsi;
    let ptid = frame.rdx;
    let ctid = frame.r10;
    let tls = frame.r8;

    // 5.3 only supports thread clones (shared address space). fork/vfork later.
    if flags & CLONE_THREAD == 0 || flags & CLONE_VM == 0 || child_stack == 0 {
        return err(ENOSYS);
    }
    let pid = sched::current_process_id();

    // Create the sibling task and seed its ring-3 entry: it resumes at the
    // parent's return RIP (rcx, saved by `syscall`) on the given child stack.
    let tid = sched::spawn_in_process("thread", thread_trampoline, pid);
    sched::set_task_clone_entry(tid, frame.rcx, child_stack);
    if flags & CLONE_SETTLS != 0 {
        sched::set_task_fs_base(tid, tls);
    }
    if flags & CLONE_CHILD_CLEARTID != 0 {
        sched::set_task_clear_child_tid(tid, ctid);
    }
    // SETTID variants: publish the new tid to the requested user words now, so a
    // joiner reading the word never races the child's creation.
    let tid_bytes = (tid as u32).to_le_bytes();
    if flags & CLONE_CHILD_SETTID != 0 && ctid != 0 {
        let _ = copy_to_user(ctid, &tid_bytes);
    }
    if flags & CLONE_PARENT_SETTID != 0 && ptid != 0 {
        let _ = copy_to_user(ptid, &tid_bytes);
    }
    tid as u64
}

/// `set_tid_address(tidptr)` — record the `clear_child_tid` word and return the
/// caller's tid. On thread exit the kernel zeroes + futex-wakes this word.
pub fn sys_set_tid_address(tidptr: u64) -> u64 {
    sched::set_current_clear_child_tid(tidptr);
    sched::current_task_id() as u64
}

/// `futex(uaddr, op, val, …)` — private WAIT/WAKE only.
pub fn sys_futex(frame: &SyscallFrame) -> u64 {
    let uaddr = frame.rdi;
    let op = frame.rsi & FUTEX_CMD_MASK;
    let val = frame.rdx as u32;
    let pid = sched::current_process_id().as_usize();

    match op {
        FUTEX_WAIT => {
            // Re-check the word equals `val` before parking. On single-core this
            // check + enqueue + block is non-preemptible, so no wakeup is lost.
            let cur = match copy_from_user(uaddr, 4) {
                Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                None => return err(EFAULT),
            };
            if cur != val {
                return err(EAGAIN);
            }
            let task = sched::current_task_id();
            FUTEX_WAITERS.lock().push(Waiter { pid, uaddr, task });
            sched::block_current_task();
            // Woken by a FUTEX_WAKE (which removed us from the queue) or a thread
            // exit clearing the word. Defensively drop any stale entry for us.
            FUTEX_WAITERS
                .lock()
                .retain(|w| !(w.pid == pid && w.uaddr == uaddr && w.task == task));
            0
        }
        FUTEX_WAKE => futex_wake(pid, uaddr, val) as u64,
        _ => err(EINVAL),
    }
}

/// Terminate the calling **thread** (Linux `exit`, number 60). Honours
/// `CLONE_CHILD_CLEARTID` — zeroes the join word and futex-wakes it so a
/// `pthread_join` completes — then destroys the task. Never returns.
pub fn thread_exit(code: u64) -> ! {
    let pid = sched::current_process_id().as_usize();
    let clear = sched::take_current_clear_child_tid();
    if clear != 0 {
        // Write 0 to the join word and wake one waiter (the joiner).
        let _ = copy_to_user(clear, &0u32.to_le_bytes());
        futex_wake(pid, clear, 1);
    }
    crate::println!("[linux] thread exit: task {} code {}", sched::current_task_id(), code);
    // Undo the syscall-entry swapgs by hand — the exit stub never runs.
    // SAFETY: matches the syscall entry's swapgs; single-core.
    unsafe {
        crate::arch::x86_64::cpu::swapgs();
    }
    sched::exit_current_task();
}

/// Terminate the whole thread group (Linux `exit_group`, number 231): kill every
/// sibling task in the process, then exit the caller. Never returns.
pub fn exit_group(code: u64) -> ! {
    let pid = sched::current_process_id();
    let me = sched::current_task_id();
    for tid in process::task_ids(pid) {
        if tid != me {
            sched::kill_task(tid);
        }
    }
    crate::println!("[linux] exit_group: task {} code {}", me, code);
    // SAFETY: matches the syscall entry's swapgs; single-core.
    unsafe {
        crate::arch::x86_64::cpu::swapgs();
    }
    sched::exit_current_task();
}

/// Trampoline entered by a freshly-`clone`d thread task. Reads its recorded
/// `(rip, rsp)` and drops to ring 3 with `rax = 0` (the child's `clone` return)
/// on its own stack. Its `%fs` base was seeded at creation and is restored by the
/// context switch. Never returns.
pub fn thread_trampoline() {
    let (user_rip, user_rsp) =
        sched::current_clone_entry().expect("thread_trampoline: task has no clone entry");
    // SAFETY: the thread shares its process's address space (code/stack mapped
    // USER); selectors match the ring-3 GDT (DPL=3). `rax=0` is the child's clone
    // return value; `iretq` performs the ring 0→3 transition.
    unsafe {
        core::arch::asm!(
            // rax is forced to 0 via the explicit input operand below — that is
            // the child's `clone` return value, and iretq does not touch rax.
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) 0x1Bu64,
            rsp = in(reg) user_rsp,
            rflags = in(reg) 0x202u64,
            cs = in(reg) 0x23u64,
            rip = in(reg) user_rip,
            in("rax") 0u64,
            options(noreturn),
        );
    }
}
