//! # Shell command implementations
//!
//! Each command is a function that takes an argument string (everything after
//! the command name) and prints its output to serial via `println!`.

// Every function here is reached only through the interactive shell's dispatch
// table (see `shell::shell_entry`), which runs solely in the normal kernel. The
// test kernel replaces the shell with the test runner, so these are dead code in
// that build only — suppress the lint there, but keep it active for normal builds
// so genuinely-unused commands are still flagged.
#![cfg_attr(feature = "test", allow(dead_code))]

use crate::mm;
use crate::println;
use crate::sched;
use crate::sched::task::TaskState;
use crate::process;
use crate::process::ProcessState;
use crate::cap::CapType;
use crate::audit;

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
    println!("  audit [n]        — show last n audit log entries (default 20)");
    println!("  mount            — list mounted filesystems");
    println!("  ls <path>        — list a directory");
    println!("  cat <path>       — print file contents");
    println!("  stat <path>      — show file size and type");
    println!("  write <path> <s> — create/write a file (overlay or /data)");
    println!("  mkdir <path>     — create a directory");
    println!("  ifconfig         — show network interface configuration");
    println!("  sockets          — list open sockets and their state");
    println!("  ping <ip> [n]    — send ICMP echo requests (default 4)");
    println!("  udpsend <ip> <port> <msg> — send a UDP datagram");
    println!("  tcpconnect <ip> <port> — open a TCP connection and exchange a line");
    println!("  run              — launch a demo container (linux-smoke as /init)");
    println!("  stop <pid>       — force-terminate a running container");
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

/// Force-terminate a running container by PID (Phase 5.7). Distinct from `kill`,
/// which operates on **task** IDs: `stop` takes a **process** ID and only tears
/// down actual containers (`container::terminate` refuses kernel services), so it
/// can't be used to nuke the block/ext2/net servers.
pub fn cmd_stop(args: &str) {
    let args = args.trim();
    if args.is_empty() {
        println!("Usage: stop <container_pid>");
        return;
    }
    match args.parse::<usize>() {
        Ok(n) => {
            let pid = crate::process::ProcessId::new(n);
            if crate::container::terminate(pid) {
                println!("Stopped container {}", n);
            } else {
                println!("Cannot stop {} (not a running container)", n);
            }
        }
        Err(_) => println!("Invalid PID: '{}'", args),
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
            CapType::SharedMemory { .. } => "ShMem",
            CapType::Filesystem { .. } => "FS",
            CapType::FileDescriptor { .. } => "FD",
            CapType::Socket { .. } => "Socket",
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
            CapType::SharedMemory { phys_base, size, owner_pid } =>
                alloc::format!("base={:#x} size={:#x} pid={}", phys_base, size, owner_pid),
            CapType::Filesystem { mount_id } =>
                alloc::format!("mount={}", mount_id),
            CapType::FileDescriptor { fd, mount_id } =>
                alloc::format!("fd={} mount={}", fd, mount_id),
            CapType::Socket { socket_id } if *socket_id == crate::cap::SOCKET_FACTORY =>
                alloc::string::String::from("factory"),
            CapType::Socket { socket_id } =>
                alloc::format!("sock={}", socket_id),
        };
        println!("  {:>12}  {:>16}  {:>8}  {}",
            handle, type_str, rights, details);
    }
    println!("  ({} capabilities)", caps.len());
}

