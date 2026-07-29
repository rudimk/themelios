//! # ELF64 loader (Phase 5.0)
//!
//! Loads a statically-linked `ET_EXEC` ELF64 image into a process's address
//! space and builds the SysV/Linux initial process stack, so the kernel can run
//! real ELF programs (not just the flat binaries the Phase 3/4 servers use).
//!
//! ## Scope for 5.0
//!
//! - **`ET_EXEC` only** (fixed load addresses). Static-PIE (`ET_DYN`) is deferred
//!   — it needs `R_X86_64_RELATIVE` relocations applied at load time; build test
//!   binaries `-no-pie -static` to stay `ET_EXEC`.
//! - The loader reads the image through a [`ByteSource`], so the same code serves
//!   an embedded `&[u8]` (the 5.0 test) and, later, a file on the container rootfs
//!   (5.5) — no rewrite when the network/FS path arrives.
//! - Segments are mapped **W^X**: writable iff `PF_W`, executable iff `PF_X`
//!   (`NO_EXECUTE` otherwise). A flat binary could not enforce this; an ELF can.
//!
//! The Linux *syscall* personality is 5.1 — 5.0 proves the loader with a program
//! that speaks the native ThemeliOS ABI, so loader bugs are isolated from
//! Linux-ABI bugs.

use crate::mm::addr::VirtAddr;
use crate::mm::page_table::PageFlags;
use crate::process::{self, ProcessId};
use alloc::vec::Vec;

extern crate alloc;

// --- ELF constants (see the System V ABI / ELF-64 spec) ---

/// `\x7fELF` — the 4-byte magic at the start of every ELF file.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `EI_CLASS` value for 64-bit objects.
const ELFCLASS64: u8 = 2;
/// `EI_DATA` value for little-endian objects.
const ELFDATA2LSB: u8 = 1;
/// `e_type` for an executable (fixed-address) object.
const ET_EXEC: u16 = 2;
/// `e_machine` for x86-64.
const EM_X86_64: u16 = 0x3e;
/// Program-header `p_type` for a loadable segment.
const PT_LOAD: u32 = 1;
/// `p_flags` bit: segment is executable.
const PF_X: u32 = 1;
/// `p_flags` bit: segment is writable.
const PF_W: u32 = 2;

/// Elf64 header size (bytes) — we read exactly this many for the header.
const EHDR_LEN: usize = 64;
/// Elf64 program-header size (bytes).
const PHDR_LEN: usize = 56;

// --- auxv keys (a subset — the ones a static musl `_start` consults) ---
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;
const AT_HWCAP: u64 = 16;
const AT_RANDOM: u64 = 25;

/// Top of the ring-3 stack for a loaded ELF process. Matches the servers' stack
/// top (`SERVER_STACK_TOP`) — the canonical high-canonical user stack address.
const ELF_STACK_TOP: u64 = 0x0000_7FFF_FFF0_0000;
/// Stack size in pages (128 KiB). The initial argv/envp/auxv block lives entirely
/// in the top page; the rest is runtime stack.
const ELF_STACK_PAGES: usize = 32;

/// Errors from parsing or loading an ELF image. Returned rather than panicking so
/// a malformed image is a clean failure, never a kernel fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// The source is shorter than a required structure (header/phdr/segment).
    TooSmall,
    /// Missing the `\x7fELF` magic.
    BadMagic,
    /// Not a 64-bit little-endian object.
    NotElf64,
    /// Not `ET_EXEC` (static-PIE and relocatable objects are out of scope).
    NotExec,
    /// Not an x86-64 object.
    NotX86_64,
    /// `e_phentsize` is not the expected 56 bytes, or `e_phnum` is unreasonable.
    BadProgramHeaders,
    /// No `PT_LOAD` segment — nothing to run.
    NoLoadable,
    /// Ran out of physical frames while mapping.
    OutOfFrames,
    /// The target process has no address space.
    NoAddressSpace,
}

/// A source of ELF bytes. Abstracts "read `buf.len()` bytes at `off`" so the
/// loader works from an embedded slice (5.0) or a rootfs file (5.5) unchanged.
pub trait ByteSource {
    /// Total length of the image in bytes.
    fn len(&self) -> usize;
    /// Read exactly `buf.len()` bytes starting at `off`, or `TooSmall` if the
    /// range runs past the end.
    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<(), ElfError>;
}

