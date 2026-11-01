//! A model of the group's stop rule over events from up to four members
//! and a client's commands. The reference follows the rules, to say
//! which events can happen next: only a running member stops, and only a
//! member being paused fails to pause. The test drives [`Stops`] as the
//! driver does, and checks what it reports against bookkeeping of its own.

use std::collections::BTreeSet;
use std::sync::Arc;

use proptest::prelude::*;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

use super::{MemberNumber, StopAction, Stops};
use crate::{StopReason, StopScope, VirtualAddress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Member {
    Running,
    Stopped,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Continue,
    Resume,
}

#[derive(Debug, Clone)]
enum Event {
    /// A running member stopped, for a pause or a breakpoint of its own.
    Stopped {
        member: MemberNumber,
        pause: bool,
    },

    /// A member being paused will not stop for it.
    PauseFailed(MemberNumber),

    /// A member's process exited or left the group.
    Gone(MemberNumber),
    PauseAll,
    Client(Command),
}

/// Where the group is, as the rules say.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Running,

    /// Waiting for `awaited`; `first` says whether the stop already has
    /// the stop it reports.
    Stopping {
        awaited: BTreeSet<MemberNumber>,
        first: bool,
    },
    Stopped,
}

/// The group as the rules say it should be.
#[derive(Debug, Clone)]
struct Reference {
    scope: StopScope,
    members: Vec<Member>,
    phase: Phase,

    /// How many stops wait to be reported at continues.
    pending: usize,
    deferred: Vec<Command>,
}

impl Reference {
    fn numbered(&self, state: Member) -> Vec<MemberNumber> {
        (1..)
            .zip(&self.members)
            .filter(|(_, member)| **member == state)
            .map(|(number, _)| number)
            .collect()
    }

    fn set(&mut self, number: MemberNumber, state: Member) {
        self.members[number as usize - 1] = state;
    }

    fn awaited(&self) -> Option<&BTreeSet<MemberNumber>> {
        match &self.phase {
            Phase::Stopping { awaited, .. } => Some(awaited),
            _ => None,
        }
    }

    /// Notes that `number` no longer holds up a stop under way, which
    /// completes once nobody does.
    fn settled(&mut self, number: MemberNumber) {
        let Phase::Stopping { awaited, .. } = &mut self.phase else {
            return;
        };
        awaited.remove(&number);
        if awaited.is_empty() {
            self.phase = Phase::Stopped;
            for command in std::mem::take(&mut self.deferred) {
                self.serve(command);
            }
        }
    }

    /// A stop pauses every running member but the one stopping. A pause
    /// asked while a stop is under way joins it.
    fn begin_stop(&mut self, first: Option<MemberNumber>) {
        let running = self
            .numbered(Member::Running)
            .into_iter()
            .filter(|number| Some(*number) != first)
            .collect::<BTreeSet<_>>();
        match &mut self.phase {
            Phase::Stopping { awaited, .. } => awaited.extend(running),
            _ => {
                self.phase = Phase::Stopping {
                    awaited: running,
                    first: first.is_some(),
                };
                self.settled(0);
            }
        }
    }

    /// A continue reports a pending stop, or resumes the stopped members;
    /// a resume forgets the pending stops first.
    fn serve(&mut self, command: Command) {
        if command == Command::Resume {
            self.pending = 0;
        }
        if self.pending > 0 {
            self.pending -= 1;
            self.phase = Phase::Stopped;
            return;
        }

        self.phase = Phase::Running;
        for member in &mut self.members {
            if *member == Member::Stopped {
                *member = Member::Running;
            }
        }
    }
}

struct Model;

impl ReferenceStateMachine for Model {
    type State = Reference;
    type Transition = Event;

    fn init_state() -> BoxedStrategy<Reference> {
        (
            prop_oneof![4 => Just(StopScope::Group), 1 => Just(StopScope::Process)],
            1_usize..=4,
        )
            .prop_map(|(scope, members)| Reference {
                scope,
                members: vec![Member::Running; members],
                phase: Phase::Running,
                pending: 0,
                deferred: Vec::new(),
            })
            .boxed()
    }

