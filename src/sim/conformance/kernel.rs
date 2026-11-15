use nix::libc;

use super::tracee::{Record, Tracee, dual_run, dual_run_then_exit};
use crate::sim::cpu::{RAX, RDI, RSP};
use crate::sim::kernel::{Tid, WaitStatus};

const ENOSYS: u64 = (-(libc::ENOSYS as i64)).cast_unsigned();

const SYS_CLONE: u64 = 56;

const SYS_FORK: u64 = 57;

fn run_to_entry(record: &mut Record<'_>, thread: Tid, number: u64) {
    let mut signal = None;
    loop {
        record
            .tracee
            .resume_to_call(thread, signal)
            .expect("continue to a call");
        signal = match record.tracee.wait(thread) {
            WaitStatus::Stopped(_, signal) => Some(signal),
            _ => None,
        };

        let info = record.tracee.call_info(thread).expect("a stopped thread");
        if info.op == 1 && info.number == number {
            return;
        }
    }
}

fn restart(record: &mut Record<'_>, thread: Tid) {
    record.resume_to_call(thread, None);
    record.wait(thread);
    record.stop(thread);
    record.system_call(thread);
    record.call(thread);
    record.children_of(thread);
    record.resume_to_call(thread, None);
    record.wait(thread);
    record.stop(thread);
    record.system_call(thread);
    record.resume_to_call(thread, None);
    record.wait(thread);
    record.stop(thread);
    record.system_call(thread);
    record.call(thread);
}

#[test]
fn k_exec_1_a_launched_program_stops_at_its_entry() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.stop(leader);
        record.system_call(leader);
        let name = record.tracee.name(leader);
        record.note(format!("name: {name}"));
    });
}

#[test]
fn k_exec_2_a_program_loads_where_linux_puts_it() {
    dual_run("frames", &[], |record| {
        let leader = record.leader();
        let (registers, _) = record
            .tracee
            .registers(leader)
            .expect("registers at the first stop");
        record.note(format!("rip: {:#x}", registers.rip));
        let maps = record.tracee.maps(leader).unwrap_or_default();

        for line in maps.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.last().is_some_and(|path| path.contains("/frames-")) {
                record.note(format!("mapped: {}", fields[..3].join(" ")));
            }
        }
    });
}

#[test]
fn k_exec_2_a_programs_pages_hold_what_linux_loads() {
    for program in ["data", "fork", "containers"] {
        dual_run(program, &[], |record| {
            let leader = record.leader();
            let image = record.landmarks.image.clone();
            let maps = record.tracee.maps(leader).unwrap_or_default();

            for line in maps.lines() {
                let fields = line.split_whitespace().collect::<Vec<_>>();
                let (start, end) = fields[0].split_once('-').expect("a range");
                let start = u64::from_str_radix(start, 16).expect("a start");
                let end = u64::from_str_radix(end, 16).expect("an end");
                if end <= image.start || start >= image.end.next_multiple_of(4096) {
                    continue;
                }

                let mut hash = 0xcbf2_9ce4_8422_2325_u64;

                for address in (start..end).step_by(8) {
                    let word = record.tracee.peek(leader, address).unwrap_or(0);
                    for byte in word.to_le_bytes() {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                    }
                }

                let place = format!("{}-{}", record.place(start), record.place(end));

                record.note(format!("{place} {}: {hash:016x}", fields[1]));
            }
        });
    }
}

const UNMODELED_AUXILIARY_ENTRIES: [u64; 10] = [
    33, 51, 16, 26, 11, 12, 13, 14, 27,
    28, // AT_RSEQ_FEATURE_SIZE, AT_RSEQ_ALIGN: rseq since Linux 6.3, sized by version.
];

struct FirstStack {
    top: u64,
    rsp: u64,
    argv: Vec<u64>,

    argv_end: u64,
    environment: usize,

    auxiliary: Vec<(u64, u64)>,

    null: u64,
    end: u64,
}

impl FirstStack {
    fn read(tracee: &dyn Tracee, thread: Tid) -> Self {
        let (registers, _) = tracee
            .registers(thread)
            .expect("registers at the first stop");
        let maps = tracee.maps(thread).unwrap_or_default();
        let top = maps
            .lines()
            .find(|line| line.ends_with("[stack]"))
            .and_then(|line| u64::from_str_radix(line.split(['-', ' ']).nth(1)?, 16).ok())
            .expect("a stack");
        let word = |address| stack_word(tracee, thread, address);
        let rsp = registers.general[RSP];
        let argv = (0..word(rsp))
            .map(|index| word(rsp + 8 + 8 * index))
            .collect::<Vec<_>>();
        let mut at = rsp + 8 + 8 * argv.len() as u64;
        let argv_end = word(at);
        let mut environment = 0;
        while word(at + 8) != 0 {
            environment += 1;
            at += 8;
        }

        at += 16;
        let mut auxiliary = Vec::new();

        let null = loop {
            let (key, value) = (word(at), word(at + 8));
            at += 16;
            if key == 0 {
                break value;
            }

            if !UNMODELED_AUXILIARY_ENTRIES.contains(&key) {
                auxiliary.push((key, value));
            }
        };

        Self {
            top,
            rsp,
            argv,
            argv_end,
            environment,
            auxiliary,
            null,
            end: at,
        }
    }

