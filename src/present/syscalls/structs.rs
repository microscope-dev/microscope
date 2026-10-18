//! Structures behind pointer arguments, under x86-64 layouts, printed as
//! strace prints them: abbreviated unless `-v` asks for every field.

use microscope::syscalls::SyscallEvent;
use microscope::syscalls::blobs;
use microscope::syscalls::sockaddr::SocketAddress;
use microscope::syscalls::specs::{Arg, Count, Direction, Spec, Struct};
use nix::libc;

use super::{Kind, Style, Tokens, sets};

/// Little-endian reads that come back `None` past the end.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn u8(&self, at: usize) -> Option<u8> {
        self.0.get(at).copied()
    }

    fn u16(&self, at: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.0.get(at..at + 2)?.try_into().ok()?))
    }

    fn u32(&self, at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.0.get(at..at + 4)?.try_into().ok()?))
    }

    fn i32(&self, at: usize) -> Option<i32> {
        Some(i32::from_le_bytes(self.0.get(at..at + 4)?.try_into().ok()?))
    }

    fn u64(&self, at: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.0.get(at..at + 8)?.try_into().ok()?))
    }

    fn i64(&self, at: usize) -> Option<i64> {
        Some(i64::from_le_bytes(self.0.get(at..at + 8)?.try_into().ok()?))
    }
}

/// strace's name for a signal's `si_code`.
pub fn si_code(signal: i32, code: i32) -> String {
    let generic = match code {
        0 => Some("SI_USER"),
        0x80 => Some("SI_KERNEL"),
        -1 => Some("SI_QUEUE"),
        -2 => Some("SI_TIMER"),
        -3 => Some("SI_MESGQ"),
        -4 => Some("SI_ASYNCIO"),
        -5 => Some("SI_SIGIO"),
        -6 => Some("SI_TKILL"),
        _ => None,
    };

    if let Some(name) = generic {
        return name.to_owned();
    }

    let specific: &[&str] = match signal {
        libc::SIGCHLD => &[
            "CLD_EXITED",
            "CLD_KILLED",
            "CLD_DUMPED",
            "CLD_TRAPPED",
            "CLD_STOPPED",
            "CLD_CONTINUED",
        ],
        libc::SIGSEGV => &["SEGV_MAPERR", "SEGV_ACCERR", "SEGV_BNDERR", "SEGV_PKUERR"],
        libc::SIGBUS => &["BUS_ADRALN", "BUS_ADRERR", "BUS_OBJERR"],
        libc::SIGTRAP => &["TRAP_BRKPT", "TRAP_TRACE", "TRAP_BRANCH", "TRAP_HWBKPT"],
        libc::SIGILL => &[
            "ILL_ILLOPC",
            "ILL_ILLOPN",
            "ILL_ILLADR",
            "ILL_ILLTRP",
            "ILL_PRVOPC",
            "ILL_PRVREG",
            "ILL_COPROC",
            "ILL_BADSTK",
        ],
        libc::SIGFPE => &[
            "FPE_INTDIV",
            "FPE_INTOVF",
            "FPE_FLTDIV",
            "FPE_FLTOVF",
            "FPE_FLTUND",
            "FPE_FLTRES",
            "FPE_FLTINV",
            "FPE_FLTSUB",
        ],
        libc::SIGSYS => &["SYS_SECCOMP", "SYS_USER_DISPATCH"],
        libc::SIGPOLL => &[
            "POLL_IN", "POLL_OUT", "POLL_MSG", "POLL_ERR", "POLL_PRI", "POLL_HUP",
        ],
        _ => &[],
    };

    usize::try_from(code - 1)
        .ok()
        .and_then(|index| specific.get(index))
        .map_or_else(|| code.to_string(), |&name| name.to_owned())
}

/// Bits by name, `|`-joined, with what is left in hex, or `0`.
fn bits(names: &[(u64, &str)], mut value: u64) -> String {
    let mut parts = Vec::new();
    for &(bit, name) in names {
        if bit != 0 && value & bit == bit {
            parts.push(name.to_owned());
            value &= !bit;
        }
    }

    if value != 0 {
        parts.push(format!("{value:#x}"));
    }

    if parts.is_empty() {
        "0".to_owned()
    } else {
        parts.join("|")
    }
}

