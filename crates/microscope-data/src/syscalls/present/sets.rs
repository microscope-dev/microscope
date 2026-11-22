use nix::libc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Flags,

    Enum,
}

#[derive(Debug, Clone, Copy)]
pub struct Set {
    pub shape: Shape,

    pub narrow: bool,
    pub names: &'static [(u64, &'static str)],
}

const fn flags(names: &'static [(u64, &'static str)]) -> Set {
    Set {
        shape: Shape::Flags,
        narrow: true,
        names,
    }
}

const fn wide_flags(names: &'static [(u64, &'static str)]) -> Set {
    Set {
        shape: Shape::Flags,
        narrow: false,
        names,
    }
}

const fn values(names: &'static [(u64, &'static str)]) -> Set {
    Set {
        shape: Shape::Enum,
        narrow: true,
        names,
    }
}

#[expect(clippy::cast_sign_loss, reason = "C constants")]
const fn c(value: libc::c_int) -> u64 {
    value as u32 as u64
}

const fn cl(value: libc::c_ulong) -> u64 {
    value
}

pub const ACCESS_MODES: &[(u64, &str)] = &[
    (c(libc::O_RDONLY), "O_RDONLY"),
    (c(libc::O_WRONLY), "O_WRONLY"),
    (c(libc::O_RDWR), "O_RDWR"),
];

const OPEN: &[(u64, &str)] = &[
    (c(libc::O_CREAT), "O_CREAT"),
    (c(libc::O_EXCL), "O_EXCL"),
    (c(libc::O_NOCTTY), "O_NOCTTY"),
    (c(libc::O_TRUNC), "O_TRUNC"),
    (c(libc::O_APPEND), "O_APPEND"),
    (c(libc::O_NONBLOCK), "O_NONBLOCK"),
    (c(libc::O_DSYNC), "O_DSYNC"),
    (0o20_000, "FASYNC"),
    (c(libc::O_DIRECT), "O_DIRECT"),
    (c(libc::O_LARGEFILE), "O_LARGEFILE"),
    (c(libc::O_DIRECTORY), "O_DIRECTORY"),
    (c(libc::O_NOFOLLOW), "O_NOFOLLOW"),
    (c(libc::O_NOATIME), "O_NOATIME"),
    (c(libc::O_CLOEXEC), "O_CLOEXEC"),
    (0o4_000_000, "__O_SYNC"),
    (c(libc::O_PATH), "O_PATH"),
    (0o20_000_000, "__O_TMPFILE"),
];

const EFD: &[(u64, &str)] = &[
    (1, "EFD_SEMAPHORE"),
    (c(libc::O_NONBLOCK), "EFD_NONBLOCK"),
    (c(libc::O_CLOEXEC), "EFD_CLOEXEC"),
];

const IN_INIT: &[(u64, &str)] = &[
    (c(libc::O_NONBLOCK), "IN_NONBLOCK"),
    (c(libc::O_CLOEXEC), "IN_CLOEXEC"),
];

const MMAP_PROT: &[(u64, &str)] = &[
    (c(libc::PROT_READ), "PROT_READ"),
    (c(libc::PROT_WRITE), "PROT_WRITE"),
    (c(libc::PROT_EXEC), "PROT_EXEC"),
    (c(libc::PROT_GROWSDOWN), "PROT_GROWSDOWN"),
    (c(libc::PROT_GROWSUP), "PROT_GROWSUP"),
];

pub const MAP_TYPES: &[(u64, &str)] = &[
    (c(libc::MAP_SHARED), "MAP_SHARED"),
    (c(libc::MAP_PRIVATE), "MAP_PRIVATE"),
    (c(libc::MAP_SHARED_VALIDATE), "MAP_SHARED_VALIDATE"),
];

const MMAP_FLAGS: &[(u64, &str)] = &[
    (c(libc::MAP_FIXED), "MAP_FIXED"),
    (c(libc::MAP_ANONYMOUS), "MAP_ANONYMOUS"),
    (c(libc::MAP_32BIT), "MAP_32BIT"),
    (c(libc::MAP_GROWSDOWN), "MAP_GROWSDOWN"),
    (c(libc::MAP_DENYWRITE), "MAP_DENYWRITE"),
    (c(libc::MAP_EXECUTABLE), "MAP_EXECUTABLE"),
    (c(libc::MAP_LOCKED), "MAP_LOCKED"),
    (c(libc::MAP_NORESERVE), "MAP_NORESERVE"),
    (c(libc::MAP_POPULATE), "MAP_POPULATE"),
    (c(libc::MAP_NONBLOCK), "MAP_NONBLOCK"),
    (c(libc::MAP_STACK), "MAP_STACK"),
    (c(libc::MAP_HUGETLB), "MAP_HUGETLB"),
    (c(libc::MAP_SYNC), "MAP_SYNC"),
    (c(libc::MAP_FIXED_NOREPLACE), "MAP_FIXED_NOREPLACE"),
];

const WHENCE: &[(u64, &str)] = &[
    (c(libc::SEEK_SET), "SEEK_SET"),
    (c(libc::SEEK_CUR), "SEEK_CUR"),
    (c(libc::SEEK_END), "SEEK_END"),
    (c(libc::SEEK_DATA), "SEEK_DATA"),
    (c(libc::SEEK_HOLE), "SEEK_HOLE"),
];