    fn transitions(state: &Reference) -> BoxedStrategy<Event> {
        let running = state.numbered(Member::Running);
        let present = (1..)
            .zip(&state.members)
            .filter(|(_, member)| **member != Member::Gone)
            .map(|(number, _)| number)
            .collect::<Vec<MemberNumber>>();
        let awaited = state
            .awaited()
            .into_iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();

        let mut events = vec![(
            2,
            prop::sample::select(vec![Command::Continue, Command::Resume])
                .prop_map(Event::Client)
                .boxed(),
        )];
        if !running.is_empty() {
            events.push((
                6,
                (prop::sample::select(running), any::<bool>())
                    .prop_map(|(member, pause)| Event::Stopped { member, pause })
                    .boxed(),
            ));
            events.push((1, Just(Event::PauseAll).boxed()));
        }
        if !awaited.is_empty() {
            events.push((
                1,
                prop::sample::select(awaited)
                    .prop_map(Event::PauseFailed)
                    .boxed(),
            ));
        }
        if !present.is_empty() {
            events.push((
                1,
                prop::sample::select(present).prop_map(Event::Gone).boxed(),
            ));
        }
        proptest::strategy::Union::new_weighted(events).boxed()
    }

    fn preconditions(state: &Reference, event: &Event) -> bool {
        let is = |number: MemberNumber, wanted: Member| {
            state.members.get(number as usize - 1) == Some(&wanted)
        };
        match event {
            Event::Stopped { member, .. } => is(*member, Member::Running),
            Event::PauseFailed(member) => {
                is(*member, Member::Running)
                    && state
                        .awaited()
                        .is_some_and(|awaited| awaited.contains(member))
            }
            Event::Gone(member) => !is(*member, Member::Gone),
            Event::PauseAll => !state.numbered(Member::Running).is_empty(),
            Event::Client(_) => true,
        }
    }

    fn apply(mut state: Reference, event: &Event) -> Reference {
        let group = state.scope == StopScope::Group;
        match *event {
            Event::Stopped { member, pause } => {
                state.set(member, Member::Stopped);

                // A member's stop is its own, but settles a pause of all.
                if !group {
                    state.settled(member);
                    return state;
                }
                match &mut state.phase {
                    Phase::Running => state.begin_stop(Some(member)),
                    Phase::Stopping { first, .. } => {
                        if !pause {
                            if *first {
                                state.pending += 1;
                            } else {
                                *first = true;
                            }
                        }
                        state.settled(member);
                    }
                    Phase::Stopped => {}
                }
            }
            Event::PauseFailed(member) => state.settled(member),
            Event::Gone(member) => {
                state.set(member, Member::Gone);
                state.settled(member);
            }
            Event::PauseAll => state.begin_stop(None),
            Event::Client(command) => {
                if state.awaited().is_some() {
                    state.deferred.push(command);
                } else {
                    state.serve(command);
                }
            }
        }
        state
    }
}

/// The rule under test and what the test knows apart from it.
struct Harness {
    scope: StopScope,
    stops: Stops<(u32, Command)>,
    members: Vec<Member>,

    /// The members the stop under way was told to wait for, and have not
    /// stopped, failed to pause, or gone since.
    awaited: Option<BTreeSet<MemberNumber>>,

    /// Breakpoint stops not yet reported, which a resume may discard.
    owed: Vec<(MemberNumber, StopReason)>,

    /// Client commands sent and not yet served.
    unserved: BTreeSet<u32>,
    next_command: u32,
    next_breakpoint: u64,
}

impl Harness {
    fn report(&mut self, member: MemberNumber, reason: &StopReason) {
        self.owed
            .retain(|(owed, owed_reason)| (*owed, owed_reason) != (member, reason));
    }

    /// A group stop the rule reports as it completes.
    fn completed(&mut self, stop: Option<(MemberNumber, StopReason)>) {
        let Some((member, reason)) = stop else {
            if self.awaited.as_ref().is_some_and(BTreeSet::is_empty) {
                self.awaited = None;
            }
            return;
        };

        // Each member's stop was reported as its own.
        assert_eq!(
            self.scope,
            StopScope::Group,
            "member {member}'s stop for {reason:?} was reported for a pause of all"
        );
        let awaited = self.awaited.take();
        assert!(
            awaited.as_ref().is_some_and(BTreeSet::is_empty),
            "member {member}'s group stop completed while {awaited:?} were awaited"
        );
        self.report(member, &reason);
    }

    fn settled(&mut self, member: MemberNumber) {
        if let Some(awaited) = &mut self.awaited {
            awaited.remove(&member);
        }
    }

    /// Pauses the running members but `except`, as the driver does.
    fn pause_all(&mut self, first: Option<(MemberNumber, StopReason)>) {
        let except = first.as_ref().map(|(member, _)| *member);
        let waiting = (1..)
            .zip(&self.members)
            .filter(|(number, member)| **member == Member::Running && Some(*number) != except)
            .map(|(number, _)| number)
            .collect::<BTreeSet<_>>();
        self.awaited = Some(waiting.clone());
        let completed = self.stops.pause_all(first, waiting);
        self.completed(completed);
    }