    fn entry(&self, key: u64) -> Option<u64> {
        self.auxiliary
            .iter()
            .find(|&&(held, _)| held == key)
            .map(|&(_, value)| value)
    }
}

fn stack_word(tracee: &dyn Tracee, thread: Tid, address: u64) -> u64 {
    tracee
        .peek(thread, address)
        .unwrap_or_else(|errno| panic!("reading the stack at {address:#x}: {errno}"))
}

fn stack_string(tracee: &dyn Tracee, thread: Tid, address: u64, top: u64) -> String {
    let mut bytes = Vec::new();

    for at in (address..top).step_by(8) {
        let chunk = stack_word(tracee, thread, at).to_le_bytes();
        if let Some(end) = chunk.iter().position(|&byte| byte == 0) {
            bytes.extend(&chunk[..end]);
            break;
        }

        bytes.extend(chunk);
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn k_exec_3_the_first_stack_is_laid_out_as_linux_lays_it() {
    const AT_PHDR: u64 = 3;
    const AT_ENTRY: u64 = 9;
    const AT_PLATFORM: u64 = 15;
    const AT_RANDOM: u64 = 25;
    const AT_EXECFN: u64 = 31;
    dual_run("straight", &["7", "", "abc"], |record| {
        let leader = record.leader();
        let bias = record.landmarks.bias;
        let tracee = &*record.tracee;
        let stack = FirstStack::read(tracee, leader);
        let string = |address| stack_string(tracee, leader, address, stack.top);
        let mut lines = vec![
            format!(
                "the stack ends at {:#x} with {:#x}; rsp % 16 is {}",
                stack.top,
                stack_word(tracee, leader, stack.top - 8),
                stack.rsp % 16
            ),
            format!("argc {}, then {:#x}", stack.argv.len(), stack.argv_end),
            format!("environment variables: {}", stack.environment),
            format!("AT_NULL holds {}", stack.null),
        ];

        let shown = stack
            .auxiliary
            .iter()
            .map(|&(key, value)| match key {
                AT_PHDR | AT_ENTRY => format!("{key}: image+{:#x}", value.wrapping_sub(bias)),
                AT_PLATFORM => format!("{key}: {:?}", string(value)),
                AT_RANDOM | AT_EXECFN => format!("{key}: on the stack"),
                _ => format!("{key}: {value:#x}"),
            })
            .collect::<Vec<_>>();
        lines.push(format!("auxiliary vector: {shown:?}"));

        let executable = stack.entry(AT_EXECFN).map(string).unwrap_or_default();
        let mut strings = stack
            .argv
            .iter()
            .map(|&address| (address, string(address)))
            .collect::<Vec<_>>();
        strings.extend(
            stack
                .entry(AT_EXECFN)
                .map(|address| (address, executable.clone())),
        );

        let shown = strings
            .iter()
            .enumerate()
            .map(|(index, (address, text))| {
                let text = if index == stack.argv.len() {
                    let name = text.rsplit('/').next().unwrap_or_default();
                    format!("AT_EXECFN, naming {name}")
                } else if index == 0 && *text == executable {
                    "the executable".to_owned()
                } else {
                    format!("{text:?}")
                };

                let after = index.checked_sub(1).map(|before| {
                    let (start, text) = &strings[before];
                    address.wrapping_sub(start + text.len() as u64 + 1)
                });
                format!("{text} {after:?} after the last")
            })
            .collect::<Vec<_>>();
        lines.push(format!("strings: {shown:?}"));

        if let Some((address, text)) = strings.last() {
            let end = address + text.len() as u64 + 1;
            lines.push(format!(
                "the last string ends {} below the stack's end",
                stack.top - end
            ));
        }

        let places = [
            (
                "argv[0] rounded down",
                stack.argv.first().map(|&address| address & !15),
            ),
            ("AT_PLATFORM", stack.entry(AT_PLATFORM)),
            ("AT_RANDOM", stack.entry(AT_RANDOM)),
            ("the vector's end", Some(stack.end)),
        ];
        for pair in places.windows(2) {
            let distance = pair[0].1.zip(pair[1].1).map(|(upper, lower)| {
                let distance = upper.wrapping_sub(lower).cast_signed();
                if pair[1].0 == "the vector's end" && (0..16).contains(&distance) {
                    "under 16".to_owned()
                } else {
                    distance.to_string()
                }
            });
            lines.push(format!("{} is {distance:?} below {}", pair[1].0, pair[0].0));
        }

        for line in lines {
            record.note(line);
        }
    });
}

#[test]
fn k_trap_1_traps_report_their_kind_and_place() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let fib = record.landmarks.symbol("fib");
        let original = record.plant(fib);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.restore(leader, fib, original);
        record.rewind(leader);
        record.step(leader);
        record.wait(leader);
        record.stop(leader);

        let syscall = record.landmarks.syscall;
        let original = record.plant(syscall);
        record.resume(leader, None);
        record.wait(leader);
        record.restore(leader, syscall, original);
        record.rewind(leader);
        record.step(leader);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
    });
}