/// Display the last N entries from the kernel audit log.
///
/// Usage: `audit [n]` where `n` is the number of entries to show (default 20).
/// Shows sequence number, tick count, source PID, operation, capability type,
/// and operation-specific detail.
pub fn cmd_audit(args: &str) {
    let args = args.trim();
    let count = if args.is_empty() {
        20usize
    } else {
        match args.parse::<usize>() {
            Ok(v) => v,
            Err(_) => {
                println!("Invalid count: '{}'", args);
                return;
            }
        }
    };

    let entries = audit::last_entries(count);
    let total = audit::total_event_count();

    if entries.is_empty() {
        println!("  Audit log is empty (0 events recorded)");
        return;
    }

    println!("  Audit log ({} total events, showing last {}):", total, entries.len());
    println!("  {:>6}  {:>8}  {:>6}  {:>14}  {:>10}  {:>12}",
        "SEQ", "TICK", "PID", "OPERATION", "CAP_TYPE", "DETAIL");
    println!("  {:->6}  {:->8}  {:->6}  {:->14}  {:->10}  {:->12}",
        "", "", "", "", "", "");

    for entry in &entries {
        let type_str = match entry.cap_type {
            CapType::Null => "Null",
            CapType::Memory { .. } => "Memory",
            CapType::Endpoint { .. } => "Endpoint",
            CapType::Process { .. } => "Process",
            CapType::Irq { .. } => "IRQ",
            CapType::SharedMemory { .. } => "ShMem",
            CapType::Filesystem { .. } => "FS",
            CapType::FileDescriptor { .. } => "FD",
            CapType::Socket { .. } => "Socket",
        };

        println!("  {:>6}  {:>8}  {:>6}  {:>14}  {:>10}  {:#012x}",
            entry.seq,
            entry.timestamp,
            entry.source_pid.as_usize(),
            entry.operation,
            type_str,
            entry.detail);
    }
}

/// We need alloc for Vec in argument parsing.
extern crate alloc;

// ============================================================
//  Filesystem commands (Phase 3.10)
// ============================================================
//
// These operate on the mounts brought up by `fs::boot_storage()` and use the
// kernel-internal FS path (the kernel is trusted; userspace must go through the
// capability-checked syscalls). A path under "/data" routes to the ext2 data
// volume; everything else routes to the overlay/SquashFS root.

/// Resolve a shell path to (mount_id, path-within-mount).
fn resolve_mount(path: &str) -> Option<(u64, alloc::string::String)> {
    use crate::fs;
    if path == "/data" {
        return fs::data_mount().map(|m| (m, alloc::string::String::from("/")));
    }
    if let Some(rest) = path.strip_prefix("/data/") {
        return fs::data_mount().map(|m| (m, alloc::format!("/{}", rest)));
    }
    fs::root_mount().map(|m| (m, alloc::string::String::from(path)))
}

/// `mount` — list mounted filesystems.
pub fn cmd_mount(_args: &str) {
    crate::fs::print_mount_status();
}

/// `ls <path>` — list a directory's entries.
pub fn cmd_ls(args: &str) {
    use crate::fs;
    let path = args.trim();
    let path = if path.is_empty() { "/" } else { path };
    let (mount, sub) = match resolve_mount(path) {
        Some(v) => v,
        None => {
            println!("ls: no filesystem mounted");
            return;
        }
    };
    let fd = match fs::kopen(mount, sub.as_bytes()) {
        Ok(fd) => fd,
        Err(e) => {
            println!("ls: {}: {:?}", path, e);
            return;
        }
    };
    let mut buf = alloc::vec![0u8; 8192];
    match fs::kreaddir(mount, fd, 256, &mut buf) {
        Ok(count) => {
            let mut pos = 0usize;
            for _ in 0..count {
                if pos + 4 > buf.len() {
                    break;
                }
                let nlen = u16::from_le_bytes([buf[pos], buf[pos + 1]]) as usize;
                let type_id = u16::from_le_bytes([buf[pos + 2], buf[pos + 3]]);
                pos += 4;
                if pos + nlen > buf.len() {
                    break;
                }
                let name = core::str::from_utf8(&buf[pos..pos + nlen]).unwrap_or("?");
                // Canonical readdir type codes (fs_proto::DT_*): 2 = directory.
                // Only directories get a trailing slash; files and everything
                // else print bare.
                let kind = if type_id == 2 { "/" } else { "" };
                println!("  {}{}", name, kind);
                pos += nlen;
            }
        }
        Err(e) => println!("ls: {}: {:?}", path, e),
    }
    let _ = fs::kclose(mount, fd);
}