/// A file mode as strace prints it: `S_IFREG|0644`.
pub fn file_mode(mode: u32) -> String {
    let kind = match mode & 0o170_000 {
        0o140_000 => "S_IFSOCK|",
        0o120_000 => "S_IFLNK|",
        0o100_000 => "S_IFREG|",
        0o060_000 => "S_IFBLK|",
        0o040_000 => "S_IFDIR|",
        0o020_000 => "S_IFCHR|",
        0o010_000 => "S_IFIFO|",
        _ => "",
    };

    let mut special = String::new();
    for (bit, name) in [
        (0o4000, "S_ISUID|"),
        (0o2000, "S_ISGID|"),
        (0o1000, "S_ISVTX|"),
    ] {
        if mode & bit != 0 {
            special.push_str(name);
        }
    }

    format!("{kind}{special}{:04o}", mode & 0o777)
}

/// Signals in a set, as `[INT TERM]`, or its complement as `~[...]` when
/// most are set.
fn sigset(bytes: &[u8]) -> String {
    let mut words = [0u64; 1];
    let reader = Bytes(bytes);
    words[0] = reader.u64(0).unwrap_or(0);
    let set: Vec<i32> = (1..=64)
        .filter(|&signal| words[0] & (1 << (signal - 1)) != 0)
        .collect();
    let name = |signal: i32| super::signal(signal).trim_start_matches("SIG").to_owned();
    if set.len() > 32 {
        let missing: Vec<String> = (1..=64)
            .filter(|signal| !set.contains(signal))
            .map(name)
            .collect();
        format!("~[{}]", missing.join(" "))
    } else {
        format!(
            "[{}]",
            set.into_iter().map(name).collect::<Vec<_>>().join(" ")
        )
    }
}

pub(super) struct Decode<'a, 'b> {
    pub out: &'a mut Tokens,
    pub event: &'b SyscallEvent,
    pub index: usize,
    pub spec: &'b Spec,
    pub style: &'b Style,
    pub context: super::Context<'b>,
}

impl Decode<'_, '_> {
    fn punct(&mut self, text: &str) {
        self.out.push(Kind::Punctuation, text);
    }

    fn field(&mut self, name: &str) {
        self.out.push(Kind::Text, name);
        self.out.push(Kind::Punctuation, "=");
    }

    fn number(&mut self, value: impl std::fmt::Display) {
        self.out.push(Kind::Number, value.to_string());
    }

    fn constant(&mut self, text: impl Into<String>) {
        self.out.push(Kind::Constant, text.into());
    }

    /// A value with a name, as `-X` asks: the name, the number, or both.
    fn named_value(&mut self, number: String, name: String) {
        match self.style.constants {
            super::Constants::Abbrev => self.constant(name),
            super::Constants::Raw => self.number(number),
            super::Constants::Verbose => {
                self.number(number);
                self.out.push(Kind::Comment, format!(" /* {name} */"));
            }
        }
    }

    /// A resource limit: infinity, or kibibytes when it is a whole number
    /// of them, as strace names them.
    fn limit(&mut self, value: u64) {
        if value == u64::MAX {
            self.named_value(value.to_string(), "RLIM64_INFINITY".to_owned());
        } else if value > 1024 && value.is_multiple_of(1024) {
            self.named_value(value.to_string(), format!("{}*1024", value / 1024));
        } else {
            self.number(value);
        }
    }

    /// A file mode: its type and permissions.
    fn mode(&mut self, mode: u32) {
        let octal = format!("{mode:#o}").replacen("0o", "0", 1);
        self.named_value(octal, file_mode(mode));
    }

    fn address(&mut self, value: u64) {
        if value == 0 {
            self.constant("NULL");
        } else {
            self.out.push(Kind::Address, format!("{value:#x}"));
        }
    }

    fn more(&mut self) {
        self.punct(", ...");
    }

