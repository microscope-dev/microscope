//! The profiler's samples, taken of running threads where a clock event's
//! tick would find them, unwound as the recorder unwinds them, and judged
//! by the threads' shadow call stacks.
//!
//! A sample is the record the simulated kernel writes (K-PERF-1 to
//! K-PERF-3), read by the recorder's own decoder and unwinder, so what the
//! oracle judges is what a profile would hold. Whether and how a run
//! samples is drawn from [`Stream::Sample`] alone, so a seed that never
//! samples chooses as it did before sampling existed.
//!
//! The BPF engine's records of Python frames, which a program on the
//! sampler's events writes beside each sample, are not modeled: no
//! simulated program runs CPython, whose frames the program walks. Their
//! decoding and their pairing with samples are tested over synthesized
//! records instead (`backend::bpf::sampler`, `profiler::record`).

use super::choices::{Choices, Stream};
use super::corpus::Variant;
use super::kernel::shadow::Shadow;
use super::kernel::{Kernel, Tid};
use super::marks::{Mark, Marks};
use super::report::Failure;
use crate::backend::perf::decode;
use crate::unwind::UnwindModule;
use crate::unwind::sample::{FrameRules, SampledFrame, SampledState, StackEnd, unwind_sample};
use crate::{LoadedModule, ModuleId, ModuleImageId};

/// How often a run samples, as a chance in a thousand that a burst ends
/// in one: never, now and then, often, or after every burst.
const RATES: [u64; 4] = [0, 50, 250, 1000];

/// The bytes of stack a sample may copy: too few for one frame, a few
/// frames, the recorder's default, and the most an event takes.
const STACK_BYTES: [u32; 4] = [64, 512, 8192, 65528];

/// The deepest stack the recorder unwinds.
const DEPTH_LIMIT: usize = 128;

/// What a run samples, and what the unwinder remembers between samples.
pub struct Sampler {
    rate: u64,
    rules: FrameRules,
}

/// What a sample showed of its thread's stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unwound {
    /// Every frame, to the thread's first.
    Whole,

    /// The innermost frames, ending explicitly where unwinding could not
    /// go on.
    Truncated,

    /// The innermost frames, ending at a caller read from a return address
    /// the program overwrote.
    Corrupt,
}

impl Sampler {
    pub fn new(choices: &mut Choices) -> Self {
        Self {
            rate: *choices.pick(Stream::Sample, &RATES),
            rules: FrameRules::default(),
        }
    }

    /// After `tid` ran a burst, samples it if it is still running in user
    /// code and the run's rate says so, and judges the sample. Returns
    /// what the trace says of it.
    pub fn after_burst(
        &mut self,
        choices: &mut Choices,
        kernel: &mut Kernel,
        variant: &Variant,
        tid: Tid,
        marks: &mut Marks,
    ) -> Result<Option<String>, Failure> {
        let Some(thread) = kernel.threads.get(&tid) else {
            return Ok(None);
        };

        // A thread inside a system call, or on its way to take a signal it
        // raised, is in the kernel, which the events exclude.
        if !thread.in_user_code() || self.rate == 0 {
            return Ok(None);
        }

        if !choices.chance(Stream::Sample, self.rate) {
            return Ok(None);
        }

        let stack_bytes = *choices.pick(Stream::Sample, &STACK_BYTES);
        let record = kernel.sample(tid, stack_bytes);
        if kernel.gap.is_some() {
            return Ok(None);
        }

        let Some(Ok((decode::Record::Sample(sample), _))) = decode::next(&record) else {
            return Err(Failure::simulator(
                "samples",
                format!("the recorder cannot decode the sample of thread {tid}"),
            ));
        };

        let registers = decode::DWARF_REGISTERS
            .iter()
            .copied()
            .zip(sample.registers.iter())
            .collect::<Vec<_>>();
        let chain = sample.user_chain().collect::<Vec<_>>();
        let info = variant.debug_info();
        let modules = [UnwindModule {
            loaded: LoadedModule {
                id: ModuleId::new(1),
                image: ModuleImageId::new(1),
                load_bias: crate::LoadBias::new(variant.image.bias()),
            },
            image: &info.image,
            unwind: info.unwind.as_ref(),
        }];
        let (frames, end) = unwind_sample(
            &modules,
            &mut self.rules,
            SampledState {
                registers: &registers,
                stack: sample.stack,
                chain: &chain,
            },
            DEPTH_LIMIT,
        );
        let thread = &kernel.threads[&tid];
        if thread.shadow.lost {
            return Ok(None);
        }

        let space = &kernel.processes[&thread.tgid].space;
        let held = |slot: u64| space.peek(slot);
        let copied_to = thread.registers.general[crate::sim::cpu::RSP] + sample.stack.len() as u64;
        let unwound = judge(
            &thread.shadow,
            thread.registers.rip,
            &frames,
            end,
            held,
            copied_to,
        )
        .map_err(|message| Failure::debugger("samples", format!("thread {tid}'s {message}")))?;
        marks.hit(match unwound {
            Unwound::Whole
                if thread
                    .shadow
                    .calls
                    .first()
                    .is_some_and(|call| call.slot + 8 > copied_to) =>
            {
                Mark::SplicedSample
            }
            Unwound::Whole => Mark::WholeSample,
            Unwound::Truncated => Mark::TruncatedSample,
            Unwound::Corrupt => Mark::CorruptSampledCaller,
        });
        Ok(Some(format!(
            "sampled {} of {} frames, copying {stack_bytes}",
            frames.len(),
            thread.shadow.depth() + 1
        )))
    }
}