const SIGPROCMASK: &[(u64, &str)] = &[
    (c(libc::SIG_BLOCK), "SIG_BLOCK"),
    (c(libc::SIG_UNBLOCK), "SIG_UNBLOCK"),
    (c(libc::SIG_SETMASK), "SIG_SETMASK"),
];

const IOCTL: &[(u64, &str)] = &[
    (0x5401, "TCGETS"),
    (0x5402, "TCSETS"),
    (0x5403, "TCSETSW"),
    (0x5404, "TCSETSF"),
    (0x5413, "TIOCGWINSZ"),
    (0x5414, "TIOCSWINSZ"),
    (0x540f, "TIOCGPGRP"),
    (0x5410, "TIOCSPGRP"),
    (0x540e, "TIOCSCTTY"),
    (0x5422, "TIOCNOTTY"),
    (0x8004_5430, "TIOCGPTN"),
    (0x4004_5431, "TIOCSPTLCK"),
    (0x541b, "FIONREAD"),
    (0x5421, "FIONBIO"),
    (0x5451, "FIOCLEX"),
    (0x5450, "FIONCLEX"),
    (0x5452, "FIOASYNC"),
    (0x8008_6601, "FS_IOC_GETFLAGS"),
    (0x4008_6602, "FS_IOC_SETFLAGS"),
];

const ACCESS: &[(u64, &str)] = &[
    (c(libc::R_OK), "R_OK"),
    (c(libc::W_OK), "W_OK"),
    (c(libc::X_OK), "X_OK"),
];

const MREMAP: &[(u64, &str)] = &[
    (c(libc::MREMAP_MAYMOVE), "MREMAP_MAYMOVE"),
    (c(libc::MREMAP_FIXED), "MREMAP_FIXED"),
    (4, "MREMAP_DONTUNMAP"),
];

const MSYNC: &[(u64, &str)] = &[
    (c(libc::MS_ASYNC), "MS_ASYNC"),
    (c(libc::MS_INVALIDATE), "MS_INVALIDATE"),
    (c(libc::MS_SYNC), "MS_SYNC"),
];

const MADVISE: &[(u64, &str)] = &[
    (c(libc::MADV_NORMAL), "MADV_NORMAL"),
    (c(libc::MADV_RANDOM), "MADV_RANDOM"),
    (c(libc::MADV_SEQUENTIAL), "MADV_SEQUENTIAL"),
    (c(libc::MADV_WILLNEED), "MADV_WILLNEED"),
    (c(libc::MADV_DONTNEED), "MADV_DONTNEED"),
    (c(libc::MADV_FREE), "MADV_FREE"),
    (c(libc::MADV_REMOVE), "MADV_REMOVE"),
    (c(libc::MADV_DONTFORK), "MADV_DONTFORK"),
    (c(libc::MADV_DOFORK), "MADV_DOFORK"),
    (c(libc::MADV_MERGEABLE), "MADV_MERGEABLE"),
    (c(libc::MADV_UNMERGEABLE), "MADV_UNMERGEABLE"),
    (c(libc::MADV_HUGEPAGE), "MADV_HUGEPAGE"),
    (c(libc::MADV_NOHUGEPAGE), "MADV_NOHUGEPAGE"),
    (c(libc::MADV_DONTDUMP), "MADV_DONTDUMP"),
    (c(libc::MADV_DODUMP), "MADV_DODUMP"),
    (c(libc::MADV_WIPEONFORK), "MADV_WIPEONFORK"),
    (c(libc::MADV_KEEPONFORK), "MADV_KEEPONFORK"),
    (c(libc::MADV_COLD), "MADV_COLD"),
    (c(libc::MADV_PAGEOUT), "MADV_PAGEOUT"),
    (c(libc::MADV_POPULATE_READ), "MADV_POPULATE_READ"),
    (c(libc::MADV_POPULATE_WRITE), "MADV_POPULATE_WRITE"),
    (c(libc::MADV_HWPOISON), "MADV_HWPOISON"),
    (25, "MADV_COLLAPSE"),
];

const AF: &[(u64, &str)] = &[
    (c(libc::AF_UNSPEC), "AF_UNSPEC"),
    (c(libc::AF_UNIX), "AF_UNIX"),
    (c(libc::AF_INET), "AF_INET"),
    (c(libc::AF_INET6), "AF_INET6"),
    (c(libc::AF_NETLINK), "AF_NETLINK"),
    (c(libc::AF_PACKET), "AF_PACKET"),
    (c(libc::AF_VSOCK), "AF_VSOCK"),
    (c(libc::AF_ALG), "AF_ALG"),
    (c(libc::AF_BLUETOOTH), "AF_BLUETOOTH"),
    (c(libc::AF_XDP), "AF_XDP"),
];

pub const SOCK_TYPES: &[(u64, &str)] = &[
    (c(libc::SOCK_STREAM), "SOCK_STREAM"),
    (c(libc::SOCK_DGRAM), "SOCK_DGRAM"),
    (c(libc::SOCK_RAW), "SOCK_RAW"),
    (c(libc::SOCK_RDM), "SOCK_RDM"),
    (c(libc::SOCK_SEQPACKET), "SOCK_SEQPACKET"),
    (10, "SOCK_PACKET"),
];