    fn timespec(&mut self, bytes: &Bytes<'_>, at: usize, fraction: &str) -> Option<()> {
        self.punct("{");
        self.field("tv_sec");
        self.number(bytes.i64(at)?);
        self.punct(", ");
        self.field(fraction);
        self.number(bytes.i64(at + 8)?);
        self.punct("}");
        Some(())
    }

    /// Prints `elements` structures from `bytes`, or the address when the
    /// bytes do not cover one.
    pub fn structure(mut self, structure: Struct, bytes: &[u8], elements: usize, truncated: bool) {
        let word = self.event.args[self.index];
        let reader = Bytes(bytes);
        let printed = match structure {
            Struct::Pollfd
            | Struct::EpollEvent
            | Struct::Iovec
            | Struct::FdSet
            | Struct::Dirents => self.array(structure, &reader, elements, truncated),
            Struct::Timespec2 | Struct::Timeval2 => {
                let fraction = if structure == Struct::Timespec2 {
                    "tv_nsec"
                } else {
                    "tv_usec"
                };

                (|| {
                    let _ = reader.u64(24)?;
                    self.punct("[");
                    self.timespec(&reader, 0, fraction)?;
                    self.punct(", ");
                    self.timespec(&reader, 16, fraction)?;
                    self.punct("]");
                    Some(())
                })()
            }
            _ => self.single(structure, &reader),
        };

        if printed.is_none() {
            self.address(word);
        }
    }