/// `cat <path>` — print a file's contents to serial.
pub fn cmd_cat(args: &str) {
    use crate::fs;
    let path = args.trim();
    if path.is_empty() {
        println!("usage: cat <path>");
        return;
    }
    let (mount, sub) = match resolve_mount(path) {
        Some(v) => v,
        None => {
            println!("cat: no filesystem mounted");
            return;
        }
    };
    let fd = match fs::kopen(mount, sub.as_bytes()) {
        Ok(fd) => fd,
        Err(e) => {
            println!("cat: {}: {:?}", path, e);
            return;
        }
    };
    let mut off = 0u64;
    let mut chunk = alloc::vec![0u8; 1024];
    loop {
        match fs::kread(mount, fd, off, &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &chunk[..n] {
                    crate::print!("{}", b as char);
                }
                off += n as u64;
                if n < chunk.len() {
                    break;
                }
            }
            Err(e) => {
                println!("cat: {}: {:?}", path, e);
                break;
            }
        }
    }
    let _ = fs::kclose(mount, fd);
}

/// `stat <path>` — show a file's size and type.
pub fn cmd_stat(args: &str) {
    let path = args.trim();
    if path.is_empty() {
        println!("usage: stat <path>");
        return;
    }
    let (mount, sub) = match resolve_mount(path) {
        Some(v) => v,
        None => {
            println!("stat: no filesystem mounted");
            return;
        }
    };
    match crate::fs::kstat(mount, sub.as_bytes()) {
        Ok((size, is_dir)) => {
            println!("  {}: {} bytes, {}", path, size, if is_dir { "directory" } else { "file" });
        }
        Err(e) => println!("stat: {}: {:?}", path, e),
    }
}

/// `write <path> <content>` — create and write a file.
pub fn cmd_write(args: &str) {
    use crate::fs;
    let args = args.trim();
    let (path, content) = match args.split_once(' ') {
        Some((p, c)) => (p, c),
        None => {
            println!("usage: write <path> <content>");
            return;
        }
    };
    let (mount, sub) = match resolve_mount(path) {
        Some(v) => v,
        None => {
            println!("write: no filesystem mounted");
            return;
        }
    };
    let fd = match fs::kcreate(mount, sub.as_bytes()) {
        Ok(fd) => fd,
        Err(e) => {
            println!("write: {}: {:?}", path, e);
            return;
        }
    };
    match fs::kwrite(mount, fd, 0, content.as_bytes()) {
        Ok(n) => println!("  wrote {} bytes to {}", n, path),
        Err(e) => println!("write: {}: {:?}", path, e),
    }
    let _ = fs::kclose(mount, fd);
}

/// `mkdir <path>` — create a directory.
pub fn cmd_mkdir(args: &str) {
    let path = args.trim();
    if path.is_empty() {
        println!("usage: mkdir <path>");
        return;
    }
    let (mount, sub) = match resolve_mount(path) {
        Some(v) => v,
        None => {
            println!("mkdir: no filesystem mounted");
            return;
        }
    };
    match crate::fs::kmkdir(mount, sub.as_bytes()) {
        Ok(()) => println!("  created {}", path),
        Err(e) => println!("mkdir: {}: {:?}", path, e),
    }
}

/// `ifconfig` — show the network interface configuration.
///
/// Reads the live status the kernel net service maintains (sub-phase 4.4): the
/// NIC's MAC and MTU, the IPv4 address/gateway/DNS the ring-3 stack acquired via
/// DHCP, and the RX-overflow drop counter. The kernel does not parse packets — it
/// only reports what the net server told it over `MSG_CONFIG`.
pub fn cmd_ifconfig(_args: &str) {
    let st = crate::net::net_service::status();
    if !st.present {
        println!("No network interface (no NIC discovered at boot).");
        return;
    }
    println!("virtio-net0:");
    println!(
        "  MAC:  {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        st.mac[0], st.mac[1], st.mac[2], st.mac[3], st.mac[4], st.mac[5],
    );
    println!("  MTU:  {}", st.mtu);
    let c = st.config;
    if c.configured {
        println!(
            "  inet: {}.{}.{}.{}/{}",
            c.addr[0], c.addr[1], c.addr[2], c.addr[3], c.prefix,
        );
        match c.gateway {
            Some(g) => println!("  gw:   {}.{}.{}.{}", g[0], g[1], g[2], g[3]),
            None => println!("  gw:   (none)"),
        }
        match c.dns {
            Some(d) => println!("  dns:  {}.{}.{}.{}", d[0], d[1], d[2], d[3]),
            None => println!("  dns:  (none)"),
        }
    } else {
        println!("  inet: (unconfigured — awaiting DHCP)");
    }
    println!("  RX drops: {}", st.rx_dropped);
}