const SOCK_FLAGS: &[(u64, &str)] = &[
    (c(libc::SOCK_CLOEXEC), "SOCK_CLOEXEC"),
    (c(libc::SOCK_NONBLOCK), "SOCK_NONBLOCK"),
];

const IPPROTO: &[(u64, &str)] = &[
    (0, "IPPROTO_IP"),
    (1, "IPPROTO_ICMP"),
    (6, "IPPROTO_TCP"),
    (17, "IPPROTO_UDP"),
    (41, "IPPROTO_IPV6"),
    (58, "IPPROTO_ICMPV6"),
    (132, "IPPROTO_SCTP"),
    (136, "IPPROTO_UDPLITE"),
    (255, "IPPROTO_RAW"),
    (262, "IPPROTO_MPTCP"),
];

const MSG: &[(u64, &str)] = &[
    (c(libc::MSG_OOB), "MSG_OOB"),
    (c(libc::MSG_PEEK), "MSG_PEEK"),
    (c(libc::MSG_DONTROUTE), "MSG_DONTROUTE"),
    (c(libc::MSG_CTRUNC), "MSG_CTRUNC"),
    (c(libc::MSG_TRUNC), "MSG_TRUNC"),
    (c(libc::MSG_DONTWAIT), "MSG_DONTWAIT"),
    (c(libc::MSG_EOR), "MSG_EOR"),
    (c(libc::MSG_WAITALL), "MSG_WAITALL"),
    (c(libc::MSG_CONFIRM), "MSG_CONFIRM"),
    (c(libc::MSG_ERRQUEUE), "MSG_ERRQUEUE"),
    (c(libc::MSG_NOSIGNAL), "MSG_NOSIGNAL"),
    (c(libc::MSG_MORE), "MSG_MORE"),
    (c(libc::MSG_WAITFORONE), "MSG_WAITFORONE"),
    (c(libc::MSG_FASTOPEN), "MSG_FASTOPEN"),
    (c(libc::MSG_CMSG_CLOEXEC), "MSG_CMSG_CLOEXEC"),
];

const SHUTDOWN: &[(u64, &str)] = &[
    (c(libc::SHUT_RD), "SHUT_RD"),
    (c(libc::SHUT_WR), "SHUT_WR"),
    (c(libc::SHUT_RDWR), "SHUT_RDWR"),
];

const SOL: &[(u64, &str)] = &[
    (c(libc::SOL_SOCKET), "SOL_SOCKET"),
    (c(libc::IPPROTO_IP), "SOL_IP"),
    (c(libc::IPPROTO_TCP), "SOL_TCP"),
    (c(libc::IPPROTO_UDP), "SOL_UDP"),
    (c(libc::IPPROTO_IPV6), "SOL_IPV6"),
    (c(libc::SOL_NETLINK), "SOL_NETLINK"),
];

const CLONE: &[(u64, &str)] = &[
    (c(libc::CLONE_VM), "CLONE_VM"),
    (c(libc::CLONE_FS), "CLONE_FS"),
    (c(libc::CLONE_FILES), "CLONE_FILES"),
    (c(libc::CLONE_SIGHAND), "CLONE_SIGHAND"),
    (c(libc::CLONE_PIDFD), "CLONE_PIDFD"),
    (c(libc::CLONE_PTRACE), "CLONE_PTRACE"),
    (c(libc::CLONE_VFORK), "CLONE_VFORK"),
    (c(libc::CLONE_PARENT), "CLONE_PARENT"),
    (c(libc::CLONE_THREAD), "CLONE_THREAD"),
    (c(libc::CLONE_NEWNS), "CLONE_NEWNS"),
    (c(libc::CLONE_SYSVSEM), "CLONE_SYSVSEM"),
    (c(libc::CLONE_SETTLS), "CLONE_SETTLS"),
    (c(libc::CLONE_PARENT_SETTID), "CLONE_PARENT_SETTID"),
    (c(libc::CLONE_CHILD_CLEARTID), "CLONE_CHILD_CLEARTID"),
    (c(libc::CLONE_DETACHED), "CLONE_DETACHED"),
    (c(libc::CLONE_UNTRACED), "CLONE_UNTRACED"),
    (c(libc::CLONE_CHILD_SETTID), "CLONE_CHILD_SETTID"),
    (c(libc::CLONE_NEWCGROUP), "CLONE_NEWCGROUP"),
    (c(libc::CLONE_NEWUTS), "CLONE_NEWUTS"),
    (c(libc::CLONE_NEWIPC), "CLONE_NEWIPC"),
    (c(libc::CLONE_NEWUSER), "CLONE_NEWUSER"),
    (c(libc::CLONE_NEWPID), "CLONE_NEWPID"),
    (c(libc::CLONE_NEWNET), "CLONE_NEWNET"),
    (c(libc::CLONE_IO), "CLONE_IO"),
];

