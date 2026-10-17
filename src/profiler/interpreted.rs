//! The Python frames the BPF engine reads at each sample
//! (`backend::bpf::sampler`), named and spliced into the samples' native
//! stacks.
//!
//! The program writes a thread's frames to the CPU's ring as the kernel
//! takes its sample, and the sample after them, so in each ring a record
//! of frames comes just before the sample of the same thread and CPU it
//! was read for. Their times may be in either order, as kernels prepare a
//! sample before or after they run its program, so the recorder pairs them
//! by ring order, not by time; a record whose sample never comes was the
//! sample's that the ring dropped, and goes with it.
//!
//! Each activation of the interpreter's loop runs a segment of the
//! thread's frames, the innermost activation the innermost segment, and
//! each segment goes just before its activation, as the debugger places
//! them (`backend::linux::interpreted`): where the activations and segments
//! differ in number, the frames follow the native ones, and the stack says
//! so. A frame's code object is read and proved as the record is
//! processed, while the program runs on, so one freed since is unreadable.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::tasks::LiveStop;
use crate::backend::bpf::sampler::{self, Gil, NO_OFFSET, PyStack, StackEnd};
use crate::capture::CaptureBuilder;
use crate::capture::records::{NONE, Zone};
use crate::debug_info::DebugInfo;
use crate::image::python::PythonLayout;
use crate::model::Pid;
use crate::runtime_model::{self, InterpretedFrame, RuntimeImage, RuntimeModel, SampledFrame};
use crate::{LoadBias, LoadedModule, VirtualAddress};

/// How long past a record of frames the drain may still bring its sample:
/// a record older than the watermark by more found its sample dropped.
pub(super) const PAIRING_SLACK: u64 = 10_000_000;

/// The label a sample of a thread that holds or waits for its
/// interpreter's GIL carries, as task snapshots label threads.
const GIL: &str = "gil";

/// A process's interpreter that the engine reads, and its layout.
struct Runtime {
    model: Arc<dyn RuntimeModel>,
    bias: LoadBias,
    layout: Result<PythonLayout, Arc<str>>,
}

/// What a process's modules said of its interpreter, when it mapped
/// `mapped` of them.
struct Found {
    mapped: usize,
    runtime: Option<Runtime>,
}

/// One thread's latest span of one label value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    value: &'static str,
    start: i64,
    end: i64,
}

/// What a sample's interpreted frames are.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Sampled {
    /// Its process runs no interpreter the engine reads.
    None,

    /// Its process does, and the frames are unknown: their record was
    /// lost, or came before the process's layout or the thread's state was
    /// known.
    Unknown,

    /// Innermost first, each activation's outermost marked, and whether
    /// they end before the thread's outermost.
    Frames {
        frames: Vec<InterpretedFrame>,
        ended: bool,
    },
}

/// What a process's interpreter tells the program: the layout it reads
/// the process by, and where it begins each thread, each by the thread's
/// id as microscope sees it, its state, and the id its state names it by.
#[derive(Debug)]
pub struct Walked {
    pub pid: u32,
    pub layout: Result<sampler::RawPyLayout, String>,
    pub threads: Vec<(u32, u64, u64)>,
    pub gaps: Vec<Arc<str>>,
}

/// The engine's part of a recording.
pub(super) struct Interpreted {
    /// The identifiers of the output events, which every record of frames
    /// begins with.
    pub(super) outputs: Vec<u64>,
    found: BTreeMap<Pid, Found>,
    spans: BTreeMap<u32, Span>,
}

impl Interpreted {
    pub(super) const fn new(outputs: Vec<u64>) -> Self {
        Self {
            outputs,
            found: BTreeMap::new(),
            spans: BTreeMap::new(),
        }
    }

    /// Whether what process `pid`'s modules say of its interpreter must
    /// be read again, now that it maps `mapped`.
    pub(super) fn stale(&self, pid: Pid, mapped: usize) -> bool {
        self.found
            .get(&pid)
            .is_none_or(|found| found.mapped != mapped)
    }

    /// Reads what process `pid`'s `modules`, `mapped` in all, say of its
    /// interpreter.
    pub(super) fn find(&mut self, pid: Pid, mapped: usize, modules: &[(LoadedModule, DebugInfo)]) {
        let runtime = modules.iter().find_map(|(loaded, info)| {
            let image: Arc<dyn RuntimeImage + Send + Sync> = Arc::clone(&info.image) as _;
            let python = image.python()?;
            let stop = LiveStop::memory(pid.get(), loaded.load_bias);
            let model = runtime_model::detect(&image)
                .into_iter()
                .flatten()
                .find(|model| model.walked_threads(&stop).is_some())?;
            Some(Runtime {
                model,
                bias: loaded.load_bias,
                layout: python.layout,
            })
        });

        self.found.insert(pid, Found { mapped, runtime });
    }