#[test]
fn k_exit_1_an_exiting_thread_stops_at_its_exit_event() {
    dual_run("straight", &["1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
        let result = record.tracee.registers(leader).map(drop);
        record.result("registers of a reaped thread", result);
    });
}

#[test]
fn k_exit_3_sigkill_ends_a_stopped_thread_through_its_exit_event() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let fib = record.landmarks.symbol("fib");
        record.plant(fib);
        record.resume(leader, None);
        record.wait(leader);
        let result = record.tracee.kill();
        record.result("kill", result);
        record.wait(leader);
        record.stop(leader);
        record.event_message(leader);
        let result = record.tracee.kill();
        record.result("kill again", result);
        let result = record.tracee.registers(leader).map(drop);
        record.result("registers at the exit event", result);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_exit_4_sigkill_leaves_a_thread_at_its_exit_event() {
    dual_run("straight", &["0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        record.wait(leader);
        let result = record.tracee.kill();
        record.result("kill", result);
        record.stop(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_exit_4_a_group_exit_ends_a_thread_at_its_own_exit_event() {
    for kill in [false, true] {
        dual_run("threads", &["2", "worker"], |record| {
            let leader = record.leader();
            record.set_options(leader);
            let share = record.landmarks.symbol("share");
            let original = record.plant(share);
            let mut workers = Vec::new();
            for _ in 0..2 {
                record.resume(leader, None);
                let worker = record.cloned(leader);
                record.wait(worker);
                workers.push(worker);
            }

            record.resume(leader, None);
            for &worker in &workers {
                record.resume(worker, None);
            }

            for &worker in &workers {
                record.wait(worker);
            }

            record.restore(workers[0], share, original);
            for &worker in &workers {
                record.rewind(worker);
            }

            record.resume(workers[0], None);
            record.wait(workers[0]);
            record.event_message(workers[0]);
            if kill {
                let result = record.tracee.kill();
                record.result("kill", result);
            } else {
                record.resume(workers[1], None);
            }

            record.wait(workers[1]);
            record.event_message(workers[1]);
            let result = record.tracee.registers(workers[0]).map(drop);
            record.result("registers of the first worker", result);
            let result = record.tracee.resume(workers[0], None, false);
            record.result("continue the first worker", result);
            record.wait(workers[0]);
            record.wait(leader);
            record.event_message(leader);
            record.resume(workers[1], None);
            record.wait(workers[1]);
            record.resume(leader, None);
            record.wait(leader);
        });
    }
}

#[test]
fn k_sig_1_a_tracer_stop_request_stops_the_thread() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let result = record.tracee.request_stop(leader);
        record.result("tgkill SIGSTOP", result);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
    });
}

#[test]
fn k_sig_1_a_tracer_stop_request_stops_a_running_thread() {
    dual_run("racing-exit", &["2", "4"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);

        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.resume(leader, None);
        record.wait(first);
        record.wait(second);
        record.resume(second, None);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP", result);
        record.wait(second);

        let signal = record
            .tracee
            .signal(second)
            .map(|(signal, code, sender, _)| (signal, code, sender.map(|tid| record.name_of(tid))));
        record.note(format!("siginfo of thread 2: {signal:?}"));
    });
}

#[test]
fn k_mem_1_ptrace_ignores_protections_but_not_holes() {
    dual_run("straight", &[], |record| {
        let leader = record.leader();
        let entry = record.landmarks.entry;
        let word = record
            .tracee
            .peek(leader, entry)
            .expect("read the entry point");
        let result = record.tracee.poke(leader, entry, word ^ 0xff);
        record.result("poke code", result);

        let changed = record
            .tracee
            .peek(leader, entry)
            .map(|changed| changed ^ word);
        record.note(format!("changed: {changed:?}"));

        let unmapped = record.landmarks.unmapped;
        let result = record.tracee.peek(leader, unmapped).map(drop);
        record.result("peek unmapped", result);
        let result = record.tracee.poke(leader, unmapped, 0);
        record.result("poke unmapped", result);
    });
}

