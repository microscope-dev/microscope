//! What each system call's arguments are, which drives decoding in both
//! engines: which arguments are descriptors or paths, which point at bytes
//! worth copying and how many, and how to print the rest.
//!
//! The table was first generated from the `SYSCALL_DEFINE` prototypes in
//! Linux 7.2's `include/linux/syscalls.h`, then corrected by hand. Each
//! entry is a signature in a small language, parsed once:
//!
//! - Scalars: `i` an `int`, `l` a `long`, `u` an `unsigned int`, `z` a
//!   `size_t` or `unsigned long`, `x` hex, `o` octal, `p` an opaque
//!   pointer, `fd`, `dfd` (a directory descriptor or `AT_FDCWD`), `pid`,
//!   `uid`, `gid`, `sig`, `mode` (permission bits).
//! - Named values: `F:set` for flags and `E:set` for one value of a set
//!   that `src/present/syscalls` names.
//! - Strings: `path`, a file name; `str`, any other string; `strv`, an
//!   array of strings, as `execve`'s `argv`.
//! - Memory: `<` for what the call reads, `>` for what it writes, `<>` for
//!   both, then a structure's name, or `buf:LEN` for bytes whose length is
//!   argument N (`aN`), the result (`ret`), or the integer argument N
//!   points at (`*aN`). Arrays of structures name their count the same
//!   way, as in `<iovec:a2`.
//! - The result follows `->`: `i`, `u`, `x`, `o`, `fd`, `pid`, or `none`
//!   for a call that does not return.
//!
//! Classes follow the signature. `file` (takes a path) and `desc` (takes
//! or makes a descriptor) are derived; the rest are written down.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use super::model::{Abi, SyscallId};

/// How to read and print one argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    /// An `int`: the low 32 bits, signed.
    Int,

    /// A `long` or `loff_t`.
    Long,

    /// An `unsigned int`: the low 32 bits.
    Uint,

    /// A `size_t` or `unsigned long`.
    Size,
    Hex,
    Octal,
    Ptr,
    Fd,
    DirFd,
    Pid,
    Uid,
    Gid,
    Signal,
    Mode,
    Flags(&'static str),
    Enum(&'static str),
    Path,
    Str,
    StrArray,

    /// Memory behind a pointer.
    Mem {
        direction: Direction,
        kind: MemKind,
        count: Count,
    },
}

/// Which way a call moves the memory behind a pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The call reads it: worth reading at entry.
    In,

    /// The call writes it: only meaningful at exit, and only on success.
    Out,

    /// Both.
    InOut,
}

impl Direction {
    #[must_use]
    pub const fn reads(self) -> bool {
        matches!(self, Self::In | Self::InOut)
    }

    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(self, Self::Out | Self::InOut)
    }
}

/// What a pointer points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemKind {
    /// Raw bytes: the count is a byte length.
    Bytes,

    /// Structures of a known layout: the count is how many, one when
    /// unstated.
    Struct(Struct),
}

/// How many bytes, or structures, a pointer covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    One,
    Arg(u8),
    Return,

    /// The integer argument N points at.
    PointedBy(u8),
}

/// The structures the decoders know, with their x86-64 sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Struct {
    Stat,
    Statx,
    Statfs,
    Timespec,

    /// Two timespecs, as `utimensat` takes.
    Timespec2,
    Timeval,

    /// Two timevals, as `utimes` takes.
    Timeval2,
    Itimerval,
    Itimerspec,
    Timezone,
    Rlimit,
    Rusage,
    Utsname,
    Sysinfo,
    Sigaction,
    Sigset,
    Pollfd,
    EpollEvent,
    Iovec,
    Msghdr,
    Mmsghdr,
    Sockaddr,
    Int,
    U64,

    /// The two descriptors `pipe` writes.
    Fds2,
    Siginfo,
    CloneArgs,
    OpenHow,
    Sigaltstack,
    SchedParam,
    Utimbuf,
    FdSet,
    Dirents,
    Tms,

    /// `struct rseq`, as long as the call's length says.
    Rseq,

    /// The status `wait4` writes.
    WaitStatus,
}