    #[expect(clippy::too_many_lines, reason = "one arm per structure")]
    fn single(&mut self, structure: Struct, b: &Bytes<'_>) -> Option<()> {
        // Check the whole structure is there before printing any of it.
        let size = structure.size() as usize;
        if structure != Struct::Sockaddr && b.0.len() < size {
            return None;
        }

        match structure {
            Struct::Rseq => {
                // Each field prints when the registered length covers it.
                let length = b.0.len();
                self.punct("{");
                self.field("cpu_id_start");
                self.number(b.u32(0)?);
                self.punct(", ");
                self.field("cpu_id");
                match b.i32(4)? {
                    -1 => self.named_value("-1".to_owned(), "RSEQ_CPU_ID_UNINITIALIZED".to_owned()),
                    -2 => self.named_value(
                        "-2".to_owned(),
                        "RSEQ_CPU_ID_REGISTRATION_FAILED".to_owned(),
                    ),
                    cpu => self.number(cpu),
                }

                self.punct(", ");
                self.field("rseq_cs");
                match b.u64(8)? {
                    0 => self.constant("NULL".to_owned()),
                    address => self.number(format!("{address:#x}")),
                }

                self.punct(", ");
                self.field("flags");
                self.number(b.u32(16)?);
                for (at, name) in [(20, "node_id"), (24, "mm_cid")] {
                    if length >= at + 4 {
                        self.punct(", ");
                        self.field(name);
                        self.number(b.u32(at)?);
                    }
                }

                if length >= 32 {
                    self.punct(", ");
                    self.field("slice_ctrl");
                    self.punct("{");
                    self.field("request");
                    self.number(b.u8(28)?);
                    self.punct(", ");
                    self.field("granted");
                    self.number(b.u8(29)?);
                    self.punct(", ");
                    self.field("__reserved");
                    self.number(b.u16(30)?);
                    self.punct("}");
                }

                // strace prints the byte after slice_ctrl once the length
                // covers slice_ctrl, as zero when the length stops short of
                // it, so a 32-byte registration prints it too.
                if length >= 32 {
                    self.punct(", ");
                    self.field("__reserved");
                    self.number(if length >= 33 { b.u8(32)? } else { 0 });
                }

                if length > 33 {
                    self.more();
                }

                self.punct("}");
            }
            Struct::Stat => {
                self.punct("{");
                if self.style.verbose {
                    self.field("st_dev");
                    self.number(makedev(b.u64(0)?));
                    self.punct(", ");
                    self.field("st_ino");
                    self.number(b.u64(8)?);
                    self.punct(", ");
                }

                self.field("st_mode");
                let mode = b.u32(24)?;
                self.mode(mode);
                self.punct(", ");
                if self.style.verbose {
                    self.field("st_nlink");
                    self.number(b.u64(16)?);
                    self.punct(", ");
                    self.field("st_uid");
                    self.number(b.u32(28)?);
                    self.punct(", ");
                    self.field("st_gid");
                    self.number(b.u32(32)?);
                    self.punct(", ");
                    self.field("st_blksize");
                    self.number(b.i64(56)?);
                    self.punct(", ");
                    self.field("st_blocks");
                    self.number(b.i64(64)?);
                    self.punct(", ");
                }

                // A device has numbers where a file has a size.
                let kind = b.u32(24)? & 0o170_000;
                if kind == 0o020_000 || kind == 0o060_000 {
                    self.field("st_rdev");
                    self.number(makedev(b.u64(40)?));
                } else {
                    self.field("st_size");
                    self.number(b.i64(48)?);
                }

                if !self.style.verbose {
                    self.more();
                }

                self.punct("}");
            }
            Struct::Statx => {
                self.punct("{");
                self.field("stx_mask");
                self.number(format!("{:#x}", b.u32(0)?));
                self.punct(", ");
                self.field("stx_attributes");
                self.number(b.u64(8)?);
                self.punct(", ");
                self.field("stx_mode");
                self.mode(u32::from(b.u16(28)?));
                self.punct(", ");
                self.field("stx_size");
                self.number(b.u64(40)?);
                self.more();
                self.punct("}");
            }
            Struct::Statfs => {
                self.punct("{");
                self.field("f_type");
                self.number(format!("{:#x}", b.u64(0)?));
                self.punct(", ");
                self.field("f_bsize");
                self.number(b.u64(8)?);
                self.punct(", ");
                self.field("f_blocks");
                self.number(b.u64(16)?);
                self.punct(", ");
                self.field("f_bfree");
                self.number(b.u64(24)?);
                self.punct(", ");
                self.field("f_bavail");
                self.number(b.u64(32)?);
                self.more();
                self.punct("}");
            }
            Struct::Timespec => self.timespec(b, 0, "tv_nsec")?,
            Struct::Timeval => self.timespec(b, 0, "tv_usec")?,
            Struct::Itimerval | Struct::Itimerspec => {
                let (fraction, step) = if structure == Struct::Itimerval {
                    ("tv_usec", 16)
                } else {
                    ("tv_nsec", 16)
                };

                self.punct("{");
                self.field("it_interval");
                self.timespec(b, 0, fraction)?;
                self.punct(", ");
                self.field("it_value");
                self.timespec(b, step, fraction)?;
                self.punct("}");
            }
            Struct::Timezone => {
                self.punct("{");
                self.field("tz_minuteswest");
                self.number(b.i32(0)?);
                self.punct(", ");
                self.field("tz_dsttime");
                self.number(b.i32(4)?);
                self.punct("}");
            }
            Struct::Rlimit => {
                self.punct("{");
                self.field("rlim_cur");
                self.limit(b.u64(0)?);
                self.punct(", ");
                self.field("rlim_max");
                self.limit(b.u64(8)?);
                self.punct("}");
            }
            Struct::Rusage => {
                self.punct("{");
                self.field("ru_utime");
                self.timespec(b, 0, "tv_usec")?;
                self.punct(", ");
                self.field("ru_stime");
                self.timespec(b, 16, "tv_usec")?;
                self.punct(", ");
                self.field("ru_maxrss");
                self.number(b.i64(32)?);
                self.more();
                self.punct("}");
            }
            Struct::Utsname => {
                let field = |at: usize| {
                    let raw = &b.0[at..at + 65];
                    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                    format!("{:?}", String::from_utf8_lossy(&raw[..end]))
                };

                self.punct("{");
                self.field("sysname");
                self.out.push(Kind::String, field(0));
                self.punct(", ");
                self.field("nodename");
                self.out.push(Kind::String, field(65));
                if self.style.verbose {
                    for (name, at) in [
                        ("release", 130),
                        ("version", 195),
                        ("machine", 260),
                        ("domainname", 325),
                    ] {
                        self.punct(", ");
                        self.field(name);
                        self.out.push(Kind::String, field(at));
                    }
                } else {
                    self.more();
                }

                self.punct("}");
            }
            Struct::Sysinfo => {
                self.punct("{");
                self.field("uptime");
                self.number(b.i64(0)?);
                self.punct(", ");
                self.field("totalram");
                self.number(b.u64(32)?);
                self.punct(", ");
                self.field("freeram");
                self.number(b.u64(40)?);
                self.punct(", ");
                self.field("procs");
                self.number(b.u16(80)?);
                self.more();
                self.punct("}");
            }
            Struct::Tms => {
                self.punct("{");
                for (position, name) in ["tms_utime", "tms_stime", "tms_cutime", "tms_cstime"]
                    .iter()
                    .enumerate()
                {
                    if position > 0 {
                        self.punct(", ");
                    }

                    self.field(name);
                    self.number(b.i64(position * 8)?);
                }

                self.punct("}");
            }
            Struct::Sigaction => {
                let handler = b.u64(0)?;
                let flags = b.u64(8)?;
                self.punct("{");
                self.field("sa_handler");
                match handler {
                    0 => self.constant("SIG_DFL"),
                    1 => self.constant("SIG_IGN"),
                    _ => self.address(handler),
                }

                self.punct(", ");
                self.field("sa_mask");
                self.constant(sigset(&b.0[24..32]));
                self.punct(", ");
                self.field("sa_flags");
                self.constant(bits(SA_FLAGS, flags));
                if flags & 0x0400_0000 != 0 {
                    self.punct(", ");
                    self.field("sa_restorer");
                    self.address(b.u64(16)?);
                }

                self.punct("}");
            }
            Struct::Sigset => self.constant(sigset(b.0)),
            Struct::Msghdr => {
                self.punct("{");
                self.field("msg_name");
                self.address(b.u64(0)?);
                self.punct(", ");
                self.field("msg_namelen");
                self.number(b.u32(8)?);
                self.punct(", ");
                self.field("msg_iov");
                self.address(b.u64(16)?);
                self.punct(", ");
                self.field("msg_iovlen");
                self.number(b.u64(24)?);
                self.punct(", ");
                self.field("msg_controllen");
                self.number(b.u64(40)?);
                self.punct(", ");
                self.field("msg_flags");
                self.constant(bits(MSG_FLAGS_FIELD, u64::from(b.u32(48)?)));
                self.punct("}");
            }
            Struct::Sockaddr => {
                let length = match self.spec.args.get(self.index).copied() {
                    Some(Arg::Mem {
                        count: Count::Arg(arg),
                        ..
                    }) => usize::try_from(self.event.args[usize::from(arg)] & 0xffff_ffff)
                        .unwrap_or(0),
                    _ => b.0.len(),
                };

                let address = SocketAddress::parse(&b.0[..b.0.len().min(length)])?;
                self.constant(address.to_string());
            }
            Struct::WaitStatus => {
                self.punct("[");
                self.constant(wait_status(b.i32(0)?));
                self.punct("]");
            }
            Struct::Int => {
                self.punct("[");
                self.number(b.i32(0)?);
                self.punct("]");
            }
            Struct::U64 => {
                self.punct("[");
                self.number(b.u64(0)?);
                self.punct("]");
            }
            Struct::Fds2 => {
                let process = self.event.process;

                // The call made these descriptors.
                let made = super::Context {
                    before: false,
                    ..self.context
                };

                self.punct("[");
                super::descriptor(self.out, process, b.i32(0)?.into(), self.style, made);
                self.punct(", ");
                super::descriptor(self.out, process, b.i32(4)?.into(), self.style, made);
                self.punct("]");
            }
            Struct::Siginfo => siginfo(self.out, b.0)?,
            Struct::CloneArgs => {
                self.punct("{");
                self.field("flags");
                let flags = sets::set("clone").map_or_else(
                    || format!("{:#x}", b.u64(0).unwrap_or(0)),
                    |set| bits(set.names, b.u64(0).unwrap_or(0)),
                );
                self.constant(flags);
                self.punct(", ");
                self.field("exit_signal");
                #[expect(clippy::cast_possible_truncation, reason = "a signal number")]
                self.constant(super::signal(b.u64(32)? as i32));
                self.punct(", ");
                self.field("stack");
                self.address(b.u64(40)?);
                self.punct(", ");
                self.field("stack_size");
                self.number(format!("{:#x}", b.u64(48)?));
                self.more();
                self.punct("}");
            }
            Struct::OpenHow => {
                self.punct("{");
                self.field("flags");
                let mut tokens = Tokens::default();
                super::flags(&mut tokens, self.event, "open", b.u64(0)?, self.style);
                self.out.0.extend(tokens.0);
                self.punct(", ");
                self.field("mode");
                self.number(format!("0{:o}", b.u64(8)?));
                self.punct(", ");
                self.field("resolve");
                self.number(format!("{:#x}", b.u64(16)?));
                self.punct("}");
            }
            Struct::Sigaltstack => {
                self.punct("{");
                self.field("ss_sp");
                self.address(b.u64(0)?);
                self.punct(", ");
                self.field("ss_flags");
                self.number(b.i32(8)?);
                self.punct(", ");
                self.field("ss_size");
                self.number(b.u64(16)?);
                self.punct("}");
            }
            Struct::SchedParam => {
                self.punct("{");
                self.field("sched_priority");
                self.number(b.i32(0)?);
                self.punct("}");
            }
            Struct::Utimbuf => {
                self.punct("{");
                self.field("actime");
                self.number(b.i64(0)?);
                self.punct(", ");
                self.field("modtime");
                self.number(b.i64(8)?);
                self.punct("}");
            }

            Struct::Pollfd
            | Struct::EpollEvent
            | Struct::Iovec
            | Struct::Mmsghdr
            | Struct::FdSet
            | Struct::Dirents
            | Struct::Timespec2
            | Struct::Timeval2 => return None,
        }

        Some(())
    }

