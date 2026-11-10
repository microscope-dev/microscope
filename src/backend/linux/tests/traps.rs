//! What each thread owes the debugger's traps, through every event that
//! changes it: a thread stopped at a hit, at several hits, at a hit whose
//! breakpoint was removed, stepping over a trap, running a signal handler
//! that interrupted that step, and running while a vfork child shares the
//! traps' memory.
//!
//! Every phase is reached through the controller's messages and wait
//! statuses, and the run's fake kernel answers what the controller asked of
//! it, as in the run-phase table. So the table holds however the controller
//! keeps a trap's state.

use super::lifecycle::{Reader, reader, send};
use super::run_phase::{ADDED_SITE, OTHER, Run, running, stopped};
use super::*;

/// The thread each phase is about: it hits, steps, takes signals and
/// vforks. It is not the leader, so its exit ends only itself.
const THREAD: Pid = OTHER;

/// The leader, the thread's one sibling.
const SIBLING: Pid = Pid::from_raw(5000);

/// Where a jump moves the thread: no site's address.
const ELSEWHERE: u64 = 0x60;

/// The vfork child, which the controller never traces as a thread.
const VFORK_CHILD: Pid = Pid::from_raw(6000);

impl Run {
    /// `pid` executes the trap at `site`.
    fn hit_at(&mut self, pid: Pid, site: u64) {
        self.harness.trace().set_pc(pid, site + 1);
        self.signal_info(pid, libc::SI_KERNEL);
        self.program_stop(WaitEvent::Stopped(pid, Signal::SIGTRAP));
    }

    /// `pid` stops at the ptrace event `event`, whose message names
    /// [`VFORK_CHILD`].
    fn event(&mut self, pid: Pid, event: libc::c_int) {
        self.harness
            .trace()
            .clone
            .replace(Some((VFORK_CHILD, VFORK_CHILD)));
        self.wait(WaitEvent::PtraceEvent(pid, Signal::SIGTRAP, event));
        self.harness.trace().clone.replace(None);
    }