const WAIT: &[(u64, &str)] = &[
    (c(libc::WNOHANG), "WNOHANG"),
    (c(libc::WUNTRACED), "WSTOPPED"),
    (c(libc::WEXITED), "WEXITED"),
    (c(libc::WCONTINUED), "WCONTINUED"),
    (c(libc::WNOWAIT), "WNOWAIT"),
    (0x2000_0000, "__WNOTHREAD"),
    (0x4000_0000, "__WALL"),
    (0x8000_0000, "__WCLONE"),
];

const FCNTL: &[(u64, &str)] = &[
    (c(libc::F_DUPFD), "F_DUPFD"),
    (c(libc::F_GETFD), "F_GETFD"),
    (c(libc::F_SETFD), "F_SETFD"),
    (c(libc::F_GETFL), "F_GETFL"),
    (c(libc::F_SETFL), "F_SETFL"),
    (c(libc::F_GETLK), "F_GETLK"),
    (c(libc::F_SETLK), "F_SETLK"),
    (c(libc::F_SETLKW), "F_SETLKW"),
    (c(libc::F_SETOWN), "F_SETOWN"),
    (c(libc::F_GETOWN), "F_GETOWN"),
    (10, "F_SETSIG"),
    (11, "F_GETSIG"),
    (c(libc::F_OFD_GETLK), "F_OFD_GETLK"),
    (c(libc::F_OFD_SETLK), "F_OFD_SETLK"),
    (c(libc::F_OFD_SETLKW), "F_OFD_SETLKW"),
    (c(libc::F_SETLEASE), "F_SETLEASE"),
    (c(libc::F_GETLEASE), "F_GETLEASE"),
    (c(libc::F_NOTIFY), "F_NOTIFY"),
    (c(libc::F_DUPFD_CLOEXEC), "F_DUPFD_CLOEXEC"),
    (c(libc::F_SETPIPE_SZ), "F_SETPIPE_SZ"),
    (c(libc::F_GETPIPE_SZ), "F_GETPIPE_SZ"),
    (c(libc::F_ADD_SEALS), "F_ADD_SEALS"),
    (c(libc::F_GET_SEALS), "F_GET_SEALS"),
];

const FLOCK: &[(u64, &str)] = &[
    (c(libc::LOCK_SH), "LOCK_SH"),
    (c(libc::LOCK_EX), "LOCK_EX"),
    (c(libc::LOCK_NB), "LOCK_NB"),
    (c(libc::LOCK_UN), "LOCK_UN"),
];

const RLIMIT: &[(u64, &str)] = &[
    (libc::RLIMIT_CPU as u64, "RLIMIT_CPU"),
    (libc::RLIMIT_FSIZE as u64, "RLIMIT_FSIZE"),
    (libc::RLIMIT_DATA as u64, "RLIMIT_DATA"),
    (libc::RLIMIT_STACK as u64, "RLIMIT_STACK"),
    (libc::RLIMIT_CORE as u64, "RLIMIT_CORE"),
    (libc::RLIMIT_RSS as u64, "RLIMIT_RSS"),
    (libc::RLIMIT_NPROC as u64, "RLIMIT_NPROC"),
    (libc::RLIMIT_NOFILE as u64, "RLIMIT_NOFILE"),
    (libc::RLIMIT_MEMLOCK as u64, "RLIMIT_MEMLOCK"),
    (libc::RLIMIT_AS as u64, "RLIMIT_AS"),
    (libc::RLIMIT_LOCKS as u64, "RLIMIT_LOCKS"),
    (libc::RLIMIT_SIGPENDING as u64, "RLIMIT_SIGPENDING"),
    (libc::RLIMIT_MSGQUEUE as u64, "RLIMIT_MSGQUEUE"),
    (libc::RLIMIT_NICE as u64, "RLIMIT_NICE"),
    (libc::RLIMIT_RTPRIO as u64, "RLIMIT_RTPRIO"),
    (libc::RLIMIT_RTTIME as u64, "RLIMIT_RTTIME"),
];

const RUSAGE: &[(u64, &str)] = &[
    (c(libc::RUSAGE_SELF), "RUSAGE_SELF"),
    (c(libc::RUSAGE_CHILDREN), "RUSAGE_CHILDREN"),
    (c(libc::RUSAGE_THREAD), "RUSAGE_THREAD"),
];

