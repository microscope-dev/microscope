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
//! drained, while the program most likely still runs: the record waits
//! for the watermark to pass it before it is processed, by when the
//! program may have exited. One freed since, or whose program exited
//! before its record was drained, is unreadable.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::tasks::LiveStop;
use crate::backend::bpf::sampler::{self, Gil, NO_OFFSET, PyStack, StackEnd};
use crate::capture::CaptureBuilder;
use crate::capture::records::{NONE, Zone};
use crate::debug_info::DebugInfo;
use crate::image::python::PythonLayout;
use crate::model::{Pid, Tid};
use crate::runtime_model::thread_ids::ThreadIds;
use crate::runtime_model::{self, InterpretedFrame, RuntimeImage, RuntimeModel, SampledFrame};
use crate::{LoadBias, LoadedModule, VirtualAddress};

/// How long past a record of frames the drain may still bring its sample:
/// a record older than the watermark by more found its sample dropped.
pub(super) const PAIRING_SLACK: u64 = 10_000_000;

/// The label a sample of a thread that holds or waits for its
/// interpreter's GIL carries, as task snapshots label threads.
const GIL: &str = "gil";

/// A process's interpreter that the engine reads, its layout, and where
/// the process keeps its runtime.
struct Runtime {
    model: Arc<dyn RuntimeModel>,
    bias: LoadBias,
    layout: Result<PythonLayout, Arc<str>>,
    state: Option<VirtualAddress>,
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

/// A record of frames, by its thread, CPU, and time.
pub(super) type RecordKey = (Tid, u32, u64);

/// What a sample's interpreted frames are.
#[derive(Debug, PartialEq, Eq)]
pub enum Sampled {
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

    /// The frames of each record drained but not yet processed, read as
    /// it was drained.
    drained: BTreeMap<RecordKey, Sampled>,
}

impl Interpreted {
    pub(super) const fn new(outputs: Vec<u64>) -> Self {
        Self {
            outputs,
            found: BTreeMap::new(),
            spans: BTreeMap::new(),
            drained: BTreeMap::new(),
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
                state: loaded.load_bias.load(python.runtime.start),
            })
        });

        self.found.insert(pid, Found { mapped, runtime });
    }

    /// Whether a native frame of process `pid` that runs the function or
    /// symbol `name`, and called what runs `inner`, is an activation of
    /// its interpreter's loop.
    pub(super) fn activation(&self, pid: Pid, name: &str, inner: Option<&str>) -> bool {
        self.found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
            .is_some_and(|runtime| runtime.model.activation(name, inner))
    }

    /// Forgets process `pid`, which exited.
    pub(super) fn forget(&mut self, pid: Pid) {
        self.found.remove(&pid);
    }

    /// Reads the frames of `record`, of process `pid`, as it is drained.
    /// A record held back to a later drain was read in the first.
    pub(super) fn drained(&mut self, pid: Pid, key: RecordKey, record: &[u8]) {
        if self.drained.contains_key(&key) {
            return;
        }

        // A record lies four bytes into the sample that carries it, short
        // of its fields' alignment; a copy aligns them.
        let aligned = record.to_vec();
        match self.sampled(pid, Some(&aligned)) {
            Sampled::None => {}
            sampled => {
                self.drained.insert(key, sampled);
            }
        }
    }

    /// Forgets the frames of a record that goes with no sample.
    pub(super) fn dropped(&mut self, key: RecordKey) {
        self.drained.remove(&key);
    }

    /// The interpreted frames of a sample of process `pid`, from the
    /// record of frames paired with it, if any, as they were read when
    /// it was drained.
    pub(super) fn paired(&mut self, pid: Pid, record: Option<(RecordKey, &[u8])>) -> Sampled {
        let drained = record.and_then(|(key, _)| self.drained.remove(&key));
        drained.unwrap_or_else(|| self.sampled(pid, record.map(|(_, bytes)| bytes)))
    }

    /// The interpreted frames of a sample of process `pid`, from the
    /// record of frames paired with it, if any, read now.
    fn sampled(&self, pid: Pid, record: Option<&[u8]>) -> Sampled {
        let Some(runtime) = self
            .found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
        else {
            return Sampled::None;
        };

        let stop = LiveStop::memory(pid.get(), runtime.bias);
        read_record(runtime.model.as_ref(), &stop, record)
    }

    /// What process `pid`'s interpreter tells the program, read now, or
    /// `None` for a process that runs no interpreter the engine reads.
    pub(super) fn walked(&self, pid: Pid) -> Option<Walked> {
        let runtime = self.found.get(&pid)?.runtime.as_ref()?;
        let layout = sampler::process_layout(
            runtime.layout.as_ref().map_err(AsRef::as_ref),
            runtime.state,
            pid.get(),
        );
        let stop = LiveStop::for_threads(pid.get(), runtime.bias);
        let hosts = match ThreadIds::new(&stop) {
            Ok(hosts) => hosts,
            Err(problem) => {
                return Some(Walked {
                    pid: pid.get(),
                    layout,
                    threads: Vec::new(),
                    gaps: vec![problem],
                });
            }
        };
        let mut walked = runtime.model.walked_threads(&stop)?;
        let threads = mapped_threads(&mut walked, &hosts);
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

/// The interpreted frames a record of the walker's holds, if any, named
/// and proved by `model` through `stop`, the process's memory read now.
pub fn read_record(
    model: &dyn RuntimeModel,
    stop: &LiveStop<'_>,
    record: Option<&[u8]>,
) -> Sampled {
    let Some(stack) = record.and_then(|bytes| sampler::decode(bytes).ok()) else {
        return Sampled::Unknown;
    };

    let ended = match stack.end {
        StackEnd::NoLayout | StackEnd::NoThread | StackEnd::Stale => return Sampled::Unknown,
        StackEnd::Complete => false,
        StackEnd::Bounded | StackEnd::Unreadable | StackEnd::BeforeBase => true,
    };

    let (frames, end) = model.sampled_frames(stop, &sampled_frames(&stack));
    Sampled::Frames {
        frames,
        ended: ended || end.is_some(),
    }
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

/// Keep examples and a total count without allocating one string for
/// each unsupported state in a large interpreter catalog.
struct MappingGaps<'a> {
    gaps: &'a mut Vec<Arc<str>>,
    failed: usize,
}

impl MappingGaps<'_> {
    const EXAMPLES: usize = 8;

    fn push(&mut self, why: impl FnOnce() -> Arc<str>) {
        self.failed += 1;
        if self.failed <= Self::EXAMPLES {
            self.gaps.push(why());
        }
    }

    fn finish(self) {
        if self.failed > Self::EXAMPLES {
            self.gaps.push(
                format!(
                    "{} Python thread mapping problems; {} additional details omitted",
                    self.failed,
                    self.failed - Self::EXAMPLES
                )
                .into(),
            );
        }
    }
}