impl Struct {
    /// Bytes in one, under x86-64.
    #[must_use]
    pub const fn size(self) -> u32 {
        match self {
            Self::Dirents => 0,
            Self::Timezone
            | Self::Int
            | Self::WaitStatus
            | Self::Fds2
            | Self::Pollfd
            | Self::Sigset
            | Self::U64
            | Self::SchedParam => 8,
            Self::EpollEvent => 12,
            Self::Timespec
            | Self::Timeval
            | Self::Itimerval
            | Self::Rlimit
            | Self::Iovec
            | Self::Utimbuf => 16,
            Self::OpenHow | Self::Sigaltstack => 24,
            Self::Timespec2
            | Self::Timeval2
            | Self::Itimerspec
            | Self::Sigaction
            | Self::Tms
            | Self::Rseq => 32,
            Self::Msghdr => 56,
            Self::Mmsghdr => 64,
            Self::CloneArgs => 88,
            Self::Sysinfo => 112,
            Self::Statfs => 120,
            Self::Sockaddr | Self::Siginfo | Self::FdSet => 128,
            Self::Stat | Self::Rusage => 144,
            Self::Statx => 256,
            Self::Utsname => 390,
        }
    }

    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "stat" => Self::Stat,
            "statx" => Self::Statx,
            "statfs" => Self::Statfs,
            "timespec" => Self::Timespec,
            "timespec2" => Self::Timespec2,
            "timeval" => Self::Timeval,
            "timeval2" => Self::Timeval2,
            "itimerval" => Self::Itimerval,
            "itimerspec" => Self::Itimerspec,
            "timezone" => Self::Timezone,
            "rlimit" => Self::Rlimit,
            "rusage" => Self::Rusage,
            "utsname" => Self::Utsname,
            "sysinfo" => Self::Sysinfo,
            "sigaction" => Self::Sigaction,
            "sigset" => Self::Sigset,
            "pollfd" => Self::Pollfd,
            "epoll_event" => Self::EpollEvent,
            "iovec" => Self::Iovec,
            "msghdr" => Self::Msghdr,
            "mmsghdr" => Self::Mmsghdr,
            "sockaddr" => Self::Sockaddr,
            "int" => Self::Int,
            "u64" => Self::U64,
            "fds2" => Self::Fds2,
            "siginfo" => Self::Siginfo,
            "clone_args" => Self::CloneArgs,
            "open_how" => Self::OpenHow,
            "rseq" => Self::Rseq,
            "wstatus" => Self::WaitStatus,
            "sigaltstack" => Self::Sigaltstack,
            "sched_param" => Self::SchedParam,
            "utimbuf" => Self::Utimbuf,
            "fd_set" => Self::FdSet,
            "dirents" => Self::Dirents,
            "tms" => Self::Tms,
            _ => return None,
        })
    }
}

/// How to print a call's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ret {
    Int,
    Uint,
    Hex,
    Octal,
    Fd,
    Pid,

    /// The call does not return when it succeeds.
    None,
}

/// A class of calls, as strace's `%file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    File,
    Desc,
    Net,
    Process,
    Signal,
    Ipc,
    Memory,
    Creds,
    Clock,
    Stat,
    Lstat,
    Fstat,
    Statfs,
    Fstatfs,
    Pure,
}

impl Class {
    pub const ALL: [Self; 15] = [
        Self::File,
        Self::Desc,
        Self::Net,
        Self::Process,
        Self::Signal,
        Self::Ipc,
        Self::Memory,
        Self::Creds,
        Self::Clock,
        Self::Stat,
        Self::Lstat,
        Self::Fstat,
        Self::Statfs,
        Self::Fstatfs,
        Self::Pure,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Desc => "desc",
            Self::Net => "net",
            Self::Process => "process",
            Self::Signal => "signal",
            Self::Ipc => "ipc",
            Self::Memory => "memory",
            Self::Creds => "creds",
            Self::Clock => "clock",
            Self::Stat => "stat",
            Self::Lstat => "lstat",
            Self::Fstat => "fstat",
            Self::Statfs => "statfs",
            Self::Fstatfs => "fstatfs",
            Self::Pure => "pure",
        }
    }

    /// The class `%name` names, accepting strace's synonyms.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "network" => Some(Self::Net),
            _ => Self::ALL.into_iter().find(|class| class.name() == name),
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// A set of classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Classes(u16);

impl Classes {
    #[must_use]
    pub const fn contains(self, class: Class) -> bool {
        self.0 & class.bit() != 0
    }

    pub const fn insert(&mut self, class: Class) {
        self.0 |= class.bit();
    }

    pub fn iter(self) -> impl Iterator<Item = Class> {
        Class::ALL
            .into_iter()
            .filter(move |&class| self.contains(class))
    }
}

/// One call's arguments, result, and classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub args: Vec<Arg>,
    pub ret: Ret,
    pub classes: Classes,
}

impl Spec {
    /// The memory arguments, with their argument numbers.
    pub fn memory(&self) -> impl Iterator<Item = (u8, Direction, MemKind, Count)> + '_ {
        self.args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| match *arg {
                #[expect(clippy::cast_possible_truncation, reason = "six arguments at most")]
                Arg::Mem {
                    direction,
                    kind,
                    count,
                } => Some((index as u8, direction, kind, count)),
                _ => None,
            })
    }
}