    /// Forgets process `pid`, which exited.
    pub(super) fn forget(&mut self, pid: Pid) {
        self.found.remove(&pid);
    }

    /// The interpreted frames of a sample of process `pid`, from the
    /// record of frames paired with it, if any.
    pub(super) fn sampled(&self, pid: Pid, record: Option<&[u8]>) -> Sampled {
        let Some(runtime) = self
            .found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
        else {
            return Sampled::None;
        };

        let Some(stack) = record.and_then(|bytes| sampler::decode(bytes).ok()) else {
            return Sampled::Unknown;
        };

        let ended = match stack.end {
            StackEnd::NoLayout | StackEnd::NoThread | StackEnd::Stale => return Sampled::Unknown,
            StackEnd::Complete => false,
            StackEnd::Bounded | StackEnd::Unreadable | StackEnd::BeforeBase => true,
        };

        let stop = LiveStop::memory(pid.get(), runtime.bias);
        let (frames, end) = runtime.model.sampled_frames(&stop, &sampled_frames(&stack));
        Sampled::Frames {
            frames,
            ended: ended || end.is_some(),
        }
    }

    /// What process `pid`'s interpreter tells the program, read now, or
    /// `None` for a process that runs no interpreter the engine reads.
    pub(super) fn walked(&self, pid: Pid) -> Option<Walked> {
        let runtime = self.found.get(&pid)?.runtime.as_ref()?;
        let layout = runtime
            .layout
            .as_ref()
            .map_err(ToString::to_string)
            .and_then(|layout| {
                sampler::layout(layout).map_err(|field| format!("CPython's layout has no {field}"))
            });
        let stop = LiveStop::memory(pid.get(), runtime.bias);
        let walked = runtime.model.walked_threads(&stop)?;
        let hosts = host_threads(pid.get());
        let threads = walked
            .value
            .iter()
            .filter_map(|thread| {
                let host = match &hosts {
                    Some(hosts) => *hosts.get(&thread.native)?,
                    None => u32::try_from(thread.native).ok()?,
                };

                Some((host, thread.state.get(), thread.native))
            })
            .collect();
        Some(Walked {
            pid: pid.get(),
            layout,
            threads,
            gaps: walked.gaps,
        })
    }

    /// Labels the thread whose row is `thread` with what it did with its
    /// GIL at a sample at `time` of `period`: each run of samples with one
    /// label is one span.
    pub(super) fn label(
        &mut self,
        builder: &mut CaptureBuilder,
        thread: u32,
        (time, period): (i64, u64),
        gil: Gil,
    ) {
        let value = match gil {
            Gil::Held => Some("held"),
            Gil::Waiting => Some("waiting"),
            Gil::Unknown | Gil::Released | Gil::Off => None,
        };

        let start = time.saturating_sub(period.cast_signed());
        let current = self.spans.get(&thread).copied();
        match (current, value) {
            // A sample takes up where the last of its value left off.
            (Some(span), Some(value)) if span.value == value && span.end >= start => {
                self.spans.insert(thread, Span { end: time, ..span });
            }
            (current, value) => {
                if let Some(span) = current {
                    record_span(builder, thread, span);
                    self.spans.remove(&thread);
                }

                if let Some(value) = value {
                    self.spans.insert(
                        thread,
                        Span {
                            value,
                            start,
                            end: time,
                        },
                    );
                }
            }
        }
    }

    /// Records each thread's open span in `builder`, as of where it last
    /// reached.
    pub(super) fn close(&self, builder: &mut CaptureBuilder) {
        for (&thread, &span) in &self.spans {
            record_span(builder, thread, span);
        }
    }
}

/// Records a span of a GIL label.
fn record_span(builder: &mut CaptureBuilder, thread: u32, span: Span) {
    let site = builder.site(GIL, "", 0, NONE, microscope_probes::abi::SITE_LABEL);
    let value = builder.string(span.value);
    builder.zone(
        thread,
        Zone {
            start: span.start.into(),
            end: span.end.into(),
            site: site.into(),
            flags: 0.into(),
            value: value.into(),
            reserved: 0.into(),
        },
    );
}

