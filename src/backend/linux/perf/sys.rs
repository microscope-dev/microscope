//! The perf event system calls and the ring buffers events write to, and
//! the one BPF map update aya cannot make. The profiler's only unsafe code
//! is here.

#![allow(unsafe_code, reason = "perf events are system calls and shared memory")]

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

use nix::libc;

/// `struct perf_event_attr` to its eighth version.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Attr {
    pub kind: u32,
    pub size: u32,
    pub config: u64,
    pub sample_period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub wakeup_watermark: u32,
    pub bp_type: u32,
    pub bp_addr: u64,
    pub bp_len: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clockid: i32,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub reserved_2: u16,
    pub aux_sample_size: u32,
    pub aux_action: u32,
    pub sig_data: u64,
    pub config3: u64,
}

/// The size `perf_event_open` takes for [`Attr`].
pub const ATTR_SIZE: u32 = 136;
const _: () = assert!(std::mem::size_of::<Attr>() == ATTR_SIZE as usize);

pub const TYPE_SOFTWARE: u32 = 1;

pub const TYPE_BREAKPOINT: u32 = 5;

pub const COUNT_SW_CPU_CLOCK: u64 = 0;

/// An event that counts nothing, for the records of mappings and threads.
pub const COUNT_SW_DUMMY: u64 = 9;

/// An event BPF programs write records through.
pub const COUNT_SW_BPF_OUTPUT: u64 = 10;

/// A write breakpoint, which only the simulator's conformance tests set.
#[cfg(test)]
pub const BREAKPOINT_WRITE: u32 = 2;

pub const BREAKPOINT_EXECUTE: u32 = 4;

/// `perf_event_attr` flag bits.
pub mod flag {
    pub const DISABLED: u64 = 1 << 0;

    pub const INHERIT: u64 = 1 << 1;

    pub const EXCLUDE_KERNEL: u64 = 1 << 5;

    pub const EXCLUDE_HV: u64 = 1 << 6;

    pub const MMAP: u64 = 1 << 8;

    pub const COMM: u64 = 1 << 9;

    pub const ENABLE_ON_EXEC: u64 = 1 << 12;

    pub const TASK: u64 = 1 << 13;

    pub const WATERMARK: u64 = 1 << 14;

    pub const SAMPLE_ID_ALL: u64 = 1 << 18;

    pub const EXCLUDE_CALLCHAIN_KERNEL: u64 = 1 << 21;

    pub const MMAP2: u64 = 1 << 23;

    pub const COMM_EXEC: u64 = 1 << 24;

    pub const USE_CLOCKID: u64 = 1 << 25;

    pub const CONTEXT_SWITCH: u64 = 1 << 26;
}

/// `perf_event_attr.sample_type` bits.
pub mod sample {
    pub const TID: u64 = 1 << 1;

    pub const TIME: u64 = 1 << 2;

    pub const CALLCHAIN: u64 = 1 << 5;

    pub const CPU: u64 = 1 << 7;

    pub const PERIOD: u64 = 1 << 8;

    pub const RAW: u64 = 1 << 10;

    pub const REGS_USER: u64 = 1 << 12;

    pub const STACK_USER: u64 = 1 << 13;

    pub const IDENTIFIER: u64 = 1 << 16;
}

const FLAG_FD_CLOEXEC: libc::c_ulong = 1 << 3;
const IOC_ENABLE: libc::Ioctl = 0x2400;
const IOC_DISABLE: libc::Ioctl = 0x2401;
const IOC_SET_OUTPUT: libc::Ioctl = 0x2405;

/// `_IOR('$', 7, __u64 *)`.
const IOC_ID: libc::Ioctl = 0x8008_2407_u32 as libc::Ioctl;

/// `_IOW('$', 8, __u32)`.
const IOC_SET_BPF: libc::Ioctl = 0x4004_2408;

const BPF_MAP_UPDATE_ELEM: libc::c_long = 2;

/// `read_format`: each read also gives how many samples the event lost.
pub const FORMAT_LOST: u64 = 1 << 4;

