use std::collections::{BTreeMap, BTreeSet};

use iced_x86::{Code, Instruction, RflagsBits};
use nix::libc;

use crate::backend::native_tracee::NativeTracee;
use crate::sim::corpus::{Corpus, Variant};
use crate::sim::cpu::{
    self, ADJUST, CARRY, DIRECTION, OVERFLOW, Outcome, PARITY, RAX, RDI, RSI, Registers, SIGN,
    STATUS_FLAGS, ZERO,
};

use crate::sim::kernel::{Tid, WaitStatus};
use crate::sim::memory::{AddressSpace, Backing, Protection};

const MAX_STEPS: u64 = 1_000_000;
const SYS_CLONE: u64 = 56;
const SYS_FORK: u64 = 57;
const SYS_EXIT: u64 = 60;
const SYS_WAIT4: u64 = 61;
const SYS_EXIT_GROUP: u64 = 231;

#[test]
fn the_interpreter_executes_every_golden_program_as_the_cpu_does() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let runs = corpus
        .programs
        .iter()
        .flat_map(|program| {
            program
                .variants
                .iter()
                .flat_map(|variant| program.runs.iter().map(move |run| (variant, run)))
        })
        .collect::<Vec<_>>();

    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    let (checked, executed) = std::thread::scope(|scope| {
        let workers = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut checked = 0;
                    let mut executed = BTreeSet::new();
                    while let Some(&(variant, run)) =
                        runs.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                    {
                        match lockstep(variant, &run.arguments, &mut executed) {
                            Ok(steps) => checked += steps,
                            Err(divergence) => panic!(
                                "{} {:?} diverged from the CPU: {divergence}",
                                variant.name, run.arguments
                            ),
                        }
                    }

                    (checked, executed)
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .fold(
                (0, BTreeSet::new()),
                |(checked, mut executed), (steps, codes)| {
                    executed.extend(codes);
                    (checked + steps, executed)
                },
            )
    });
    assert!(checked > 0, "no instruction was checked");

    let unexecuted = cpu::IMPLEMENTED
        .iter()
        .filter(|&code| !executed.contains(code) && !cpu::PROBED_THROUGH_THE_KERNEL.contains(code))
        .collect::<Vec<_>>();

    assert!(
        unexecuted.is_empty(),
        "the model implements forms no golden program executes: {unexecuted:?}"
    );
}

#[derive(Clone, Copy)]
struct Stepped {
    tid: Tid,
    process: Tid,
    registers: Registers,
}