/// Samples: the frames of a sample are the thread's location and then,
/// within each call it has not returned from, innermost first, the byte
/// before the call's return address. A sample may end early only by saying
/// why. It may show a caller read from a return address the program
/// overwrote, which is what the stack holds, but nothing past it, and not
/// as a whole stack. Having found every frame, it must say so where its
/// copy, which ends at `copied_to`, holds the word above the outermost
/// call's return address, where the thread's first frame would keep its
/// own; short of that, nothing in the sample tells the first frame from
/// one with a caller. `held` reads the word now at a call's slot.
fn judge(
    shadow: &Shadow,
    rip: u64,
    frames: &[SampledFrame],
    end: StackEnd,
    held: impl Fn(u64) -> Option<u64>,
    copied_to: u64,
) -> Result<Unwound, String> {
    let calls = &shadow.calls;
    let depth = calls.len() + 1;
    let complete = end == StackEnd::Complete;
    let address = |frame: &SampledFrame| frame.address.get();
    match frames.first() {
        Some(frame) if address(frame) == rip => {}
        Some(frame) => {
            return Err(format!(
                "sample's innermost frame is at {:#x}, but the thread is at {rip:#x}",
                address(frame)
            ));
        }
        None if complete => return Err("sample is complete without a frame".to_owned()),
        None => return Ok(Unwound::Truncated),
    }

    for (level, frame) in frames.iter().enumerate().skip(1) {
        let address = address(frame);
        let Some(call) = calls.len().checked_sub(level).map(|index| calls[index]) else {
            return Err(format!(
                "sample's frame {level} is at {address:#x}, but the thread is only {depth} \
                 frames deep: {calls:x?}"
            ));
        };

        let held = held(call.slot);
        if held != Some(call.return_address) {
            let caller = held.unwrap_or_default().wrapping_sub(1);
            if address != caller {
                return Err(format!(
                    "sample's frame {level} is at {address:#x}; its return address {:#x} at \
                     {:#x} was overwritten with {held:x?}",
                    call.return_address, call.slot
                ));
            }

            if level + 1 != frames.len() || complete {
                return Err(format!(
                    "sample goes on past frame {level}, whose return address the program \
                     overwrote, ending {end:?}"
                ));
            }

            return Ok(Unwound::Corrupt);
        }

        if address != call.return_address - 1 {
            return Err(format!(
                "sample's frame {level} is at {address:#x}, but its call returns to {:#x}",
                call.return_address
            ));
        }
    }

    match (frames.len() < depth, complete) {
        (true, true) => Err(format!(
            "sample says it is complete after {} frames, but the thread is {depth} frames deep",
            frames.len()
        )),
        (false, false) if calls.first().is_none_or(|call| call.slot + 16 <= copied_to) => {
            Err(format!("sample found all {depth} frames, but ends {end:?}"))
        }
        (_, false) => Ok(Unwound::Truncated),
        (false, true) => Ok(Unwound::Whole),
    }
}
