//! Checks the unwinder against the truth at every instruction a program
//! runs.
//!
//! The program is single-stepped, and the return addresses its calls
//! push are kept as they run: an entry lives while the stack pointer has
//! not passed its slot, so returns, `longjmp`, and exceptions all drop it.
//! Each step is then unwound by call-frame information from the live
//! process ("live"), as the profiler unwinds a sample from a copy of the
//! stack and the kernel's frame-pointer walk ("sample N"), and from the
//! copy alone ("copy cfi N"), and each stack is judged against the truth:
//! exact, a true prefix with its reason, or wrong.
//!
//! The truth is the calls the program made, so it differs from call-frame
//! information that deliberately describes another frame: glibc's
//! `__longjmp` names the frame it jumps to as its caller once it has
//! restored the stack pointer.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use nix::sys::ptrace;
use nix::sys::signal::Signal;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use object::{Object as _, ObjectSegment as _};

use crate::debug_info::{DebugFileSearch, DebugInfo};
use crate::unwind::sample::{
    CHAIN_LIMIT, FrameRules, SampledState, StackEnd, unwind_memory, unwind_sample,
};
use crate::unwind::{DwarfRegister, MemoryReader, UnwindModule, unwind_module_at};
use crate::{LoadBias, LoadedModule, ModuleId, ModuleImageId, VirtualAddress};

/// The frames a path may report.
const LIMIT: usize = 256;

/// How the oracle runs a program.
#[derive(Debug, Clone)]
pub struct Options {
    pub program: PathBuf,
    pub args: Vec<String>,

    /// The stack copies the profiler's paths are checked with, in bytes.
    pub copies: Vec<usize>,

    /// Keep every this many steps' samples for the replay benchmark; none
    /// when 0.
    pub keep_every: u64,

    /// Rounds of the replay benchmark.
    pub rounds: usize,

    /// Examples of each kind of failure to print.
    pub examples: usize,

    /// Only keep samples and replay them, without checking each step.
    pub replay_only: bool,
}

/// One frame of the true stack, in the unwinder's terms: the address its
/// frame is named by, and the stack slot that keeps it alive.
#[derive(Debug, Clone, Copy)]
struct Entry {
    address: u64,
    slot: u64,

    /// The frame a signal interrupted, which only its `sigreturn` ends.
    interrupted: bool,
}

#[derive(Default)]
struct Modules {
    /// Every image loaded, by path.
    images: HashMap<PathBuf, Option<(ModuleImageId, DebugInfo)>>,

    /// The images mapped now, with their biases.
    mapped: Vec<(PathBuf, LoadBias, ModuleId)>,
}

impl Modules {
    fn refresh(&mut self, pid: Pid, memory: &File) -> Result<()> {
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
        self.mapped.clear();
        let mut seen = std::collections::HashSet::new();
        for line in maps.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 5 {
                continue;
            }
            let (Some((start, end)), offset) = (
                fields[0].split_once('-'),
                u64::from_str_radix(fields[2], 16).unwrap_or(1),
            ) else {
                continue;
            };
            let start = u64::from_str_radix(start, 16)?;
            let end = u64::from_str_radix(end, 16)?;
            let name = fields.get(5).copied().unwrap_or("");

            let path = if name == "[vdso]" {
                PathBuf::from(format!("[vdso]@{start:x}"))
            } else if name.starts_with('/') && offset == 0 {
                PathBuf::from(name)
            } else {
                continue;
            };

            if !seen.insert(path.clone()) {
                continue;
            }

            let next = self.images.len();
            let loaded = self.images.entry(path.clone()).or_insert_with(|| {
                let data = if name == "[vdso]" {
                    let mut bytes = vec![0; usize::try_from(end - start).unwrap_or(0)];
                    memory.read_exact_at(&mut bytes, start).ok().map(|()| bytes)
                } else {
                    std::fs::read(&path).ok()
                }?;
                let id = ModuleImageId::new(u32::try_from(next + 1).ok()?);
                let info = crate::debug_info::load_profile_module_bytes(
                    Path::new(name),
                    &data,
                    id,
                    &DebugFileSearch::default(),
                )
                .ok()?
                .0;
                Some((id, info))
            });

            if loaded.is_none() {
                continue;
            }

            let data = if name == "[vdso]" {
                None
            } else {
                std::fs::read(&path).ok()
            };
            let lowest = match &data {
                None => 0,
                Some(data) => {
                    let Ok(file) = object::File::parse(&**data) else {
                        continue;
                    };
                    file.segments()
                        .find(|segment| segment.file_range().0 == 0)
                        .map_or(0, |segment| segment.address() & !0xfff)
                }
            };

            let id = ModuleId::new(u32::try_from(self.mapped.len() + 1)?);
            self.mapped
                .push((path, LoadBias::new(start.wrapping_sub(lowest)), id));
        }

