use std::sync::Arc;

use super::signals::SIGTRAP;
use super::{Kernel, State, StopKind, Tgid, Tid};
use crate::sim::corpus::Corpus;
use crate::sim::cpu::{RDI, RSI};

const TRACER: i32 = 100;

const SYS_WRITE: u64 = 1;
const SYS_WAIT4: u64 = 61;

fn launch(program: &str, arguments: &[&str]) -> (Kernel, Tid) {
    let corpus = Corpus::load().expect("load the golden corpus");
    let variant = &corpus
        .programs
        .iter()
        .find(|candidate| candidate.name == program)
        .expect("the golden program")
        .variants[0];
    let arguments = arguments
        .iter()
        .map(|&argument| argument.to_owned())
        .collect::<Vec<_>>();

    let mut kernel = Kernel::new(TRACER);
    let pid = kernel.spawn(
        Arc::clone(&variant.image),
        &variant.path,
        &arguments,
        [0; 16],
    );

    assert!(run_until_report(&mut kernel, pid));
    (kernel, pid)
}

fn run_until_report(kernel: &mut Kernel, tid: Tid) -> bool {
    for _ in 0..100_000 {
        if kernel.gap.is_some() {
            return false;
        }

        if kernel.collect(tid).is_some() {
            return true;
        }

        let runnable = kernel.runnable().collect::<Vec<_>>();
        assert!(!runnable.is_empty(), "{tid} can never report");
        for thread in runnable {
            kernel.run(thread, 16);
        }
    }

    panic!("{tid} never reported");
}

fn to_entry(kernel: &mut Kernel, tid: Tid, call: u64, ready: impl Fn(&Kernel) -> bool) {
    loop {
        kernel.resume_to_call(tid, None).expect("resume to a call");
        assert!(run_until_report(kernel, tid), "a gap: {:?}", kernel.gap);
        let (_, orig_rax) = kernel
            .get_registers_and_call(tid)
            .expect("registers at a stop");
        let entry = kernel.event_message(tid) == Ok(1);
        if entry && orig_rax == call && ready(kernel) {
            return;
        }
    }
}

#[test]
fn a_write_to_another_descriptor_is_a_gap() {
    let (mut kernel, leader) = launch("straight", &["1"]);
    to_entry(&mut kernel, leader, SYS_WRITE, |_| true);

    let (mut registers, _) = kernel
        .get_registers_and_call(leader)
        .expect("registers at the entry");
    registers.general[RDI] = 3;
    kernel
        .set_registers(leader, registers)
        .expect("set registers");

    kernel.resume_to_call(leader, None).expect("resume");
    assert!(!run_until_report(&mut kernel, leader));

    let gap = kernel.gap.expect("a gap").0;
    assert!(gap.contains("write to descriptor 3"), "{gap}");
}

#[test]
fn a_wait_that_cannot_store_the_status_is_a_gap() {
    let (mut kernel, leader) = launch("fork", &["1", "0"]);
    to_entry(&mut kernel, leader, SYS_WAIT4, |kernel| {
        kernel
            .zombies
            .keys()
            .any(|&process| process != Tgid::led_by(leader))
    });

    let (mut registers, _) = kernel
        .get_registers_and_call(leader)
        .expect("registers at the entry");
    registers.general[RSI] = 8;
    kernel
        .set_registers(leader, registers)
        .expect("set registers");

    kernel.resume_to_call(leader, None).expect("resume");
    assert!(!run_until_report(&mut kernel, leader));

    let gap = kernel.gap.expect("a gap").0;
    assert!(gap.contains("wait4 storing"), "{gap}");
}

#[test]
fn a_thread_that_raised_a_trap_is_not_in_user_code() {
    let (mut kernel, leader) = launch("straight", &["1"]);
    let (registers, _) = kernel
        .get_registers_and_call(leader)
        .expect("registers at the first stop");
    let entry = registers.rip;
    let word = kernel.peek(leader, entry).expect("read code");

    kernel
        .poke(leader, entry, (word & !0xff) | 0xcc)
        .expect("plant a trap");

    kernel.resume(leader, None, false).expect("resume");
    assert!(kernel.threads[&leader].in_user_code());
    kernel.run(leader, 16);
    let thread = &kernel.threads[&leader];
    assert_eq!(thread.state, State::Running);
    assert!(thread.pending.contains(SIGTRAP));
    assert!(!thread.in_user_code());

    kernel.run(leader, 16);
    let thread = &kernel.threads[&leader];
    assert!(matches!(
        thread.state,
        State::Stopped {
            kind: StopKind::Signal(SIGTRAP),
            ..
        }
    ));
    assert_eq!(thread.registers.rip, entry + 1);
}