const PTRACE: &[(u64, &str)] = &[
    (libc::PTRACE_TRACEME as u64, "PTRACE_TRACEME"),
    (libc::PTRACE_PEEKTEXT as u64, "PTRACE_PEEKTEXT"),
    (libc::PTRACE_PEEKDATA as u64, "PTRACE_PEEKDATA"),
    (libc::PTRACE_PEEKUSER as u64, "PTRACE_PEEKUSER"),
    (libc::PTRACE_POKETEXT as u64, "PTRACE_POKETEXT"),
    (libc::PTRACE_POKEDATA as u64, "PTRACE_POKEDATA"),
    (libc::PTRACE_POKEUSER as u64, "PTRACE_POKEUSER"),
    (libc::PTRACE_CONT as u64, "PTRACE_CONT"),
    (libc::PTRACE_KILL as u64, "PTRACE_KILL"),
    (libc::PTRACE_SINGLESTEP as u64, "PTRACE_SINGLESTEP"),
    (libc::PTRACE_GETREGS as u64, "PTRACE_GETREGS"),
    (libc::PTRACE_SETREGS as u64, "PTRACE_SETREGS"),
    (libc::PTRACE_ATTACH as u64, "PTRACE_ATTACH"),
    (libc::PTRACE_DETACH as u64, "PTRACE_DETACH"),
    (libc::PTRACE_SYSCALL as u64, "PTRACE_SYSCALL"),
    (libc::PTRACE_SETOPTIONS as u64, "PTRACE_SETOPTIONS"),
    (libc::PTRACE_GETEVENTMSG as u64, "PTRACE_GETEVENTMSG"),
    (libc::PTRACE_GETSIGINFO as u64, "PTRACE_GETSIGINFO"),
    (libc::PTRACE_SETSIGINFO as u64, "PTRACE_SETSIGINFO"),
    (libc::PTRACE_GETREGSET as u64, "PTRACE_GETREGSET"),
    (libc::PTRACE_SETREGSET as u64, "PTRACE_SETREGSET"),
    (libc::PTRACE_SEIZE as u64, "PTRACE_SEIZE"),
    (libc::PTRACE_INTERRUPT as u64, "PTRACE_INTERRUPT"),
    (libc::PTRACE_LISTEN as u64, "PTRACE_LISTEN"),
    (0x420e, "PTRACE_GET_SYSCALL_INFO"),
];

const PRIO: &[(u64, &str)] = &[(0, "PRIO_PROCESS"), (1, "PRIO_PGRP"), (2, "PRIO_USER")];

const SCHED_POLICY: &[(u64, &str)] = &[
    (c(libc::SCHED_OTHER), "SCHED_OTHER"),
    (c(libc::SCHED_FIFO), "SCHED_FIFO"),
    (c(libc::SCHED_RR), "SCHED_RR"),
    (c(libc::SCHED_BATCH), "SCHED_BATCH"),
    (c(libc::SCHED_IDLE), "SCHED_IDLE"),
    (6, "SCHED_DEADLINE"),
];

const MLOCKALL: &[(u64, &str)] = &[
    (c(libc::MCL_CURRENT), "MCL_CURRENT"),
    (c(libc::MCL_FUTURE), "MCL_FUTURE"),
    (c(libc::MCL_ONFAULT), "MCL_ONFAULT"),
];

const PRCTL: &[(u64, &str)] = &[
    (c(libc::PR_SET_PDEATHSIG), "PR_SET_PDEATHSIG"),
    (c(libc::PR_GET_PDEATHSIG), "PR_GET_PDEATHSIG"),
    (c(libc::PR_GET_DUMPABLE), "PR_GET_DUMPABLE"),
    (c(libc::PR_SET_DUMPABLE), "PR_SET_DUMPABLE"),
    (c(libc::PR_SET_NAME), "PR_SET_NAME"),
    (c(libc::PR_GET_NAME), "PR_GET_NAME"),
    (c(libc::PR_SET_SECCOMP), "PR_SET_SECCOMP"),
    (c(libc::PR_GET_SECCOMP), "PR_GET_SECCOMP"),
    (c(libc::PR_CAPBSET_READ), "PR_CAPBSET_READ"),
    (c(libc::PR_SET_TIMERSLACK), "PR_SET_TIMERSLACK"),
    (c(libc::PR_SET_CHILD_SUBREAPER), "PR_SET_CHILD_SUBREAPER"),
    (c(libc::PR_GET_CHILD_SUBREAPER), "PR_GET_CHILD_SUBREAPER"),
    (c(libc::PR_SET_NO_NEW_PRIVS), "PR_SET_NO_NEW_PRIVS"),
    (c(libc::PR_GET_NO_NEW_PRIVS), "PR_GET_NO_NEW_PRIVS"),
    (c(libc::PR_SET_THP_DISABLE), "PR_SET_THP_DISABLE"),
    (c(libc::PR_CAP_AMBIENT), "PR_CAP_AMBIENT"),
    (c(libc::PR_SET_VMA), "PR_SET_VMA"),
    (c(libc::PR_SET_PTRACER), "PR_SET_PTRACER"),
    (c(libc::PR_SET_MM), "PR_SET_MM"),
];

const ARCH_PRCTL: &[(u64, &str)] = &[
    (0x1001, "ARCH_SET_GS"),
    (0x1002, "ARCH_SET_FS"),
    (0x1003, "ARCH_GET_FS"),
    (0x1004, "ARCH_GET_GS"),
    (0x1011, "ARCH_GET_CPUID"),
    (0x1012, "ARCH_SET_CPUID"),
    (0x1021, "ARCH_GET_XCOMP_SUPP"),
    (0x1022, "ARCH_GET_XCOMP_PERM"),
    (0x1023, "ARCH_REQ_XCOMP_PERM"),
    (0x5001, "ARCH_SHSTK_ENABLE"),
    (0x5002, "ARCH_SHSTK_DISABLE"),
    (0x5003, "ARCH_SHSTK_LOCK"),
    (0x5005, "ARCH_SHSTK_STATUS"),
];

