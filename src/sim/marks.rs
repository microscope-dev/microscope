//! Coverage marks: interesting states a run reached, counted so that the
//! gate can require its seeds to reach every one. A sweep that never
//! reaches what it claims to test passes for nothing.

/// Defines [`Mark`] and [`Mark::ALL`] from one list.
macro_rules! marks {
    ($($(#[$doc:meta])* $mark:ident,)*) => {
        /// One interesting state.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Mark {
            $($(#[$doc])* $mark,)*
        }

        impl Mark {
            pub const ALL: [Self; [$(Mark::$mark),*].len()] = [$(Self::$mark),*];
        }
    };
}

marks! {
    /// A launch stopped at the program's first instruction.
    EntryStop,

    /// A breakpoint stopped the program.
    BreakpointStop,

    /// A step completed.
    StepStop,

    /// A pause stopped the running program.
    PauseStop,

    /// The program ran to its own exit.
    ProgramExited,

    /// The client killed the program while it ran.
    KilledRunning,

    /// The client killed the program while it was stopped.
    KilledStopped,

    /// The client launched the program a second time.
    Relaunched,

    /// The client added or removed a breakpoint while the program ran.
    EditWhileRunning,

    /// A memory read covered a trap the debugger planted.
    ReadOverTrap,

    /// A memory write covered a trap the debugger planted.
    WroteOverTrap,

    /// The client inspected a caller's frame, which it selected.
    OuterFrameInspected,

    /// A step out of a caller's frame the client selected completed.
    SteppedOutOfCaller,

    /// The client stepped a thread alone.
    ThreadStepped,

    /// The frame a thread stopped in showed the registers it holds.
    StoppedRegistersTrue,

    /// A caller's frame showed the registers its call left.
    CallerRegistersTrue,

    /// A request naming a stale stop was refused.
    StaleStopRefused,

    /// Stepping out of the outermost frame was refused.
    StepOutRefused,

    /// The client's event receiver fell behind.
    ClientLagged,

    /// The waiter held a status because the controller's queue was full.
    QueueFull,

    /// A program created a thread.
    ThreadCreated,

    /// The debugger reported a thread exiting while its process ran on.
    ThreadExited,

    /// A group leader exited while its other threads ran on.
    LeaderExitedAlone,

    /// The debugger reported a leader's exit at its exit event.
    LeaderExitReported,

    /// An execution of a leader alone ended in its exit.
    LeaderExitEndedExecution,

    /// A program's `exit_group` took a thread out of a ptrace-stop.
    GroupExitEndedStop,

    /// A published stop found two threads at breakpoints.
    CoHit,

    /// The client resumed one thread alone.
    ThreadContinued,

    /// The client selected a thread other than the one that stopped.
    ThreadSelected,

    /// A thread ran between two of the controller's calls into the kernel.
    PreemptedCall,

    /// The waiter reaped a status between two of the controller's calls.
    ReapedInsideCall,

    /// A planned SIGKILL from outside landed at a chosen action.
    KilledAtStep,

    /// A planned SIGKILL from outside landed right after a clone.
    KilledNearClone,

    /// A planned SIGKILL from outside landed between two ptrace requests.
    KilledInsideCall,

    /// A backtrace showed every call its thread made.
    WholeBacktrace,

    /// A backtrace stopped, saying why, at a frame without call-frame
    /// information.
    TruncatedBacktrace,

    /// A backtrace ended at a caller read from a return address the program
    /// overwrote.
    CorruptCaller,

    /// A sample showed every call its thread made.
    WholeSample,

    /// A sample showed every call its thread made, past the end of its
    /// copy of the stack, through the kernel's frame-pointer walk.
    SplicedSample,

    /// A sample stopped, saying why, short of its thread's first frame.
    TruncatedSample,

    /// A sample ended at a caller read from a return address the program
    /// overwrote.
    CorruptSampledCaller,

    /// A backtrace showed functions that left by tail calls, at jumps
    /// their activation took.
    TailCallFrames,

    /// The stepping oracle judged where a step ended.
    StepJudged,

    /// A step the client did not wait for ended.
    UnwaitedStepEnded,

    /// The client paused while a step ran.
    PauseDuringStep,

    /// The client changed a breakpoint, watchpoint, or catchpoint while a
    /// step ran.
    EditDuringStep,

    /// The client killed the program while a step ran.
    KillDuringStep,

    /// A source step in unoptimized code passed the exact rules.
    SourceStepExact,

    /// A marker's condition held with the values the debugger read.
    MarkerHeld,

    /// A marker's condition, evaluated as an expression, was true.
    MarkerEvaluated,

    /// A variable's name evaluated to the value the variables view shows.
    NameEvaluated,

    /// An evaluated value's bytes were in memory where the debugger said.
    StorageTrue,

    /// A variable's address evaluated to where the variables view says it
    /// lives.
    AddressEvaluated,

    /// Arithmetic over variables evaluated exactly.
    ArithmeticEvaluated,

    /// What a marker expects, in the debugger's own language, was true.
    ExpectationHeld,

    /// A variable cast to a narrower integer type kept its low bits.
    CastEvaluated,

    /// An ill-typed expression was refused.
    IllTypedRefused,

    /// An evaluated value's bytes were in the register the debugger said.
    RegisterTrue,

    /// A value the debugger recovered from a caller's call was what the
    /// register it names held when the function was entered.
    EntryValueTrue,

    /// The client changed a breakpoint's hit condition or condition.
    BreakpointAmended,

    /// A breakpoint counted a hit that did not stop.
    HitDeclined,

    /// A breakpoint stopped at a hit whose condition the client knew held.
    ConditionHeld,

    /// A breakpoint logged a message instead of stopping.
    HitLogged,

    /// The debugger disabled a breakpoint.
    BreakpointDisabled,

    /// The debugger enabled a disabled breakpoint again.
    BreakpointEnabled,

    /// The debugger made a temporary breakpoint.
    TemporaryAdded,

    /// A temporary breakpoint stopped the program and was gone.
    TemporaryStop,

    /// A temporary breakpoint's stop found more than one thread at it.
    TemporaryCoHit,

    /// An advance reached its location.
    AdvanceReached,

    /// An advance ended where the selected frame returned.
    AdvanceReturned,

    /// The stepping oracle judged where an advance ended.
    AdvanceJudged,

    /// A thread moved to where it stood, which published its stop again.
    JumpedInPlace,

    /// A step went into one call its line listed, past the line's others.
    SteppedIntoCall,

    /// The debugger armed a watchpoint.
    WatchAdded,

    /// The debug registers refused a watchpoint, discarding writes or with
    /// their slots busy.
    WatchRefused,

    /// A watchpoint stopped the program.
    WatchpointStop,

    /// Watch accounting judged a reported hit.
    WatchHit,

    /// A thread other than the first reported a watchpoint hit.
    WatchHitOnAnotherThread,

    /// A store left watched bytes as they were, which a watchpoint on
    /// stores reports and one on changes does not.
    UnchangedStore,

    /// A watchpoint counted a hit that did not stop.
    WatchHitDeclined,

    /// A watchpoint stopped at a hit its hit condition or condition, as
    /// the client knew, let stop.
    WatchConditionHeld,

    /// The client changed a watchpoint's hit condition or condition.
    WatchAmended,

    /// The debugger disabled a watchpoint.
    WatchDisabled,

    /// The debugger enabled a disabled watchpoint again.
    WatchEnabled,

    /// A new thread could not be armed, its slots busy.
    WatchArmFailed,

    /// The debugger refused to run while a thread could not be armed.
    RunRefusedUnarmed,

    /// A traced program forked.
    Forked,

    /// A thread other than a process's first forked.
    ForkedFromThread,

    /// The debugger released a fork child.
    ForkChildReleased,

    /// The debugger released a fork child whose parent had exited.
    ReleasedAfterParentExit,

    /// A program reaped a child it forked.
    ChildReaped,

    /// Init reaped a child that outlived its parent.
    OrphanReaped,

    /// SIGKILL from outside landed right after a fork.
    KilledNearFork,

    /// The client attached to a program running untraced.
    Attached,

    /// Attaching found a thread that had finished exiting, which it could
    /// not seize.
    SeizeRefused,

    /// An interrupt reached a thread already stopped.
    InterruptWaited,

    /// A fork or clone entered with a signal pending failed, to be made
    /// again.
    CallRestarted,

    /// An interrupt stopped a thread ahead of the trap it raised.
    InterruptAheadOfTrap,

    /// A thread or process a seized thread created started in an
    /// interrupt's stop.
    SeizedChildStarted,

    /// The debugger released the program it attached to.
    Detached,

    /// A released program ran on to its own end, which transparency judged.
    FinishedAfterDetach,

    /// A container's presentation was what its view makes of memory.
    ViewPresented,

    /// A container's elements read the same in one page and in small ones.
    ViewPaged,

    /// A cyclic list was refused as the cycle it is.
    ViewCycleRefused,

    /// The debugger made a catchpoint.
    CatchAdded,

    /// The debugger removed a catchpoint.
    CatchRemoved,

    /// The client added or removed a catchpoint while the program ran.
    CatchEditWhileRunning,

    /// Catch accounting judged a stop at a system call's entry.
    CatchEntryStop,

    /// Catch accounting judged a stop at a system call's exit.
    CatchExitStop,

    /// A step ended at a catchpoint.
    CatchStepStop,

    /// The decoder read a call from the model of the tracepoint stream.
    TraceDecoded,
}

/// How many times a run reached each mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marks([u64; Mark::ALL.len()]);

impl Default for Marks {
    fn default() -> Self {
        Self([0; Mark::ALL.len()])
    }
}

impl Marks {
    pub const fn hit(&mut self, mark: Mark) {
        self.0[mark as usize] += 1;
    }

    #[must_use]
    pub const fn count(&self, mark: Mark) -> u64 {
        self.0[mark as usize]
    }
}

/// How many runs reached each mark. One run that reaches a mark many times
/// counts once: a mark few runs reach is lost to any change that redraws
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    runs: u64,
    reached: [u64; Mark::ALL.len()],
}

impl Default for Reach {
    fn default() -> Self {
        Self {
            runs: 0,
            reached: [0; Mark::ALL.len()],
        }
    }
}

impl Reach {
    /// Counts the marks one run reached.
    pub fn add(&mut self, marks: &Marks) {
        self.runs += 1;
        for (reached, count) in self.reached.iter_mut().zip(marks.0) {
            *reached += u64::from(count > 0);
        }
    }

    /// Adds the runs `other` counted.
    pub fn merge(&mut self, other: &Self) {
        self.runs += other.runs;
        for (reached, other) in self.reached.iter_mut().zip(other.reached) {
            *reached += other;
        }
    }

    /// How many runs reached `mark`.
    #[must_use]
    pub const fn runs(&self, mark: Mark) -> u64 {
        self.reached[mark as usize]
    }

    /// The marks fewer than `floor` runs reached, with how many did, rarest
    /// first.
    #[must_use]
    pub fn below(&self, floor: u64) -> Vec<(Mark, u64)> {
        let mut rare = Mark::ALL
            .into_iter()
            .map(|mark| (mark, self.runs(mark)))
            .filter(|&(_, runs)| runs < floor)
            .collect::<Vec<_>>();
        rare.sort_by_key(|&(_, runs)| runs);

        rare
    }
}

impl std::fmt::Display for Reach {
    /// Each mark with how many runs reached it, rarest first.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "runs reaching each mark, of {}:", self.runs)?;
        for (mark, runs) in self.below(u64::MAX) {
            write!(formatter, "\n  {runs:>5} {mark:?}")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run counts once for each mark it reached, however often it did.
    #[test]
    fn reach_counts_runs_not_hits() {
        let mut marks = Marks::default();
        marks.hit(Mark::CoHit);
        marks.hit(Mark::CoHit);

        let mut reach = Reach::default();
        reach.add(&marks);
        marks.hit(Mark::StepStop);
        reach.add(&marks);
        assert_eq!(reach.runs(Mark::CoHit), 2);
        assert_eq!(reach.runs(Mark::StepStop), 1);
        let rare = reach.below(2);
        assert_eq!(rare.first(), Some(&(Mark::EntryStop, 0)));
        assert!(rare.contains(&(Mark::StepStop, 1)));
        assert!(!rare.iter().any(|&(mark, _)| mark == Mark::CoHit));

        let mut merged = Reach::default();
        merged.merge(&reach);
        merged.merge(&reach);
        assert_eq!(merged.runs(Mark::CoHit), 4);
    }
}
