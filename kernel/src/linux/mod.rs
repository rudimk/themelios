//! # Linux compatibility layer (Phase 5)
//!
//! ThemeliOS is not Linux, but it aims to run unmodified OCI/Docker container
//! images — which are Linux ELF binaries speaking the Linux syscall ABI. This
//! module is the compatibility layer that makes that possible.
//!
//! ## Phase 5.0 (this sub-phase)
//!
//! - [`elf`] — an ELF64 loader that maps a statically-linked `ET_EXEC` image into
//!   a process address space (W^X segments) and builds the SysV/Linux initial
//!   stack.
//! - [`exec_elf`] — the driver: create a process, load an ELF into it, build its
//!   stack, and enter it in ring 3.
//!
//! The Linux *syscall personality* (a per-process flag routing the `syscall`
//! entry through a Linux dispatch table) arrives in 5.1. 5.0 runs a program that
//! speaks the **native** ThemeliOS ABI, so loader correctness is validated
//! independently of Linux-ABI correctness.

pub mod elf;

use crate::process::{self, ProcessId};
use elf::{ByteSource, ElfError};

/// Load an ELF image from `src` into a fresh process, build its initial stack
/// with `args`/`envs`, and spawn it in ring 3. Returns the new process id.
///
/// For 5.0 the program uses the native ThemeliOS syscall ABI (so it can exit via
/// `SYS_EXIT`); 5.1 marks such processes with a Linux personality and routes
/// their syscalls through the Linux table.
pub fn exec_elf(
    name: &str,
    src: &dyn ByteSource,
    args: &[&str],
    envs: &[&str],
) -> Result<ProcessId, ElfError> {
    let (pid, _) = process::create_process(name, None);
    let img = elf::load_into(pid, src)?;
    let rsp = elf::build_initial_stack(pid, &img, args, envs)?;
    // Record the per-process entry so the generic trampoline can `iretq` to it —
    // an ELF's entry and initial %rsp vary per image, unlike the servers' fixed
    // load address.
    process::set_user_entry(pid, img.entry, rsp);
    spawn_loaded(name, pid);
    Ok(pid)
}

/// Spawn a ring-3 task for a process whose ELF image and entry have already been
/// loaded/recorded (via [`elf::load_into`], [`elf::build_initial_stack`], and
/// [`process::set_user_entry`]). Split out from [`exec_elf`] so a caller can do
/// extra setup on the process — e.g. mapping a result region in the 5.0 test —
/// between loading and launch.
pub fn spawn_loaded(name: &str, pid: ProcessId) {
    crate::sched::spawn_in_process(name, elf_trampoline, pid);
}

/// Trampoline that drops a freshly-spawned task into ring 3 at its process's
/// loaded ELF entry point. Runs in kernel mode within the process's address
/// space (the scheduler switches CR3 by the task's `process_id` before it runs),
/// builds an `iretq` frame from the recorded `(entry_rip, entry_rsp)`, and never
/// returns.
///
/// Distinct from the servers' `server_trampoline`, which hardcodes the fixed
/// load address — an ELF's entry/stack are per-image, read from the process.
fn elf_trampoline() {
    let pid = crate::sched::current_process_id();
    let (user_rip, user_rsp) =
        process::user_entry(pid).expect("elf_trampoline: process has no recorded ELF entry");

    // SAFETY: the ELF's code/stack pages were mapped USER in this process's
    // address space by the loader; the selectors match the ring-3 GDT entries
    // (DPL=3). `iretq` performs the ring 0→3 transition atomically.
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) 0x1Bu64,      // USER_DATA_SELECTOR | RPL=3
            rsp = in(reg) user_rsp,
            rflags = in(reg) 0x202u64, // IF=1 + reserved bit 1
            cs = in(reg) 0x23u64,      // USER_CODE_SELECTOR | RPL=3
            rip = in(reg) user_rip,
            options(noreturn),
        );
    }
}