    /// Serves a client's command as the driver does, once nothing defers
    /// it.
    fn serve(&mut self, (id, command): (u32, Command)) {
        assert!(
            self.awaited.is_none(),
            "command {id} was served while {:?} were awaited",
            self.awaited
        );
        assert!(self.unserved.remove(&id), "command {id} was served twice");

        if command == Command::Resume {
            self.stops.discard_pending();
            self.owed.clear();
        }
        match self.stops.continue_all() {
            Some((member, reason)) => self.report(member, &reason),
            None => {
                for member in &mut self.members {
                    if *member == Member::Stopped {
                        *member = Member::Running;
                    }
                }
            }
        }
    }

    /// Serves the commands that waited for a stop, after a member's event.
    fn after_internal(&mut self) {
        for command in self.stops.take_deferred() {
            self.serve(command);
        }
    }

    fn apply(&mut self, event: &Event) {
        match *event {
            Event::Stopped { member, pause } => {
                let reason = if pause {
                    StopReason::Pause
                } else {
                    self.next_breakpoint += 1;
                    StopReason::Breakpoint {
                        address: Some(VirtualAddress::new(self.next_breakpoint)),
                        hits: Arc::from([]),
                    }
                };
                self.members[member as usize - 1] = Member::Stopped;
                if !pause {
                    self.owed.push((member, reason.clone()));
                }

                let under_way = self.awaited.is_some() && self.scope == StopScope::Group;
                self.settled(member);
                if self.awaited.as_ref().is_some_and(BTreeSet::is_empty)
                    && self.scope == StopScope::Process
                {
                    self.awaited = None;
                }
                match self.stops.stopped(member, reason.clone()) {
                    Some(StopAction::Report(reported, why)) if under_way => {
                        self.completed(Some((reported, why)));
                    }

                    // Outside a group stop, a member's stop is its own.
                    Some(StopAction::Report(reported, why)) => {
                        assert_eq!((reported, &why), (member, &reason), "a stop of its own");
                        self.report(reported, &why);
                    }
                    Some(StopAction::PauseOthers(first, why)) => {
                        assert!(!under_way, "member {member} began a stop inside another");
                        self.pause_all(Some((first, why)));
                    }
                    None => assert!(under_way, "member {member}'s stop was ignored"),
                }
                self.after_internal();
            }
            Event::PauseFailed(member) => {
                self.settled(member);
                let completed = self.stops.paused(member, None);
                self.completed(completed);
                self.after_internal();
            }
            Event::Gone(member) => {
                self.members[member as usize - 1] = Member::Gone;
                self.settled(member);
                let completed = self.stops.paused(member, None);
                self.completed(completed);
                self.after_internal();
            }
            Event::PauseAll => self.pause_all(None),
            Event::Client(command) => {
                let id = self.next_command;
                self.next_command += 1;
                self.unserved.insert(id);
                if let Some(command) = self.stops.defer((id, command)) {
                    self.serve(command);
                }
            }
        }
    }
}

impl StateMachineTest for Harness {
    type SystemUnderTest = Self;
    type Reference = Model;

    fn init_test(reference: &Reference) -> Self {
        Self {
            scope: reference.scope,
            stops: Stops::new(reference.scope),
            members: reference.members.clone(),
            awaited: None,
            owed: Vec::new(),
            unserved: BTreeSet::new(),
            next_command: 0,
            next_breakpoint: 0,
        }
    }

    fn apply(mut harness: Self, reference: &Reference, event: Event) -> Self {
        harness.apply(&event);
        assert_eq!(
            harness.members, reference.members,
            "the members after {event:?}"
        );
        assert_eq!(
            harness.awaited.as_ref(),
            reference.awaited(),
            "the members a stop waits for after {event:?}"
        );
        harness
    }

    fn check_invariants(harness: &Self, _: &Reference) {
        if harness.awaited.is_none() {
            assert!(
                harness.unserved.is_empty(),
                "commands {:?} wait for no stop",
                harness.unserved
            );
        }
    }

    /// Lets every awaited member stop and continues until nothing is left
    /// to report: every breakpoint stop no resume discarded is reported.
    fn teardown(mut harness: Self, _: Reference) {
        for _ in 0..64 {
            if let Some(member) = harness.awaited.iter().flatten().next().copied() {
                harness.apply(&Event::Stopped {
                    member,
                    pause: true,
                });
                continue;
            }

            if harness.owed.is_empty() {
                return;
            }
            harness.apply(&Event::Client(Command::Continue));
        }

        assert!(
            harness.owed.is_empty(),
            "stops {:?} were never reported",
            harness.owed
        );
    }
}

prop_state_machine! {
    #[test]
    fn group_stops_report_each_stop_once_everyone_is_paused(sequential 1..24 => Harness);
}