/// Maps each state to one proven Linux thread. The kernel map holds one
/// state per host thread, so multiple interpreter states are ambiguous.
fn mapped_threads(
    walked: &mut runtime_model::Partial<Vec<runtime_model::WalkedThread>>,
    hosts: &ThreadIds,
) -> Vec<(u32, u64, u64)> {
    let mut gaps = MappingGaps {
        gaps: &mut walked.gaps,
        failed: 0,
    };
    let mut mapped = BTreeMap::<u32, Option<(u64, u64)>>::new();
    for thread in &walked.value {
        let host = match hosts.lookup(thread.native) {
            Ok(host) => host,
            Err(problem) => {
                gaps.push(|| problem.to_string().into());
                continue;
            }
        };
        let Some(kernel_id) = i32::try_from(host.get()).ok().filter(|host| *host > 0) else {
            gaps.push(|| format!("debugger thread {host} is outside Linux's TID range").into());
            continue;
        };
        let host = kernel_id.cast_unsigned();
        match mapped.entry(host) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(Some((thread.state.get(), thread.native)));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if slot.get().is_some() {
                    gaps.push(|| format!("debugger thread {host} has multiple Python thread states; the BPF engine cannot select one").into());
                }
                slot.insert(None);
            }
        }
    }
    gaps.finish();
    mapped
        .into_iter()
        .filter_map(|(host, state)| state.map(|(state, native)| (host, state, native)))
        .collect()
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