/// Parse a dotted-quad IPv4 address ("10.0.2.3") into octets.
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut it = s.split('.');
    for o in octets.iter_mut() {
        *o = it.next()?.parse().ok()?;
    }
    if it.next().is_some() {
        return None;
    }
    Some(octets)
}

/// `udpsend <ip> <port> <msg>` — send a UDP datagram via the net server.
///
/// A manual test for the Phase 4.5 socket path: it creates a UDP socket through
/// the kernel socket router (which routes to the ring-3 net server), binds an
/// ephemeral local port, sends `msg`, and closes. Uses the kernel-internal
/// socket ops (the shell runs in the kernel); userspace goes through the
/// capability-checked syscalls instead.
pub fn cmd_udpsend(args: &str) {
    use crate::net::socket;

    let mut parts = args.splitn(3, ' ');
    let ip_s = parts.next().unwrap_or("").trim();
    let port_s = parts.next().unwrap_or("").trim();
    let msg = parts.next().unwrap_or("");
    if ip_s.is_empty() || port_s.is_empty() || msg.is_empty() {
        println!("usage: udpsend <ip> <port> <msg>");
        return;
    }
    let ip = match parse_ipv4(ip_s) {
        Some(v) => v,
        None => {
            println!("udpsend: invalid IP '{}'", ip_s);
            return;
        }
    };
    let port: u16 = match port_s.parse() {
        Ok(p) => p,
        Err(_) => {
            println!("udpsend: invalid port '{}'", port_s);
            return;
        }
    };

    let id = match socket::ksocket_open() {
        Ok(id) => id,
        Err(e) => {
            println!("udpsend: socket: {:?}", e);
            return;
        }
    };
    if let Err(e) = socket::ksocket_bind(id, [0, 0, 0, 0], 49152) {
        println!("udpsend: bind: {:?}", e);
        let _ = socket::ksocket_close(id);
        return;
    }
    match socket::ksocket_sendto(id, msg.as_bytes(), ip, port) {
        Ok(n) => println!(
            "udpsend: sent {} bytes to {}.{}.{}.{}:{}",
            n, ip[0], ip[1], ip[2], ip[3], port
        ),
        Err(e) => println!("udpsend: send: {:?}", e),
    }
    let _ = socket::ksocket_close(id);
}

