//! Random sequences of a client's edits and runs, and of what the program
//! and its agent do, against a runtime agent's installation. The agent's
//! side is known exactly, from what the controller asked of the agent and
//! what the test made it do, and after every step the controller must
//! agree with it: a breakpoint is ready only where the agent holds its
//! site, all are once it acknowledged the newest request that has them,
//! a stale report changes nothing, one script at a time is outstanding,
//! only an outstanding source request refuses a thread-only continue, the
//! acknowledgement alarm is set exactly while an unexpired request is
//! outstanding and the program runs, and a detach leaves no request behind.
//!
//! Every step is valid in every state, so the sequences are plain vectors.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use proptest::prelude::*;

use super::agents::{ENTRY, Session};
use super::run_phase::OTHER;
use super::*;

#[derive(Debug, Clone, Copy)]
enum Step {
    /// The client adds a breakpoint at the line, unless one is there.
    Add(u64),

    /// The client removes the breakpoint at the line, if one is there.
    Remove(u64),
    Continue,

    /// The client continues only the leader, which no source request
    /// goes to.
    ContinueLeader,
    Pause,

    /// The source request's thread takes it from its slot.
    Take,

    /// The agent acknowledges the newest request.
    Ack,

    /// The agent acknowledges the request before the newest.
    StaleAck,

    /// The agent fails, running the newest request or later.
    Failure,

    /// The acknowledgement alarm rings once the budget is spent.
    Alarm,

    /// A thread enters the interpreter where the controller traps it.
    Entry,

    /// Whether the interpreter has a thread a source request can go to.
    Thread(bool),

    /// Whether the agent refuses requests before any write.
    Refuse(bool),
}

#[derive(Debug, Clone, Copy)]
enum End {
    Detach,
    Exit,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (1..=3_u64).prop_map(Step::Add),
        2 => (1..=3_u64).prop_map(Step::Remove),
        3 => Just(Step::Continue),
        1 => Just(Step::ContinueLeader),
        2 => Just(Step::Pause),
        2 => Just(Step::Take),
        4 => Just(Step::Ack),
        1 => Just(Step::StaleAck),
        1 => Just(Step::Failure),
        2 => Just(Step::Alarm),
        1 => Just(Step::Entry),
        1 => any::<bool>().prop_map(Step::Thread),
        1 => any::<bool>().prop_map(Step::Refuse),
    ]
}

/// How the newest request stands.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Newest {
    /// The agent reported on it, or none was made.
    #[default]
    Reported,
    Outstanding,

    /// Outstanding, with its budget spent.
    Expired,
}

/// What the agent was asked and holds, as the test knows it.
#[derive(Debug, Default)]
struct Agent {
    /// The sites each request carries, and whether it is a source request.
    published: BTreeMap<u64, (BTreeSet<u64>, bool)>,
    newest: Option<u64>,
    state: Newest,

    /// Whether the agent runs callbacks, which read commands: it loaded a
    /// configuration and has not failed since.
    live: bool,

    /// The sites of the request the agent last acknowledged, since it last
    /// failed.
    held: BTreeSet<u64>,

    /// The generations whose script files exist.
    scripts: BTreeSet<u64>,

    /// Whether the agent's files were ever made.
    files: bool,
}

/// One case: the controller with its agent, and what the test knows.
struct Case {
    session: Session,
    agent: Agent,

    /// The breakpoint at each line, and its site's number.
    breakpoints: BTreeMap<u64, (u64, u64)>,
    next_breakpoint: u64,
    next_site: u64,

    /// Whether the last step's agent acknowledged the newest request.
    acknowledged: bool,
}

/// The generation a logged request publishes, and whether it is a source
/// request.
fn published(entry: &str) -> Option<(u64, bool)> {
    let source = entry
        .strip_prefix("inject /tmp/agent/agent-")
        .and_then(|rest| rest.split_once(".py"))
        .map(|(generation, _)| (generation, true));
    let command = || {
        entry
            .strip_prefix("command ")
            .and_then(|rest| rest.split_once("->"))
            .and_then(|(_, rest)| rest.split_once(' '))
            .map(|(generation, _)| (generation, false))
    };
    source
        .or_else(command)
        .map(|(generation, source)| (generation.parse().expect("a generation"), source))
}

fn sites(text: &str) -> BTreeSet<u64> {
    let inside = text
        .rsplit_once('[')
        .and_then(|(_, rest)| rest.strip_suffix(']'))
        .expect("a logged request names its sites");
    inside
        .split_whitespace()
        .map(|number| number.parse().expect("a site number"))
        .collect()
}

impl Case {
    fn new() -> Self {
        Self {
            session: Session::new(),
            agent: Agent::default(),
            breakpoints: BTreeMap::new(),
            next_breakpoint: 1,
            next_site: 0,
            acknowledged: false,
        }
    }