const MOUNT: &[(u64, &str)] = &[
    (cl(libc::MS_RDONLY), "MS_RDONLY"),
    (cl(libc::MS_NOSUID), "MS_NOSUID"),
    (cl(libc::MS_NODEV), "MS_NODEV"),
    (cl(libc::MS_NOEXEC), "MS_NOEXEC"),
    (cl(libc::MS_SYNCHRONOUS), "MS_SYNCHRONOUS"),
    (cl(libc::MS_REMOUNT), "MS_REMOUNT"),
    (cl(libc::MS_MANDLOCK), "MS_MANDLOCK"),
    (cl(libc::MS_DIRSYNC), "MS_DIRSYNC"),
    (cl(libc::MS_NOATIME), "MS_NOATIME"),
    (cl(libc::MS_NODIRATIME), "MS_NODIRATIME"),
    (cl(libc::MS_BIND), "MS_BIND"),
    (cl(libc::MS_MOVE), "MS_MOVE"),
    (cl(libc::MS_REC), "MS_REC"),
    (cl(libc::MS_SILENT), "MS_SILENT"),
    (cl(libc::MS_UNBINDABLE), "MS_UNBINDABLE"),
    (cl(libc::MS_PRIVATE), "MS_PRIVATE"),
    (cl(libc::MS_SLAVE), "MS_SLAVE"),
    (cl(libc::MS_SHARED), "MS_SHARED"),
    (cl(libc::MS_RELATIME), "MS_RELATIME"),
    (cl(libc::MS_STRICTATIME), "MS_STRICTATIME"),
    (cl(libc::MS_LAZYTIME), "MS_LAZYTIME"),
];

const UMOUNT: &[(u64, &str)] = &[
    (c(libc::MNT_FORCE), "MNT_FORCE"),
    (c(libc::MNT_DETACH), "MNT_DETACH"),
    (c(libc::MNT_EXPIRE), "MNT_EXPIRE"),
    (c(libc::UMOUNT_NOFOLLOW), "UMOUNT_NOFOLLOW"),
];

const XATTR: &[(u64, &str)] = &[(1, "XATTR_CREATE"), (2, "XATTR_REPLACE")];

pub const FUTEX_OPS: &[(u64, &str)] = &[
    (0, "FUTEX_WAIT"),
    (1, "FUTEX_WAKE"),
    (2, "FUTEX_FD"),
    (3, "FUTEX_REQUEUE"),
    (4, "FUTEX_CMP_REQUEUE"),
    (5, "FUTEX_WAKE_OP"),
    (6, "FUTEX_LOCK_PI"),
    (7, "FUTEX_UNLOCK_PI"),
    (8, "FUTEX_TRYLOCK_PI"),
    (9, "FUTEX_WAIT_BITSET"),
    (10, "FUTEX_WAKE_BITSET"),
    (11, "FUTEX_WAIT_REQUEUE_PI"),
    (12, "FUTEX_CMP_REQUEUE_PI"),
    (13, "FUTEX_LOCK_PI2"),
];

const FADVISE: &[(u64, &str)] = &[
    (c(libc::POSIX_FADV_NORMAL), "POSIX_FADV_NORMAL"),
    (c(libc::POSIX_FADV_RANDOM), "POSIX_FADV_RANDOM"),
    (c(libc::POSIX_FADV_SEQUENTIAL), "POSIX_FADV_SEQUENTIAL"),
    (c(libc::POSIX_FADV_WILLNEED), "POSIX_FADV_WILLNEED"),
    (c(libc::POSIX_FADV_DONTNEED), "POSIX_FADV_DONTNEED"),
    (c(libc::POSIX_FADV_NOREUSE), "POSIX_FADV_NOREUSE"),
];

const CLOCK: &[(u64, &str)] = &[
    (c(libc::CLOCK_REALTIME), "CLOCK_REALTIME"),
    (c(libc::CLOCK_MONOTONIC), "CLOCK_MONOTONIC"),
    (
        c(libc::CLOCK_PROCESS_CPUTIME_ID),
        "CLOCK_PROCESS_CPUTIME_ID",
    ),
    (c(libc::CLOCK_THREAD_CPUTIME_ID), "CLOCK_THREAD_CPUTIME_ID"),
    (c(libc::CLOCK_MONOTONIC_RAW), "CLOCK_MONOTONIC_RAW"),
    (c(libc::CLOCK_REALTIME_COARSE), "CLOCK_REALTIME_COARSE"),
    (c(libc::CLOCK_MONOTONIC_COARSE), "CLOCK_MONOTONIC_COARSE"),
    (c(libc::CLOCK_BOOTTIME), "CLOCK_BOOTTIME"),
    (c(libc::CLOCK_REALTIME_ALARM), "CLOCK_REALTIME_ALARM"),
    (c(libc::CLOCK_BOOTTIME_ALARM), "CLOCK_BOOTTIME_ALARM"),
    (c(libc::CLOCK_TAI), "CLOCK_TAI"),
];

const TIMER: &[(u64, &str)] = &[(c(libc::TIMER_ABSTIME), "TIMER_ABSTIME")];