/// Every call's signature, under x86-64 names.
const SIGNATURES: &[(&str, &str, &str)] = &[
    ("read", "fd >buf:ret z -> i", ""),
    ("write", "fd <buf:a2 z -> i", ""),
    ("open", "path F:open mode -> fd", ""),
    ("close", "fd -> i", ""),
    ("stat", "path >stat -> i", "stat"),
    ("fstat", "fd >stat -> i", "fstat"),
    ("lstat", "path >stat -> i", "lstat"),
    ("poll", "<>pollfd:a1 u i -> i", "desc"),
    ("lseek", "fd l E:whence -> i", ""),
    ("mmap", "p u F:mmap_prot F:mmap_flags fd x -> x", "memory"),
    ("mprotect", "p z F:mmap_prot -> i", "memory"),
    ("munmap", "p z -> i", "memory"),
    ("brk", "p -> x", "memory"),
    ("rt_sigaction", "sig <sigaction >sigaction z -> i", "signal"),
    (
        "rt_sigprocmask",
        "E:sigprocmask <sigset >sigset u -> i",
        "signal",
    ),
    ("rt_sigreturn", " -> none", "signal"),
    ("ioctl", "fd E:ioctl x -> i", ""),
    ("pread64", "fd >buf:ret z l -> i", ""),
    ("pwrite64", "fd <buf:a2 z l -> i", ""),
    ("readv", "fd >iovec:a2 z -> i", ""),
    ("writev", "fd <iovec:a2 z -> i", ""),
    ("access", "path F:access -> i", ""),
    ("pipe", ">fds2 -> i", ""),
    (
        "select",
        "i <>fd_set <>fd_set <>fd_set <>timeval -> i",
        "desc",
    ),
    ("sched_yield", " -> i", ""),
    ("mremap", "p z z F:mremap p -> x", "memory"),
    ("msync", "p z F:msync -> i", "memory"),
    ("mincore", "p z p -> i", "memory"),
    ("madvise", "p z E:madvise -> i", "memory"),
    ("shmget", "i z x -> i", "ipc"),
    ("shmat", "i p x -> x", "ipc memory"),
    ("shmctl", "i i p -> i", "ipc"),
    ("dup", "fd -> fd", ""),
    ("dup2", "fd fd -> fd", ""),
    ("pause", " -> i", "signal"),
    ("nanosleep", "<timespec >timespec -> i", ""),
    ("getitimer", "i >itimerval -> i", ""),
    ("alarm", "u -> u", ""),
    ("setitimer", "i <itimerval >itimerval -> i", ""),
    ("getpid", " -> pid", "pure"),
    ("sendfile", "fd fd <>u64 z -> i", "net desc"),
    ("socket", "E:af F:sock_type E:ipproto -> fd", "net"),
    ("connect", "fd <sockaddr:a2 u -> i", "net"),
    ("accept", "fd >sockaddr:*a2 <>int -> fd", "net"),
    ("sendto", "fd <buf:a2 z F:msg <sockaddr:a5 u -> i", "net"),
    (
        "recvfrom",
        "fd >buf:ret u F:msg >sockaddr:*a5 <>int -> i",
        "net",
    ),
    ("sendmsg", "fd <msghdr F:msg -> i", "net"),
    ("recvmsg", "fd <>msghdr F:msg -> i", "net"),
    ("shutdown", "fd E:shutdown -> i", "net"),
    ("bind", "fd <sockaddr:a2 u -> i", "net"),
    ("listen", "fd i -> i", "net"),
    ("getsockname", "fd >sockaddr:*a2 <>int -> i", "net"),
    ("getpeername", "fd >sockaddr:*a2 <>int -> i", "net"),
    ("socketpair", "E:af F:sock_type E:ipproto >fds2 -> i", "net"),
    ("setsockopt", "fd E:sol i <buf:a4 u -> i", "net"),
    ("getsockopt", "fd E:sol i >buf:*a4 <>int -> i", "net"),
    ("clone", "F:clone p p p x -> pid", "process"),
    ("fork", " -> pid", "process"),
    ("vfork", " -> pid", "process"),
    ("execve", "path strv strv -> i", "process"),
    ("exit", "i -> none", "process"),
    ("wait4", "pid >wstatus F:wait >rusage -> pid", "process"),
    ("kill", "pid sig -> i", "signal process"),
    ("uname", ">utsname -> i", ""),
    ("semget", "i i x -> i", "ipc"),
    ("semop", "i p u -> i", "ipc"),
    ("semctl", "i i i x -> i", "ipc"),
    ("shmdt", "p -> i", "ipc memory"),
    ("msgget", "i x -> i", "ipc"),
    ("msgsnd", "i p z x -> i", "ipc"),
    ("msgrcv", "i p z l x -> i", "ipc"),
    ("msgctl", "i i p -> i", "ipc"),
    ("fcntl", "fd E:fcntl x -> i", ""),
    ("flock", "fd F:flock -> i", ""),
    ("fsync", "fd -> i", ""),
    ("fdatasync", "fd -> i", ""),
    ("truncate", "path l -> i", ""),
    ("ftruncate", "fd l -> i", ""),
    ("getdents", "fd >dirents:ret u -> i", ""),
    ("getcwd", ">buf:ret z -> i", "file"),
    ("chdir", "path -> i", ""),
    ("fchdir", "fd -> i", ""),
    ("rename", "path path -> i", ""),
    ("mkdir", "path mode -> i", ""),
    ("rmdir", "path -> i", ""),
    ("creat", "path mode -> fd", ""),
    ("link", "path path -> i", ""),
    ("unlink", "path -> i", ""),
    ("symlink", "path path -> i", ""),
    ("readlink", "path >buf:ret u -> i", ""),
    ("chmod", "path mode -> i", ""),
    ("fchmod", "fd mode -> i", ""),
    ("chown", "path uid gid -> i", ""),
    ("fchown", "fd uid gid -> i", ""),
    ("lchown", "path uid gid -> i", ""),
    ("umask", "o -> o", ""),
    ("gettimeofday", ">timeval >timezone -> i", "clock"),
    ("getrlimit", "E:rlimit >rlimit -> i", ""),
    ("getrusage", "E:rusage >rusage -> i", ""),
    ("sysinfo", ">sysinfo -> i", ""),
    ("times", ">tms -> i", ""),
    ("ptrace", "E:ptrace pid x x -> i", ""),
    ("getuid", " -> u", "creds pure"),
    ("syslog", "i >buf:ret i -> i", ""),
    ("getgid", " -> u", "creds pure"),
    ("setuid", "uid -> i", "creds"),
    ("setgid", "gid -> i", "creds"),
    ("geteuid", " -> u", "creds pure"),
    ("getegid", " -> u", "creds pure"),
    ("setpgid", "pid pid -> i", ""),
    ("getppid", " -> pid", "pure"),
    ("getpgrp", " -> pid", "pure"),
    ("setsid", " -> pid", ""),
    ("setreuid", "uid uid -> i", "creds"),
    ("setregid", "gid gid -> i", "creds"),
    ("getgroups", "i p -> i", "creds"),
    ("setgroups", "i p -> i", "creds"),
    ("setresuid", "uid uid uid -> i", "creds"),
    ("getresuid", ">int >int >int -> i", "creds"),
    ("setresgid", "gid gid gid -> i", "creds"),
    ("getresgid", ">int >int >int -> i", "creds"),
    ("getpgid", "pid -> pid", ""),
    ("setfsuid", "uid -> i", "creds"),
    ("setfsgid", "gid -> i", "creds"),
    ("getsid", "pid -> pid", ""),
    ("capget", "p p -> i", "creds"),
    ("capset", "p p -> i", "creds"),
    ("rt_sigpending", ">sigset z -> i", "signal"),
    (
        "rt_sigtimedwait",
        "<sigset >siginfo <timespec u -> i",
        "signal",
    ),
    ("rt_sigqueueinfo", "pid sig <siginfo -> i", "signal process"),
    ("rt_sigsuspend", "<sigset z -> i", "signal"),
    ("sigaltstack", "<sigaltstack >sigaltstack -> i", "signal"),
    ("utime", "path <utimbuf -> i", ""),
    ("mknod", "path mode x -> i", ""),
    ("uselib", "path -> i", ""),
    ("personality", "x -> x", ""),
    ("ustat", "x p -> i", ""),
    ("statfs", "path >statfs -> i", "statfs"),
    ("fstatfs", "fd >statfs -> i", "fstatfs"),
    ("sysfs", "i x x -> i", ""),
    ("getpriority", "E:prio i -> i", ""),
    ("setpriority", "E:prio i i -> i", ""),
    ("sched_setparam", "pid <sched_param -> i", ""),
    ("sched_getparam", "pid >sched_param -> i", ""),
    (
        "sched_setscheduler",
        "pid E:sched_policy <sched_param -> i",
        "",
    ),
    ("sched_getscheduler", "pid -> i", ""),
    ("sched_get_priority_max", "E:sched_policy -> i", ""),
    ("sched_get_priority_min", "E:sched_policy -> i", ""),
    ("sched_rr_get_interval", "pid >timespec -> i", ""),
    ("mlock", "p z -> i", "memory"),
    ("munlock", "p z -> i", "memory"),
    ("mlockall", "F:mlockall -> i", "memory"),
    ("munlockall", " -> i", "memory"),
    ("vhangup", " -> i", ""),
    ("modify_ldt", "i p u -> i", ""),
    ("pivot_root", "path path -> i", ""),
    ("_sysctl", "p -> i", ""),
    ("prctl", "E:prctl x x x x -> i", "creds"),
    ("arch_prctl", "E:arch_prctl x -> i", ""),
    ("adjtimex", "p -> i", "clock"),
    ("setrlimit", "E:rlimit <rlimit -> i", ""),
    ("chroot", "path -> i", ""),
    ("sync", " -> i", ""),
    ("acct", "path -> i", ""),
    ("settimeofday", "<timeval <timezone -> i", "clock"),
    ("mount", "str path str F:mount p -> i", ""),
    ("umount2", "path F:umount -> i", ""),
    ("swapon", "path x -> i", ""),
    ("swapoff", "path -> i", ""),
    ("reboot", "x x x p -> i", ""),
    ("sethostname", "<buf:a1 u -> i", ""),
    ("setdomainname", "<buf:a1 u -> i", ""),
    ("iopl", "i -> i", ""),
    ("ioperm", "x x i -> i", ""),
    ("create_module", "str u -> i", ""),
    ("init_module", "p z str -> i", ""),
    ("delete_module", "str x -> i", ""),
    ("get_kernel_syms", "p -> i", ""),
    ("query_module", "str i p u p -> i", ""),
    ("quotactl", "x path i p -> i", ""),
    ("nfsservctl", "i p p -> i", ""),
    ("getpmsg", "x x x x x -> i", "net"),
    ("putpmsg", "x x x x x -> i", "net"),
    ("afs_syscall", "x x x x x -> i", ""),
    ("tuxcall", "x x x -> i", ""),
    ("security", "x x x -> i", ""),
    ("gettid", " -> pid", "pure"),
    ("readahead", "fd l z -> i", ""),
    ("setxattr", "path str <buf:a3 z F:xattr -> i", ""),
    ("lsetxattr", "path str <buf:a3 z F:xattr -> i", ""),
    ("fsetxattr", "fd str <buf:a3 z F:xattr -> i", ""),
    ("getxattr", "path str >buf:ret z -> i", ""),
    ("lgetxattr", "path str >buf:ret z -> i", ""),
    ("fgetxattr", "fd str >buf:ret z -> i", ""),
    ("listxattr", "path >buf:ret z -> i", ""),
    ("llistxattr", "path >buf:ret z -> i", ""),
    ("flistxattr", "fd >buf:ret z -> i", ""),
    ("removexattr", "path str -> i", ""),
    ("lremovexattr", "path str -> i", ""),
    ("fremovexattr", "fd str -> i", ""),
    ("tkill", "pid sig -> i", "signal process"),
    ("time", ">u64 -> i", "clock"),
    ("futex", "p E:futex u <timespec p u -> i", ""),
    ("sched_setaffinity", "pid u p -> i", ""),
    ("sched_getaffinity", "pid u p -> i", ""),
    ("set_thread_area", "p -> i", ""),
    ("io_setup", "u p -> i", "memory"),
    ("io_destroy", "x -> i", "memory"),
    ("io_getevents", "x l l p <timespec -> i", ""),
    ("io_submit", "x l p -> i", ""),
    ("io_cancel", "x p p -> i", ""),
    ("get_thread_area", "p -> i", ""),
    ("lookup_dcookie", "x >buf:ret u -> i", ""),
    ("epoll_create", "i -> fd", ""),
    ("epoll_ctl_old", "x x x x -> i", ""),
    ("epoll_wait_old", "x x x -> i", ""),
    (
        "remap_file_pages",
        "p u F:mmap_prot u F:mmap_flags -> i",
        "memory",
    ),
    ("getdents64", "fd >dirents:ret u -> i", ""),
    ("set_tid_address", "p -> pid", ""),
    ("restart_syscall", " -> i", ""),
    ("semtimedop", "i p u <timespec -> i", "ipc"),
    ("fadvise64", "fd l l E:fadvise -> i", ""),
    ("timer_create", "E:clock p p -> i", ""),
    (
        "timer_settime",
        "i F:timer <itimerspec >itimerspec -> i",
        "",
    ),
    ("timer_gettime", "i >itimerspec -> i", ""),
    ("timer_getoverrun", "i -> i", ""),
    ("timer_delete", "i -> i", ""),
    ("clock_settime", "E:clock <timespec -> i", "clock"),
    ("clock_gettime", "E:clock >timespec -> i", "clock"),
    ("clock_getres", "E:clock >timespec -> i", "clock"),
    (
        "clock_nanosleep",
        "E:clock F:timer <timespec >timespec -> i",
        "",
    ),
    ("exit_group", "i -> none", "process"),
    ("epoll_wait", "fd >epoll_event:ret i i -> i", ""),
    ("epoll_ctl", "fd E:epoll_ctl fd <epoll_event -> i", ""),
    ("tgkill", "pid pid sig -> i", "signal process"),
    ("utimes", "path <timeval2 -> i", ""),
    ("vserver", "x x x x x -> i", ""),
    ("mbind", "p z x p z x -> i", "memory"),
    ("set_mempolicy", "x p z -> i", "memory"),
    ("get_mempolicy", ">int p z p x -> i", "memory"),
    ("mq_open", "str F:open mode p -> fd", ""),
    ("mq_unlink", "str -> i", ""),
    ("mq_timedsend", "fd <buf:a2 z u <timespec -> i", ""),
    ("mq_timedreceive", "fd >buf:ret z p <timespec -> i", ""),
    ("mq_notify", "fd p -> i", ""),
    ("mq_getsetattr", "fd p p -> i", ""),
    ("kexec_load", "x z p x -> i", ""),
    (
        "waitid",
        "E:idtype pid >siginfo F:wait >rusage -> i",
        "process",
    ),
    ("add_key", "str str <buf:a3 z l -> i", ""),
    ("request_key", "str str str l -> i", ""),
    ("keyctl", "i x x x x -> i", ""),
    ("ioprio_set", "i i x -> i", ""),
    ("ioprio_get", "i i -> i", ""),
    ("inotify_init", " -> fd", ""),
    ("inotify_add_watch", "fd path x -> i", ""),
    ("inotify_rm_watch", "fd i -> i", ""),
    ("migrate_pages", "pid z p p -> i", "memory"),
    ("openat", "dfd path F:open mode -> fd", ""),
    ("mkdirat", "dfd path mode -> i", ""),
    ("mknodat", "dfd path mode x -> i", ""),
    ("fchownat", "dfd path uid gid F:at -> i", ""),
    ("futimesat", "dfd path <timeval2 -> i", ""),
    ("newfstatat", "dfd path >stat F:at -> i", "fstat"),
    ("unlinkat", "dfd path F:at -> i", ""),
    ("renameat", "dfd path dfd path -> i", ""),
    ("linkat", "dfd path dfd path F:at -> i", ""),
    ("symlinkat", "path dfd path -> i", ""),
    ("readlinkat", "dfd path >buf:ret u -> i", ""),
    ("fchmodat", "dfd path mode -> i", ""),
    ("faccessat", "dfd path F:access -> i", ""),
    (
        "pselect6",
        "i <>fd_set <>fd_set <>fd_set <>timespec p -> i",
        "desc",
    ),
    ("ppoll", "<>pollfd:a1 u <timespec <sigset z -> i", "desc"),
    ("unshare", "F:clone -> i", ""),
    ("set_robust_list", "p z -> i", ""),
    ("get_robust_list", "pid p p -> i", ""),
    ("splice", "fd <>u64 fd <>u64 z F:splice -> i", ""),
    ("tee", "fd fd z F:splice -> i", ""),
    ("sync_file_range", "fd l l x -> i", ""),
    ("vmsplice", "fd <iovec:a2 z F:splice -> i", ""),
    ("move_pages", "pid z p p p F:mpol -> i", "memory"),
    ("utimensat", "dfd path <timespec2 F:at -> i", ""),
    ("epoll_pwait", "fd >epoll_event:ret i i <sigset z -> i", ""),
    ("signalfd", "fd <sigset z -> fd", "signal"),
    ("timerfd_create", "E:clock F:open -> fd", ""),
    ("eventfd", "u -> fd", ""),
    ("fallocate", "fd F:fallocate l l -> i", ""),
    (
        "timerfd_settime",
        "fd F:timer <itimerspec >itimerspec -> i",
        "",
    ),
    ("timerfd_gettime", "fd >itimerspec -> i", ""),
    ("accept4", "fd >sockaddr:*a2 <>int F:sock_type -> fd", "net"),
    ("signalfd4", "fd <sigset z F:open -> fd", "signal"),
    ("eventfd2", "u F:efd -> fd", ""),
    ("epoll_create1", "F:open -> fd", ""),
    ("dup3", "fd fd F:oflags -> fd", ""),
    ("pipe2", ">fds2 F:oflags -> i", ""),
    ("inotify_init1", "F:in_init -> fd", ""),
    ("preadv", "fd >iovec:a2 z z z -> i", ""),
    ("pwritev", "fd <iovec:a2 z z z -> i", ""),
    (
        "rt_tgsigqueueinfo",
        "pid pid sig <siginfo -> i",
        "signal process",
    ),
    ("perf_event_open", "p pid i fd x -> fd", ""),
    ("recvmmsg", "fd p u F:msg <timespec -> i", "net"),
    ("fanotify_init", "x x -> fd", ""),
    ("fanotify_mark", "fd x x x dfd path -> i", ""),
    ("prlimit64", "pid E:rlimit <rlimit >rlimit -> i", ""),
    ("name_to_handle_at", "dfd path p p F:at -> i", ""),
    ("open_by_handle_at", "fd p F:open -> fd", ""),
    ("clock_adjtime", "E:clock p -> i", "clock"),
    ("syncfs", "fd -> i", ""),
    ("sendmmsg", "fd p u F:msg -> i", "net"),
    ("setns", "fd F:clone -> i", ""),
    ("getcpu", ">int >int p -> i", ""),
    ("process_vm_readv", "pid p z p z x -> i", ""),
    ("process_vm_writev", "pid p z p z x -> i", ""),
    ("kcmp", "pid pid i x x -> i", ""),
    ("finit_module", "fd str x -> i", ""),
    ("sched_setattr", "pid p x -> i", ""),
    ("sched_getattr", "pid p u x -> i", ""),
    ("renameat2", "dfd path dfd path F:rename -> i", ""),
    ("seccomp", "i x p -> i", ""),
    ("getrandom", ">buf:ret z F:getrandom -> i", ""),
    ("memfd_create", "str F:memfd -> fd", ""),
    ("kexec_file_load", "fd fd z str x -> i", ""),
    ("bpf", "E:bpf p u p u -> i", "desc"),
    ("execveat", "dfd path strv strv F:at -> i", "process"),
    ("userfaultfd", "F:open -> fd", ""),
    ("membarrier", "i x i -> i", ""),
    ("mlock2", "p z x -> i", "memory"),
    ("copy_file_range", "fd <>u64 fd <>u64 z x -> i", ""),
    ("preadv2", "fd >iovec:a2 z z z x -> i", ""),
    ("pwritev2", "fd <iovec:a2 z z z x -> i", ""),
    ("pkey_mprotect", "p z F:mmap_prot i -> i", "memory"),
    ("pkey_alloc", "x x -> i", ""),
    ("pkey_free", "i -> i", ""),
    ("statx", "dfd path F:at x >statx -> i", "fstat"),
    ("io_pgetevents", "x l l p <timespec p -> i", ""),
    ("rseq", "<rseq:a1 u F:rseq x -> i", ""),
    ("uretprobe", " -> i", ""),
    ("uprobe", " -> i", ""),
    (
        "pidfd_send_signal",
        "fd sig <siginfo x -> i",
        "signal process",
    ),
    ("io_uring_setup", "u p -> fd", ""),
    ("io_uring_enter", "fd u u x p z -> i", "signal"),
    ("io_uring_register", "fd u p u -> i", "memory"),
    ("open_tree", "dfd path x -> fd", ""),
    ("move_mount", "dfd path dfd path x -> i", ""),
    ("fsopen", "str x -> fd", ""),
    ("fsconfig", "fd u str p i -> i", "file"),
    ("fsmount", "fd x x -> fd", ""),
    ("fspick", "dfd path x -> fd", ""),
    ("pidfd_open", "pid x -> fd", ""),
    ("clone3", "<clone_args z -> pid", "process"),
    ("close_range", "fd fd F:close_range -> i", "-desc"),
    ("openat2", "dfd path <open_how z -> fd", ""),
    ("pidfd_getfd", "fd i x -> fd", ""),
    ("faccessat2", "dfd path F:access F:at -> i", ""),
    ("process_madvise", "fd p z E:madvise x -> i", ""),
    (
        "epoll_pwait2",
        "fd >epoll_event:ret i <timespec <sigset u -> i",
        "",
    ),
    ("mount_setattr", "dfd path F:at p z -> i", ""),
    ("quotactl_fd", "fd x i p -> i", ""),
    ("landlock_create_ruleset", "p z x -> fd", ""),
    ("landlock_add_rule", "fd i p x -> i", ""),
    ("landlock_restrict_self", "fd x -> i", ""),
    ("memfd_secret", "x -> fd", ""),
    ("process_mrelease", "fd x -> i", ""),
    ("futex_waitv", "p u x <timespec E:clock -> i", ""),
    ("set_mempolicy_home_node", "p z x x -> i", "memory"),
    ("cachestat", "fd p p x -> i", ""),
    ("fchmodat2", "dfd path mode F:at -> i", ""),
    ("map_shadow_stack", "p z x -> x", "memory"),
    ("futex_wake", "p x i x -> i", ""),
    ("futex_wait", "p x x x <timespec E:clock -> i", ""),
    ("futex_requeue", "p x i i -> i", ""),
    ("statmount", "p p z x -> i", ""),
    ("listmount", "p p z x -> i", ""),
    ("lsm_get_self_attr", "u p p x -> i", ""),
    ("lsm_set_self_attr", "u p u x -> i", ""),
    ("lsm_list_modules", "p p x -> i", ""),
    ("mseal", "p z x -> i", "memory"),
    ("setxattrat", "dfd path F:at str p z -> i", ""),
    ("getxattrat", "dfd path F:at str p z -> i", ""),
    ("listxattrat", "dfd path F:at >buf:ret z -> i", ""),
    ("removexattrat", "dfd path F:at str -> i", ""),
    ("open_tree_attr", "dfd path x p z -> fd", ""),
    ("file_getattr", "dfd path p z F:at -> i", ""),
    ("file_setattr", "dfd path p z F:at -> i", ""),
];