    /// Reads what the controller asked of the agent and its files.
    fn read_log(&mut self) {
        let logged = std::mem::take(&mut *self.session.agent.log.lock().expect("the log"));
        for entry in logged {
            if let Some((generation, source)) = published(&entry) {
                // Only a detach's disarming command is left unawaited.
                self.agent
                    .published
                    .insert(generation, (sites(&entry), source));
                self.agent.newest = Some(generation);
                self.agent.state = Newest::Outstanding;
            } else if let Some(generation) = entry.strip_prefix("file ") {
                self.agent
                    .scripts
                    .insert(generation.parse().expect("a generation"));
            } else if let Some(generation) = entry.strip_prefix("drop file ") {
                let generation = generation.parse().expect("a generation");
                assert!(
                    self.agent.scripts.remove(&generation),
                    "generation {generation}'s file dropped twice"
                );
            } else if entry.starts_with("create files") {
                self.agent.files = true;
            }
        }
        assert!(
            self.agent.scripts.len() <= 1,
            "two source requests are outstanding: {:?}",
            self.agent.scripts
        );
    }

    fn running(&self) -> bool {
        !self.session.run.is_stopped()
    }

    fn source_outstanding(&self) -> bool {
        self.agent.state != Newest::Reported
            && self
                .agent
                .newest
                .and_then(|newest| self.agent.published.get(&newest))
                .is_some_and(|(_, source)| *source)
    }

    /// Each breakpoint's id, site, and pending reason, if any.
    fn pending(&self) -> Vec<(u64, u64, Option<String>)> {
        let snapshot = self.session.run.snapshot();
        self.breakpoints
            .values()
            .map(|&(id, site)| {
                let breakpoint = snapshot
                    .breakpoints
                    .iter()
                    .find(|breakpoint| breakpoint.id.get() == id)
                    .expect("the breakpoint is listed");
                let reason = breakpoint
                    .pending_locations
                    .first()
                    .map(|pending| pending.reason.to_string());
                (id, site, reason)
            })
            .collect()
    }

    /// Whether the newest request may be reported on: a thread runs a
    /// source request, and a live agent reads a command or fails.
    fn agent_can_report(&self, failure: bool) -> bool {
        let Some((_, source)) = self
            .agent
            .newest
            .and_then(|newest| self.agent.published.get(&newest))
        else {
            return false;
        };
        (*source && (self.agent.state != Newest::Reported || failure)) || self.agent.live
    }

    fn apply(&mut self, step: Step) {
        let before = self.pending();
        let stopped_before = self.session.run.is_stopped();
        self.acknowledged = false;

        match step {
            Step::Add(line) => {
                if self.breakpoints.contains_key(&line) {
                    return;
                }
                let said = self.session.add(line)();
                assert_eq!(said, "ok", "adding at line {line}");
                self.breakpoints
                    .insert(line, (self.next_breakpoint, self.next_site));
                self.next_breakpoint += 1;
                self.next_site += 1;
            }
            Step::Remove(line) => {
                let Some((id, _)) = self.breakpoints.remove(&line) else {
                    return;
                };
                let said = self.session.remove(id)();
                assert_eq!(said, "ok", "removing breakpoint {id}");
            }
            Step::Continue => {
                if self.running() {
                    return;
                }
                let said = self.session.resume()();
                assert_eq!(said, "ok", "continuing");
            }
            Step::ContinueLeader => self.continue_leader(),
            Step::Pause => {
                if !self.running() {
                    return;
                }
                let said = self.session.pause()();
                assert_eq!(said, "ok", "pausing");
            }
            Step::Take => {
                if !self.session.agent.pending.load(Ordering::SeqCst) {
                    return;
                }
                if !self.running() {
                    self.session.resume()();
                }
                self.session.agent.pending.store(false, Ordering::SeqCst);
            }
            Step::Ack | Step::StaleAck | Step::Failure => self.report(step, &before),
            Step::Alarm => {
                let expires = self.agent.state == Newest::Outstanding && self.running();
                self.session.alarm();
                if expires {
                    self.agent.state = Newest::Expired;
                    assert!(!self.running(), "a timeout stops the program");
                }
            }
            Step::Entry => {
                let trapped = self
                    .session
                    .run
                    .harness
                    .inferior()
                    .breakpoints
                    .contains_key(&VirtualAddress::new(ENTRY));
                if !trapped {
                    return;
                }
                self.session
                    .report(ENTRY, crate::runtime_model::HookEvent::AgentEntry);
            }
            Step::Thread(present) => {
                *self.session.agent.thread.lock().expect("the script") =
                    present.then(|| debug_thread_id(OTHER));
            }
            Step::Refuse(refusing) => {
                *self.session.agent.refusal.lock().expect("the script") =
                    refusing.then(|| "scripted refusal".into());
            }
        }

        self.read_log();
        self.check(step, stopped_before);
    }