const EPOLL_CTL: &[(u64, &str)] = &[
    (c(libc::EPOLL_CTL_ADD), "EPOLL_CTL_ADD"),
    (c(libc::EPOLL_CTL_DEL), "EPOLL_CTL_DEL"),
    (c(libc::EPOLL_CTL_MOD), "EPOLL_CTL_MOD"),
];

#[expect(clippy::cast_sign_loss, reason = "C constants")]
pub const EPOLL_EVENTS: &[(u64, &str)] = &[
    (libc::EPOLLIN as u64, "EPOLLIN"),
    (libc::EPOLLPRI as u64, "EPOLLPRI"),
    (libc::EPOLLOUT as u64, "EPOLLOUT"),
    (libc::EPOLLERR as u64, "EPOLLERR"),
    (libc::EPOLLHUP as u64, "EPOLLHUP"),
    (libc::EPOLLRDNORM as u64, "EPOLLRDNORM"),
    (libc::EPOLLRDBAND as u64, "EPOLLRDBAND"),
    (libc::EPOLLWRNORM as u64, "EPOLLWRNORM"),
    (libc::EPOLLWRBAND as u64, "EPOLLWRBAND"),
    (libc::EPOLLMSG as u64, "EPOLLMSG"),
    (libc::EPOLLRDHUP as u64, "EPOLLRDHUP"),
    (libc::EPOLLEXCLUSIVE as u64, "EPOLLEXCLUSIVE"),
    (libc::EPOLLWAKEUP as u64, "EPOLLWAKEUP"),
    (libc::EPOLLONESHOT as u64, "EPOLLONESHOT"),
    (libc::EPOLLET as u64 & 0xffff_ffff, "EPOLLET"),
];

pub const POLL_EVENTS: &[(u64, &str)] = &[
    (libc::POLLIN as u64, "POLLIN"),
    (libc::POLLPRI as u64, "POLLPRI"),
    (libc::POLLOUT as u64, "POLLOUT"),
    (libc::POLLERR as u64, "POLLERR"),
    (libc::POLLHUP as u64, "POLLHUP"),
    (libc::POLLNVAL as u64, "POLLNVAL"),
    (libc::POLLRDNORM as u64, "POLLRDNORM"),
    (libc::POLLRDBAND as u64, "POLLRDBAND"),
    (libc::POLLWRNORM as u64, "POLLWRNORM"),
    (libc::POLLWRBAND as u64, "POLLWRBAND"),
    (0x2000, "POLLRDHUP"),
];

const IDTYPE: &[(u64, &str)] = &[(0, "P_ALL"), (1, "P_PID"), (2, "P_PGID"), (3, "P_PIDFD")];

const AT: &[(u64, &str)] = &[
    (c(libc::AT_SYMLINK_NOFOLLOW), "AT_SYMLINK_NOFOLLOW"),
    (c(libc::AT_REMOVEDIR), "AT_REMOVEDIR"),
    (c(libc::AT_SYMLINK_FOLLOW), "AT_SYMLINK_FOLLOW"),
    (c(libc::AT_NO_AUTOMOUNT), "AT_NO_AUTOMOUNT"),
    (c(libc::AT_EMPTY_PATH), "AT_EMPTY_PATH"),
    (0x2000, "AT_STATX_FORCE_SYNC"),
    (0x4000, "AT_STATX_DONT_SYNC"),
    (c(libc::AT_RECURSIVE), "AT_RECURSIVE"),
];

const SPLICE: &[(u64, &str)] = &[
    (1, "SPLICE_F_MOVE"),
    (2, "SPLICE_F_NONBLOCK"),
    (4, "SPLICE_F_MORE"),
    (8, "SPLICE_F_GIFT"),
];

const MPOL: &[(u64, &str)] = &[(2, "MPOL_MF_MOVE"), (4, "MPOL_MF_MOVE_ALL")];

const FALLOCATE: &[(u64, &str)] = &[
    (c(libc::FALLOC_FL_KEEP_SIZE), "FALLOC_FL_KEEP_SIZE"),
    (c(libc::FALLOC_FL_PUNCH_HOLE), "FALLOC_FL_PUNCH_HOLE"),
    (
        c(libc::FALLOC_FL_COLLAPSE_RANGE),
        "FALLOC_FL_COLLAPSE_RANGE",
    ),
    (c(libc::FALLOC_FL_ZERO_RANGE), "FALLOC_FL_ZERO_RANGE"),
    (c(libc::FALLOC_FL_INSERT_RANGE), "FALLOC_FL_INSERT_RANGE"),
    (c(libc::FALLOC_FL_UNSHARE_RANGE), "FALLOC_FL_UNSHARE_RANGE"),
];

const RENAME: &[(u64, &str)] = &[
    (1, "RENAME_NOREPLACE"),
    (2, "RENAME_EXCHANGE"),
    (4, "RENAME_WHITEOUT"),
];

const GETRANDOM: &[(u64, &str)] = &[
    (1, "GRND_NONBLOCK"),
    (2, "GRND_RANDOM"),
    (4, "GRND_INSECURE"),
];

const RSEQ: &[(u64, &str)] = &[(1, "RSEQ_FLAG_UNREGISTER")];