    fn iovecs(&mut self, b: &Bytes<'_>, elements: usize, truncated: bool) {
        let iovecs = blobs::iovecs(b.0);

        // An output vector holds only as many bytes as the call moved,
        // across all its buffers.
        let output = self.spec.args.get(self.index).is_some_and(|arg| {
            matches!(
                arg,
                Arg::Mem {
                    direction: Direction::Out,
                    ..
                }
            )
        });
        let mut budget = match self.event.outcome.raw() {
            Some(moved) if output => usize::try_from(moved).unwrap_or(0),
            _ => usize::MAX,
        };

        self.punct("[");
        for (position, iovec) in iovecs.iter().enumerate() {
            if position > 0 {
                self.punct(", ");
            }

            self.punct("{");
            self.field("iov_base");
            let length = usize::try_from(iovec.len).unwrap_or(usize::MAX).min(budget);
            budget -= length;
            let shown = &iovec.bytes[..iovec.bytes.len().min(length)];
            super::quote(
                self.out,
                shown,
                Some(self.style.string_limit),
                shown.len() < length,
                self.style,
            );
            self.punct(", ");
            self.field("iov_len");
            self.number(iovec.len);
            self.punct("}");
        }

        if iovecs.len() < elements || truncated {
            if !iovecs.is_empty() {
                self.punct(", ");
            }

            self.punct("...");
        }

        self.punct("]");
    }