/// An event's count, with those of the events inherited from it, and what
/// it has lost: every sample it and they found no room for, whether or not
/// a loss record has said so yet. The event reads [`FORMAT_LOST`].
pub fn counts(fd: &OwnedFd) -> std::io::Result<(u64, u64)> {
    let mut bytes = [0_u8; 16];
    let read = rustix::io::read(fd, &mut bytes[..])?;
    if read != bytes.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("a perf event's counts read {read} of 16 bytes"),
        ));
    }

    let [
        c0,
        c1,
        c2,
        c3,
        c4,
        c5,
        c6,
        c7,
        l0,
        l1,
        l2,
        l3,
        l4,
        l5,
        l6,
        l7,
    ] = bytes;
    Ok((
        u64::from_ne_bytes([c0, c1, c2, c3, c4, c5, c6, c7]),
        u64::from_ne_bytes([l0, l1, l2, l3, l4, l5, l6, l7]),
    ))
}

/// Opens an event for `pid` on `cpu`.
pub fn open(attr: &Attr, pid: i32, cpu: i32) -> std::io::Result<OwnedFd> {
    // The kernel writes its own size into the attributes it was given when
    // it refuses them as too large, so it gets a copy of its own.
    let mut attr = *attr;

    // SAFETY: `attr` is a valid, initialized perf_event_attr of the size
    // it declares, which the call may write and which outlives it. The
    // integers are passed as the `long`s the variadic call reads.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            std::ptr::from_mut(&mut attr),
            libc::c_long::from(pid),
            libc::c_long::from(cpu),
            libc::c_long::from(-1_i32),
            FLAG_FD_CLOEXEC,
        )
    };

    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let fd = i32::try_from(fd).map_err(|_| std::io::Error::from_raw_os_error(libc::EBADF))?;

    // SAFETY: the kernel just returned this descriptor, which nothing else
    // owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Enables or disables an event.