/// `tcpconnect <ip> <port>` — open a TCP connection, send a line, print the reply.
///
/// Exercises the Phase 4.6 TCP client path through the net server: create a TCP
/// socket, connect (non-blocking; poll until established or refused), send a
/// short request, and print whatever comes back. Uses the kernel-internal socket
/// ops (the shell runs in the kernel).
pub fn cmd_tcpconnect(args: &str) {
    use crate::net::socket::{self, SockError};

    let mut parts = args.splitn(2, ' ');
    let ip_s = parts.next().unwrap_or("").trim();
    let port_s = parts.next().unwrap_or("").trim();
    if ip_s.is_empty() || port_s.is_empty() {
        println!("usage: tcpconnect <ip> <port>");
        return;
    }
    let ip = match parse_ipv4(ip_s) {
        Some(v) => v,
        None => {
            println!("tcpconnect: invalid IP '{}'", ip_s);
            return;
        }
    };
    let port: u16 = match port_s.parse() {
        Ok(p) => p,
        Err(_) => {
            println!("tcpconnect: invalid port '{}'", port_s);
            return;
        }
    };

    let id = match socket::ksocket_open_tcp() {
        Ok(id) => id,
        Err(e) => {
            println!("tcpconnect: socket: {:?}", e);
            return;
        }
    };
    if let Err(e) = socket::ksocket_connect(id, ip, port) {
        println!("tcpconnect: connect: {:?}", e);
        let _ = socket::ksocket_close(id);
        return;
    }

    // Poll a zero-length send until the connection is established or refused.
    let mut established = false;
    for _ in 0..500_000 {
        match socket::ksocket_send(id, &[]) {
            Ok(_) => {
                established = true;
                break;
            }
            Err(SockError::WouldBlock) => crate::sched::yield_now(),
            Err(SockError::ConnectionRefused) => {
                println!("tcpconnect: connection refused");
                let _ = socket::ksocket_close(id);
                return;
            }
            Err(e) => {
                println!("tcpconnect: {:?}", e);
                let _ = socket::ksocket_close(id);
                return;
            }
        }
    }
    if !established {
        println!("tcpconnect: timed out connecting to {}.{}.{}.{}:{}",
            ip[0], ip[1], ip[2], ip[3], port);
        let _ = socket::ksocket_close(id);
        return;
    }
    println!("tcpconnect: connected to {}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

    // Send a minimal request line.
    let _ = socket::ksocket_send(id, b"hello\r\n");

    // Poll for a reply for a bounded time.
    let mut buf = [0u8; 256];
    for _ in 0..500_000 {
        match socket::ksocket_recv(id, &mut buf) {
            Ok(n) if n > 0 => {
                println!("tcpconnect: received {} bytes", n);
                break;
            }
            Ok(_) => break, // 0 = peer closed
            Err(SockError::WouldBlock) => crate::sched::yield_now(),
            Err(e) => {
                println!("tcpconnect: recv: {:?}", e);
                break;
            }
        }
    }
    let _ = socket::ksocket_close(id);
}

/// Human-readable name for a `SOCK_LIST` kind code (`net_proto::SLK_*`).
fn sock_kind_name(kind: u8) -> &'static str {
    match kind {
        0 => "udp",
        1 => "tcp",
        2 => "icmp",
        _ => "?",
    }
}

/// Human-readable name for a `SOCK_LIST` state code (`net_proto::SLS_*`).
fn sock_state_name(state: u8) -> &'static str {
    match state {
        0 => "-",              // UDP: connectionless
        1 => "CLOSED",
        2 => "LISTEN",
        3 => "SYN-SENT",
        4 => "SYN-RCVD",
        5 => "ESTABLISHED",
        6 => "FIN-WAIT-1",
        7 => "FIN-WAIT-2",
        8 => "CLOSE-WAIT",
        9 => "CLOSING",
        10 => "LAST-ACK",
        11 => "TIME-WAIT",
        12 => "icmp",
        _ => "?",
    }
}

/// `sockets` — list the net server's open sockets and their state.
///
/// Asks the ring-3 net server (via the kernel socket router) to enumerate its
/// socket table: each socket's id, transport, TCP state (or `-`/`icmp`), bound
/// local port, and connected peer. The kernel only relays the listing — the
/// socket state lives entirely in the net server.
pub fn cmd_sockets(_args: &str) {
    use crate::net::socket;
    let list = match socket::ksocket_list() {
        Ok(l) => l,
        Err(e) => {
            println!("sockets: {:?} (is the net server up?)", e);
            return;
        }
    };
    if list.is_empty() {
        println!("  No open sockets.");
        return;
    }
    println!("  {:>4}  {:>5}  {:>12}  {:>6}  {}", "ID", "KIND", "STATE", "LPORT", "PEER");
    println!("  {:->4}  {:->5}  {:->12}  {:->6}  {:->21}", "", "", "", "", "");
    for s in &list {
        // A zero remote address means "no connected peer".
        let peer = if s.remote_ip == [0, 0, 0, 0] && s.remote_port == 0 {
            alloc::string::String::from("-")
        } else {
            alloc::format!(
                "{}.{}.{}.{}:{}",
                s.remote_ip[0], s.remote_ip[1], s.remote_ip[2], s.remote_ip[3], s.remote_port
            )
        };
        println!(
            "  {:>4}  {:>5}  {:>12}  {:>6}  {}",
            s.id, sock_kind_name(s.kind), sock_state_name(s.state), s.local_port, peer
        );
    }
    println!("  ({} sockets)", list.len());
}

