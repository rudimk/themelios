//! # Interrupt-driven serial shell
//!
//! An interactive debug shell that runs as a scheduler task. It reads input
//! from the serial port (COM1) via IRQ4 interrupts and provides commands for
//! inspecting and controlling the kernel at runtime.
//!
//! ## Architecture
//!
//! The shell task blocks (via `sched::block_current_task()`) when there's no
//! input to process. When the IRQ4 handler receives a byte, it pushes it into
//! the input ring buffer and wakes the shell task. This means the shell uses
//! zero CPU time while idle — no busy-waiting.
//!
//! ## Line editing
//!
//! The shell supports basic line editing:
//! - Printable characters are echoed and appended to the line buffer
//! - Backspace (0x7F or 0x08) deletes the last character
//! - Enter (0x0D) submits the command
//! - The line buffer is 128 bytes (longer input is silently truncated)
//!
//! ## Command dispatch
//!
//! Commands are split into a command name and an argument string. The command
//! name is matched against a static dispatch table. Unknown commands print an
//! error message suggesting `help`.

pub mod commands;
pub mod input;

use crate::println;
use crate::sched;

/// Maximum length of a shell input line.
const MAX_LINE_LEN: usize = 128;

/// Entry function for the shell task. Spawned by `init()`.
///
/// Runs an infinite loop: print prompt, read a line of input (blocking
/// until characters arrive via IRQ4), parse and dispatch the command.
fn shell_entry() {
    println!();
    println!("ThemeliOS debug shell. Type 'help' for commands.");

    let mut line_buf = [0u8; MAX_LINE_LEN];
    let mut line_len: usize;

    loop {
        // Print the shell prompt
        crate::print!("themeliOS> ");

        // Read a line of input (blocking until Enter is pressed)
        line_len = read_line(&mut line_buf);

        // Parse the line into command + args
        let line = core::str::from_utf8(&line_buf[..line_len]).unwrap_or("");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Split into command name and argument string
        let (cmd, args) = match line.find(' ') {
            Some(pos) => (&line[..pos], line[pos + 1..].trim()),
            None => (line, ""),
        };

        // Dispatch to command handler
        match cmd {
            "help" => commands::cmd_help(args),
            "mem" => commands::cmd_mem(args),
            "tasks" => commands::cmd_tasks(args),
            "spawn" => commands::cmd_spawn(args),
            "kill" => commands::cmd_kill(args),
            "peek" => commands::cmd_peek(args),
            "pgtable" => commands::cmd_pgtable(args),
            "procs" => commands::cmd_procs(args),
            "caps" => commands::cmd_caps(args),
            "audit" => commands::cmd_audit(args),
            _ => {
                println!("Unknown command: '{}'. Type 'help' for available commands.", cmd);
            }
        }
    }
}

/// Read a line of input from the serial ring buffer with line editing.
///
/// Blocks the shell task until characters are available, echoes printable
/// characters, handles backspace, and returns when Enter is pressed.
/// Returns the number of bytes in the line buffer (not including the
/// newline).
fn read_line(buf: &mut [u8; MAX_LINE_LEN]) -> usize {
    let mut pos: usize = 0;

    loop {
        // Block until input is available
        while !input::has_input() {
            sched::block_current_task();
        }

        // Drain all available bytes from the ring buffer
        while let Some(byte) = input::pop_byte() {
            match byte {
                // Enter (carriage return) — submit the line
                b'\r' | b'\n' => {
                    // Echo newline
                    println!();
                    return pos;
                }

                // Backspace (DEL or BS) — delete last character
                0x7F | 0x08 => {
                    if pos > 0 {
                        pos -= 1;
                        // Echo: move cursor back, overwrite with space, move back again
                        crate::print!("\x08 \x08");
                    }
                }

                // Printable ASCII characters — append to buffer and echo
                0x20..=0x7E => {
                    if pos < MAX_LINE_LEN {
                        buf[pos] = byte;
                        pos += 1;
                        // Echo the character
                        crate::print!("{}", byte as char);
                    }
                    // Silently drop if buffer is full
                }

                // Non-printable characters (control codes, etc.) — ignore
                _ => {}
            }
        }
    }
}

/// Initialize the shell subsystem and spawn the shell task.
///
/// Enables serial receive interrupts (IRQ4), unmasks IRQ4 in the PIC,
/// and spawns the shell task. Call this after the scheduler is initialized.
pub fn init() {
    // Enable COM1 receive interrupts so IRQ4 fires on input
    #[cfg(target_arch = "x86_64")]
    {
        crate::arch::x86_64::serial::enable_receive_interrupt();
        crate::arch::x86_64::pic::unmask(4);
    }

    // Spawn the shell task and register its ID for IRQ4 wakeup
    let shell_id = sched::spawn("shell", shell_entry);
    input::set_shell_task_id(shell_id);

    // Assign the shell task to the kernel process (PID 0)
    crate::process::assign_task_to_kernel(shell_id);

    println!("Shell initialized (task {}, IRQ4 enabled)", shell_id);
}
