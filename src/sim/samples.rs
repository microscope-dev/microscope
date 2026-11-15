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

const RATES: [u64; 4] = [0, 50, 250, 1000];

const STACK_BYTES: [u32; 4] = [64, 512, 8192, 65528];

const DEPTH_LIMIT: usize = 128;

pub struct Sampler {
    rate: u64,
    rules: FrameRules,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unwound {
    Whole,

    Truncated,

    Corrupt,
}

impl Sampler {
    pub fn new(choices: &mut Choices) -> Self {
        Self {
            rate: *choices.pick(Stream::Sample, &RATES),
            rules: FrameRules::default(),
        }
    }

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
                window: None,
            },
            image: &info.image,
            unwind: info.unwind.as_ref(),
        }];

        let (frames, end) = unwind_sample(
            &modules[..],
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