/// `ping <ip> [count]` — send ICMP echo requests and report replies.
///
/// Drives the Phase 4.7 ICMP socket path: open an ICMP socket via the net server,
/// send `count` echo requests (default 4) one at a time, and poll for each reply
/// within a ~1s window, printing the round-trip time. **Best-effort**: QEMU's
/// slirp proxies guest ICMP to host pings, which needs host raw-socket/ping
/// privileges that may be unavailable — a timeout there is an environment limit,
/// not a stack bug (`ifconfig` + `udpsend` still prove the stack is live).
pub fn cmd_ping(args: &str) {
    use crate::net::socket::{self, SockError};

    let mut parts = args.split_whitespace();
    let ip_s = parts.next().unwrap_or("").trim();
    if ip_s.is_empty() {
        println!("usage: ping <ip> [count]");
        return;
    }
    let ip = match parse_ipv4(ip_s) {
        Some(v) => v,
        None => {
            println!("ping: invalid IP '{}'", ip_s);
            return;
        }
    };
    let count: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(4);

    let id = match socket::ksocket_open_icmp() {
        Ok(id) => id,
        Err(e) => {
            println!("ping: socket: {:?}", e);
            return;
        }
    };

    println!("PING {}.{}.{}.{} ({} requests):", ip[0], ip[1], ip[2], ip[3], count);
    let mut received = 0u32;
    for seq in 0..count {
        let start = now_ms();
        if let Err(e) = socket::ksocket_ping(id, ip, seq) {
            println!("  seq={} send failed: {:?}", seq, e);
            continue;
        }
        // Poll for the echo reply for up to ~1 second.
        let mut got = false;
        loop {
            match socket::ksocket_ping_recv(id) {
                Ok((rseq, src)) => {
                    let rtt = now_ms().saturating_sub(start);
                    println!(
                        "  reply from {}.{}.{}.{}: seq={} time={}ms",
                        src[0], src[1], src[2], src[3], rseq, rtt
                    );
                    received += 1;
                    got = true;
                    break;
                }
                Err(SockError::WouldBlock) => {
                    if now_ms().saturating_sub(start) > 1000 {
                        break;
                    }
                    crate::sched::yield_now();
                }
                Err(e) => {
                    println!("  seq={} recv error: {:?}", seq, e);
                    break;
                }
            }
        }
        if !got {
            println!("  seq={} timeout", seq);
        }
    }
    let _ = socket::ksocket_close(id);
    println!("  {} transmitted, {} received", count, received);
}

/// Monotonic milliseconds since boot (the same source as `SYS_UPTIME_MS`), for
/// the `ping` round-trip timer. The 100 Hz tick gives ~10 ms resolution.
fn now_ms() -> u64 {
    crate::arch::x86_64::idt::tick_count().wrapping_mul(10)
}

/// `run` — launch a demo container (Phase 5.5).
///
/// Assembles a self-contained demo image (its `/init` entrypoint is the embedded
/// `linux-smoke` binary) onto the `/data` mount, launches it as a
/// capability-isolated Linux container, waits for it to exit, and prints the exit
/// code. This demonstrates the whole 5.0–5.5 pipeline live: unpack → assemble
/// rootfs → load the entrypoint from that rootfs → run it in ring 3. Real image
/// staging (pulling `run <image>` from a registry) arrives in Phase 5.6.
pub fn cmd_run(_args: &str) {
    use crate::container;
    let mount = match crate::fs::data_mount() {
        Some(m) => m,
        None => {
            println!("run: no writable filesystem (/data) available");
            return;
        }
    };
    println!("run: launching demo container (entrypoint /init = linux-smoke)...");
    let bundle = container::demo_bundle();
    let pid = match container::create(&bundle, mount) {
        Ok(p) => p,
        Err(e) => {
            println!("run: failed to create container: {:?}", e);
            return;
        }
    };
    container::start(pid);
    // Wait (bounded) for the container to exit, then report its status.
    for _ in 0..5_000_000 {
        if let Some(code) = crate::process::exit_status(pid) {
            println!("run: container exited with code {}", code);
            crate::process::destroy_process(pid);
            return;
        }
        crate::sched::yield_now();
    }
    println!("run: container is still running (timed out waiting for exit)");
}