#[expect(
    clippy::too_many_lines,
    reason = "one loop follows every way a step can end, as the CPU reports it"
)]
fn lockstep(
    variant: &Variant,
    arguments: &[String],
    executed: &mut BTreeSet<Code>,
) -> Result<u64, String> {
    let (native, first) = NativeTracee::spawn(&variant.file, arguments);
    let leader = native.pid();
    if first != WaitStatus::Stopped(leader, libc::SIGTRAP) {
        return Err(format!("the first stop was {first}"));
    }

    let failed = |error: nix::errno::Errno| error.to_string();

    native.set_options(leader, true).map_err(failed)?;
    let mut threads = vec![Stepped {
        tid: leader,
        process: leader,
        registers: native.registers(leader).map_err(failed)?,
    }];
    let mut memories = BTreeMap::from([(leader, copy_memory(&native, leader))]);
    let mut current = 0;

    for step in 1..=MAX_STEPS {
        let Stepped {
            tid,
            process,
            mut registers,
        } = threads[current];
        let memory = memories.get_mut(&process).expect("a process has memory");
        let instruction = cpu::decode(registers.rip, memory)
            .map_err(|outcome| format!("decoding at {:#x}: {outcome:?}", registers.rip))?;
        let before = registers;
        let outcome = cpu::execute(&mut registers, memory, true).outcome;

        native.resume(tid, None, true).map_err(failed)?;
        let mut status = native.wait(tid);

        while status == WaitStatus::Stopped(tid, libc::SIGCHLD) {
            native
                .resume(tid, Some(libc::SIGCHLD), true)
                .map_err(failed)?;
            status = native.wait(tid);
        }

        let at = format!("thread {tid}: {}", cpu::describe(&instruction));
        if matches!(outcome, Outcome::Completed | Outcome::Syscall) {
            executed.insert(instruction.code());
        }

        match outcome {
            Outcome::Completed => {}
            Outcome::Syscall => {
                #[expect(clippy::cast_possible_truncation, reason = "exit codes are ints")]
                let code = (before.general[RDI] as i32) & 0xff;
                match before.general[RAX] {
                    SYS_EXIT_GROUP => {
                        expect_exit_event(&native, tid, status, code)
                            .and_then(|()| compare_memory(&native, process, &memories[&process]))
                            .map_err(|difference| format!("{at}: {difference}"))?;
                        if threads.iter().all(|thread| thread.process == process) {
                            return Ok(step);
                        }

                        if threads
                            .iter()
                            .any(|thread| thread.process == process && thread.tid != tid)
                        {
                            return Err(format!(
                                "{at}: a process exited its group with threads running while \
                                 another process runs, which the lockstep does not follow"
                            ));
                        }

                        native.resume(tid, None, false).map_err(failed)?;
                        let ended = native.wait(tid);
                        if ended != WaitStatus::Exited(tid, code) {
                            return Err(format!("{at}: the process ended {ended}"));
                        }

                        threads.remove(current);
                        memories.remove(&process);
                        current %= threads.len();
                        continue;
                    }
                    SYS_EXIT => {
                        expect_exit_event(&native, tid, status, code)
                            .and_then(|()| compare_memory(&native, process, &memories[&process]))
                            .map_err(|difference| format!("{at}: {difference}"))?;

                        native.resume(tid, None, false).map_err(failed)?;
                        threads.remove(current);
                        if tid != process {
                            let exited = native.wait(tid);
                            if exited != WaitStatus::Exited(tid, code) {
                                return Err(format!("{at}: then it reported {exited}"));
                            }
                        }

                        if threads.iter().all(|thread| thread.process != process) {
                            let ended = native.wait(process);
                            if ended != WaitStatus::Exited(process, code) {
                                return Err(format!("{at}: the process ended {ended}"));
                            }

                            memories.remove(&process);
                            if threads.is_empty() {
                                return Ok(step);
                            }
                        }

                        current %= threads.len();
                        continue;
                    }
                    SYS_CLONE | SYS_FORK => {
                        let event = if before.general[RAX] == SYS_CLONE {
                            libc::PTRACE_EVENT_CLONE
                        } else {
                            libc::PTRACE_EVENT_FORK
                        };

                        if status != WaitStatus::Event(tid, event) {
                            return Err(format!("{at}: the CPU reported {status}"));
                        }

                        let child =
                            native
                                .event_message(tid)
                                .map_err(failed)
                                .and_then(|message| {
                                    Tid::try_from(message).map_err(|error| error.to_string())
                                })?;

                        native.resume(tid, None, true).map_err(failed)?;
                        status = native.wait(tid);
                        let started = native.wait(child);
                        if started != WaitStatus::Stopped(child, libc::SIGSTOP) {
                            return Err(format!("{at}: the new thread reported {started}"));
                        }

                        let child_process = if event == libc::PTRACE_EVENT_FORK {
                            let copy = memories[&process].clone();
                            memories.insert(child, copy);
                            child
                        } else {
                            process
                        };

                        threads.push(Stepped {
                            tid: child,
                            process: child_process,
                            registers: native.registers(child).map_err(failed)?,
                        });
                    }
                    _ => {}
                }

                let result = native.registers(tid).map_err(failed)?.general[RAX];
                registers.general[RAX] = result;
                if before.general[RAX] == SYS_WAIT4 && result == before.general[RDI] {
                    copy_stored(&native, process, &mut memories, before.general[RSI], 4)
                        .map_err(|difference| format!("{at}: {difference}"))?;
                }
            }
            other => return Err(format!("{at}: the interpreter reported {other:?}")),
        }

        if status != WaitStatus::Stopped(tid, libc::SIGTRAP) {
            return Err(format!("{at}: the CPU reported {status}"));
        }

        let expected = native.registers(tid).map_err(failed)?;
        compare(&instruction, &mut registers, &expected)
            .map_err(|difference| format!("{at}: {difference}"))?;
        threads[current].registers = registers;

        if outcome == Outcome::Syscall {
            compare_memory(&native, process, &memories[&process])
                .map_err(|difference| format!("{at}: {difference}"))?;

            if before.general[RAX] != SYS_FORK {
                current = (current + 1) % threads.len();
            }
        }
    }

    Err(format!("still running after {MAX_STEPS} instructions"))
}

fn copy_stored(
    native: &NativeTracee,
    process: Tid,
    memories: &mut BTreeMap<Tid, AddressSpace>,
    address: u64,
    length: usize,
) -> Result<(), String> {
    if address == 0 {
        return Ok(());
    }

    let bytes = native
        .read_memory(process, address, length)
        .ok_or_else(|| format!("the kernel stored at {address:#x}, which cannot be read"))?;
    let memory = memories.get_mut(&process).expect("a process has memory");
    if memory.poke_bytes(address, &bytes) {
        Ok(())
    } else {
        Err(format!(
            "the kernel stored at {address:#x}, which the interpreter has not mapped"
        ))
    }
}

