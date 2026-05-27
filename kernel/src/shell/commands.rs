//! # Shell command implementations
//!
//! Each command is a function that takes an argument string (everything after
//! the command name) and prints its output to serial via `println!`.

use crate::mm;
use crate::println;
use crate::sched;
use crate::sched::task::TaskState;
use crate::process;
use crate::process::ProcessState;
use crate::cap::CapType;

/// Print a list of available commands.
pub fn cmd_help(_args: &str) {
    println!("Available commands:");
    println!("  help             — show this message");
    println!("  mem              — show memory statistics");
    println!("  tasks            — list all tasks");
    println!("  spawn [name]     — spawn a test task");
    println!("  kill <id>        — kill a task by ID");
    println!("  peek <addr> [n]  — hex dump n bytes at virtual address");
    println!("  pgtable <addr>   — walk page tables for a virtual address");
    println!("  procs            — list all processes");
    println!("  caps [pid]       — list capabilities in a process's CSpace");
}

/// Print memory statistics: frame allocator and heap usage.
pub fn cmd_mem(_args: &str) {
    let free_frames = mm::frame::free_frame_count();
    let total_frames = mm::frame::total_frame_count();
    let used_frames = total_frames - free_frames;

    let free_mib = (free_frames as u64 * mm::PAGE_SIZE) / (1024 * 1024);
    let used_mib = (used_frames as u64 * mm::PAGE_SIZE) / (1024 * 1024);

    println!("Memory:");
    println!("  Frames: {} free / {} total ({} MiB free, {} MiB used)",
        free_frames, total_frames, free_mib, used_mib);
    println!("  Heap:   {} used, {} free (of {} KiB total)",
        mm::heap::used(), mm::heap::free(), mm::heap::total_size() / 1024);
    let growth = mm::heap::growth_count();
    if growth > 0 {
        println!("  Heap growth events: {}", growth);
    }
}

/// List all tasks with their ID, state, and name.
pub fn cmd_tasks(_args: &str) {
    let tasks = sched::task_list();
    let current = sched::current_task_id();

    println!("  {:>4}  {:>8}  {}", "ID", "STATE", "NAME");
    println!("  {:->4}  {:->8}  {:->20}", "", "", "");
    for info in &tasks {
        let state_str = match info.state {
            TaskState::Ready => "ready",
            TaskState::Running => "running",
            TaskState::Blocked => "blocked",
            TaskState::Dead => "dead",
        };
        let marker = if info.id == current { " <-" } else { "" };
        println!("  {:>4}  {:>8}  {}{}", info.id, state_str, info.name, marker);
    }
    println!("  ({} tasks)", tasks.len());
}

/// Spawn a test task that prints a counter. The task name defaults to "test"
/// but can be overridden via the argument.
pub fn cmd_spawn(args: &str) {
    let name = if args.is_empty() { "test" } else { args.trim() };

    // We can't capture `name` in a fn pointer, so all spawned test tasks
    // use the same entry function and identify themselves by task ID.
    let id = sched::spawn(name, test_task_entry);
    println!("Spawned task {} (\"{}\")", id, name);
}

/// Entry function for tasks created by the `spawn` shell command.
fn test_task_entry() {
    let id = sched::current_task_id();
    for i in 0..10 {
        println!("[task {}] count {}", id, i);
        sched::yield_now();
    }
}

/// Kill a task by ID.
pub fn cmd_kill(args: &str) {
    let args = args.trim();
    if args.is_empty() {
        println!("Usage: kill <task_id>");
        return;
    }

    match args.parse::<usize>() {
        Ok(id) => {
            if sched::kill_task(id) {
                println!("Killed task {}", id);
            } else {
                println!("Cannot kill task {} (not found, already dead, or protected)", id);
            }
        }
        Err(_) => {
            println!("Invalid task ID: '{}'", args);
        }
    }
}