const MEMFD: &[(u64, &str)] = &[
    (1, "MFD_CLOEXEC"),
    (2, "MFD_ALLOW_SEALING"),
    (4, "MFD_HUGETLB"),
    (8, "MFD_NOEXEC_SEAL"),
    (16, "MFD_EXEC"),
];

const BPF: &[(u64, &str)] = &[
    (0, "BPF_MAP_CREATE"),
    (1, "BPF_MAP_LOOKUP_ELEM"),
    (2, "BPF_MAP_UPDATE_ELEM"),
    (3, "BPF_MAP_DELETE_ELEM"),
    (4, "BPF_MAP_GET_NEXT_KEY"),
    (5, "BPF_PROG_LOAD"),
    (6, "BPF_OBJ_PIN"),
    (7, "BPF_OBJ_GET"),
    (8, "BPF_PROG_ATTACH"),
    (9, "BPF_PROG_DETACH"),
    (10, "BPF_PROG_TEST_RUN"),
    (11, "BPF_PROG_GET_NEXT_ID"),
    (12, "BPF_MAP_GET_NEXT_ID"),
    (13, "BPF_PROG_GET_FD_BY_ID"),
    (14, "BPF_MAP_GET_FD_BY_ID"),
    (15, "BPF_OBJ_GET_INFO_BY_FD"),
    (16, "BPF_PROG_QUERY"),
    (17, "BPF_RAW_TRACEPOINT_OPEN"),
    (18, "BPF_BTF_LOAD"),
    (19, "BPF_BTF_GET_FD_BY_ID"),
    (20, "BPF_TASK_FD_QUERY"),
    (21, "BPF_MAP_LOOKUP_AND_DELETE_ELEM"),
    (22, "BPF_MAP_FREEZE"),
    (23, "BPF_BTF_GET_NEXT_ID"),
    (24, "BPF_MAP_LOOKUP_BATCH"),
    (25, "BPF_MAP_LOOKUP_AND_DELETE_BATCH"),
    (26, "BPF_MAP_UPDATE_BATCH"),
    (27, "BPF_MAP_DELETE_BATCH"),
    (28, "BPF_LINK_CREATE"),
    (29, "BPF_LINK_UPDATE"),
    (30, "BPF_LINK_GET_FD_BY_ID"),
    (31, "BPF_LINK_GET_NEXT_ID"),
    (32, "BPF_ENABLE_STATS"),
    (33, "BPF_ITER_CREATE"),
    (34, "BPF_LINK_DETACH"),
    (35, "BPF_PROG_BIND_MAP"),
    (36, "BPF_TOKEN_CREATE"),
];

const CLOSE_RANGE: &[(u64, &str)] = &[(2, "CLOSE_RANGE_UNSHARE"), (4, "CLOSE_RANGE_CLOEXEC")];

#[must_use]
pub fn set(name: &str) -> Option<Set> {
    Some(match name {
        "open" | "oflags" => flags(OPEN),
        "efd" => flags(EFD),
        "in_init" => flags(IN_INIT),
        "mmap_prot" => flags(MMAP_PROT),
        "mmap_flags" => flags(MMAP_FLAGS),
        "whence" => values(WHENCE),
        "sigprocmask" => values(SIGPROCMASK),
        "ioctl" => values(IOCTL),
        "access" => flags(ACCESS),
        "mremap" => flags(MREMAP),
        "msync" => flags(MSYNC),
        "madvise" => values(MADVISE),
        "af" => values(AF),
        "sock_type" => flags(SOCK_FLAGS),
        "ipproto" => values(IPPROTO),
        "msg" => flags(MSG),
        "shutdown" => values(SHUTDOWN),
        "sol" => values(SOL),
        "clone" => wide_flags(CLONE),
        "wait" => flags(WAIT),
        "fcntl" => values(FCNTL),
        "flock" => flags(FLOCK),
        "rlimit" => values(RLIMIT),
        "rusage" => values(RUSAGE),
        "ptrace" => values(PTRACE),
        "prio" => values(PRIO),
        "sched_policy" => values(SCHED_POLICY),
        "mlockall" => flags(MLOCKALL),
        "prctl" => values(PRCTL),
        "arch_prctl" => values(ARCH_PRCTL),
        "mount" => wide_flags(MOUNT),
        "umount" => flags(UMOUNT),
        "xattr" => flags(XATTR),
        "futex" => values(FUTEX_OPS),
        "fadvise" => values(FADVISE),
        "clock" => values(CLOCK),
        "timer" => flags(TIMER),
        "epoll_ctl" => values(EPOLL_CTL),
        "idtype" => values(IDTYPE),
        "at" => flags(AT),
        "splice" => flags(SPLICE),
        "mpol" => flags(MPOL),
        "fallocate" => flags(FALLOCATE),
        "rename" => flags(RENAME),
        "getrandom" => flags(GETRANDOM),
        "rseq" => flags(RSEQ),
        "memfd" => flags(MEMFD),
        "bpf" => values(BPF),
        "close_range" => flags(CLOSE_RANGE),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_set_a_spec_names_is_known() {
        for name in crate::syscalls::specs::sets() {
            assert!(
                super::set(name).is_some(),
                "the specs name the unknown set {name:?}"
            );
        }
    }
}