pub fn enable(fd: &OwnedFd, enabled: bool) -> std::io::Result<()> {
    let request = if enabled { IOC_ENABLE } else { IOC_DISABLE };

    // SAFETY: the request takes an integer argument, and the descriptor is
    // a perf event this process owns.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), request, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// Sends an event's records to the ring of `ring`'s event, which must be
/// on the same CPU.
pub fn set_output(fd: &OwnedFd, ring: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: the request takes the descriptor of another perf event, and
    // both descriptors are perf events this process owns.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), IOC_SET_OUTPUT, ring.as_raw_fd()) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// The kernel's identifier of an event, which each of its samples that
/// asks for it begins with.
pub fn id(fd: &OwnedFd) -> std::io::Result<u64> {
    let mut id = 0_u64;

    // SAFETY: the request writes one u64 through the pointer, which points
    // at a u64 that outlives the call; the descriptor is a perf event this
    // process owns.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), IOC_ID, std::ptr::from_mut(&mut id)) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(id)
}

/// Runs the BPF program `program` as the event takes each sample, before
/// the kernel writes it. Events inherited from the event later run it too.
pub fn set_bpf(fd: &OwnedFd, program: BorrowedFd<'_>) -> std::io::Result<()> {
    // SAFETY: the request takes a program's descriptor as an integer, and
    // both descriptors are open and this process's.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), IOC_SET_BPF, program.as_raw_fd()) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// `union bpf_attr` as `BPF_MAP_UPDATE_ELEM` reads it.
#[repr(C)]
struct MapUpdate {
    map_fd: u32,
    pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

/// Puts the event `event` in slot `slot` of the perf event array `map`, so
/// that a program's output for that CPU goes through it.
pub fn set_output_slot(map: BorrowedFd<'_>, slot: u32, event: &OwnedFd) -> std::io::Result<()> {
    let value = u32::try_from(event.as_raw_fd()).map_err(|_| std::io::Error::other("fd"))?;
    let attr = MapUpdate {
        map_fd: u32::try_from(map.as_raw_fd()).map_err(|_| std::io::Error::other("fd"))?,
        pad: 0,
        key: std::ptr::from_ref(&slot) as u64,
        value: std::ptr::from_ref(&value) as u64,
        flags: 0,
    };

    // SAFETY: `attr` is the update the call reads, of the size given, and
    // its key and value point at u32s, as a perf event array's are, that
    // outlive the call. The kernel only reads them.
    let result = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_MAP_UPDATE_ELEM,
            std::ptr::from_ref(&attr),
            std::mem::size_of::<MapUpdate>(),
        )
    };

    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// An event's ring buffer: one metadata page, then a power of two of data
/// pages the kernel writes records to.
#[derive(Debug)]
pub struct Ring {
    fd: OwnedFd,
    memory: RingMemory,
}

impl Ring {
    /// Maps `pages` data pages, a power of two, for an event.
    pub fn map(fd: OwnedFd, pages: usize) -> std::io::Result<Self> {
        let page = page_size();
        let length = (pages + 1) * page;

        // SAFETY: a fresh shared mapping of the event's descriptor, which
        // the kernel sizes and owns; nothing else refers to it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };

        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        let base = std::ptr::NonNull::new(base.cast::<u8>())
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOMEM))?;
        Ok(Self {
            fd,
            memory: RingMemory {
                base,
                data: page,
                size: pages * page,
                owner: Owner::Mapping { length },
                untrusted: 0,
            },
        })
    }

    pub const fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    /// Appends every record the kernel has written since the last drain
    /// to `out` and returns their bytes to the kernel.
    pub fn drain(&mut self, out: &mut Vec<u8>) -> usize {
        self.memory.drain(out)
    }

    /// How many times the ring's head could not be trusted, so that the
    /// ring skipped what lay before it.
    pub const fn untrusted(&self) -> u64 {
        self.memory.untrusted()
    }
}

/// The memory a ring shares with the kernel: a metadata page whose words
/// at [`DATA_HEAD`] and [`DATA_TAIL`] say what it holds, then its data,
/// a power of two of bytes.
#[derive(Debug)]
pub struct RingMemory {
    base: std::ptr::NonNull<u8>,

    /// Where the data begins.
    data: usize,
    size: usize,
    owner: Owner,

    /// Heads [`RingMemory::drain`] could not trust.
    untrusted: u64,
}

/// What a ring's memory is, and so how it is freed.
#[derive(Debug)]
enum Owner {
    /// The kernel's mapping, of this many bytes.
    Mapping { length: usize },

    /// A block from [`RingMemory::heap`].
    Heap(std::ptr::NonNull<[AtomicU64]>),
}

#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the owner's pointer is the ring's own memory, as `base` is"
)]
// SAFETY: the memory belongs to the ring alone, and every access to it
// from this side goes through `&mut self`.
unsafe impl Send for RingMemory {}

/// `perf_event_mmap_page` offsets.
pub const DATA_HEAD: usize = 1024;

pub const DATA_TAIL: usize = 1032;

/// Where a heap ring's data begins: just past its metadata words.
pub const HEAP_DATA: usize = DATA_TAIL + 8;

impl RingMemory {
    /// A ring over a heap block of [`HEAP_DATA`] bytes of metadata, then
    /// `size` bytes of data, a power of two, all zero. What the kernel
    /// does to a ring, a test does through the returned pointer to the
    /// block's start, with the same orderings.
    #[doc(hidden)]
    #[must_use]
    pub fn heap(size: usize) -> (Self, std::ptr::NonNull<u8>) {
        assert!(size.is_power_of_two(), "a ring holds a power of two");
        let words = (HEAP_DATA + size).div_ceil(8);
        let block: Box<[AtomicU64]> = (0..words).map(|_| AtomicU64::new(0)).collect();
        let block = std::ptr::NonNull::from(Box::leak(block));
        let base = block.cast::<u8>();
        let memory = Self {
            base,
            data: HEAP_DATA,
            size,
            owner: Owner::Heap(block),
            untrusted: 0,
        };

        (memory, base)
    }