#[test]
fn k_clone_1_a_new_thread_starts_stopped() {
    dual_run("threads", &["1", "main"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let child = record.cloned(leader);
        record.wait(child);
        record.stop(child);
        record.system_call(child);
        let stack = record
            .tracee
            .registers(child)
            .map(|(registers, _)| registers.general[RSP] - record.landmarks.symbol("stacks"));
        record.note(format!("stack of thread 1, from stacks: {stack:x?}"));
        let name = record.tracee.name(child);
        record.note(format!("name of thread 1: {name}"));

        record.step(leader);
        record.wait(leader);
        record.stop(leader);

        let returned = record
            .tracee
            .registers(leader)
            .map(|(registers, _)| registers.general[RAX] == u64::from(child.cast_unsigned()));
        record.note(format!("clone returned thread 1: {returned:?}"));

        record.resume(child, None);
        record.wait(child);
        record.stop(child);
        record.system_call(child);
        record.event_message(child);
        record.resume(child, None);
        record.wait(child);
        let group = record.tracee.thread_group(child);
        record.note(format!("thread 1 is gone: {}", group.is_none()));

        record.resume(leader, None);
        record.wait(leader);
        record.system_call(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_exit_2_exit_group_ends_every_sibling_through_its_exit_event() {
    dual_run("racing-exit", &["3", "2"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.resume(leader, None);
        let third = record.cloned(leader);

        record.wait(first);
        record.wait(second);
        record.wait(third);
        record.resume(third, None);
        record.resume(first, None);
        record.wait(first);
        record.stop(first);
        record.system_call(first);
        record.event_message(first);
        for thread in [leader, second] {
            record.wait(thread);
            record.stop(thread);
            record.event_message(thread);
        }

        record.wait(third);
        record.event_message(third);

        let leader_call = record
            .tracee
            .registers(leader)
            .map(|(registers, orig_rax)| {
                (
                    registers.general[RAX] == u64::from(third.cast_unsigned()),
                    orig_rax,
                )
            });
        record.note(format!(
            "leader returned thread 3, orig_rax: {leader_call:?}"
        ));
        record.resume(leader, None);
        record.has_report(leader);
        for thread in [first, second, third] {
            record.resume(thread, None);
            record.wait(thread);
        }

        record.wait(leader);
    });
}

#[test]
fn k_exit_3_sigkill_ends_every_thread_through_its_exit_event() {
    dual_run("threads", &["3", "main"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let mut workers = Vec::new();
        for _ in 0..3 {
            record.resume(leader, None);
            let worker = record.cloned(leader);
            record.wait(worker);
            workers.push(worker);
        }

        let result = record.tracee.kill();
        record.result("kill", result);
        for &thread in std::iter::once(&leader).chain(&workers) {
            record.wait(thread);
            record.event_message(thread);
        }

        record.resume(leader, None);
        record.has_report(leader);
        for &worker in &workers {
            record.resume(worker, None);
            record.wait(worker);
        }

        record.wait(leader);
    });
}

#[test]
fn k_exit_3_sigkill_after_a_fork_event_finds_the_call_returned() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        record.resume(leader, None);
        let result = record.tracee.kill();
        record.result("kill", result);
        record.wait(leader);
        record.event_message(leader);

        let inside = record
            .tracee
            .registers(leader)
            .map(|(registers, _)| registers.general[RAX] == ENOSYS);
        record.note(format!("rax of leader is -ENOSYS: {inside:?}"));
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_exit_5_a_leader_exiting_alone_waits_for_its_threads() {
    dual_run("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.event_message(leader);
        record.resume(leader, None);

        let started = std::time::Instant::now();
        while record
            .tracee
            .maps(leader)
            .is_some_and(|maps| !maps.is_empty())
        {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            std::thread::yield_now();
        }

        record.note("the leader's maps read empty");

        let result = record.tracee.registers(leader).map(drop);
        record.result("registers of the zombie leader", result);
        let result = record.tracee.request_stop(leader);
        record.result("tgkill SIGSTOP to the zombie leader", result);
        let group = record.tracee.thread_group(leader);
        record.note(format!("the zombie leader is listed: {}", group.is_some()));
        record.has_report(leader);
        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
        record.resume(worker, None);
        record.wait(worker);
        record.wait(leader);
    });
}

#[test]
fn k_exit_5_a_zombie_cannot_be_seized() {
    dual_run("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.detach(leader, None);
        let started = std::time::Instant::now();
        while !record.tracee.is_zombie(leader) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            record.tracee.pass_time();
        }

        record.seize(leader);
        record.seize(worker);
        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
        record.resume(worker, None);
        record.wait(worker);
    });
}

#[test]
fn k_exit_6_the_last_thread_to_begin_exiting_decides_the_status() {
    for leader_last in [false, true] {
        dual_run("threads", &["1", "leader"], |record| {
            let leader = record.leader();
            record.set_options(leader);
            record.resume(leader, None);
            let worker = record.cloned(leader);
            record.wait(worker);
            if !leader_last {
                record.resume(leader, None);
                record.wait(leader);
                record.event_message(leader);
            }

            let rt_exit = record.landmarks.symbol("rt_exit");
            let original = record.plant(rt_exit);

            record.resume(worker, None);
            record.wait(worker);
            record.restore(worker, rt_exit, original);
            record.rewind(worker);

            let (mut registers, _) = record.tracee.registers(worker).expect("registers");
            registers.general[RDI] = 5;
            let result = record.tracee.set_registers(worker, &registers);
            record.result("exit with 5", result);
            record.resume(worker, None);
            record.wait(worker);
            record.event_message(worker);
            record.resume(worker, None);
            record.wait(worker);

            if leader_last {
                record.resume(leader, None);
                record.wait(leader);
                record.event_message(leader);
            }

            record.resume(leader, None);
            record.wait(leader);
        });
    }
}

#[test]
fn k_exit_6_a_group_exit_decides_the_status_of_unreaped_threads() {
    dual_run("threads", &["2", "worker"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let share = record.landmarks.symbol("share");
        let original = record.plant(share);
        let mut workers = Vec::new();
        for _ in 0..2 {
            record.resume(leader, None);
            let worker = record.cloned(leader);
            record.wait(worker);
            workers.push(worker);
        }

        record.resume(leader, None);

        for &worker in &workers {
            record.resume(worker, None);
        }

        for &worker in &workers {
            record.wait(worker);
        }

        record.restore(workers[0], share, original);
        for &worker in &workers {
            record.rewind(worker);
        }

        record.resume(workers[0], None);
        record.wait(workers[0]);
        record.event_message(workers[0]);
        record.resume(workers[0], None);

        record.resume(workers[1], None);
        record.wait(workers[1]);
        record.event_message(workers[1]);
        record.wait(leader);
        record.event_message(leader);
        record.wait(workers[0]);
        record.resume(workers[1], None);
        record.wait(workers[1]);
        record.resume(leader, None);
        record.wait(leader);
    });
}

const WRITE_8: u64 = 1 | 1 << 16 | 2 << 18;

const DR6_IDLE: u64 = 0xffff_0ff0;

const fn records_hit(dr6: u64) -> bool {
    dr6 & 0xf != 0
}

#[test]
fn k_dr_1_a_watched_store_traps_after_the_instruction() {
    dual_run("stores", &["2", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let counter = record.landmarks.symbol("counter");
        let _ = record.debug(leader, 6);
        record.watch(leader, counter, WRITE_8);
        let _ = record.debug(leader, 0);
        let _ = record.debug(leader, 7);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.rip(leader);
        let _ = record.debug(leader, 6);
        let bump = record.landmarks.symbol("bump");
        record.plant(bump);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        let _ = record.debug(leader, 6);
    });
}

#[test]
fn k_dr_2_a_step_over_a_watched_store_reports_both() {
    dual_run("stores", &["2", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let counter = record.landmarks.symbol("counter");
        record.watch(leader, counter, WRITE_8);
        record.resume(leader, None);
        record.wait(leader);
        record.set_debug(leader, 6, DR6_IDLE);

        let mut steps = 0;

        loop {
            record.tracee.resume(leader, None, true).expect("step");
            let status = record.tracee.wait(leader);
            steps += 1;
            let dr6 = record.tracee.read_debug(leader, 6).expect("read DR6");
            if records_hit(dr6) || steps == 10_000 {
                let status = status
                    .to_string()
                    .replacen(&leader.to_string(), "leader", 1);
                record.note(format!("steps to the store: {steps}, last {status}"));
                break;
            }
        }

        record.stop(leader);
        record.rip(leader);
        let _ = record.debug(leader, 6);
    });
}

#[test]
fn k_dr_3_a_new_thread_starts_unarmed() {
    dual_run("stores", &["1", "1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let shared = record.landmarks.symbol("shared");
        record.watch(leader, shared, WRITE_8);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.stop(worker);
        for index in [0, 6, 7] {
            let _ = record.debug(worker, index);
        }

        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
    });
}

#[test]
fn k_dr_4_slots_are_reserved_and_dr7_is_transactional() {
    dual_run("stores", &["1", "0"], |record| {
        let leader = record.leader();
        let counter = record.landmarks.symbol("counter");
        let steady = record.landmarks.symbol("steady");
        record.tracee.hold_debug_slots(leader, 3, counter);
        record.set_debug(leader, 0, counter);
        record.set_debug(leader, 1, steady);
        record.set_debug(leader, 0, counter + 4);
        record.set_debug(leader, 7, WRITE_8);
        let _ = record.debug(leader, 7);
        let _ = record.debug(leader, 0);
        record.set_debug(leader, 0, 0x7fff_ffff_f000);
        record.set_debug(leader, 5, 0);
    });
}

#[test]
fn k_dr_5_every_store_traps_but_ptrace_writes() {
    dual_run("stores", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let steady = record.landmarks.symbol("steady");
        record.watch(leader, steady, WRITE_8);
        let value = record.tracee.peek(leader, steady).expect("read steady");
        let result = record.tracee.poke(leader, steady, value);
        record.result("poke steady", result);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.rip(leader);
        let pattern = record.landmarks.symbol("pattern");
        record.watch(leader, pattern, WRITE_8);
        for _ in 0..3 {
            record.resume(leader, None);
            record.wait(leader);
            record.stop(leader);
            record.rip(leader);
        }
    });
}

