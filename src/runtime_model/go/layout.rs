//! The runtime contract: every name the Go model reads, bound against the
//! runtime's own debug information.
//!
//! Offsets, sizes, and statuses come from DWARF by name, never from a table
//! of versions. A name the binary lacks makes the features that read it
//! unavailable, with a reason that names it; nothing guesses an offset.

use std::sync::Arc;

use super::super::{Member, RuntimeImage};
use crate::{ImageAddress, IntegerValue, ThreadLocal};

/// A missing name, which makes a feature unavailable.
pub type Missing = Arc<str>;

pub(super) fn member(image: &dyn RuntimeImage, ty: &str, path: &[&str]) -> Result<Member, Missing> {
    image
        .member(ty, path)
        .ok_or_else(|| format!("the runtime has no member {ty}.{}", path.join(".")).into())
}

pub(super) fn offset(
    image: &dyn RuntimeImage,
    ty: &str,
    path: &[&str],
    size: u64,
) -> Result<u64, Missing> {
    let found = member(image, ty, path)?;
    if found.size != size {
        return Err(format!(
            "{ty}.{} is {} bytes, not {size}",
            path.join("."),
            found.size
        )
        .into());
    }

    Ok(found.offset)
}

pub(super) fn symbol(image: &dyn RuntimeImage, name: &str) -> Result<ImageAddress, Missing> {
    image
        .symbol(name)
        .map(|symbol| symbol.address)
        .ok_or_else(|| format!("the runtime has no symbol {name}").into())
}

pub(super) fn constant(image: &dyn RuntimeImage, name: &str) -> Result<u64, Missing> {
    match image.constant(name) {
        Some(IntegerValue::Signed(value)) => u64::try_from(value).ok(),
        Some(IntegerValue::Unsigned(value)) => u64::try_from(value).ok(),
        _ => None,
    }
    .ok_or_else(|| format!("the runtime has no constant {name}").into())
}

/// A goroutine's status words, as `runtime._G*` names them.
#[derive(Debug, Clone, Copy)]
pub struct Statuses {
    pub idle: u64,
    pub runnable: u64,
    pub running: u64,
    pub syscall: u64,
    pub waiting: u64,
    pub dead: u64,
    pub copystack: u64,
    pub preempted: u64,

    /// The bit a garbage collector sets while it scans a stack; the rest of
    /// the word is the status the goroutine returns to.
    pub scan: u64,

    /// Statuses newer than every release the contract was checked on, such
    /// as `_Gleaked`, when the runtime has them.
    pub leaked: Option<u64>,
    pub dead_extra: Option<u64>,
}

/// What a goroutine's status word says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GStatus {
    Dead,
    Idle,
    OnThread,
    Runnable,
    Waiting,
    Blocked,
    Unknown,
}

impl Statuses {
    /// Classifies a status word, ignoring the bit a scan sets.
    pub fn classify(&self, status: u64) -> GStatus {
        let base = status & !self.scan;
        let is = |known: Option<u64>| Some(base) == known;
        if base == self.dead || is(self.dead_extra) {
            GStatus::Dead
        } else if base == self.idle {
            GStatus::Idle
        } else if base == self.running || base == self.syscall {
            GStatus::OnThread
        } else if base == self.runnable || base == self.preempted {
            GStatus::Runnable
        } else if base == self.waiting {
            GStatus::Waiting
        } else if base == self.copystack || is(self.leaked) {
            GStatus::Blocked
        } else {
            GStatus::Unknown
        }
    }

    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let status = |name: &str| constant(image, &format!("runtime._G{name}"));
        Ok(Self {
            idle: status("idle")?,
            runnable: status("runnable")?,
            running: status("running")?,
            syscall: status("syscall")?,
            waiting: status("waiting")?,
            dead: status("dead")?,
            copystack: status("copystack")?,
            preempted: status("preempted")?,
            scan: status("scan")?,
            leaked: status("leaked").ok(),
            dead_extra: status("deadextra").ok(),
        })
    }
}

/// What reading the goroutine list needs.
#[derive(Debug, Clone)]
pub struct Goroutines {
    /// `runtime.allgs`, a slice of `*g`.
    pub allgs: ImageAddress,

    /// `runtime.allglen`, the published length of `allgs`.
    pub allglen: ImageAddress,
    pub status: u64,
    pub goid: u64,
    pub wait_reason: u64,
    pub m: u64,
    pub sched_sp: u64,
    pub sched_pc: u64,
    pub sched_bp: u64,
    pub gopc: u64,
    pub startpc: u64,
    pub parent_goid: Option<u64>,
    pub statuses: Statuses,

    /// The `procid` of an `m`: the thread it runs on.
    pub m_procid: u64,