/// Calls whose i386 arguments differ from their x86-64 namesakes', as a
/// 64-bit offset split across two registers does. They print raw.
const I386_DIFFERS: &[&str] = &[
    "mmap",
    "pread64",
    "pwrite64",
    "truncate64",
    "ftruncate64",
    "fallocate",
    "fadvise64",
    "fadvise64_64",
    "sync_file_range",
    "readahead",
    "lookup_dcookie",
    "preadv",
    "pwritev",
    "preadv2",
    "pwritev2",
    "select",
    "ipc",
    "socketcall",
    "clone",
];

/// Why a signature did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

fn count(text: &str) -> Result<Count, ParseError> {
    let number = |digits: &str| {
        digits
            .parse::<u8>()
            .ok()
            .filter(|&index| index < 6)
            .ok_or_else(|| ParseError(format!("bad argument number in {text:?}")))
    };

    if text == "ret" {
        Ok(Count::Return)
    } else if let Some(digits) = text.strip_prefix("*a") {
        Ok(Count::PointedBy(number(digits)?))
    } else if let Some(digits) = text.strip_prefix('a') {
        Ok(Count::Arg(number(digits)?))
    } else {
        Err(ParseError(format!("bad count {text:?}")))
    }
}

fn arg(token: &'static str) -> Result<Arg, ParseError> {
    Ok(match token {
        "i" => Arg::Int,
        "l" => Arg::Long,
        "u" => Arg::Uint,
        "z" => Arg::Size,
        "x" => Arg::Hex,
        "o" => Arg::Octal,
        "p" => Arg::Ptr,
        "fd" => Arg::Fd,
        "dfd" => Arg::DirFd,
        "pid" => Arg::Pid,
        "uid" => Arg::Uid,
        "gid" => Arg::Gid,
        "sig" => Arg::Signal,
        "mode" => Arg::Mode,
        "path" => Arg::Path,
        "str" => Arg::Str,
        "strv" => Arg::StrArray,
        _ => {
            if let Some(set) = token.strip_prefix("F:") {
                return Ok(Arg::Flags(set));
            }

            if let Some(set) = token.strip_prefix("E:") {
                return Ok(Arg::Enum(set));
            }

            let (direction, rest) = if let Some(rest) = token.strip_prefix("<>") {
                (Direction::InOut, rest)
            } else if let Some(rest) = token.strip_prefix('<') {
                (Direction::In, rest)
            } else if let Some(rest) = token.strip_prefix('>') {
                (Direction::Out, rest)
            } else {
                return Err(ParseError(format!("unknown argument {token:?}")));
            };

            let (name, count_text) = rest
                .split_once(':')
                .map_or((rest, None), |(n, c)| (n, Some(c)));
            let kind = if name == "buf" {
                MemKind::Bytes
            } else {
                MemKind::Struct(
                    Struct::named(name)
                        .ok_or_else(|| ParseError(format!("unknown structure {name:?}")))?,
                )
            };

            let count = match (kind, count_text) {
                (MemKind::Bytes, None) => {
                    return Err(ParseError(format!("{token:?} needs a length")));
                }
                (_, None) => Count::One,
                (_, Some(text)) => count(text)?,
            };

            Arg::Mem {
                direction,
                kind,
                count,
            }
        }
    })
}