    fn jump(&mut self, pid: Pid) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        self.harness.trace().set_pc(pid, ELSEWHERE);
        let process_id = self.process();
        let stop_id = self.stop_id();
        send(
            &mut self.harness,
            Request::Jump {
                process_id,
                stop_id,
                context: crate::ExecutionContext::Thread(debug_thread_id(pid)),
                spec: BreakpointSpec::Address(VirtualAddress::new(ELSEWHERE)),
                reply,
            },
        );
        reader(receiver)
    }

    /// Whether `pid` runs freely, not stepping.
    fn runs(&self, pid: Pid) -> bool {
        self.kernel.running.contains(&pid) && !self.kernel.stepping.contains(&pid)
    }

    /// Every breakpoint's hits so far.
    fn hits(&self) -> String {
        self.harness
            .controller
            .breakpoints
            .iter()
            .map(|breakpoint| format!("b{}:{}", breakpoint.id, breakpoint.hit_count))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A run stopped at a pause with the breakpoint at [`HIT_SITE`], which
/// declines every hit when `declining`, and a breakpoint at [`ADDED_SITE`]
/// that stops every hit, then continued.
fn running_with_second(declining: bool) -> Run {
    let mut run = stopped(declining);
    let mut added = run.add(ADDED_SITE, None);
    run.settle(true);
    assert!(added().starts_with("ok"), "the setup's second breakpoint");
    let mut resumed = run.resume();
    run.settle(true);
    assert_eq!(resumed(), "ok", "the setup's continue");
    run
}

/// Builds the trap phase `name`. The breakpoint at [`HIT_SITE`] stops every
/// hit, except in the phases that step over a trap, where it declines them.
fn phase(name: &str) -> Run {
    let mut run = match name {
        "at a hit" => {
            let mut run = running(false);
            run.hit_at(THREAD, HIT_SITE);
            run.settle(true);
            run
        }
        "at a co-hit" => {
            let mut run = running(false);
            run.hit_at(THREAD, HIT_SITE);
            run.hit_at(SIBLING, HIT_SITE);
            run.settle(true);
            run
        }
        "at hits of two sites" => {
            let mut run = running_with_second(false);
            run.hit_at(THREAD, HIT_SITE);
            run.hit_at(SIBLING, ADDED_SITE);
            run.settle(true);
            run
        }
        "at a hit since removed" => {
            let mut run = phase("at a hit");
            let mut removed = run.remove();
            run.settle(true);
            assert!(removed().starts_with("ok"), "the setup's removal");
            run
        }
        "stepping over a trap" => {
            let mut run = running_with_second(true);
            run.hit_at(THREAD, HIT_SITE);
            run.settle(false);
            run
        }
        "stepping over a co-hit" => {
            let mut run = running_with_second(true);
            run.hit_at(THREAD, HIT_SITE);
            run.hit_at(SIBLING, HIT_SITE);
            run.settle(false);
            run
        }
        "stopped by a signal in a step over a trap" => {
            let mut run = phase("stepping over a trap");
            run.signal(THREAD, libc::SIGUSR1);
            run.settle(false);
            run
        }
        "running a handler that interrupted a step over a trap" => {
            let mut run = phase("stepping over a trap");
            run.signal(THREAD, libc::SIGALRM);
            run.settle(false);
            run
        }
        "in a vfork window" => {
            let mut run = running(false);
            run.event(THREAD, libc::PTRACE_EVENT_VFORK);
            run.settle(false);
            run
        }
        "in a vfork window opened during a step over a trap" => {
            let mut run = running(true);
            run.hit_at(SIBLING, HIT_SITE);
            run.settle(false);
            run.event(THREAD, libc::PTRACE_EVENT_VFORK);
            run.settle(true);
            run
        }
        _ => unreachable!("no phase {name}"),
    };

    run.calls.clear();
    run.harness.published();
    run
}

const PHASES: [&str; 10] = [
    "at a hit",
    "at a co-hit",
    "at hits of two sites",
    "at a hit since removed",
    "stepping over a trap",
    "stepping over a co-hit",
    "stopped by a signal in a step over a trap",
    "running a handler that interrupted a step over a trap",
    "in a vfork window",
    "in a vfork window opened during a step over a trap",
];

const EVENTS: [&str; 12] = [
    "continue",
    "pause",
    "remove",
    "add at the hit",
    "jump",
    "jump, then add at the hit",
    "thread traps again",
    "thread hits the second site",
    "thread gets a signal",
    "sibling hits",
    "thread exits",
    "vfork done",
];

/// One event in one phase: what each request was answered, where the run
/// settles once the kernel has answered everything asked of it, and how
/// many hits each breakpoint counted. An edit or jump at a stop is followed
/// by a continue, and an event in a vfork window by the window's end, to
/// show what each thread does next.
fn outcome(name: &str, event: &str) -> Option<String> {
    let mut run = phase(name);
    let stopped = run.is_stopped();
    let thread_runs = run.runs(THREAD);
    let vforked = name.starts_with("in a vfork window");
    let none = || -> Reader { Box::new(|| "-".to_owned()) };
    let mut asked: Reader = match event {
        "continue" => run.resume(),
        "pause" => run.pause(),
        "remove" => run.remove(),
        "add at the hit" => run.add(HIT_SITE, None),
        "jump" if stopped => run.jump(THREAD),
        "jump, then add at the hit" if stopped => {
            let mut jumped = run.jump(THREAD);
            run.settle(true);
            assert!(jumped().starts_with("ok"), "the jump");
            run.add(HIT_SITE, Some(">=1"))
        }
        "thread traps again" if thread_runs && !vforked => {
            run.hit_at(THREAD, HIT_SITE);
            none()
        }
        "thread hits the second site" if thread_runs && !vforked => {
            run.hit_at(THREAD, ADDED_SITE);
            none()
        }
        // A thread waiting for its vfork child takes no signal until the
        // child execs or exits.
        "thread gets a signal" if thread_runs && !vforked => {
            run.signal(THREAD, libc::SIGALRM);
            none()
        }
        "sibling hits" if run.runs(SIBLING) => {
            run.hit_at(SIBLING, HIT_SITE);
            none()
        }
        "thread exits" if thread_runs => {
            run.exit(THREAD);
            none()
        }
        "vfork done" if vforked && thread_runs => {
            run.event(THREAD, libc::PTRACE_EVENT_VFORK_DONE);
            none()
        }
        _ => return None,
    };
    run.settle(true);
    let mut replies = vec![asked()];

    let edited = matches!(
        event,
        "remove" | "add at the hit" | "jump" | "jump, then add at the hit"
    );
    if edited && stopped && run.is_stopped() {
        let mut resumed = run.resume();
        run.settle(true);
        replies.push(format!("continue {}", resumed()));
    }
    if vforked && run.runs(THREAD) && !matches!(event, "vfork done" | "thread exits") {
        run.event(THREAD, libc::PTRACE_EVENT_VFORK_DONE);
        run.settle(true);
        replies.push("vfork done".to_owned());
    }

    Some(format!(
        "{name} / {event}: {} -> {} ({})",
        replies.join(", "),
        run.describe(),
        run.hits()
    ))
}

/// Each run-control request and thread event, in each phase of a thread's
/// relation to a trap, is answered and settles as it always has: which stop
/// is published, which traps are lifted and put back, which threads step
/// over a trap and in what order, which run alone, and how many hits each
/// breakpoint counted.
#[test]
fn each_trap_event_settles_the_same_in_each_phase() {
    let actual: Vec<String> = PHASES
        .iter()
        .flat_map(|name| EVENTS.iter().filter_map(move |event| outcome(name, event)))
        .collect();

    let expected = [
        "at a hit / continue: ok -> running e3 [continued e3] {remove_site 0x40, step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None} (b1:1)",
        "at a hit / pause: AlreadyStopped -> stopped hit b1#1 by 5001 [] {} (b1:1)",
        "at a hit / remove: ok, continue ok -> running e3 [breakpoints, continued e3] {remove_site 0x40, continue 5000 None, continue 5001 None} ()",
        "at a hit / add at the hit: ok, continue ok -> running e3 [continued e3] {remove_site 0x40, step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None} (b1:1)",
        "at a hit / jump: ok, continue ok -> running e4 [stopped e3 Jump by 5001, continued e4] {set_registers 5001 rip=0x60, continue 5000 None, continue 5001 None} (b1:1)",
        "at a hit / jump, then add at the hit: ok, continue ok -> running e4 [stopped e3 Jump by 5001, breakpoints, continued e4] {set_registers 5001 rip=0x60, continue 5000 None, continue 5001 None} (b1:1 b2:0)",
        "at a co-hit / continue: ok -> running e3 [continued e3] {remove_site 0x40, step 5000, step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:2)",
        "at a co-hit / pause: AlreadyStopped -> stopped hit b1#1 by 5001 [] {} (b1:2)",
        "at a co-hit / remove: ok, continue ok -> running e3 [breakpoints, continued e3] {remove_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} ()",
        "at a co-hit / add at the hit: ok, continue ok -> running e3 [continued e3] {remove_site 0x40, step 5000, step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:2)",
        "at a co-hit / jump: ok, continue ok -> running e4 [stopped e3 Jump by 5001, continued e4] {set_registers 5001 rip=0x60, remove_site 0x40, step 5000, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:2)",
        "at a co-hit / jump, then add at the hit: ok, continue ok -> running e4 [stopped e3 Jump by 5001, breakpoints, continued e4] {set_registers 5001 rip=0x60, remove_site 0x40, step 5000, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:2 b2:0)",
        "at hits of two sites / continue: ok -> running e3 [continued e3] {remove_site 0x40, step 5001, reinstall_site 0x40, remove_site 0x50, step 5000, reinstall_site 0x50, continue 5000 None, continue 5001 None, continue 5000 None} (b1:1 b2:1)",
        "at hits of two sites / pause: AlreadyStopped -> stopped hit b1#1 by 5001 [] {} (b1:1 b2:1)",
        "at hits of two sites / remove: ok, continue ok -> running e3 [breakpoints, continued e3] {remove_site 0x40, remove_site 0x50, step 5000, reinstall_site 0x50, continue 5000 None, continue 5001 None, continue 5000 None} (b2:1)",
        "at hits of two sites / add at the hit: ok, continue ok -> running e3 [continued e3] {remove_site 0x40, step 5001, reinstall_site 0x40, remove_site 0x50, step 5000, reinstall_site 0x50, continue 5000 None, continue 5001 None, continue 5000 None} (b1:1 b2:1)",
        "at hits of two sites / jump: ok, continue ok -> running e4 [stopped e3 Jump by 5001, continued e4] {set_registers 5001 rip=0x60, remove_site 0x50, step 5000, reinstall_site 0x50, continue 5000 None, continue 5001 None, continue 5000 None} (b1:1 b2:1)",
        "at hits of two sites / jump, then add at the hit: ok, continue ok -> running e4 [stopped e3 Jump by 5001, breakpoints, continued e4] {set_registers 5001 rip=0x60, remove_site 0x50, step 5000, reinstall_site 0x50, continue 5000 None, continue 5001 None, continue 5000 None} (b1:1 b2:1 b3:0)",
        "at a hit since removed / continue: ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} ()",
        "at a hit since removed / pause: AlreadyStopped -> stopped hit b1#1 by 5001 [] {} ()",
        "at a hit since removed / remove: BreakpointNotFound(1), continue ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} ()",
        "at a hit since removed / add at the hit: ok, continue ok -> running e3 [breakpoints, continued e3] {install_site 0x40, remove_site 0x40, step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None} (b2:0)",
        "at a hit since removed / jump: ok, continue ok -> running e4 [stopped e3 Jump by 5001, continued e4] {set_registers 5001 rip=0x60, continue 5000 None, continue 5001 None} ()",
        "at a hit since removed / jump, then add at the hit: ok, continue ok -> running e4 [stopped e3 Jump by 5001, breakpoints, continued e4] {set_registers 5001 rip=0x60, install_site 0x40, continue 5000 None, continue 5001 None} (b2:0)",
        "stepping over a trap / continue: NotStopped -> running e2 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None} (b1:1 b2:0)",
        "stepping over a trap / pause: ok -> stopped Pause by 5001 [stopped e2 Pause by 5001] {request_stop 5001, reinstall_site 0x40} (b1:1 b2:0)",
        "stepping over a trap / remove: ok -> running e2 [breakpoints] {request_stop 5001, reinstall_site 0x40, remove_site 0x40, continue 5000 None, continue 5001 None, continue 5001 None} (b2:0)",
        "stepping over a trap / add at the hit: ok -> running e2 [breakpoints] {request_stop 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5001 None} (b1:1 b2:0 b3:0)",
        "stepping over a co-hit / continue: NotStopped -> running e2 [] {step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:2 b2:0)",
        "stepping over a co-hit / pause: ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {reinstall_site 0x40} (b1:2 b2:0)",
        "stepping over a co-hit / remove: ok -> running e2 [breakpoints] {reinstall_site 0x40, remove_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b2:0)",
        "stepping over a co-hit / add at the hit: ok -> running e2 [breakpoints] {reinstall_site 0x40, remove_site 0x40, step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:2 b2:0 b3:0)",
        "stopped by a signal in a step over a trap / continue: ok -> running e3 [continued e3] {continue 5001 Some(SIGUSR1)} (b1:1 b2:0)",
        "stopped by a signal in a step over a trap / pause: AlreadyStopped -> stopped Exception(ExceptionInfo { code: 10, description: \"SIGUSR1 (SI_USER)\" }) by 5001 [] {} (b1:1 b2:0)",
        "stopped by a signal in a step over a trap / remove: ok, continue ok -> running e3 [breakpoints, continued e3] {remove_site 0x40, continue 5000 None, continue 5001 Some(SIGUSR1)} (b2:0)",
        "stopped by a signal in a step over a trap / add at the hit: ok, continue ok -> running e3 [breakpoints, continued e3] {continue 5001 Some(SIGUSR1)} (b1:1 b2:0 b3:0)",
        "stopped by a signal in a step over a trap / jump: ok, continue ok -> running e4 [stopped e3 Jump by 5001, continued e4] {set_registers 5001 rip=0x60, continue 5000 None, continue 5001 Some(SIGUSR1)} (b1:1 b2:0)",
        "stopped by a signal in a step over a trap / jump, then add at the hit: ok, continue ok -> running e4 [stopped e3 Jump by 5001, breakpoints, continued e4] {set_registers 5001 rip=0x60, continue 5000 None, continue 5001 Some(SIGUSR1)} (b1:1 b2:0 b3:0)",
        "running a handler that interrupted a step over a trap / continue: NotStopped -> running e2 [] {} (b1:1 b2:0)",
        "running a handler that interrupted a step over a trap / pause: ok -> stopped Pause by 5001 [stopped e2 Pause by 5001] {request_stop 5001} (b1:1 b2:0)",
        "running a handler that interrupted a step over a trap / remove: ok -> running e2 [breakpoints] {request_stop 5001, remove_site 0x40, continue 5000 None, continue 5001 None} (b2:0)",
        "running a handler that interrupted a step over a trap / add at the hit: ok -> running e2 [breakpoints] {request_stop 5001, continue 5001 None} (b1:1 b2:0 b3:0)",
        "running a handler that interrupted a step over a trap / thread traps again: - -> running e2 [] {set_registers 5001 rip=0x40, remove_site 0x40, step 5001, reinstall_site 0x40, continue 5000 None, continue 5001 None} (b1:1 b2:0)",
        "running a handler that interrupted a step over a trap / thread hits the second site: - -> stopped hit b2#1 by 5001 [stopped e2 hit b2#1 by 5001] {set_registers 5001 rip=0x50} (b1:1 b2:1)",
        "running a handler that interrupted a step over a trap / thread gets a signal: - -> running e2 [] {continue 5001 Some(SIGALRM)} (b1:1 b2:0)",
        "running a handler that interrupted a step over a trap / thread exits: - -> running e2 [5001 exited] {continue 5001 None, continue 5000 None} (b1:1 b2:0)",
        "in a vfork window / continue: NotStopped, vfork done -> running e2 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None} (b1:0)",
        "in a vfork window / pause: ok, vfork done -> stopped Pause by 5000 [stopped e2 Pause by 5000] {reinstall_site 0x40} (b1:0)",
        "in a vfork window / remove: pending, vfork done -> running e2 [breakpoints] {reinstall_site 0x40, remove_site 0x40, continue 5000 None, continue 5001 None} ()",
        "in a vfork window / add at the hit: pending, vfork done -> running e2 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None} (b1:0)",
        "in a vfork window / thread exits: - -> running e2 [5001 exited] {continue 5001 None, reinstall_site 0x40, continue 5000 None} (b1:0)",
        "in a vfork window / vfork done: - -> running e2 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None} (b1:0)",
        "in a vfork window opened during a step over a trap / continue: NotStopped, vfork done -> running e2 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:1)",
        "in a vfork window opened during a step over a trap / pause: ok, vfork done -> stopped Pause by 5000 [stopped e2 Pause by 5000] {reinstall_site 0x40} (b1:1)",
        "in a vfork window opened during a step over a trap / remove: pending, vfork done -> running e2 [breakpoints] {reinstall_site 0x40, remove_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} ()",
        "in a vfork window opened during a step over a trap / add at the hit: pending, vfork done -> running e2 [breakpoints] {reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:1 b2:0)",
        "in a vfork window opened during a step over a trap / thread exits: - -> running e2 [5001 exited] {continue 5001 None, reinstall_site 0x40, continue 5000 None, continue 5000 None} (b1:1)",
        "in a vfork window opened during a step over a trap / vfork done: - -> running e2 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None} (b1:1)",
    ];
    assert_eq!(actual, expected, "\n{}", actual.join("\n"));
}

/// A breakpoint in a signal handler that interrupted a thread's step over
/// a trap stops the thread there. Once continued, the thread steps over
/// that trap and runs on alone to execute the first trap again, which is
/// the same hit, not a second one.
#[test]
fn a_hit_in_a_handler_that_interrupted_a_step_over_a_trap_counts_that_trap_once() {
    let mut run = phase("running a handler that interrupted a step over a trap");
    run.hit_at(THREAD, ADDED_SITE);
    run.settle(true);
    assert!(
        run.is_stopped(),
        "the handler's hit stops: {}",
        run.describe()
    );

    let mut resumed = run.resume();
    run.settle(true);
    assert_eq!(resumed(), "ok");
    assert!(
        run.runs(THREAD) && !run.runs(SIBLING),
        "the thread runs on alone to the trap its handler interrupted: {}",
        run.describe()
    );

    run.hit_at(THREAD, HIT_SITE);
    run.settle(true);
    assert_eq!(run.hits(), "b1:1 b2:1", "{}", run.describe());
    assert!(
        run.runs(THREAD) && run.runs(SIBLING),
        "both threads run once the trap is stepped over: {}",
        run.describe()
    );
}

/// A thread stopped by a signal before it stepped over a trap would
/// execute that trap again once its handler returns, so it runs alone to
/// it. Moved elsewhere, its handler returns there instead, and it runs
/// with its sibling.
#[test]
fn a_thread_moved_off_a_trap_a_signal_kept_it_from_stepping_over_runs_with_its_siblings() {
    let mut run = phase("stopped by a signal in a step over a trap");
    let mut jumped = run.jump(THREAD);
    run.settle(true);
    assert!(jumped().starts_with("ok"), "the jump");

    let mut resumed = run.resume();
    run.settle(true);
    assert_eq!(resumed(), "ok");
    assert!(
        run.runs(THREAD) && run.runs(SIBLING),
        "both threads run: {}",
        run.describe()
    );
}

/// A thread moved off the trap it hit owes that trap nothing, even once a
/// breakpoint is added there again: it continues without stepping over it.
#[test]
fn a_thread_moved_off_its_hit_does_not_step_over_a_breakpoint_added_there() {
    let mut run = phase("at a hit");
    let mut jumped = run.jump(THREAD);
    run.settle(true);
    assert!(jumped().starts_with("ok"), "the jump");

    // A hit condition makes it a breakpoint of its own, not the first again.
    let mut added = run.add(HIT_SITE, Some(">=1"));
    run.settle(true);
    assert!(added().starts_with("ok"), "the breakpoint");
    run.calls.clear();

    let mut resumed = run.resume();
    run.settle(true);
    assert_eq!(resumed(), "ok");
    assert!(
        !run.calls.iter().any(|call| call.starts_with("step")),
        "nothing steps over a trap: {}",
        run.describe()
    );
}

/// The invariant check fires on each trap state that contradicts itself.
mod invariants {
    use super::*;

    const SITE: VirtualAddress = VirtualAddress::new(0x40);
    const OTHER_SITE: VirtualAddress = VirtualAddress::new(0x48);

    /// A harness whose state passes the check, with breakpoints at `SITE`
    /// and `OTHER_SITE`.
    fn checked() -> WatchHarness {
        let mut harness = watch_harness(2);
        harness.add_breakpoint(SITE.get());
        harness.add_breakpoint(OTHER_SITE.get());
        harness.controller.check_invariants();
        harness
    }

    fn lift(harness: &mut WatchHarness, address: VirtualAddress, lift: Lift) {
        harness
            .inferior()
            .breakpoints
            .get_mut(&address)
            .expect("a site")
            .trap = Trap::Lifted(lift);
    }

    #[test]
    #[should_panic(expected = "the trap at 0x40 is installed while a vfork child runs")]
    fn a_trap_in_memory_a_vfork_child_runs_in() {
        let mut harness = checked();
        let parent = harness.threads[0];
        harness.inferior().vfork_windows.insert(
            parent,
            VforkWindow {
                child: VFORK_CHILD,
                resumed: true,
            },
        );
        harness.controller.check_invariants();
    }

    #[test]
    #[should_panic(expected = "the trap at 0x40 is lifted with no vfork window open")]
    fn a_trap_lifted_for_a_vfork_that_is_over() {
        let mut harness = checked();
        lift(&mut harness, SITE, Lift::Vfork);
        harness.controller.check_invariants();
    }

    #[test]
    #[should_panic(expected = "are both lifted for repairs")]
    fn two_traps_lifted_for_repairs() {
        let mut harness = checked();
        lift(&mut harness, SITE, Lift::Repair);
        lift(&mut harness, OTHER_SITE, Lift::Repair);
        harness.controller.check_invariants();
    }

    #[test]
    #[should_panic(expected = "both step over traps")]
    fn two_threads_stepping_over_traps() {
        let mut harness = checked();
        lift(&mut harness, SITE, Lift::Repair);
        for pid in harness.threads.clone() {
            harness.thread(pid).trap = AtTrap::Repairing { at: SITE };
        }
        harness.controller.check_invariants();
    }

    #[test]
    #[should_panic(expected = "stands at a trap at 0x70, which has no site")]
    fn a_thread_at_a_trap_with_no_site() {
        let mut harness = checked();
        let pid = harness.threads[1];
        harness.thread(pid).trap = AtTrap::MustStepOver {
            at: VirtualAddress::new(0x70),
        };
        harness.controller.check_invariants();
    }

    #[test]
    #[should_panic(expected = "steps over the trap at 0x40, which is not lifted for it")]
    fn a_thread_stepping_over_a_trap_in_memory() {
        let mut harness = checked();
        let pid = harness.threads[1];
        harness.thread(pid).trap = AtTrap::Repairing { at: SITE };
        harness.controller.check_invariants();
    }
}

/// SIGKILL takes the leader on its way to its exit as it steps over a trap,
/// and a pause stops the rest. The trap goes back in, and the leader still
/// stands at it, never having stepped.
#[test]
fn a_stop_after_sigkill_took_a_thread_stepping_over_a_trap_puts_the_trap_back() {
    let mut harness = repairing_harness_of(0);
    let leader = harness.threads[0];
    let site = VirtualAddress::new(0x40);
    harness.trace().sigkill(leader);
    harness
        .event(leader, libc::PTRACE_EVENT_EXIT)
        .expect("the exit event");
    harness
        .controller
        .begin_pause(process_id(leader))
        .expect("pause");
    harness.settle_requested_stops();

    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
    let inferior = harness.inferior();
    assert_eq!(inferior.breakpoints[&site].trap, Trap::Installed);
    assert_eq!(
        inferior.threads[&leader].trap,
        AtTrap::MustStepOver { at: site }
    );
}

/// SIGKILL takes a thread out of its stop at a breakpoint's trap just as
/// the debugger rewinds it over the trap, and the thread reaches its exit
/// event. The trap is still the debugger's, not one the program executed:
/// no stop is published, and the thread goes on to its exit.
#[test]
fn a_hit_sigkill_interrupts_as_it_is_rewound_is_not_the_programs_trap() {
    let mut run = running(false);
    run.describe();
    *run.harness.trace().exit_point.borrow_mut() = Some((THREAD, "set_registers"));
    run.hit(THREAD);
    run.settle(true);
    assert!(
        run.harness.trace().exit_point.borrow().is_none(),
        "the rewind was made"
    );

    let described = run.describe();
    assert!(described.starts_with("running e2 []"), "{described}");
    run.exit(THREAD);
    let described = run.describe();
    assert!(described.starts_with("running e2 ["), "{described}");
    assert!(!described.contains("stopped"), "{described}");
}