        Ok(())
    }

    fn unwind_modules(&self) -> Vec<UnwindModule<'_>> {
        self.mapped
            .iter()
            .filter_map(|(path, bias, id)| {
                let (image, info) = self.images.get(path)?.as_ref()?;
                Some(UnwindModule {
                    loaded: LoadedModule {
                        id: *id,
                        image: *image,
                        load_bias: *bias,
                        window: None,
                    },
                    image: &info.image,
                    unwind: info.unwind.as_ref(),
                })
            })
            .collect()
    }
}

/// A copy of the stack from the address it was taken at.
struct CopiedStack<'a>(u64, &'a [u8]);

impl MemoryReader for CopiedStack<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let offset = usize::try_from(address.get().checked_sub(self.0)?).ok()?;
        let word = self.1.get(offset..offset.checked_add(8)?)?;
        Some(u64::from_le_bytes(word.try_into().ok()?))
    }
}

struct Live<'a>(&'a File);

impl MemoryReader for Live<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let mut word = [0; 8];
        self.0.read_exact_at(&mut word, address.get()).ok()?;
        Some(u64::from_le_bytes(word))
    }
}

/// Names `address` by its module's file and symbol.
fn name(modules: &[UnwindModule<'_>], address: u64) -> String {
    let Some((index, image_address)) = unwind_module_at(modules, VirtualAddress::new(address))
    else {
        return format!("{address:#x}");
    };
    let module = &modules[index];
    let file = module
        .image
        .path_arc()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match module.image.symbolize(image_address) {
        Some(symbol) => format!("{file}:{}+{:#x}", symbol.name, symbol.offset),
        None => format!("{file}:{:#x}", image_address.get()),
    }
}

/// The function `address` is in, without its offset.
fn function(modules: &[UnwindModule<'_>], address: u64) -> String {
    let named = name(modules, address);
    named
        .rsplit_once('+')
        .map_or_else(|| named.clone(), |(function, _)| function.to_owned())
}

/// How one path's stack compares with the truth.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    /// Every frame, and it said the stack was complete.
    Exact,

    /// Every frame, but it could not prove the last was the outermost.
    WholeUnproven(String),

    /// A true prefix, ending with a reason.
    Short(String),

    /// A true prefix that it said was the whole stack.
    FalselyComplete,

    /// Every true frame and more after them.
    Extra,

    /// A frame that is not the true one.
    Wrong,
}

impl Verdict {
    const fn bad(&self) -> bool {
        matches!(self, Self::FalselyComplete | Self::Extra | Self::Wrong)
    }
}

fn reason(end: StackEnd) -> String {
    match end {
        StackEnd::Complete => "complete".into(),
        StackEnd::Truncated(truncation, _) => format!("{truncation:?}"),
    }
}

fn judge(truth: &[u64], got: &[u64], end: StackEnd) -> (Verdict, usize) {
    let common = truth
        .iter()
        .zip(got)
        .take_while(|(truth, got)| truth == got)
        .count();
    let verdict = if common < got.len() && common < truth.len() {
        Verdict::Wrong
    } else if got.len() > truth.len() {
        Verdict::Extra
    } else if got.len() == truth.len() {
        match end {
            StackEnd::Complete => Verdict::Exact,
            StackEnd::Truncated(..) => Verdict::WholeUnproven(reason(end)),
        }
    } else if end == StackEnd::Complete {
        Verdict::FalselyComplete
    } else if got.len() == LIMIT {
        Verdict::Short("DepthLimit".into())
    } else {
        Verdict::Short(reason(end))
    };
    (verdict, common)
}

/// What one path found over the run.
#[derive(Default)]
struct Tally {
    verdicts: BTreeMap<Verdict, u64>,

    /// Steps by verdict and the function the verdict is about: where a
    /// short stack stopped, or the callee of the first wrong frame.
    places: BTreeMap<(Verdict, String), (u64, String)>,
    frames: u64,
    true_frames: u64,
    elapsed: Duration,
}

/// One kept sample, for the replay benchmark.
struct Kept {
    registers: Vec<(DwarfRegister, u64)>,
    stack: Vec<u8>,
    chain: Vec<u64>,
}

fn sample_registers(registers: &nix::libc::user_regs_struct) -> Vec<(DwarfRegister, u64)> {
    let r = DwarfRegister::new;
    vec![
        (r(0), registers.rax),
        (r(1), registers.rdx),
        (r(2), registers.rcx),
        (r(3), registers.rbx),
        (r(4), registers.rsi),
        (r(5), registers.rdi),
        (r(6), registers.rbp),
        (r(7), registers.rsp),
        (r(8), registers.r8),
        (r(9), registers.r9),
        (r(10), registers.r10),
        (r(11), registers.r11),
        (r(12), registers.r12),
        (r(13), registers.r13),
        (r(14), registers.r14),
        (r(15), registers.r15),
        (r(16), registers.rip),
        (r(49), registers.eflags),
    ]
}

/// The kernel's walk of the frame-pointer chain, as `perf_callchain_user`
/// makes it.
fn kernel_chain(memory: &File, rip: u64, rbp: u64) -> Vec<u64> {
    let mut chain = vec![rip];
    let mut frame = rbp;
    while chain.len() < usize::from(CHAIN_LIMIT) {
        let mut words = [0; 16];
        if frame >= 0x7fff_ffff_f000 || memory.read_exact_at(&mut words, frame).is_err() {
            break;
        }
        let next = u64::from_le_bytes(words[..8].try_into().unwrap_or_default());
        let return_address = u64::from_le_bytes(words[8..].try_into().unwrap_or_default());
        chain.push(return_address);
        frame = next;
    }
    chain
}

/// Runs the program to its end under the oracle and prints its report.
#[expect(clippy::too_many_lines, reason = "one loop over the program's steps")]
#[expect(clippy::cast_precision_loss, reason = "reported approximately")]
pub fn run(options: &Options) -> Result<String> {
    let mut command = Command::new(&options.program);
    command.args(&options.args);
    crate::backend::trace_on_exec(&mut command);
    let child = command.spawn().context("start the program")?;
    let pid = Pid::from_raw(i32::try_from(child.id())?);
    match waitpid(pid, None)? {
        WaitStatus::Stopped(_, Signal::SIGTRAP) => {}
        status => bail!("the program stopped as {status:?}, not at its exec"),
    }

    let memory = File::open(format!("/proc/{pid}/mem"))?;
    let mut modules = Modules::default();
    modules.refresh(pid, &memory)?;

    let paths: Vec<String> = std::iter::once("live".to_owned())
        .chain(options.copies.iter().map(|bytes| format!("sample {bytes}")))
        .chain(
            options
                .copies
                .iter()
                .map(|bytes| format!("copy cfi {bytes}")),
        )
        .collect();
    let mut tallies: Vec<Tally> = paths.iter().map(|_| Tally::default()).collect();
    let mut rules: Vec<FrameRules> = options
        .copies
        .iter()
        .map(|_| FrameRules::default())
        .collect();
    let mut shadow: Vec<Entry> = Vec::new();
    let mut instructions: HashMap<u64, (bool, bool, u64)> = HashMap::new();
    let mut kept: Vec<Kept> = Vec::new();
    let mut steps = 0_u64;
    let mut signal = None;
    let deepest = options.copies.iter().copied().max().unwrap_or(0);
    let mut stack = vec![0_u8; deepest];

    loop {
        let registers = ptrace::getregs(pid)?;
        let (rip, rsp) = (registers.rip, registers.rsp);

        // Entries whose slots the stack pointer passed have returned.
        while let Some(top) = shadow.last() {
            if top.slot >= rsp || top.interrupted {
                break;
            }
            shadow.pop();
        }

        let mut truth = vec![rip];
        truth.extend(shadow.iter().rev().map(|entry| entry.address));

        let modules_now = modules.unwind_modules();
        let live_registers = sample_registers(&registers);

        if options.replay_only {
            if steps.is_multiple_of(options.keep_every.max(1)) {
                let copied = memory.read_at(&mut stack, rsp).unwrap_or(0);
                kept.push(Kept {
                    registers: live_registers.clone(),
                    stack: stack[..copied].to_vec(),
                    chain: kernel_chain(&memory, rip, registers.rbp),
                });
            }
        } else {
            let started = Instant::now();
            let (frames, end) =
                unwind_memory(&modules_now, &live_registers, &mut Live(&memory), LIMIT);
            tallies[0].elapsed += started.elapsed();
            let mut results = vec![(frames, end)];

            let copied = if deepest > 0 {
                memory.read_at(&mut stack, rsp).unwrap_or_default()
            } else {
                0
            };
            let chain = kernel_chain(&memory, rip, registers.rbp);
            for (index, bytes) in options.copies.iter().enumerate() {
                let started = Instant::now();
                let result = unwind_sample(
                    &modules_now,
                    &mut rules[index],
                    SampledState {
                        registers: &live_registers,
                        stack: &stack[..copied.min(*bytes)],
                        chain: &chain,
                    },
                    LIMIT,
                );
                tallies[index + 1].elapsed += started.elapsed();
                results.push(result);
            }

            for (index, bytes) in options.copies.iter().enumerate() {
                let started = Instant::now();
                let result = unwind_memory(
                    &modules_now,
                    &live_registers,
                    &mut CopiedStack(rsp, &stack[..copied.min(*bytes)]),
                    LIMIT,
                );
                tallies[index + 1 + options.copies.len()].elapsed += started.elapsed();
                results.push(result);
            }

            if options.keep_every > 0 && steps.is_multiple_of(options.keep_every) {
                kept.push(Kept {
                    registers: live_registers.clone(),
                    stack: stack[..copied].to_vec(),
                    chain: chain.clone(),
                });
            }

            for ((tally, (frames, end)), path) in tallies.iter_mut().zip(&results).zip(&paths) {
                let got: Vec<u64> = frames.iter().map(|frame| frame.address.get()).collect();
                let (verdict, common) = judge(&truth, &got, *end);
                tally.frames += got.len() as u64;
                tally.true_frames += truth.len() as u64;
                *tally.verdicts.entry(verdict.clone()).or_default() += 1;
                if verdict != Verdict::Exact {
                    let at = match &verdict {
                        Verdict::Wrong => truth[common.saturating_sub(1)],
                        _ => got.last().copied().unwrap_or(rip),
                    };
                    let place = if verdict.bad() {
                        format!(
                            "{} (in {})",
                            function(&modules_now, at),
                            function(&modules_now, rip)
                        )
                    } else {
                        function(&modules_now, at)
                    };
                    let example = tally.places.entry((verdict.clone(), place)).or_default();
                    example.0 += 1;
                    if example.1.is_empty() {
                        let mut text = format!(
                            "  [{path}] {verdict:?} step {steps} at {}\n",
                            name(&modules_now, rip)
                        );
                        for (level, address) in truth.iter().enumerate() {
                            let got = got.get(level).copied();
                            let mark = if got == Some(*address) { " " } else { "!" };
                            let _ = writeln!(
                                text,
                                "    {mark}{level:3} {:<60} got {}",
                                name(&modules_now, *address),
                                got.map_or_else(|| "-".to_owned(), |got| name(&modules_now, got)),
                            );
                        }
                        for (level, address) in got.iter().enumerate().skip(truth.len()) {
                            let _ = writeln!(
                                text,
                                "    +{level:3} {:<60}",
                                name(&modules_now, *address)
                            );
                        }
                        let _ = writeln!(text, "    end {end:?}");
                        example.1 = text;
                    }
                }
            }
        }

        // The instruction about to run.
        let (is_call, is_syscall, next) = *instructions.entry(rip).or_insert_with(|| {
            let mut bytes = [0; 16];
            let length = memory.read_at(&mut bytes, rip).unwrap_or(0);
            let mut decoder = iced_x86::Decoder::with_ip(
                64,
                &bytes[..length],
                rip,
                iced_x86::DecoderOptions::NONE,
            );
            let instruction = decoder.decode();
            (
                instruction.flow_control() == iced_x86::FlowControl::Call
                    || instruction.flow_control() == iced_x86::FlowControl::IndirectCall,
                instruction.mnemonic() == iced_x86::Mnemonic::Syscall,
                instruction.next_ip(),
            )
        });
        let rax = registers.rax;

        ptrace::step(pid, signal.take())?;
        steps += 1;
        let delivered = match waitpid(pid, None)? {
            WaitStatus::Stopped(_, Signal::SIGTRAP) => false,
            WaitStatus::Stopped(_, other) => {
                // Deliver it on the next step, which stops at its handler.
                signal = Some(other);
                ptrace::step(pid, signal.take())?;
                match waitpid(pid, None)? {
                    WaitStatus::Stopped(_, Signal::SIGTRAP) => true,
                    WaitStatus::Exited(..) | WaitStatus::Signaled(..) => break,
                    status => bail!("unexpected {status:?}"),
                }
            }
            WaitStatus::Exited(..) | WaitStatus::Signaled(..) => break,
            status => bail!("unexpected {status:?}"),
        };

        let after = ptrace::getregs(pid)?;
        if delivered {
            // The handler's return address is the trampoline; the
            // ucontext above it holds the interrupted rip.
            let mut word = [0; 8];
            memory.read_exact_at(&mut word, after.rsp)?;
            let restorer = u64::from_le_bytes(word);
            memory.read_exact_at(&mut word, after.rsp + 8 + 40 + 16 * 8)?;
            let interrupted = u64::from_le_bytes(word);
            shadow.push(Entry {
                address: interrupted,
                slot: after.rsp + 8,
                interrupted: true,
            });
            shadow.push(Entry {
                address: restorer,
                slot: after.rsp,
                interrupted: false,
            });
            continue;
        }

        if is_syscall {
            if rax == 15 {
                // rt_sigreturn: the interrupted frame runs again.
                while let Some(entry) = shadow.pop() {
                    if entry.interrupted {
                        break;
                    }
                }
            }
            if matches!(rax, 9 | 10 | 11 | 25) {
                modules.refresh(pid, &memory)?;
            }
        }

        if is_call && after.rsp == rsp.wrapping_sub(8) {
            shadow.push(Entry {
                address: next - 1,
                slot: after.rsp,
                interrupted: false,
            });
        }
    }

    let mut report = String::new();
    let _ = writeln!(
        report,
        "{} under the oracle: {steps} steps, {} images",
        options.program.display(),
        modules.images.len()
    );
    for (path, tally) in paths.iter().zip(&tallies) {
        let bad: u64 = tally
            .verdicts
            .iter()
            .filter(|(verdict, _)| verdict.bad())
            .map(|(_, count)| count)
            .sum();
        let _ = writeln!(
            report,
            "\n== {path}: {bad} bad steps; {} of {} true frames found; {:.0} ns a step",
            tally.frames,
            tally.true_frames,
            tally.elapsed.as_nanos() as f64 / steps.max(1) as f64,
        );
        for (verdict, count) in &tally.verdicts {
            let _ = writeln!(report, "  {count:>8} {verdict:?}");
        }
        let mut places: Vec<_> = tally.places.iter().collect();
        places.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
        for ((verdict, place), (count, _)) in places.iter().take(30) {
            let _ = writeln!(report, "    {count:>8} {verdict:?} {place}");
        }
        let mut shown = BTreeMap::<Verdict, usize>::new();
        for ((verdict, _), (_, example)) in places {
            let seen = shown.entry(verdict.clone()).or_default();
            if *seen < options.examples {
                *seen += 1;
                report.push_str(example);
            }
        }
    }

    if !kept.is_empty() && options.rounds > 0 {
        let modules_now = modules.unwind_modules();
        let _ = writeln!(report, "\n== replay: {} samples", kept.len());
        for bytes in &options.copies {
            for warm in [false, true] {
                let mut per_sample = Vec::new();
                let mut frames = 0_u64;
                let mut warm_rules = FrameRules::default();
                for _ in 0..options.rounds {
                    let mut cold_rules = FrameRules::default();
                    let rules = if warm {
                        &mut warm_rules
                    } else {
                        &mut cold_rules
                    };
                    frames = 0;
                    let started = Instant::now();
                    for sample in &kept {
                        let (unwound, _) = unwind_sample(
                            &modules_now,
                            rules,
                            SampledState {
                                registers: &sample.registers,
                                stack: &sample.stack[..sample.stack.len().min(*bytes)],
                                chain: &sample.chain,
                            },
                            LIMIT,
                        );
                        frames += unwound.len() as u64;
                    }
                    per_sample.push(started.elapsed().as_nanos() as f64 / kept.len() as f64);
                }
                let n = per_sample.len() as f64;
                let mean = per_sample.iter().sum::<f64>() / n;
                let deviation = (per_sample.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
                    / (n - 1.0).max(1.0))
                .sqrt();
                let mut sorted = per_sample.clone();
                sorted.sort_by(f64::total_cmp);
                let _ = writeln!(
                    report,
                    "  sample {bytes} {}: mean {mean:.0} ns/sample ±{:.0} (95% CI), median {:.0}, {:.1} frames/sample, {:.1} ns/frame",
                    if warm { "warm" } else { "cold" },
                    1.96 * deviation / n.sqrt(),
                    sorted[sorted.len() / 2],
                    frames as f64 / kept.len() as f64,
                    mean * kept.len() as f64 / frames.max(1) as f64,
                );
            }
        }
    }

    Ok(report)
}