/// A [`ByteSource`] over an in-memory slice (embedded test images).
pub struct SliceSource<'a>(pub &'a [u8]);

impl ByteSource for SliceSource<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<(), ElfError> {
        let end = off.checked_add(buf.len()).ok_or(ElfError::TooSmall)?;
        if end > self.0.len() {
            return Err(ElfError::TooSmall);
        }
        buf.copy_from_slice(&self.0[off..end]);
        Ok(())
    }
}

// --- little-endian field readers over a fixed byte buffer ---

fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn rd_u64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

/// One parsed `PT_LOAD` program header (the fields the loader needs).
struct LoadSegment {
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

/// The result of loading an image: where to enter and the auxv values that
/// describe the loaded program headers to the C runtime.
pub struct LoadedImage {
    /// The program entry point (`e_entry`).
    pub entry: u64,
    /// Virtual address of the program-header table in the loaded image (for
    /// `AT_PHDR`). 0 if it fell outside every `PT_LOAD` segment.
    pub phdr_vaddr: u64,
    /// `AT_PHENT` — size of one program header.
    pub phentsize: u64,
    /// `AT_PHNUM` — number of program headers.
    pub phnum: u64,
}

/// Parse and load an `ET_EXEC` ELF64 from `src` into process `pid`'s address
/// space, mapping every `PT_LOAD` segment W^X. Returns the entry point and the
/// program-header info needed to build the initial stack.
pub fn load_into(pid: ProcessId, src: &dyn ByteSource) -> Result<LoadedImage, ElfError> {
    // --- Read and validate the ELF header ---
    let mut eh = [0u8; EHDR_LEN];
    src.read_at(0, &mut eh)?;
    if eh[0..4] != ELF_MAGIC {
        return Err(ElfError::BadMagic);
    }
    if eh[4] != ELFCLASS64 || eh[5] != ELFDATA2LSB {
        return Err(ElfError::NotElf64);
    }
    let e_type = rd_u16(&eh, 16);
    let e_machine = rd_u16(&eh, 18);
    if e_machine != EM_X86_64 {
        return Err(ElfError::NotX86_64);
    }
    if e_type != ET_EXEC {
        return Err(ElfError::NotExec);
    }
    let e_entry = rd_u64(&eh, 24);
    let e_phoff = rd_u64(&eh, 32);
    let e_phentsize = rd_u16(&eh, 54) as usize;
    let e_phnum = rd_u16(&eh, 56) as usize;
    // Reject anything we don't understand rather than trusting it: a fixed phdr
    // size and a sane count (a 4 KiB table is already 73 headers).
    if e_phentsize != PHDR_LEN || e_phnum == 0 || e_phnum > 64 {
        return Err(ElfError::BadProgramHeaders);
    }

    // --- Read the program headers and collect the loadable segments ---
    let mut segments: Vec<LoadSegment> = Vec::new();
    let mut phdr_vaddr = 0u64;
    for i in 0..e_phnum {
        let mut ph = [0u8; PHDR_LEN];
        src.read_at(e_phoff as usize + i * PHDR_LEN, &mut ph)?;
        let p_type = rd_u32(&ph, 0);
        if p_type != PT_LOAD {
            continue;
        }
        let seg = LoadSegment {
            flags: rd_u32(&ph, 4),
            offset: rd_u64(&ph, 8),
            vaddr: rd_u64(&ph, 16),
            filesz: rd_u64(&ph, 32),
            memsz: rd_u64(&ph, 40),
        };
        // The program-header table lives inside whichever PT_LOAD covers e_phoff
        // (normally the first, which maps file offset 0). Record its vaddr so we
        // can hand the runtime a valid AT_PHDR.
        if e_phoff >= seg.offset && e_phoff < seg.offset + seg.filesz {
            phdr_vaddr = seg.vaddr + (e_phoff - seg.offset);
        }
        segments.push(seg);
    }
    if segments.is_empty() {
        return Err(ElfError::NoLoadable);
    }

    for seg in &segments {
        map_segment(pid, src, seg)?;
    }

    Ok(LoadedImage {
        entry: e_entry,
        phdr_vaddr,
        phentsize: PHDR_LEN as u64,
        phnum: e_phnum as u64,
    })
}

/// Map one `PT_LOAD` segment: allocate a frame per page it spans, zero it (so the
/// `.bss` tail past `p_filesz` starts zeroed), copy the file bytes that fall in
/// the page, and map it with W^X permissions derived from `p_flags`.
fn map_segment(pid: ProcessId, src: &dyn ByteSource, seg: &LoadSegment) -> Result<(), ElfError> {
    let page = crate::mm::PAGE_SIZE;
    let vstart = seg.vaddr;
    let vend = seg.vaddr + seg.memsz;
    // Page-aligned span [first_page, last_page) covering the whole segment.
    let first_page = vstart & !(page - 1);
    let last_page = (vend + page - 1) & !(page - 1);

    // W^X permissions: writable iff PF_W, executable iff PF_X.
    let mut flags = PageFlags::PRESENT | PageFlags::USER;
    if seg.flags & PF_W != 0 {
        flags = flags | PageFlags::WRITABLE;
    }
    if seg.flags & PF_X == 0 {
        flags = flags | PageFlags::NO_EXECUTE;
    }

    let mut v = first_page;
    while v < last_page {
        let phys = crate::mm::frame::allocate_frame().ok_or(ElfError::OutOfFrames)?;
        // Zero the whole frame first (covers the .bss portion of this page).
        // SAFETY: freshly allocated, HHDM-mapped, exclusively owned frame.
        unsafe {
            core::ptr::write_bytes(phys.to_virt().as_mut_ptr::<u8>(), 0, page as usize);
        }

        // Copy the file-backed bytes that intersect this page, if any.
        let page_lo = v;
        let page_hi = v + page;
        let file_hi = vstart + seg.filesz;
        let copy_lo = page_lo.max(vstart);
        let copy_hi = page_hi.min(file_hi);
        if copy_hi > copy_lo {
            let dst_off = (copy_lo - page_lo) as usize;
            let src_off = (seg.offset + (copy_lo - vstart)) as usize;
            let n = (copy_hi - copy_lo) as usize;
            // SAFETY: dst_off + n <= page; the frame is HHDM-mapped and ours.
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    phys.to_virt().as_mut_ptr::<u8>().add(dst_off),
                    n,
                )
            };
            src.read_at(src_off, dst)?;
        }