/// Parses one signature and its written classes.
pub fn parse(signature: &'static str, classes: &str) -> Result<Spec, ParseError> {
    let (args, ret) = signature
        .split_once("->")
        .ok_or_else(|| ParseError(format!("{signature:?} has no result")))?;
    let args = args
        .split_whitespace()
        .map(arg)
        .collect::<Result<Vec<_>, _>>()?;
    if args.len() > 6 {
        return Err(ParseError(format!(
            "{signature:?} has more than six arguments"
        )));
    }

    let ret = match ret.trim() {
        "i" => Ret::Int,
        "u" => Ret::Uint,
        "x" => Ret::Hex,
        "o" => Ret::Octal,
        "fd" => Ret::Fd,
        "pid" => Ret::Pid,
        "none" => Ret::None,
        other => return Err(ParseError(format!("unknown result {other:?}"))),
    };

    let mut set = Classes::default();

    // `-desc` keeps a call that takes a descriptor out of %desc, as
    // strace's tables do for a few.
    let mut not_desc = false;

    for name in classes.split_whitespace() {
        if name == "-desc" {
            not_desc = true;
            continue;
        }

        set.insert(
            Class::named(name).ok_or_else(|| ParseError(format!("unknown class {name:?}")))?,
        );
    }

    if args.iter().any(|arg| matches!(arg, Arg::Path)) {
        set.insert(Class::File);
    }

    // strace's %desc: calls that take or make descriptors, a directory's
    // included, except the network's, which are %net alone.
    let makes = |arg: &Arg| {
        matches!(
            arg,
            Arg::Mem {
                kind: MemKind::Struct(Struct::Fds2),
                ..
            }
        )
    };

    let takes = |arg: &Arg| matches!(arg, Arg::Fd | Arg::DirFd);
    if !not_desc
        && !set.contains(Class::Net)
        && (ret == Ret::Fd || args.iter().any(|arg| takes(arg) || makes(arg)))
    {
        set.insert(Class::Desc);
    }

    Ok(Spec {
        args,
        ret,
        classes: set,
    })
}