    /// The goroutine an `m` runs.
    pub m_curg: u64,
}

impl Goroutines {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let g = |path: &[&str], size| offset(image, "runtime.g", path, size);
        Ok(Self {
            allgs: symbol(image, "runtime.allgs")?,
            allglen: symbol(image, "runtime.allglen")?,
            status: g(&["atomicstatus", "value"], 4)?,
            goid: g(&["goid"], 8)?,
            wait_reason: g(&["waitreason"], 1)?,
            m: g(&["m"], 8)?,
            sched_sp: g(&["sched", "sp"], 8)?,
            sched_pc: g(&["sched", "pc"], 8)?,
            sched_bp: g(&["sched", "bp"], 8)?,
            gopc: g(&["gopc"], 8)?,
            startpc: g(&["startpc"], 8)?,
            parent_goid: g(&["parentGoid"], 8).ok(),
            statuses: Statuses::bind(image)?,
            m_procid: offset(image, "runtime.m", &["procid"], 8)?,
            m_curg: offset(image, "runtime.m", &["curg"], 8)?,
        })
    }
}

/// What finding a thread's goroutine needs.
#[derive(Debug, Clone)]
pub struct Threads {
    /// Where a thread's current goroutine is stored relative to its
    /// thread pointer.
    pub tls_g: ThreadLocal,
    pub g_m: u64,

    /// The bounds of a g's stack, `[lo, hi)`.
    pub g_stack_lo: u64,
    pub g_stack_hi: u64,
    pub m_g0: u64,
    pub m_gsignal: u64,
    pub m_curg: u64,

    /// Where a vDSO call that has saved its stack pointer says its caller
    /// went on, and zero otherwise.
    pub m_vdso_sp: u64,
}

impl Threads {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        // An executable linked by Go's own linker keeps g in the last word
        // of its thread-local block, just below the thread pointer.
        // External linking places it at `runtime.tlsg` within the block,
        // among the C's thread-locals, or a library's loader decides.
        let tls_g = match image.thread_local("runtime.tlsg") {
            None => ThreadLocal::Offset(-8),
            Some(Ok(place)) => place,
            Some(Err(reason)) => {
                return Err(
                    format!("the goroutine's place, runtime.tlsg, is unknown: {reason}").into(),
                );
            }
        };

        let m = |path: &[&str]| offset(image, "runtime.m", path, 8);
        Ok(Self {
            tls_g,
            g_m: offset(image, "runtime.g", &["m"], 8)?,
            g_stack_lo: offset(image, "runtime.g", &["stack", "lo"], 8)?,
            g_stack_hi: offset(image, "runtime.g", &["stack", "hi"], 8)?,
            m_g0: m(&["g0"])?,
            m_gsignal: m(&["gsignal"])?,
            m_curg: m(&["curg"])?,
            m_vdso_sp: m(&["vdsoSP"])?,
        })
    }
}

/// What reading a goroutine's profiler labels needs.
#[derive(Debug, Clone)]
pub struct Labels {
    /// `g.labels`, a pointer to a `label.Set`.
    pub g_labels: u64,

    /// How a set is laid out. A program that never sets labels may not
    /// describe it, and then no goroutine has any.
    pub set: Result<LabelSet, Missing>,
}

/// How a `label.Set` holds its labels.
#[derive(Debug, Clone)]
pub struct LabelSet {
    /// `label.Set.List`, a slice of `label.Label`.
    pub list: u64,

    /// The size of a `label.Label`, and where its key and value strings
    /// are.
    pub stride: u64,
    pub key: u64,
    pub value: u64,
}

impl Labels {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        const SET: &str = "internal/runtime/pprof/label.Set";
        const LABEL: &str = "internal/runtime/pprof/label.Label";
        let set = || {
            Ok(LabelSet {
                list: offset(image, SET, &["List"], 24)?,
                stride: member(image, LABEL, &[])?.size,
                key: offset(image, LABEL, &["Key"], 16)?,
                value: offset(image, LABEL, &["Value"], 16)?,
            })
        };

        Ok(Self {
            g_labels: offset(image, "runtime.g", &["labels"], 8)?,
            set: set(),
        })
    }
}

/// Every part of the contract, each bound or missing on its own.
#[derive(Debug, Clone)]
pub struct Layout {
    pub goroutines: Result<Goroutines, Missing>,
    pub threads: Result<Threads, Missing>,
    pub labels: Result<Labels, Missing>,
}

impl Layout {
    pub fn bind(image: &dyn RuntimeImage) -> Self {
        Self {
            goroutines: Goroutines::bind(image),
            threads: Threads::bind(image),
            labels: Labels::bind(image),
        }
    }
}