#[test]
fn k_fork_1_a_fork_child_starts_stopped_in_a_copy_of_its_parent() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let child_code = record.landmarks.symbol("child");
        let original = record.plant(child_code);
        let unmapped = record.landmarks.unmapped;
        record.watch(leader, unmapped, WRITE_8);
        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        record.stop(child);
        record.system_call(child);
        let _ = record.debug(child, 0);
        let _ = record.debug(child, 7);
        let byte = record
            .tracee
            .peek(child, child_code)
            .map(|word| word & 0xff);
        record.note(format!("the child's byte at child: {byte:x?}"));

        record.resume(child, None);
        record.wait(child);
        record.stop(child);
        record.restore(child, child_code, original);
        record.rewind(child);

        let byte = record
            .tracee
            .peek(leader, child_code)
            .map(|word| word & 0xff);
        record.note(format!("the parent's byte at child: {byte:x?}"));
        record.resume(child, None);
        record.wait(child);
        record.event_message(child);
        record.resume(child, None);
        record.wait(child);

        record.restore(leader, child_code, original);
        record.set_debug(leader, 7, 0);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.event_message(leader);
        record.children_of(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_fork_2_a_detached_child_runs_untraced() {
    dual_run("fork", &["2", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let child_code = record.landmarks.symbol("child");
        let original = record.plant(child_code);
        record.resume(leader, None);
        let first = record.forked(leader);
        record.wait(first);
        record.restore(first, child_code, original);
        record.detach(first, None);
        let result = record.tracee.registers(first).map(drop);
        record.result("registers of a detached child", result);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));

        let second = record.forked(leader);
        record.wait(second);
        record.detach(second, None);
        record.restore(leader, child_code, original);
        record.resume(leader, None);
        record.wait(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.event_message(leader);
        record.children_of(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_fork_3_an_orphan_sees_its_parent_change() {
    dual_run("fork", &["1", "1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let waited = record.forked(leader);
        record.wait(waited);
        record.detach(waited, None);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        let orphan = record.forked(leader);
        record.wait(orphan);
        record.resume(orphan, None);
        record.resume(leader, None);
        record.wait(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.wait(orphan);
        record.stop(orphan);
        record.event_message(orphan);
        record.resume(orphan, None);
        record.wait(orphan);
    });
}

#[test]
fn k_seize_1_a_seized_thread_runs_on_and_its_children_start_in_an_event_stop() {
    dual_run("fork", &["1", "2"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);

        let first = record.forked(leader);
        record.wait(first);
        record.detach(leader, None);
        record.seize(leader);
        record.interrupt(leader);
        record.wait(leader);
        record.signal(leader);
        record.event_message(leader);
        record.detach(first, None);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        let worker = record.cloned(leader);
        record.wait(worker);
        record.signal(worker);
        record.event_message(worker);
        record.resume(worker, None);
        let second = record.forked(worker);
        record.wait(second);
        record.signal(second);
        record.event_message(second);
    });
}

#[test]
fn k_int_1_an_interrupt_stops_a_seized_thread() {
    dual_run("racing-exit", &["2", "4"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);

        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.resume(leader, None);
        record.wait(first);
        record.wait(second);
        record.detach(second, None);
        record.seize(second);
        record.interrupt(second);
        record.wait(second);
        record.signal(second);

        record.interrupt(second);
        record.resume(second, None);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP", result);
        record.wait(second);
        record.signal(second);
        record.resume(second, None);
        record.wait(second);
        record.signal(second);
    });
}

#[test]
fn k_int_2_an_interrupt_reaches_an_exiting_thread_until_it_is_reaped() {
    dual_run("racing-exit", &["2", "4"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let first = record.cloned(leader);
        record.resume(leader, None);
        let second = record.cloned(leader);
        record.wait(first);
        record.wait(second);
        record.detach(second, None);
        record.seize(second);

        record.resume(first, None);
        record.wait(second);
        record.event_message(second);
        record.interrupt(second);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP at the exit event", result);
        record.resume(second, None);
        while !record.tracee.has_report(second) {
            std::thread::yield_now();
        }

        record.interrupt(second);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP to a zombie", result);
        record.wait(second);
        record.interrupt(second);
        let result = record.tracee.request_stop(second);
        record.result("tgkill SIGSTOP once reaped", result);
    });
}

#[test]
fn k_wait_3_a_tracers_exit_releases_a_traced_zombie() {
    dual_run_then_exit("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.resume(leader, None);
        let started = std::time::Instant::now();
        while !record.tracee.is_zombie(leader) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            record.tracee.pass_time();
        }

        record.detach(leader, None);
    });
}

#[test]
fn k_exit_5_reaping_the_last_thread_ends_an_untraced_leaders_process() {
    dual_run_then_exit("threads", &["1", "leader"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let worker = record.cloned(leader);
        record.wait(worker);
        record.detach(leader, None);
        let started = std::time::Instant::now();
        while !record.tracee.is_zombie(leader) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the leader never finished exiting"
            );
            record.tracee.pass_time();
        }

        record.resume(worker, None);
        record.wait(worker);
        record.event_message(worker);
        record.resume(worker, None);
        record.wait(worker);
    });
}