    fn array(
        &mut self,
        structure: Struct,
        b: &Bytes<'_>,
        elements: usize,
        truncated: bool,
    ) -> Option<()> {
        match structure {
            Struct::Dirents => {
                // Entries are variable-length; their count is what helps.
                let mut count = 0;
                let mut at = 0;
                while let Some(length) = b.u16(at + 16) {
                    if length == 0 {
                        break;
                    }

                    count += 1;
                    at += usize::from(length);
                }

                self.address(self.event.args[self.index]);
                self.out
                    .push(Kind::Comment, format!(" /* {count} entries */"));
                return Some(());
            }
            Struct::FdSet => {
                let limit = usize::try_from(self.event.args[0] & 0xffff_ffff)
                    .unwrap_or(0)
                    .min(b.0.len() * 8);
                let set: Vec<String> = (0..limit)
                    .filter(|&fd| b.0[fd / 8] & (1 << (fd % 8)) != 0)
                    .map(|fd| fd.to_string())
                    .collect();
                self.punct("[");
                self.out.push(Kind::Fd, set.join(" "));
                self.punct("]");
                return Some(());
            }
            Struct::Iovec => {
                self.iovecs(b, elements, truncated);
                return Some(());
            }
            _ => {}
        }

        let size = structure.size() as usize;
        let available = b.0.len() / size;
        let shown = elements.min(available);
        self.punct("[");
        for element in 0..shown {
            if element > 0 {
                self.punct(", ");
            }

            let at = element * size;
            match structure {
                Struct::Pollfd => {
                    self.punct("{");
                    self.field("fd");
                    self.out.push(Kind::Fd, b.i32(at)?.to_string());
                    self.punct(", ");
                    self.field("events");
                    self.constant(bits(sets::POLL_EVENTS, u64::from(b.u16(at + 4)?)));
                    let revents = b.u16(at + 6)?;
                    if revents != 0 {
                        self.punct(", ");
                        self.field("revents");
                        self.constant(bits(sets::POLL_EVENTS, u64::from(revents)));
                    }

                    self.punct("}");
                }
                Struct::EpollEvent => {
                    self.punct("{");
                    self.field("events");
                    self.constant(bits(sets::EPOLL_EVENTS, u64::from(b.u32(at)?)));
                    self.punct(", ");
                    self.field("data");
                    let data = b.u64(at + 4)?;
                    self.punct("{");
                    self.field("u32");
                    self.number(data & 0xffff_ffff);
                    self.punct(", ");
                    self.field("u64");
                    self.number(data);
                    self.punct("}}");
                }
                _ => return None,
            }
        }

        if shown < elements {
            if shown > 0 {
                self.punct(", ");
            }

            self.punct("...");
        }

        self.punct("]");
        Some(())
    }
}