static SPECS: LazyLock<BTreeMap<&'static str, Spec>> = LazyLock::new(|| {
    SIGNATURES
        .iter()
        .filter_map(|&(name, signature, classes)| {
            // A test parses every signature; a call without one prints its
            // arguments as raw words.
            let spec = parse(signature, classes);
            debug_assert!(spec.is_ok(), "{name}: {spec:?}");
            Some((name, spec.ok()?))
        })
        .collect()
});

/// What is known of each call, in the order its ABI's table lists them,
/// so that looking a call up compares no names.
static BY_CALL: LazyLock<[Vec<Known>; 3]> = LazyLock::new(|| {
    Abi::ALL.map(|abi| {
        super::table::calls(abi)
            .iter()
            .map(|&(_, name)| Known {
                spec: (abi != Abi::X86_32 || !I386_DIFFERS.contains(&name))
                    .then(|| SPECS.get(name))
                    .flatten(),
                arg_names: arg_names_named(name),
            })
            .collect()
    })
});

#[derive(Clone, Copy)]
struct Known {
    spec: Option<&'static Spec>,
    arg_names: Option<&'static [&'static str]>,
}

fn known(id: SyscallId) -> Option<Known> {
    let index = super::table::index(id)?;
    BY_CALL[id.abi as usize].get(index).copied()
}