        process::with_address_space(pid, |a| {
            a.map_page(VirtAddr::new(v), phys, flags);
        })
        .ok_or(ElfError::NoAddressSpace)?;

        v += page;
    }
    Ok(())
}

/// Build the SysV/Linux initial process stack for a freshly-loaded image and
/// return the entry `%rsp` (16-byte aligned, pointing at `argc`).
///
/// Stack layout at `_start` (low → high address), per the x86-64 System V ABI:
/// ```text
/// rsp → argc
///       argv[0] … argv[argc-1], NULL
///       envp[0] … NULL
///       auxv[0].{a_type,a_val} … {AT_NULL,0}
///       (alignment padding)
///       argv/envp string data, AT_RANDOM bytes   (near the top of the stack)
/// ```
/// Everything fits comfortably in the top stack page, which we write through the
/// frame's HHDM alias (the process's own address space isn't active here).
pub fn build_initial_stack(
    pid: ProcessId,
    img: &LoadedImage,
    args: &[&str],
    envs: &[&str],
) -> Result<u64, ElfError> {
    let page = crate::mm::PAGE_SIZE;

    // Allocate + map the stack; remember the top page's frame so we can write the
    // initial data into it via the HHDM.
    let mut top_frame = None;
    for i in 0..ELF_STACK_PAGES {
        let phys = crate::mm::frame::allocate_frame().ok_or(ElfError::OutOfFrames)?;
        // SAFETY: fresh HHDM-mapped frame, exclusively owned.
        unsafe {
            core::ptr::write_bytes(phys.to_virt().as_mut_ptr::<u8>(), 0, page as usize);
        }
        // Page i sits at ELF_STACK_TOP - (ELF_STACK_PAGES - i) * page; the last
        // iteration (i == PAGES-1) is the top page just below ELF_STACK_TOP.
        let v = ELF_STACK_TOP - ((ELF_STACK_PAGES - i) as u64) * page;
        process::with_address_space(pid, |a| {
            a.map_page(
                VirtAddr::new(v),
                phys,
                PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXECUTE,
            );
        })
        .ok_or(ElfError::NoAddressSpace)?;
        if i == ELF_STACK_PAGES - 1 {
            top_frame = Some((v, phys));
        }
    }
    let (top_page_base, top_phys) = top_frame.ok_or(ElfError::OutOfFrames)?;
    let base_ptr = top_phys.to_virt().as_mut_ptr::<u8>();

    // Write `bytes` at user virtual address `va` (which must lie in the top page).
    // SAFETY (all uses): va is within [top_page_base, ELF_STACK_TOP); base_ptr is
    // the HHDM alias of that page; every write stays in-bounds by construction.
    let put = |va: u64, bytes: &[u8]| unsafe {
        let off = (va - top_page_base) as usize;
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), base_ptr.add(off), bytes.len());
    };
    let put_u64 = |va: u64, val: u64| unsafe {
        let off = (va - top_page_base) as usize;
        (base_ptr.add(off) as *mut u64).write_unaligned(val);
    };

    // 1. Push argv/envp strings and the 16 AT_RANDOM bytes near the top,
    //    descending. Track each string's user address for the pointer arrays.
    let mut sp = ELF_STACK_TOP;
    let mut argv_ptrs: Vec<u64> = Vec::with_capacity(args.len());
    for a in args {
        let bytes = a.as_bytes();
        sp -= bytes.len() as u64 + 1; // +1 for the NUL terminator
        put(sp, bytes);
        put(sp + bytes.len() as u64, &[0]);
        argv_ptrs.push(sp);
    }
    let mut envp_ptrs: Vec<u64> = Vec::with_capacity(envs.len());
    for e in envs {
        let bytes = e.as_bytes();
        sp -= bytes.len() as u64 + 1;
        put(sp, bytes);
        put(sp + bytes.len() as u64, &[0]);
        envp_ptrs.push(sp);
    }
    // AT_RANDOM: 16 bytes the C runtime uses to seed the stack canary. A fixed
    // pattern is fine here (no getrandom source in 5.0; 5.1 wires the real one).
    sp -= 16;
    let random_ptr = sp;
    put(sp, &[
        0x5a, 0x3c, 0x11, 0x9e, 0x7d, 0x42, 0xa8, 0x63,
        0x04, 0xf1, 0x2b, 0x99, 0xc7, 0x6e, 0x85, 0x30,
    ]);
    // Keep everything below 8-byte-aligned so the pointer vector is aligned.
    sp &= !0xf;

    // 2. Compute the vector size and lay it out so the final rsp is 16-aligned.
    let auxv: [(u64, u64); 7] = [
        (AT_PHDR, img.phdr_vaddr),
        (AT_PHENT, img.phentsize),
        (AT_PHNUM, img.phnum),
        (AT_ENTRY, img.entry),
        (AT_PAGESZ, page),
        (AT_RANDOM, random_ptr),
        (AT_HWCAP, 0),
    ];
    // Entries: argc + argv + NULL + envp + NULL + auxv*2 + {AT_NULL,0}.
    let nwords = 1 + argv_ptrs.len() + 1 + envp_ptrs.len() + 1 + auxv.len() * 2 + 2;
    let vec_bytes = (nwords * 8) as u64;
    let mut rsp = sp - vec_bytes;
    rsp &= !0xf; // 16-byte aligned entry %rsp (the ABI contract at _start)

    // 3. Write the vector upward from rsp.
    let mut w = rsp;
    put_u64(w, args.len() as u64);
    w += 8;
    for &p in &argv_ptrs {
        put_u64(w, p);
        w += 8;
    }
    put_u64(w, 0); // argv NULL terminator
    w += 8;
    for &p in &envp_ptrs {
        put_u64(w, p);
        w += 8;
    }
    put_u64(w, 0); // envp NULL terminator
    w += 8;
    for (k, val) in auxv {
        put_u64(w, k);
        w += 8;
        put_u64(w, val);
        w += 8;
    }
    put_u64(w, AT_NULL);
    w += 8;
    put_u64(w, 0);

    Ok(rsp)
}