const SA_FLAGS: &[(u64, &str)] = &[
    (0x0000_0001, "SA_NOCLDSTOP"),
    (0x0000_0002, "SA_NOCLDWAIT"),
    (0x0000_0004, "SA_SIGINFO"),
    (0x0400_0000, "SA_RESTORER"),
    (0x0800_0000, "SA_ONSTACK"),
    (0x1000_0000, "SA_RESTART"),
    (0x4000_0000, "SA_NODEFER"),
    (0x8000_0000, "SA_RESETHAND"),
];

const MSG_FLAGS_FIELD: &[(u64, &str)] = &[
    (0x08, "MSG_CTRUNC"),
    (0x20, "MSG_TRUNC"),
    (0x80, "MSG_EOR"),
    (0x2000, "MSG_ERRQUEUE"),
];

/// A device number as strace prints it, with glibc's split of the bits.
fn makedev(device: u64) -> String {
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff);
    let minor = (device & 0xff) | ((device >> 12) & !0xff);
    format!("makedev({major:#x}, {minor:#x})")
}

/// A wait status as strace prints it: `{WIFEXITED(s) && WEXITSTATUS(s) == 4}`.
fn wait_status(status: i32) -> String {
    let low = status & 0x7f;
    let mut text = if low == 0 {
        format!(
            "{{WIFEXITED(s) && WEXITSTATUS(s) == {}",
            (status >> 8) & 0xff
        )
    } else if low == 0x7f {
        format!(
            "{{WIFSTOPPED(s) && WSTOPSIG(s) == {}",
            super::signal((status >> 8) & 0xff)
        )
    } else if status == 0xffff {
        "{WIFCONTINUED(s)".to_owned()
    } else {
        let mut text = format!("{{WIFSIGNALED(s) && WTERMSIG(s) == {}", super::signal(low));
        if status & 0x80 != 0 {
            text.push_str(" && WCOREDUMP(s)");
        }

        text
    };

    text.push('}');

    // A ptrace event sits above the stop signal.
    let event = (status >> 16) & 0xff;
    if event != 0 {
        let name = match event {
            1 => "PTRACE_EVENT_FORK".to_owned(),
            2 => "PTRACE_EVENT_VFORK".to_owned(),
            3 => "PTRACE_EVENT_CLONE".to_owned(),
            4 => "PTRACE_EVENT_EXEC".to_owned(),
            5 => "PTRACE_EVENT_VFORK_DONE".to_owned(),
            6 => "PTRACE_EVENT_EXIT".to_owned(),
            7 => "PTRACE_EVENT_SECCOMP".to_owned(),
            128 => "PTRACE_EVENT_STOP".to_owned(),
            other => other.to_string(),
        };

        text = format!("{text} | {name} << 16");
    }

    text
}