#[test]
fn k_wait_3_a_tracers_exit_releases_an_exiting_thread() {
    dual_run_then_exit("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let first = record.forked(leader);
        record.wait(first);
        record.detach(leader, None);
        record.seize(leader);
        record.interrupt(leader);
        record.wait(leader);
        record.detach(first, None);
        let result = record.tracee.kill();
        record.result("SIGKILL", result);
    });
}

#[test]
fn k_stop_1_a_detached_child_takes_its_sigstop_untraced() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        record.rip(child);
        record.request_stop_of(child, child);
        record.detach(child, None);
        record.job_stopped(child);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));

        record.seize(child);
        record.wait(child);
        record.signal(child);
        record.rip(child);
        record.detach(child, None);
        record.job_stopped(child);

        record.continue_process(child);
        record.wait(leader);
        record.signal(leader);
    });
}

#[test]
fn k_stop_3_sigcont_ends_a_stop_and_interrupts_a_seized_thread() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        record.request_stop_of(child, child);
        record.detach(child, None);
        record.job_stopped(child);
        record.seize(child);
        record.wait(child);
        record.interrupt(child);

        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));

        record.continue_process(child);
        record.resume(child, None);
        record.wait(child);
        record.signal(child);
        record.wait(leader);
        record.signal(leader);
        record.resume(child, None);
        record.wait(child);
        record.signal(child);

        record.request_stop_of(child, child);
        record.continue_process(child);
        record.resume(child, None);
        record.wait(child);
        record.signal(child);
        record.resume(child, None);
        record.wait(child);
        record.signal(child);

        record.continue_process(child);
        record.request_stop_of(child, child);
        record.resume(child, None);
        record.wait(child);
        record.signal(child);
        record.resume(child, None);
        record.wait(child);
        record.signal(child);

        record.resume(child, None);
        record.wait(child);
        record.event_message(child);
        record.resume(child, None);
        record.wait(child);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_seize_2_a_thread_answers_only_its_own_tracer() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        record.as_another_tracer(leader);
        record.as_another_tracer(child);
    });
}

