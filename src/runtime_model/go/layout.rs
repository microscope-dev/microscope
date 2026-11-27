//! DWARF-bound field offsets, symbols, and status constants for Go's runtime.
//!
//! Sub-layouts (`goroutines`, `threads`, `labels`) are bound independently so a
//! missing optional feature (such as `pprof` labels) does not disable core
//! goroutine inspection.

use std::sync::Arc;

use super::super::{Member, RuntimeImage};
use crate::{ImageAddress, IntegerValue, ThreadLocal};

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

    pub scan: u64,

    pub leaked: Option<u64>,
    pub dead_extra: Option<u64>,
}

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
    /// Classifies a raw `g.atomicstatus` word after masking off the `_Gscan`
    /// GC stack-scanning bit.
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

#[derive(Debug, Clone)]
pub struct Goroutines {
    pub allgs: ImageAddress,

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

    pub m_procid: u64,

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

#[derive(Debug, Clone)]
pub struct Threads {
    pub tls_g: ThreadLocal,
    pub g_m: u64,

    pub g_stack_lo: u64,
    pub g_stack_hi: u64,
    pub m_g0: u64,
    pub m_gsignal: u64,
    pub m_curg: u64,

    pub m_vdso_sp: u64,
}

impl Threads {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let tls_g = match image.thread_local("runtime.tlsg") {
            // Static Linux `x86_64` Go binaries without a `runtime.tlsg` symbol
            // store the current `g` pointer at `fs:-8`.
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

#[derive(Debug, Clone)]
pub struct Labels {
    pub g_labels: u64,

    pub set: Result<LabelSet, Missing>,
}

#[derive(Debug, Clone)]
pub struct LabelSet {
    pub list: u64,

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