    #[expect(
        clippy::cast_ptr_alignment,
        reason = "the metadata is page or word aligned, and its words at multiples of eight"
    )]
    const fn word(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: the metadata is the ring's for its life, the word is
        // aligned, and the kernel accesses it atomically too.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(offset).cast::<u64>()) }
    }

    /// Appends every record the kernel has written since the last drain
    /// to `out` and returns their bytes to the kernel. Records are whole:
    /// the kernel publishes its head only after a record is complete.
    ///
    /// A head more than a whole ring past the tail, or behind it, is not
    /// one the kernel and this reader could have left. The ring reads
    /// nothing of it, moves its tail to the head so that the kernel can go
    /// on writing, and counts it in [`RingMemory::untrusted`].
    pub fn drain(&mut self, out: &mut Vec<u8>) -> usize {
        let head = self.word(DATA_HEAD).load(Ordering::Acquire);
        let tail = self.word(DATA_TAIL).load(Ordering::Relaxed);
        let size = self.size;
        let available = usize::try_from(head.wrapping_sub(tail)).unwrap_or(usize::MAX);
        if available == 0 {
            return 0;
        }

        if available > size {
            self.untrusted += 1;
            self.word(DATA_TAIL).store(head, Ordering::Release);
            return 0;
        }

        let Ok(start) = usize::try_from(tail % size as u64) else {
            return 0;
        };

        let first = available.min(size - start);
        out.reserve(available);

        // SAFETY: [tail, head) lies inside the data, which the kernel does
        // not write until the tail passes it; the destination has the
        // capacity just reserved, and the two never overlap.
        unsafe {
            let data = self.base.as_ptr().add(self.data);
            let destination = out.as_mut_ptr().add(out.len());
            std::ptr::copy_nonoverlapping(data.add(start), destination, first);
            std::ptr::copy_nonoverlapping(data, destination.add(first), available - first);
            out.set_len(out.len() + available);
        }

        self.word(DATA_TAIL).store(head, Ordering::Release);
        available
    }

    /// How many heads [`RingMemory::drain`] could not trust.
    #[must_use]
    pub const fn untrusted(&self) -> u64 {
        self.untrusted
    }
}

impl Drop for RingMemory {
    fn drop(&mut self) {
        match self.owner {
            // SAFETY: the mapping was made with this address and length
            // and is unmapped once, here.
            Owner::Mapping { length } => unsafe {
                libc::munmap(self.base.as_ptr().cast(), length);
            },
            // SAFETY: the block was leaked from a box and is taken back
            // once, here.
            Owner::Heap(block) => unsafe {
                drop(Box::from_raw(block.as_ptr()));
            },
        }
    }
}

/// The system's page size.
pub fn page_size() -> usize {
    rustix::param::page_size()
}

/// Whether `fd` is readable now, as a process descriptor is once its
/// process has exited.
pub fn readable(fd: &OwnedFd) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec};
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    rustix::event::poll(&mut fds, Some(&now)).is_ok_and(|ready| ready > 0)
        && fds[0].revents().contains(PollFlags::IN)
}

/// Waits until `fd` is readable, as a process's descriptor is once it
/// has exited.
pub fn wait_readable(fd: &OwnedFd) {
    use rustix::event::{PollFd, PollFlags};
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    while rustix::event::poll(&mut fds, None) == Err(rustix::io::Errno::INTR) {}
}

/// Waits until one of `rings` has data past its watermark, or `timeout`.
pub fn poll(rings: &[Ring], timeout: std::time::Duration) {
    use rustix::event::{PollFd, PollFlags, Timespec};
    let mut fds: Vec<PollFd<'_>> = rings
        .iter()
        .map(|ring| PollFd::new(&ring.fd, PollFlags::IN))
        .collect();
    let timeout = Timespec::try_from(timeout).ok();
    let _ = rustix::event::poll(&mut fds, timeout.as_ref());
}

/// The time-stamp counter.
pub fn tsc() -> u64 {
    // SAFETY: RDTSC reads no memory. Every x86-64 processor has it; a
    // process that `PR_SET_TSC` forbade it faults, which microscope never
    // asks for and is not undefined behavior.
    unsafe { core::arch::x86_64::_rdtsc() }
}
