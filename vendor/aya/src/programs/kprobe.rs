//! Kernel space probes.
use std::{
    ffi::OsStr,
    fmt::{self, Write},
    io,
    path::{Path, PathBuf},
};

use std::{
    ffi::{CStr, c_char},
    os::fd::AsFd as _,
};

use aya_obj::generated::{
    BPF_F_KPROBE_MULTI_RETURN, bpf_attach_type::BPF_TRACE_KPROBE_MULTI, bpf_link_type,
    bpf_prog_type::BPF_PROG_TYPE_KPROBE,
};
use thiserror::Error;

use crate::{
    VerifierLogLevel,
    sys::{BpfLinkCreateArgs, LinkTarget, SyscallError, bpf_link_create},
    programs::{
        ProgramData, ProgramError, ProgramType, define_link_wrapper, impl_try_from_fdlink,
        impl_try_into_fdlink, links::FdLink, load_program_with_attach_type,
        load_program_without_attach_type,
        perf_attach::{PerfLinkIdInner, PerfLinkInner},
        probe::{Probe, ProbeKind, attach},
    },
};

/// A kernel probe.
///
/// Kernel probes are eBPF programs that can be attached to almost any function inside
/// the kernel. They can be of two kinds:
///
/// - `kprobe`: get attached to the *start* of the target functions
/// - `kretprobe`: get attached to the *return address* of the target functions
///
/// # Minimum kernel version
///
/// The minimum kernel version required to use this feature is 4.1.
///
/// # Examples
///
/// ```no_run
/// # let mut bpf = Ebpf::load_file("ebpf_programs.o")?;
/// use aya::{Ebpf, programs::KProbe};
///
/// let program: &mut KProbe = bpf.program_mut("intercept_wakeups").unwrap().try_into()?;
/// program.load()?;
/// program.attach("try_to_wake_up", 0)?;
/// # Ok::<(), aya::EbpfError>(())
/// ```
#[derive(Debug)]
#[doc(alias = "BPF_PROG_TYPE_KPROBE")]
pub struct KProbe {
    pub(crate) data: ProgramData<KProbeLink>,
    pub(crate) kind: ProbeKind,
}

impl KProbe {
    /// The type of the program according to the kernel.
    pub const PROGRAM_TYPE: ProgramType = ProgramType::KProbe;

    /// Loads the program inside the kernel.
    pub fn load(&mut self) -> Result<(), ProgramError> {
        let Self { data, kind: _ } = self;
        load_program_without_attach_type(BPF_PROG_TYPE_KPROBE, data)
    }

    /// Returns [`ProbeKind::Entry`] if the program is a `kprobe`, or
    /// [`ProbeKind::Return`] if the program is a `kretprobe`.
    pub const fn kind(&self) -> ProbeKind {
        self.kind
    }

    /// Attaches the program.
    ///
    /// Attaches the probe to the given function name inside the kernel. If
    /// `offset` is non-zero, it is added to the address of the target
    /// function.
    ///
    /// If the program is a `kprobe`, it is attached to the *start* address of the target function.
    /// Conversely if the program is a `kretprobe`, it is attached to the return address of the
    /// target function.
    ///
    /// The returned value can be used to detach from the given function, see [`KProbe::detach`].
    pub fn attach<T: AsRef<OsStr>>(
        &mut self,
        fn_name: T,
        offset: u64,
    ) -> Result<KProbeLinkId, ProgramError> {
        let Self { data, kind } = self;
        attach::<Self, _>(
            data,
            *kind,
            fn_name.as_ref(),
            offset,
            None, // pid
            None, // cookie
        )
    }

    /// Loads the program for [`KProbe::attach_multi`], as a `kprobe.multi`
    /// program, which the kernel runs with preemption disabled.
    ///
    /// # Minimum kernel version
    ///
    /// The minimum kernel version required to use this feature is 5.18.
    pub fn load_multi(&mut self) -> Result<(), ProgramError> {
        let Self { data, kind: _ } = self;
        load_program_with_attach_type(BPF_PROG_TYPE_KPROBE, BPF_TRACE_KPROBE_MULTI, data)
    }

    /// Attaches a program loaded with [`KProbe::load_multi`] to every function
    /// in `functions` with one link, at their starts for a `kprobe` and at their
    /// returns for a `kretprobe`. `cookies` is empty or holds each function's
    /// cookie, which the program reads with `bpf_get_attach_cookie`.
    ///
    /// The kernel refuses the whole link if it can't probe one of the functions.
    pub fn attach_multi<T: AsRef<CStr>>(
        &mut self,
        functions: &[T],
        cookies: &[u64],
    ) -> Result<FdLink, ProgramError> {
        if !cookies.is_empty() && cookies.len() != functions.len() {
            return Err(ProgramError::IOError(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a kprobe.multi link takes no cookies or one for each function",
            )));
        }
        let prog_fd = self.fd()?;
        let symbols: Vec<*const c_char> = functions.iter().map(|name| name.as_ref().as_ptr()).collect();
        let flags = match self.kind {
            ProbeKind::Entry => 0,
            ProbeKind::Return => BPF_F_KPROBE_MULTI_RETURN,
        };
        let fd = bpf_link_create(
            prog_fd.as_fd(),
            LinkTarget::None,
            BPF_TRACE_KPROBE_MULTI,
            0,
            Some(BpfLinkCreateArgs::KprobeMulti {
                symbols: &symbols,
                cookies,
                flags,
            }),
        )
        .map_err(|io_error| SyscallError {
            call: "bpf_link_create",
            io_error,
        })?;
        Ok(FdLink::new(fd))
    }

    /// Creates a program from a pinned entry on a bpffs.
    ///
    /// Existing links will not be populated. To work with existing links you should use [`crate::programs::links::PinnedLink`].
    ///
    /// On drop, any managed links are detached and the program is unloaded. This will not result in
    /// the program being unloaded from the kernel if it is still pinned.
    pub fn from_pin<P: AsRef<Path>>(path: P, kind: ProbeKind) -> Result<Self, ProgramError> {
        let data = ProgramData::from_pinned_path(path, VerifierLogLevel::default())?;
        Ok(Self { data, kind })
    }
}

impl Probe for KProbe {
    const PMU: &'static str = "kprobe";

    type Error = KProbeError;

    fn file_error(filename: PathBuf, io_error: io::Error) -> Self::Error {
        KProbeError::FileError { filename, io_error }
    }

    fn write_offset<W: Write>(w: &mut W, kind: ProbeKind, offset: u64) -> fmt::Result {
        match kind {
            ProbeKind::Entry => write!(w, "+{offset}"),
            ProbeKind::Return => Ok(()),
        }
    }
}

define_link_wrapper!(
    KProbeLink,
    KProbeLinkId,
    PerfLinkInner,
    PerfLinkIdInner,
    KProbe,
);

/// The type returned when attaching a [`KProbe`] fails.
#[derive(Debug, Error)]
pub enum KProbeError {
    /// Error detaching from debugfs
    #[error("`{filename}`")]
    FileError {
        /// The file name
        filename: PathBuf,
        /// The [`io::Error`] returned from the file operation
        #[source]
        io_error: io::Error,
    },
}

impl_try_into_fdlink!(KProbeLink, PerfLinkInner);
impl_try_from_fdlink!(
    KProbeLink,
    PerfLinkInner,
    bpf_link_type::BPF_LINK_TYPE_PERF_EVENT
);