/// Hex dump memory at a virtual address. Validates the address against
/// known mapped ranges before dereferencing.
///
/// Usage: `peek <hex_addr> [count]`
/// - addr: virtual address in hex (with or without 0x prefix)
/// - count: number of bytes to dump (default 64, max 256)
pub fn cmd_peek(args: &str) {
    let parts: alloc::vec::Vec<&str> = args.split_whitespace().collect();
    if parts.is_empty() {
        println!("Usage: peek <addr> [count]");
        println!("  addr:  virtual address in hex (e.g., 0xffff800000100000)");
        println!("  count: bytes to dump (default 64, max 256)");
        return;
    }

    // Parse the address (strip optional 0x prefix)
    let addr_str = parts[0].trim_start_matches("0x").trim_start_matches("0X");
    let addr = match u64::from_str_radix(addr_str, 16) {
        Ok(a) => a,
        Err(_) => {
            println!("Invalid address: '{}'", parts[0]);
            return;
        }
    };

    // Parse the count (default 64, max 256)
    let count = if parts.len() > 1 {
        match parts[1].parse::<usize>() {
            Ok(n) if n > 0 && n <= 256 => n,
            Ok(_) => {
                println!("Count must be between 1 and 256");
                return;
            }
            Err(_) => {
                println!("Invalid count: '{}'", parts[1]);
                return;
            }
        }
    } else {
        64
    };

    // Validate the address falls within known mapped ranges.
    // In Phase 1, we know these ranges are mapped:
    // - HHDM: hhdm_offset .. hhdm_offset + physical_memory_size
    // - Kernel image: 0xffffffff80000000 .. (some upper bound)
    //
    // We use a simple heuristic: the address must be in the upper half
    // (kernel space, above 0xffff800000000000) and within a reasonable
    // range. A page fault will still crash the kernel, so we do our best.
    if !is_valid_address(addr, count) {
        println!("Address {:#x} is not in a known mapped range.", addr);
        println!("Valid ranges: HHDM (0xffff800000000000+), kernel image (0xffffffff80000000+)");
        return;
    }

    // Dump the memory in hex + ASCII format
    hex_dump(addr, count);
}

/// Check if a virtual address range is likely to be mapped.
///
/// We can't do a perfect check without walking page tables (Phase 2),
/// so we use a conservative heuristic based on known memory layout:
/// - HHDM region: maps all physical memory at hhdm_offset
/// - Kernel image: loaded at 0xffffffff80000000
fn is_valid_address(addr: u64, count: usize) -> bool {
    let end = addr.wrapping_add(count as u64);
    let hhdm = mm::hhdm_offset();

    // Must be in the upper half (kernel space)
    if addr < 0xffff_8000_0000_0000 {
        return false;
    }

    // HHDM region: hhdm_offset .. hhdm_offset + some physical memory limit.
    // With 256 MiB QEMU memory, physical addresses go up to ~0x10000000.
    // Be generous and allow up to 4 GiB above the HHDM base.
    let hhdm_end = hhdm + 4 * 1024 * 1024 * 1024; // 4 GiB
    if addr >= hhdm && end <= hhdm_end {
        return true;
    }

    // Kernel image region: 0xffffffff80000000 .. 0xffffffff80000000 + 16 MiB
    let kernel_base: u64 = 0xffff_ffff_8000_0000;
    let kernel_end: u64 = kernel_base + 16 * 1024 * 1024;
    if addr >= kernel_base && end <= kernel_end {
        return true;
    }

    false
}

/// Print a hex dump of memory in traditional format.
///
/// Each line shows: address, 16 hex bytes, ASCII representation.
fn hex_dump(addr: u64, count: usize) {
    let ptr = addr as *const u8;
    let aligned_start = addr & !0xF; // align down to 16-byte boundary

    let mut offset = 0usize;
    while offset < count {
        let line_addr = aligned_start + offset as u64;
        // Print address
        crate::print!("{:016x}  ", line_addr);

        // Print 16 hex bytes
        let mut ascii = [b'.'; 16];
        for i in 0..16 {
            let byte_addr = line_addr + i as u64;
            if byte_addr >= addr && byte_addr < addr + count as u64 {
                let byte = unsafe { *ptr.add((byte_addr - addr) as usize) };
                crate::print!("{:02x} ", byte);
                if byte >= 0x20 && byte < 0x7f {
                    ascii[i] = byte;
                }
            } else {
                crate::print!("   ");
            }
            if i == 7 {
                crate::print!(" ");
            }
        }

        // Print ASCII representation
        crate::print!(" |");
        for (i, &ch) in ascii.iter().enumerate() {
            let byte_addr = line_addr + i as u64;
            if byte_addr >= addr && byte_addr < addr + count as u64 {
                crate::print!("{}", ch as char);
            } else {
                crate::print!(" ");
            }
        }
        println!("|");

        offset += 16;
    }
}

