#![allow(unsafe_code, reason = "perf events are system calls and shared memory")]

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

use nix::libc;

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

pub const ATTR_SIZE: u32 = 136;
const _: () = assert!(std::mem::size_of::<Attr>() == ATTR_SIZE as usize);

pub const TYPE_SOFTWARE: u32 = 1;

pub const TYPE_BREAKPOINT: u32 = 5;

pub const COUNT_SW_CPU_CLOCK: u64 = 0;

pub const COUNT_SW_DUMMY: u64 = 9;

pub const COUNT_SW_BPF_OUTPUT: u64 = 10;

#[cfg(test)]
pub const BREAKPOINT_WRITE: u32 = 2;

pub const BREAKPOINT_EXECUTE: u32 = 4;

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

const IOC_ID: libc::Ioctl = 0x8008_2407_u32 as libc::Ioctl;

const IOC_SET_BPF: libc::Ioctl = 0x4004_2408;

const BPF_MAP_UPDATE_ELEM: libc::c_long = 2;

pub const FORMAT_LOST: u64 = 1 << 4;

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

pub fn open(attr: &Attr, pid: i32, cpu: i32) -> std::io::Result<OwnedFd> {
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

#[repr(C)]
#[derive(Debug, Default)]
struct ProcmapQuery {
    size: u64,
    query_flags: u64,
    query_addr: u64,
    vma_start: u64,
    vma_end: u64,
    vma_flags: u64,
    vma_page_size: u64,
    vma_offset: u64,
    inode: u64,
    dev_major: u32,
    dev_minor: u32,
    vma_name_size: u32,
    build_id_size: u32,
    vma_name_addr: u64,
    build_id_addr: u64,
}

const PROCMAP_QUERY: libc::Ioctl = 0xC068_6611_u32 as libc::Ioctl;

const VMA_WRITABLE: u64 = 0x02;
const VMA_EXECUTABLE: u64 = 0x04;

pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub inode: u64,
    pub writable: bool,

    pub name: Vec<u8>,
}

pub fn executable_vma(maps: &std::fs::File, address: u64) -> std::io::Result<Option<Vma>> {
    let mut name = vec![0_u8; 4096];
    let mut query = ProcmapQuery {
        size: std::mem::size_of::<ProcmapQuery>() as u64,
        query_flags: VMA_EXECUTABLE,
        query_addr: address,
        vma_name_size: u32::try_from(name.len()).unwrap_or(u32::MAX),
        vma_name_addr: name.as_mut_ptr() as u64,
        ..ProcmapQuery::default()
    };

    // SAFETY: the request reads and writes `query`, which is the kernel's
    // struct and says its own size, and writes at most `vma_name_size`
    // bytes to `name`, which outlives the call.
    let result = unsafe { libc::ioctl(maps.as_raw_fd(), PROCMAP_QUERY, &raw mut query) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err(error),
        };
    }

    name.truncate(query.vma_name_size.saturating_sub(1) as usize);
    Ok(Some(Vma {
        start: query.vma_start,
        end: query.vma_end,
        offset: query.vma_offset,
        inode: query.inode,
        writable: query.vma_flags & VMA_WRITABLE != 0,
        name,
    }))
}

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

pub fn set_output(fd: &OwnedFd, ring: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: the request takes the descriptor of another perf event, and
    // both descriptors are perf events this process owns.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), IOC_SET_OUTPUT, ring.as_raw_fd()) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

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

pub fn set_bpf(fd: &OwnedFd, program: BorrowedFd<'_>) -> std::io::Result<()> {
    // SAFETY: the request takes a program's descriptor as an integer, and
    // both descriptors are open and this process's.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), IOC_SET_BPF, program.as_raw_fd()) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

#[repr(C)]
struct MapUpdate {
    map_fd: u32,
    pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

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

#[derive(Debug)]
pub struct Ring {
    fd: OwnedFd,
    memory: std::sync::Mutex<RingMemory>,
}

impl Ring {
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
            memory: std::sync::Mutex::new(RingMemory {
                base,
                data: page,
                size: pages * page,
                owner: Owner::Mapping { length },
                untrusted: 0,
            }),
        })
    }

    pub const fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    pub fn drain(&self, out: &mut Vec<u8>) -> usize {
        self.memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(out)
    }

    #[cfg(test)]
    pub fn unread(&self) -> u64 {
        self.memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unread()
    }

    pub fn untrusted(&self) -> u64 {
        self.memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .untrusted()
    }
}

#[derive(Debug)]
pub struct RingMemory {
    base: std::ptr::NonNull<u8>,

    data: usize,
    size: usize,
    owner: Owner,

    untrusted: u64,
}

#[derive(Debug)]
enum Owner {
    Mapping { length: usize },

    Heap(std::ptr::NonNull<[AtomicU64]>),
}