/// Whether interpreted frames that cannot be known leave a stack of
/// `native` frames short: only where an activation of the interpreter's
/// loop could run them. A thread that is in no activation, as a library's
/// worker thread is, runs no interpreted frames.
pub(super) fn unknown_cuts(native: &[(u32, bool)]) -> bool {
    native.iter().any(|&(_, activation)| activation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stack_in_the_loop_can_lack_interpreted_frames() {
        assert!(unknown_cuts(&[(1, false), (2, true), (3, false)]));
        assert!(!unknown_cuts(&[(1, false), (3, false)]));
        assert!(!unknown_cuts(&[]));
    }

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
    fn missing_host_identity_never_assigns_python_state_by_numeric_equality() {
        let mut walked = runtime_model::Partial {
            value: vec![runtime_model::WalkedThread {
                native: 7,
                state: VirtualAddress::new(0x1234),
            }],
            gaps: Vec::new(),
        };
        let hosts = ThreadIds::read(Vec::new(), |_| Vec::new()).unwrap();
        let mapped = mapped_threads(&mut walked, &hosts);
        assert!(
            mapped.is_empty(),
            "unproven state was assigned to host thread 7: {mapped:?}"
        );
        assert!(
            !walked.gaps.is_empty(),
            "missing identity evidence is visible"
        );
    }

    #[test]
    fn proven_namespace_ids_preserve_good_states_and_report_missing_ones() {
        let host = crate::ThreadId::new(5000);
        let collision = crate::ThreadId::new(7);
        let hosts = ThreadIds::read(vec![host, collision], |_| {
            vec![
                (host, runtime_model::TargetThreadId::new(7).unwrap()),
                (collision, runtime_model::TargetThreadId::new(8).unwrap()),
            ]
        })
        .unwrap();
        let mut walked = runtime_model::Partial {
            value: [(7, 0x1000), (8, 0x2000), (9, 0x3000)]
                .map(|(native, state)| runtime_model::WalkedThread {
                    native,
                    state: VirtualAddress::new(state),
                })
                .to_vec(),
            gaps: vec!["existing runtime gap".into()],
        };
        assert_eq!(
            mapped_threads(&mut walked, &hosts),
            [(7, 0x2000, 8), (5000, 0x1000, 7)]
        );
        assert_eq!(walked.gaps.len(), 2);
        assert_eq!(&*walked.gaps[0], "existing runtime gap");
        assert!(walked.gaps[1].contains('9') && walked.gaps[1].contains("no proven"));
    }

    #[test]
    fn ambiguous_namespace_ids_and_host_ids_outside_the_kernel_range_are_visible() {
        for ambiguous in [false, true] {
            let first = crate::ThreadId::new(if ambiguous {
                5000
            } else {
                u64::from(u32::MAX) + 1
            });
            let second = crate::ThreadId::new(5001);
            let hosts = ThreadIds::read(vec![first, second], |_| {
                let mut pairs = vec![(first, runtime_model::TargetThreadId::new(7).unwrap())];
                if ambiguous {
                    pairs.push((second, runtime_model::TargetThreadId::new(7).unwrap()));
                }
                pairs
            })
            .unwrap();
            let mut walked = runtime_model::Partial {
                value: vec![runtime_model::WalkedThread {
                    native: 7,
                    state: VirtualAddress::new(0x1000),
                }],
                gaps: Vec::new(),
            };
            assert!(mapped_threads(&mut walked, &hosts).is_empty());
            assert_eq!(walked.gaps.len(), 1);
            assert!(walked.gaps[0].contains(if ambiguous { "ambiguous" } else { "TID range" }));
        }
    }

    #[test]
    fn mapping_diagnostics_are_bounded_without_losing_later_proven_states() {
        let host = crate::ThreadId::new(5000);
        let hosts = ThreadIds::read(vec![host], |_| {
            vec![(host, runtime_model::TargetThreadId::new(7).unwrap())]
        })
        .unwrap();
        let mut walked = runtime_model::Partial {
            value: (10..10010)
                .map(|native| runtime_model::WalkedThread {
                    native,
                    state: VirtualAddress::new(native * 8),
                })
                .collect::<Vec<_>>(),
            gaps: vec!["existing runtime gap".into()],
        };
        walked.value.push(runtime_model::WalkedThread {
            native: 7,
            state: VirtualAddress::new(0x7777),
        });
        assert_eq!(mapped_threads(&mut walked, &hosts), [(5000, 0x7777, 7)]);
        assert!(
            walked.gaps.len() <= 10,
            "{} mapping diagnostics",
            walked.gaps.len()
        );
        assert!(walked.gaps.last().unwrap().contains("10000"));
        assert_eq!(&*walked.gaps[0], "existing runtime gap");
    }

    #[test]
    fn multiple_python_states_on_one_host_thread_are_not_chosen_arbitrarily() {
        let mut walked = runtime_model::Partial {
            value: vec![
                runtime_model::WalkedThread {
                    native: 7,
                    state: VirtualAddress::new(0x1000),
                },
                runtime_model::WalkedThread {
                    native: 7,
                    state: VirtualAddress::new(0x2000),
                },
            ],
            gaps: Vec::new(),
        };
        let hosts = ThreadIds::read(vec![crate::ThreadId::new(5000)], |_| {
            vec![(
                crate::ThreadId::new(5000),
                runtime_model::TargetThreadId::new(7).unwrap(),
            )]
        })
        .unwrap();
        let mapped = mapped_threads(&mut walked, &hosts);
        assert!(
            mapped.is_empty(),
            "one kernel key cannot select between interpreter states: {mapped:?}"
        );
        assert!(!walked.gaps.is_empty());
    }
}