/// A `siginfo_t`, with the fields its signal and code fill, as strace's
/// `printsiginfo` prints it.
pub fn siginfo(out: &mut Tokens, bytes: &[u8]) -> Option<()> {
    let b = Bytes(bytes);
    let signo = b.i32(0)?;
    let errno = b.i32(4)?;
    let code = b.i32(8)?;
    let field = |out: &mut Tokens, name: &str, kind: Kind, value: String| {
        out.push(Kind::Punctuation, ", ");
        out.push(Kind::Text, name);
        out.push(Kind::Punctuation, "=");
        out.push(kind, value);
    };

    let address = |value: u64| {
        if value == 0 {
            (Kind::Constant, "NULL".to_owned())
        } else {
            (Kind::Address, format!("{value:#x}"))
        }
    };

    out.push(Kind::Punctuation, "{");
    out.push(Kind::Text, "si_signo");
    out.push(Kind::Punctuation, "=");
    out.push(Kind::Constant, super::signal(signo));
    field(out, "si_code", Kind::Constant, si_code(signo, code));
    if errno != 0 {
        let (name, _) = super::errno(microscope::syscalls::Errno(
            u16::try_from(errno.unsigned_abs()).unwrap_or(0),
        ));
        field(out, "si_errno", Kind::Error, name);
    }

    if code <= 0 {
        // Sent by a process: its pid and uid, and a queued value.
        if code == -2 {
            field(
                out,
                "si_timerid",
                Kind::Number,
                format!("{:#x}", b.i32(16)?),
            );
            field(out, "si_overrun", Kind::Number, b.i32(20)?.to_string());
        } else {
            field(out, "si_pid", Kind::Pid, b.i32(16)?.to_string());
            field(out, "si_uid", Kind::Number, b.u32(20)?.to_string());
        }

        if matches!(code, -3..=-1) {
            field(out, "si_int", Kind::Number, b.i32(24)?.to_string());
            let (kind, text) = address(b.u64(24)?);
            field(out, "si_ptr", kind, text);
        }
    } else {
        match signo {
            libc::SIGCHLD => {
                field(out, "si_pid", Kind::Pid, b.i32(16)?.to_string());
                field(out, "si_uid", Kind::Number, b.u32(20)?.to_string());
                let status = b.i32(24)?;
                let status = if code == 1 {
                    (Kind::Number, status.to_string())
                } else {
                    (Kind::Constant, super::signal(status))
                };

                field(out, "si_status", status.0, status.1);
                field(out, "si_utime", Kind::Number, b.i64(32)?.to_string());
                field(out, "si_stime", Kind::Number, b.i64(40)?.to_string());
            }
            libc::SIGILL | libc::SIGFPE | libc::SIGSEGV | libc::SIGBUS | libc::SIGTRAP => {
                let (kind, text) = address(b.u64(16)?);
                field(out, "si_addr", kind, text);
            }
            libc::SIGPOLL => {
                field(out, "si_band", Kind::Number, b.i64(16)?.to_string());
                field(out, "si_fd", Kind::Number, b.i32(24)?.to_string());
            }
            libc::SIGSYS => {
                let (kind, text) = address(b.u64(16)?);
                field(out, "si_call_addr", kind, text);
                field(out, "si_syscall", Kind::Number, b.i32(24)?.to_string());
                let arch = b.u32(28)?;
                let arch = match arch {
                    0xc000_003e => "AUDIT_ARCH_X86_64".to_owned(),
                    0x4000_0003 => "AUDIT_ARCH_I386".to_owned(),
                    other => format!("{other:#x}"),
                };

                field(out, "si_arch", Kind::Constant, arch);
            }
            _ => {}
        }
    }

    out.push(Kind::Punctuation, "}");
    Some(())
}