/// The spec of a call, when its arguments are known under its ABI.
#[must_use]
pub fn spec(id: SyscallId) -> Option<&'static Spec> {
    known(id)?.spec
}

/// The spec of a call by its name, whatever its ABI.
#[must_use]
pub fn spec_named(name: &str) -> Option<&'static Spec> {
    SPECS.get(name)
}

/// The names the kernel gives a call's arguments, which `-N` prints.
#[must_use]
pub fn arg_names(id: SyscallId) -> Option<&'static [&'static str]> {
    known(id)?.arg_names
}

fn arg_names_named(name: &str) -> Option<&'static [&'static str]> {
    super::argnames::ARG_NAMES
        .iter()
        .find(|&&(candidate, _)| candidate == name)
        .map(|&(_, names)| names)
}

/// The names every flag and enumeration argument uses, for the decoders'
/// tests.
pub fn sets() -> impl Iterator<Item = &'static str> {
    SPECS.values().flat_map(|spec| {
        spec.args.iter().filter_map(|arg| match *arg {
            Arg::Flags(set) | Arg::Enum(set) => Some(set),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::table;

    #[test]
    fn every_signature_parses_and_names_a_64_bit_call() {
        let mut names = std::collections::BTreeSet::new();

        for &(name, signature, classes) in SIGNATURES {
            parse(signature, classes).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert!(names.insert(name), "{name} is listed twice");
            assert!(
                SyscallId::named(Abi::X86_64, name).is_some(),
                "{name} is not a 64-bit call"
            );
        }

        // Every 64-bit call has a spec, so none prints as raw words, and
        // names for as many arguments as its spec has.
        for &(_, name) in table::calls(Abi::X86_64) {
            assert!(names.contains(name), "{name} has no spec");
            let arity = spec_named(name).expect("a spec").args.len();
            if let Some(arg_names) = arg_names_named(name) {
                assert_eq!(arg_names.len(), arity, "{name}'s argument names");
            }
        }
    }

    #[test]
    fn classes_are_derived_and_written() {
        let openat = spec_named("openat").expect("openat");
        assert!(openat.classes.contains(Class::File) && openat.classes.contains(Class::Desc));
        let connect = spec_named("connect").expect("connect");
        assert!(connect.classes.contains(Class::Net) && !connect.classes.contains(Class::File));
        assert_eq!(
            spec_named("read")
                .expect("read")
                .memory()
                .collect::<Vec<_>>(),
            [(1, Direction::Out, MemKind::Bytes, Count::Return)]
        );
        assert!(spec(SyscallId::named(Abi::X86_32, "pread64").expect("pread64")).is_none());
        assert!(spec(SyscallId::named(Abi::X86_32, "openat").expect("openat")).is_some());
    }
}