/// Walk and print the page table entries for a virtual address.
///
/// Usage: `pgtable <hex_addr>`
/// Shows the PML4, PDP, PD, and PT entries with their physical addresses,
/// flags, and the final mapping (if present). Useful for debugging page
/// table issues and verifying that mappings are correct.
pub fn cmd_pgtable(args: &str) {
    let args = args.trim();
    if args.is_empty() {
        println!("Usage: pgtable <addr>");
        println!("  addr: virtual address in hex (e.g., 0xffff800000100000)");
        return;
    }

    let addr_str = args.trim_start_matches("0x").trim_start_matches("0X");
    let addr = match u64::from_str_radix(addr_str, 16) {
        Ok(a) => a,
        Err(_) => {
            println!("Invalid address: '{}'", args);
            return;
        }
    };

    let kernel_as = mm::page_table::kernel_address_space();
    kernel_as.walk_and_print(mm::addr::VirtAddr::new(addr));

    // Don't let the AddressSpace drop (which would free the kernel PML4!).
    // kernel_address_space() returns a lightweight handle — forgetting it
    // is correct because the kernel PML4 is managed globally.
    core::mem::forget(kernel_as);
}

/// List all processes with PID, name, task count, capability count, and state.
pub fn cmd_procs(_args: &str) {
    let procs = process::process_list();

    println!("  {:>4}  {:>8}  {:>5}  {:>4}  {}", "PID", "STATE", "TASKS", "CAPS", "NAME");
    println!("  {:->4}  {:->8}  {:->5}  {:->4}  {:->20}", "", "", "", "", "");
    for info in &procs {
        let state_str = match info.state {
            ProcessState::Running => "running",
            ProcessState::Exited => "exited",
        };
        println!("  {:>4}  {:>8}  {:>5}  {:>4}  {}",
            info.pid.as_usize(), state_str, info.task_count, info.cap_count, info.name);
    }
    println!("  ({} processes)", procs.len());
}

/// List capabilities in a process's CSpace.
///
/// Usage: `caps [pid]` — defaults to PID 0 (kernel process) if no PID given.
/// Shows each capability's handle, type, rights, and parent relationship.
pub fn cmd_caps(args: &str) {
    let args = args.trim();
    let pid_val = if args.is_empty() {
        0usize
    } else {
        match args.parse::<usize>() {
            Ok(v) => v,
            Err(_) => {
                println!("Invalid PID: '{}'", args);
                return;
            }
        }
    };

    let pid = process::ProcessId::new(pid_val);
    let caps = process::process_caps(pid);

    if caps.is_empty() {
        println!("  No capabilities (PID {} has no CSpace or is invalid)", pid_val);
        return;
    }

    println!("  Capabilities for PID {}:", pid_val);
    println!("  {:>12}  {:>16}  {:>8}  {}", "HANDLE", "TYPE", "RIGHTS", "DETAILS");
    println!("  {:->12}  {:->16}  {:->8}  {:->30}", "", "", "", "");
    for (handle, cap_type, rights) in &caps {
        let type_str = match cap_type {
            CapType::Null => "Null",
            CapType::Memory { .. } => "Memory",
            CapType::Endpoint { .. } => "Endpoint",
            CapType::Process { .. } => "Process",
            CapType::Irq { .. } => "IRQ",
        };
        let details = match cap_type {
            CapType::Null => alloc::string::String::from("-"),
            CapType::Memory { base, page_count } =>
                alloc::format!("base={:#x} pages={}", base, page_count),
            CapType::Endpoint { endpoint_id, badge } =>
                alloc::format!("eid={} badge={}", endpoint_id, badge),
            CapType::Process { pid } =>
                alloc::format!("pid={}", pid),
            CapType::Irq { irq_number } =>
                alloc::format!("irq={}", irq_number),
        };
        println!("  {:>12}  {:>16}  {:>8}  {}",
            handle, type_str, rights, details);
    }
    println!("  ({} capabilities)", caps.len());
}

/// We need alloc for Vec in argument parsing.
extern crate alloc;