/// The frames of a record, as the runtime model reads them.
fn sampled_frames(stack: &PyStack<'_>) -> Vec<SampledFrame> {
    stack
        .frames
        .iter()
        .map(|frame| SampledFrame {
            code: VirtualAddress::new(frame.code),
            proof: (frame.qualname, frame.first_line),
            offset: (frame.offset != NO_OFFSET).then_some(i64::from(frame.offset)),
            owner: frame.owner,
        })
        .collect()
}

/// Each thread of process `pid` by its id in the process's own pid
/// namespace, to its id as microscope sees it; `None` where the two
/// namespaces are one, so that the ids agree.
fn host_threads(pid: u32) -> Option<BTreeMap<u64, u32>> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let depth = nspid(&status)?.len();
    if depth <= 1 {
        return None;
    }

    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    Some(
        tasks
            .flatten()
            .filter_map(|task| {
                let host: u32 = task.file_name().to_str()?.parse().ok()?;
                let status = std::fs::read_to_string(task.path().join("status")).ok()?;
                let own = *nspid(&status)?.last()?;
                Some((own, host))
            })
            .collect(),
    )
}

/// A `/proc/PID/status`'s `NSpid` ids, from microscope's namespace inward.
fn nspid(status: &str) -> Option<Vec<u64>> {
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))?;
    ids.split_whitespace().map(|id| id.parse().ok()).collect()
}

/// A stack's frames with the interpreted ones spliced in, innermost first:
/// each segment of `segments` just before the activation of the
/// interpreter's loop that runs it, the innermost segment before the
/// innermost activation, where `native` marks each frame that is one.
/// `None` where activations and segments differ in number, as no segment's
/// place is then known.
pub(super) fn splice(native: &[(u32, bool)], segments: &[Vec<u32>]) -> Option<Vec<u32>> {
    let activations = native.iter().filter(|(_, activation)| *activation).count();
    if activations != segments.len() {
        return None;
    }

    let mut spliced =
        Vec::with_capacity(native.len() + segments.iter().map(Vec::len).sum::<usize>());
    let mut segments = segments.iter();
    for &(frame, activation) in native {
        if activation {
            spliced.extend(segments.next().into_iter().flatten());
        }

        spliced.push(frame);
    }

    Some(spliced)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each segment goes before its activation, innermost with innermost;
    /// where the two differ in number, nothing is placed.
    #[test]
    fn segments_go_before_their_activations_in_order() {
        let native = [(1, false), (2, true), (3, false), (4, true), (5, false)];
        let segments = [vec![10, 11], vec![20]];
        assert_eq!(
            splice(&native, &segments),
            Some(vec![1, 10, 11, 2, 3, 20, 4, 5])
        );
        assert_eq!(splice(&native, &segments[..1]), None);
        assert_eq!(splice(&[(1, false)], &[]), Some(vec![1]));
        assert_eq!(splice(&[(1, true)], &[vec![]]), Some(vec![1]));
    }

    /// A run of samples with one label is one span, from the first's
    /// period to the last; a change of value, or a sample with none, or a
    /// run that lapses, begins another.
    #[test]
    fn each_run_of_one_gil_label_is_one_span() {
        let mut interpreted = Interpreted::new(Vec::new());
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(Pid::new(1), crate::model::Tid::new(1), 0);
        for (time, gil) in [
            (10, Gil::Held),
            (20, Gil::Held),
            (30, Gil::Waiting),
            (40, Gil::Released),
            (50, Gil::Held),
            (90, Gil::Held),
        ] {
            interpreted.label(&mut builder, thread, (time, 10), gil);
        }

        interpreted.close(&mut builder);
        let capture = builder.finish();
        let mut spans: Vec<(i64, i64, String)> = capture
            .zones
            .iter()
            .map(|zone| {
                (
                    zone.start.get(),
                    zone.end.get(),
                    capture.string(zone.value.get()).to_owned(),
                )
            })
            .collect();
        spans.sort();
        assert_eq!(
            spans,
            [
                (0, 20, "held".to_owned()),
                (20, 30, "waiting".to_owned()),
                (40, 50, "held".to_owned()),
                (80, 90, "held".to_owned()),
            ]
        );
    }

    #[test]
    fn nspid_lists_a_threads_ids_from_the_outside_in() {
        assert_eq!(nspid("Name:\tx\nNSpid:\t812\t7\n"), Some(vec![812, 7]));
        assert_eq!(nspid("NSpid:\t812\n"), Some(vec![812]));
        assert_eq!(nspid("Name:\tx\n"), None);
    }
}