    /// Continues only the leader, which is refused while a source request
    /// that another thread is to run is outstanding.
    fn continue_leader(&mut self) {
        if self.running() {
            return;
        }
        let refused = self.source_outstanding();
        let said = self.session.resume_leader()();
        if refused {
            assert!(
                said.starts_with("RuntimeResumeUnsupported"),
                "a thread-only continue with a source request outstanding: {said}"
            );
            assert!(!self.running(), "a refused continue ran the program");
        } else {
            assert_eq!(said, "ok", "a thread-only continue");
            // The other thread stays stopped: stop the leader too,
            // so that every later step finds both alike.
            let said = self.session.pause()();
            assert_eq!(said, "ok", "pausing the leader");
        }
    }

    /// The agent reports on the newest request, or the one before it.
    fn report(&mut self, step: Step, before: &[(u64, u64, Option<String>)]) {
        let failure = matches!(step, Step::Failure);
        if !self.agent_can_report(failure) {
            return;
        }
        let newest = self.agent.newest.expect("a request was published");
        let stale = matches!(step, Step::StaleAck);
        if stale && !self.agent.published.contains_key(&(newest - 1)) {
            return;
        }

        self.session
            .agent_reports(failure.then_some("scripted failure"), stale);

        if stale {
            assert_eq!(
                self.pending(),
                before,
                "a stale acknowledgement changed what is pending"
            );
        } else if failure {
            self.agent.live = false;
            self.agent.held.clear();
            self.agent.state = Newest::Reported;
        } else {
            self.agent.live = true;
            self.agent.held.clone_from(&self.agent.published[&newest].0);
            self.agent.state = Newest::Reported;
            self.acknowledged = true;
        }
    }

    fn check(&self, step: Step, stopped_before: bool) {
        let pending = self.pending();
        for (id, site, reason) in &pending {
            assert!(
                reason.is_some() || self.agent.held.contains(site),
                "after {step:?}, breakpoint {id} is ready, but the agent holds only sites {:?}",
                self.agent.held
            );
        }

        if self.acknowledged
            && self.agent.state == Newest::Reported
            && let Some(newest) = self.agent.newest
        {
            let wanted: BTreeSet<u64> = pending.iter().map(|&(_, site, _)| site).collect();
            if self.agent.published[&newest].0 == wanted {
                assert!(
                    pending.iter().all(|(_, _, reason)| reason.is_none()),
                    "the agent acknowledged every site, yet some are pending: {pending:?}"
                );
            }
        }

        let alarm = self
            .session
            .run
            .harness
            .controller
            .agent_deadline()
            .is_some();
        let expected = self.agent.state == Newest::Outstanding && self.running();
        assert_eq!(
            alarm, expected,
            "after {step:?} (stopped before: {stopped_before}), whether the alarm is set: {:?}",
            self.agent
        );
    }

    fn end(mut self, end: End) {
        match end {
            End::Exit => {
                self.session.exit();
                let logged = std::mem::take(&mut *self.session.agent.log.lock().expect("the log"));
                assert_eq!(
                    logged
                        .iter()
                        .filter(|entry| *entry == "remove files")
                        .count(),
                    usize::from(self.agent.files),
                    "an exit removes the agent's files once: {logged:?}"
                );
            }
            End::Detach => {
                *self.session.agent.refusal.lock().expect("the script") = None;
                let taken =
                    self.source_outstanding() && !self.session.agent.pending.load(Ordering::SeqCst);
                let said = self.session.detach()();
                let logged = std::mem::take(&mut *self.session.agent.log.lock().expect("the log"));
                assert!(
                    !self.session.agent.pending.load(Ordering::SeqCst),
                    "a detach left a source request in its slot: {logged:?}"
                );
                if taken {
                    assert!(
                        said.contains("unconfirmed"),
                        "a detach after a thread took the source request: {said}"
                    );
                } else {
                    assert_eq!(said, "ok", "detaching: {logged:?}");
                    assert_eq!(
                        logged.iter().any(|entry| entry == "close files"),
                        self.agent.files,
                        "a detach closes the agent's files: {logged:?}"
                    );
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        ..ProptestConfig::default()
    })]

    #[test]
    fn the_controller_agrees_with_its_agent(
        steps in prop::collection::vec(step(), 1..32),
        detach in any::<bool>(),
    ) {
        let mut case = Case::new();
        for step in steps {
            case.apply(step);
        }
        case.end(if detach { End::Detach } else { End::Exit });
    }
}