fn expect_exit_event(
    native: &NativeTracee,
    tid: Tid,
    status: WaitStatus,
    code: i32,
) -> Result<(), String> {
    if status != WaitStatus::Event(tid, libc::PTRACE_EVENT_EXIT) {
        return Err(format!("exiting with {code}, the CPU reported {status}"));
    }

    let message = native
        .event_message(tid)
        .map_err(|error| error.to_string())?;
    if message != u64::from(code.cast_unsigned()) << 8 {
        return Err(format!(
            "exiting with {code}, the exit event says {message:#x}"
        ));
    }

    Ok(())
}

fn compare(
    instruction: &Instruction,
    registers: &mut Registers,
    expected: &Registers,
) -> Result<(), String> {
    for (index, (&actual, &wanted)) in registers.general.iter().zip(&expected.general).enumerate() {
        if actual != wanted {
            return Err(format!(
                "general register {index} is {actual:#x}, the CPU has {wanted:#x}"
            ));
        }
    }

    if registers.rip != expected.rip {
        return Err(format!(
            "rip is {:#x}, the CPU has {:#x}",
            registers.rip, expected.rip
        ));
    }

    let undefined = undefined_flags(instruction);
    let defined = (STATUS_FLAGS | DIRECTION) & !undefined;
    if registers.rflags & defined != expected.rflags & defined {
        return Err(format!(
            "rflags is {:#x}, the CPU has {:#x}, comparing {defined:#x}",
            registers.rflags, expected.rflags
        ));
    }

    registers.rflags = (registers.rflags & !undefined) | (expected.rflags & undefined);
    Ok(())
}

fn undefined_flags(instruction: &Instruction) -> u64 {
    let iced = instruction.rflags_undefined();
    [
        (RflagsBits::CF, CARRY),
        (RflagsBits::PF, PARITY),
        (RflagsBits::AF, ADJUST),
        (RflagsBits::ZF, ZERO),
        (RflagsBits::SF, SIGN),
        (RflagsBits::OF, OVERFLOW),
    ]
    .into_iter()
    .filter(|&(bit, _)| iced & bit != 0)
    .fold(0, |flags, (_, flag)| flags | flag)
}

fn copy_memory(native: &NativeTracee, process: Tid) -> AddressSpace {
    let mut memory = AddressSpace::default();
    for region in regions(native, process) {
        if region.name == "[vsyscall]" {
            continue;
        }

        memory.map(
            region.start,
            region.end,
            region.protection,
            Backing::Anonymous,
        );
        let length = usize::try_from(region.end - region.start).expect("a mapping fits usize");

        if let Some(bytes) = native.read_memory(process, region.start, length) {
            assert!(
                memory.poke_bytes(region.start, &bytes),
                "the region was just mapped"
            );
        }
    }

    memory
}

fn compare_memory(
    native: &NativeTracee,
    process: Tid,
    memory: &AddressSpace,
) -> Result<(), String> {
    for region in regions(native, process).filter(|region| region.protection.write) {
        let length = usize::try_from(region.end - region.start).expect("a mapping fits usize");
        let Some(wanted) = native.read_memory(process, region.start, length) else {
            continue;
        };

        let actual = memory
            .peek_bytes(region.start, length)
            .ok_or_else(|| format!("{:#x} is not mapped in the interpreter", region.start))?;
        if let Some(offset) = actual.iter().zip(&wanted).position(|(a, b)| a != b) {
            return Err(format!(
                "memory at {:#x} is {:#x}, the CPU has {:#x}",
                region.start + offset as u64,
                actual[offset],
                wanted[offset]
            ));
        }
    }

    Ok(())
}

struct Region {
    start: u64,
    end: u64,
    protection: Protection,
    name: String,
}

fn regions(native: &NativeTracee, process: Tid) -> impl Iterator<Item = Region> {
    native
        .maps(process)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let range = fields.next().expect("a range");
            let permissions = fields.next().expect("permissions").as_bytes();
            let name = fields.nth(3).unwrap_or("").to_owned();
            let (start, end) = range.split_once('-').expect("a range");
            Region {
                start: u64::from_str_radix(start, 16).expect("a start"),
                end: u64::from_str_radix(end, 16).expect("an end"),
                protection: Protection {
                    read: permissions[0] == b'r',
                    write: permissions[1] == b'w',
                    execute: permissions[2] == b'x',
                },
                name,
            }
        })
        .collect::<Vec<_>>()
        .into_iter()
}