#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the owner's pointer is the ring's own memory, as `base` is"
)]
// SAFETY: the memory belongs to the ring alone, and every access to it
// from this side goes through `&mut self`.
unsafe impl Send for RingMemory {}

pub const DATA_HEAD: usize = 1024;

pub const DATA_TAIL: usize = 1032;

pub const HEAP_DATA: usize = DATA_TAIL + 8;

impl RingMemory {
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

    #[cfg(test)]
    fn unread(&self) -> u64 {
        let head = self.word(DATA_HEAD).load(Ordering::Acquire);
        head.wrapping_sub(self.word(DATA_TAIL).load(Ordering::Relaxed))
    }

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

pub fn page_size() -> usize {
    rustix::param::page_size()
}

pub fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

pub fn thread_cpu() -> std::time::Duration {
    cpu_clock(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID)
}

pub fn process_cpu() -> std::time::Duration {
    cpu_clock(nix::time::ClockId::CLOCK_PROCESS_CPUTIME_ID)
}

fn cpu_clock(clock: nix::time::ClockId) -> std::time::Duration {
    nix::time::clock_gettime(clock).map_or(std::time::Duration::ZERO, |time| {
        std::time::Duration::new(
            u64::try_from(time.tv_sec()).unwrap_or(0),
            u32::try_from(time.tv_nsec()).unwrap_or(0),
        )
    })
}

pub fn readable(fd: impl AsFd, timeout: std::time::Duration) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec};
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let timeout = Timespec::try_from(timeout).ok();

    rustix::event::poll(&mut fds, timeout.as_ref()).is_ok_and(|ready| ready > 0)
        && fds[0].revents().contains(PollFlags::IN)
}

pub fn wait_readable(fd: &OwnedFd) {
    use rustix::event::{PollFd, PollFlags};
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    while rustix::event::poll(&mut fds, None) == Err(rustix::io::Errno::INTR) {}
}

pub fn poll<'a>(fds: impl IntoIterator<Item = BorrowedFd<'a>>, timeout: std::time::Duration) {
    use rustix::event::{PollFd, PollFlags, Timespec};
    let mut fds: Vec<PollFd<'_>> = fds
        .into_iter()
        .map(|fd| PollFd::from_borrowed_fd(fd, PollFlags::IN))
        .collect();
    let timeout = Timespec::try_from(timeout).ok();
    let _ = rustix::event::poll(&mut fds, timeout.as_ref());
}

pub fn handoff() -> std::io::Result<OwnedFd> {
    use rustix::event::{EventfdFlags, eventfd};
    Ok(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?)
}

pub fn signal(handoff: &OwnedFd) {
    let _ = rustix::io::write(handoff, &1u64.to_ne_bytes());
}

pub fn clear(handoff: &OwnedFd) {
    let _ = rustix::io::read(handoff, &mut [0; 8]);
}

pub struct Poller {
    epoll: OwnedFd,
    wake: OwnedFd,
}

const WAKE: u64 = u64::MAX;

impl Poller {
    pub fn new(rings: &[Ring]) -> std::io::Result<Self> {
        use rustix::event::{EventfdFlags, epoll, eventfd};
        let fd = epoll::create(epoll::CreateFlags::CLOEXEC)?;
        let wake = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        let indexed = rings.iter().map(|ring| &ring.fd).zip(0..);
        for (source, data) in indexed.chain([(&wake, WAKE)]) {
            epoll::add(
                &fd,
                source,
                epoll::EventData::new_u64(data),
                epoll::EventFlags::IN,
            )?;
        }
        Ok(Self { epoll: fd, wake })
    }

    pub fn wake(&self) {
        let _ = rustix::io::write(&self.wake, &1u64.to_ne_bytes());
    }

    pub fn wait(&self, rings: &[Ring], timeout: std::time::Duration) {
        use rustix::event::{Timespec, epoll};
        let mut ready = [std::mem::MaybeUninit::<epoll::Event>::uninit(); 8];
        let timeout = Timespec::try_from(timeout).ok();
        let Ok((ready, _)) = epoll::wait(&self.epoll, &mut ready, timeout.as_ref()) else {
            return;
        };

        let ended = epoll::EventFlags::HUP | epoll::EventFlags::ERR;
        for &epoll::Event { flags, data } in ready.iter() {
            let ring = usize::try_from(data.u64())
                .ok()
                .and_then(|index| rings.get(index));
            if flags.intersects(ended)
                && let Some(ring) = ring
            {
                let _ = epoll::delete(&self.epoll, &ring.fd);
            }
        }
    }
}

pub fn tsc() -> u64 {
    // SAFETY: RDTSC reads no memory. Every x86-64 processor has it; a
    // process that `PR_SET_TSC` forbade it faults, which microscope never
    // asks for and is not undefined behavior.
    unsafe { core::arch::x86_64::_rdtsc() }
}