#[test]
fn k_perf_1_a_sample_holds_the_threads_registers_chain_and_stack() {
    dual_run("frames", &["1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        let leaf = record.landmarks.symbol("leaf");
        record.tracee.sample_at(leaf, 65528);
        let original = record.plant(leaf);
        record.resume(leader, None);
        let orphan = record.cloned(leader);
        record.wait(orphan);
        record.resume(leader, None);
        record.wait(leader);
        record.sampled_at_stop(leader);
        record.restore(leader, leaf, original);
        record.rewind(leader);

        let spin = record.landmarks.symbol("rt_yield");
        let original = record.plant(spin);
        record.resume(leader, None);
        record.wait(leader);
        record.samples();
        record.resume(orphan, None);
        record.wait(orphan);
        record.samples();
        record.resume(orphan, None);
        record.wait(orphan);
        record.restore(leader, spin, original);
        record.rewind(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_sys_1_a_thread_stops_at_a_system_calls_entry_and_exit() {
    dual_run("straight", &["1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.call(leader);

        for _ in 0..3 {
            record.resume_to_call(leader, None);
            record.wait(leader);
            record.stop(leader);
            record.system_call(leader);
            record.event_message(leader);
            record.call(leader);
        }

        record.resume(leader, None);
        record.wait(leader);
        record.system_call(leader);
        record.event_message(leader);
        record.call(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_sys_3_event_stops_fall_between_a_calls_entry_and_exit() {
    dual_run("threads", &["1", "main"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume_to_call(leader, None);
        record.wait(leader);
        record.call(leader);
        record.resume_to_call(leader, None);
        let child = record.cloned(leader);
        record.call(leader);
        record.wait(child);
        record.stop(child);
        record.resume_to_call(leader, None);
        record.wait(leader);
        record.stop(leader);

        let returned = record
            .tracee
            .registers(leader)
            .map(|(registers, _)| registers.general[RAX] == u64::from(child.cast_unsigned()));
        record.note(format!("clone returned thread 1: {returned:?}"));
        record.call(leader);

        record.resume_to_call(child, None);
        record.wait(child);
        record.stop(child);
        record.system_call(child);
        record.call(child);
        record.resume_to_call(child, None);
        record.wait(child);
        record.event_message(child);
        record.call(child);
        record.resume(child, None);
        record.wait(child);
        record.resume(leader, None);
        record.wait(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_sys_4_a_step_from_a_calls_entry_traps_after_the_call() {
    dual_run("straight", &["1"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume_to_call(leader, None);
        record.wait(leader);
        record.step(leader);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.call(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_sys_5_sigkill_at_a_calls_entry_skips_the_call() {
    for passed in [false, true] {
        dual_run("straight", &["1"], |record| {
            let leader = record.leader();
            record.set_options(leader);
            record.resume_to_call(leader, None);
            record.wait(leader);
            if passed {
                record.resume_to_call(leader, Some(libc::SIGKILL));
            } else {
                let result = record.tracee.kill();
                record.result("kill", result);
            }

            record.wait(leader);
            record.system_call(leader);
            record.event_message(leader);
            record.call(leader);
            record.resume(leader, None);
            record.wait(leader);
        });
    }
}

#[test]
fn k_sys_6_a_thread_detached_at_a_call_makes_it() {
    for stops in [1, 2] {
        dual_run_then_exit("straight", &["1"], |record| {
            let leader = record.leader();
            record.set_options(leader);
            for _ in 0..stops {
                record.resume_to_call(leader, None);
                record.wait(leader);
            }

            record.detach(leader, None);
        });
    }
}

#[test]
fn k_sys_7_a_fork_entered_with_a_signal_pending_restarts() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        run_to_entry(record, leader, SYS_FORK);
        record.note("at the fork's entry");
        record.system_call(leader);

        let result = record.tracee.request_stop(leader);
        record.result("tgkill SIGSTOP", result);
        restart(record, leader);

        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        record.detach(child, None);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_sys_7_a_clone_entered_with_a_signal_pending_restarts() {
    dual_run("threads", &["1", "main"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        run_to_entry(record, leader, SYS_CLONE);
        record.note("at the clone's entry");

        let result = record.tracee.request_stop(leader);
        record.result("tgkill SIGSTOP", result);
        restart(record, leader);

        record.resume(leader, None);
        let child = record.cloned(leader);
        record.wait(child);

        record.resume(child, None);
        record.wait(child);
        record.event_message(child);
        record.resume(child, None);
        record.wait(child);
        record.resume(leader, None);
        record.wait(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}

#[test]
fn k_sys_7_a_fork_entered_with_an_interrupt_pending_restarts() {
    dual_run("fork", &["2", "0"], |record| {
        let leader = seized_at_second_fork(record);
        record.interrupt(leader);
        record.resume_to_call(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.call(leader);
        record.resume_to_call(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.call(leader);
        record.resume(leader, None);
        finish_second_fork(record, leader);
    });
}

fn seized_at_second_fork(record: &mut Record<'_>) -> Tid {
    let leader = record.leader();
    record.set_options(leader);
    record.resume(leader, None);

    let first = record.forked(leader);
    record.wait(first);
    record.detach(leader, None);
    record.seize(leader);
    record.interrupt(leader);
    record.wait(leader);
    record.signal(leader);
    record.detach(first, None);
    run_to_entry(record, leader, SYS_FORK);
    record.note("at the second fork's entry");
    record.system_call(leader);
    leader
}

fn finish_second_fork(record: &mut Record<'_>, leader: Tid) {
    let second = record.forked(leader);
    release_second_child(record, second);
    record.resume(leader, None);
    record.wait(leader);
    record.signal(leader);
    record.resume(leader, Some(libc::SIGCHLD));
    record.wait(leader);
    record.event_message(leader);
    record.resume(leader, None);
    record.wait(leader);
}

fn release_second_child(record: &mut Record<'_>, second: Tid) {
    record.wait(second);
    record.signal(second);
    record.detach(second, None);
}

#[test]
fn k_trap_2_an_interrupt_stops_a_thread_ahead_of_its_trap() {
    for detach in [false, true] {
        dual_run_then_exit("fork", &["2", "0"], |record| {
            let leader = seized_at_second_fork(record);
            record.resume(leader, None);
            let second = record.forked(leader);
            record.interrupt(leader);
            record.step(leader);
            record.wait(leader);
            record.stop(leader);

            let returned = record
                .tracee
                .registers(leader)
                .map(|(registers, _)| registers.general[RAX] == u64::from(second.cast_unsigned()));
            record.note(format!("fork returned child 2: {returned:?}"));
            if !detach {
                for _ in 0..2 {
                    record.step(leader);
                    record.wait(leader);
                    record.stop(leader);
                }
            }

            release_second_child(record, second);
            record.detach(leader, None);
        });
    }
}

#[test]
fn k_trap_2_a_step_into_a_restarting_fork_meets_the_interrupt_first() {
    dual_run("fork", &["2", "0"], |record| {
        let leader = seized_at_second_fork(record);
        record.interrupt(leader);
        record.step(leader);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.system_call(leader);
        record.resume(leader, None);
        finish_second_fork(record, leader);
    });
}

#[test]
fn k_trap_2_a_trap_goes_before_a_pending_stop() {
    dual_run("fork", &["1", "0"], |record| {
        let leader = record.leader();
        record.set_options(leader);
        record.resume(leader, None);
        let child = record.forked(leader);
        record.wait(child);
        let result = record.tracee.request_stop(leader);
        record.result("tgkill SIGSTOP", result);
        record.step(leader);
        record.wait(leader);
        record.stop(leader);
        record.resume(leader, None);
        record.wait(leader);
        record.stop(leader);
        record.detach(child, None);
        record.resume(leader, None);
        record.wait(leader);
        record.signal(leader);
        record.resume(leader, Some(libc::SIGCHLD));
        record.wait(leader);
        record.event_message(leader);
        record.resume(leader, None);
        record.wait(leader);
    });
}
