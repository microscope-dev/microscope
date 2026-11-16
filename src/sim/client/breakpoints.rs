use std::collections::{BTreeMap, BTreeSet};

use super::{Client, protocol};
use crate::sim::choices::Stream;
use crate::sim::hits::{self, Baseline, Known, Policy};
use crate::sim::kernel::DebugBehavior;
use crate::sim::markers::Marker;
use crate::sim::marks::Mark;
use crate::sim::report::Failure;
use crate::sim::watches::{self, Intent};
use crate::{
    BreakpointId, BreakpointLocation, BreakpointOptions, BreakpointSpec, Condition, Error,
    HitComparison, HitCondition, LineNumber, LogMessage, ProcessId, StateSnapshot, WatchAccess,
    Watchpoint, WatchpointId, WatchpointOptions, WatchpointSpec,
};

pub(super) struct Added {
    id: BreakpointId,
    pub(super) traps: Vec<u64>,

    enabled: bool,
    temporary: bool,

    marker_line: Option<u64>,

    policies: Vec<Policy>,
}

impl Added {
    pub(super) const fn id(&self) -> BreakpointId {
        self.id
    }
}

impl Client {
    pub(super) async fn add_watch(&self) -> Result<(), Failure> {
        if self.script.globals.is_empty() {
            return Ok(());
        }

        let (name, image_address, size) = self
            .choices
            .borrow_mut()
            .pick(Stream::Client, &self.script.globals)
            .clone();

        let accesses = [
            WatchAccess::Change,
            WatchAccess::Change,
            WatchAccess::Write,
            WatchAccess::ReadWrite,
            WatchAccess::Read,
        ];
        let access = *self.choices.borrow_mut().pick(Stream::Client, &accesses);

        let whole = size <= 32;
        let (name, image_address, size) = if whole {
            (name, image_address, size)
        } else {
            let width = 1 + self.draw(16);
            let offset = self.draw(size - width + 1);
            (format!("{name}+{offset}"), image_address + offset, width)
        };

        let (options, policy) = self.choose_watch_options();
        let result = if whole && self.draw(2) == 0 {
            match crate::Expression::name(&name) {
                Some(expression) => self.handle.watch_with(&expression, access, options).await,
                None => {
                    return Err(Failure::simulator(
                        "client",
                        format!("`{name}` has no expression"),
                    ));
                }
            }
        } else {
            let image_address = crate::ImageAddress::new(image_address);
            let address = self.main_bias().await?.wrapping_load(image_address);
            self.handle
                .add_watchpoint_with(
                    WatchpointSpec::Location {
                        address,
                        byte_size: size,
                    },
                    access,
                    options,
                )
                .await
        };

        let wanted = (image_address + self.script.image.bias(), size);

        match result {
            Ok(watchpoint) => self.armed(&watchpoint, &name, wanted, policy)?,
            Err(Error::UnsupportedWatchAccess(WatchAccess::Read))
                if access == WatchAccess::Read =>
            {
                self.note("watching loads alone refused");
            }
            Err(Error::WatchpointCapacity { .. }) => self.note(format!("no slots for {name}")),
            Err(Error::HardwareWatchpointsUnavailable(description))
                if self.script.debug == DebugBehavior::Discarding =>
            {
                self.note(format!("watching refused: {description}"));
                self.mark(Mark::WatchRefused);
            }

            Err(Error::WatchpointHardwareBusy { thread })
                if matches!(self.script.debug, DebugBehavior::Contended(_)) =>
            {
                self.note(format!("thread {thread}'s slots are busy"));
                self.mark(Mark::WatchRefused);
            }
            Err(error) => {
                return Err(protocol(format!(
                    "watching {name} for {access} failed: {error}"
                )));
            }
        }

        Ok(())
    }

    fn armed(
        &self,
        watchpoint: &Watchpoint,
        name: &str,
        wanted: (u64, u64),
        policy: Policy,
    ) -> Result<(), Failure> {
        let (address, size) = wanted;
        let covered = (address..address + size).all(|byte| {
            watchpoint
                .coverage
                .iter()
                .any(|span| (span.start.get()..span.end.get()).contains(&byte))
        });
        if (watchpoint.address.get(), watchpoint.byte_size) != wanted || !covered {
            return Err(Failure::debugger(
                "watch accounting",
                format!(
                    "{name} is {size} bytes at {address:#x}, but watchpoint {} watches {} at {:#x} \
                     with slots over {:x?}",
                    watchpoint.id,
                    watchpoint.byte_size,
                    watchpoint.address.get(),
                    watchpoint.coverage
                ),
            ));
        }

        self.note(format!(
            "watchpoint {} on {name} for {}: {:x?} {:?} {:?}",
            watchpoint.id,
            watchpoint.access,
            watchpoint.coverage,
            watchpoint.hit_condition,
            watchpoint.condition
        ));
        self.shared.watches.borrow_mut().insert(
            watchpoint.id.get(),
            Intent {
                id: watchpoint.id.get(),
                address: watchpoint.address.get(),
                size: watchpoint.byte_size,
                access: watchpoint.access,
                spans: watchpoint
                    .coverage
                    .iter()
                    .map(|span| (span.start.get(), span.end.get()))
                    .collect(),
                policies: vec![policy],
            },
        );
        self.mark(Mark::WatchAdded);
        Ok(())
    }

    fn choose_watch_options(&self) -> (WatchpointOptions, Policy) {
        if self.draw(2) == 0 {
            return (WatchpointOptions::default(), UNCONDITIONAL);
        }

        let condition = self.choose_condition(None);
        let options = WatchpointOptions {
            hit_condition: self.choose_hit_condition(),
            condition: condition.parse(&self.script.markers, None),
            ..WatchpointOptions::default()
        };

        let policy = Policy {
            hit_condition: options.hit_condition,
            condition: self.known(condition, None, &[]),
            logs: false,
            enabled: true,
        };

        (options, policy)
    }

    pub(super) async fn amend_watch(&self) -> Result<(), Failure> {
        let ids = self
            .shared
            .watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }

        let id = *self.choices.borrow_mut().pick(Stream::Client, &ids);
        let mut policy = *self.shared.watches.borrow()[&id]
            .policies
            .last()
            .expect("a watch has a policy");
        let watchpoint = WatchpointId::new(id);
        let request = if self.draw(2) == 0 {
            let hit_condition = self.choose_hit_condition();
            policy.hit_condition = hit_condition;
            Ok(hit_condition)
        } else {
            let choice = self.choose_condition(None);
            policy.condition = self.known(choice, None, &[]);
            Err(choice.parse(&self.script.markers, None))
        };

        if let Some(intent) = self.shared.watches.borrow_mut().get_mut(&id) {
            intent.policies.push(policy);
        }

        let amended = match request {
            Ok(hit_condition) => {
                self.handle
                    .set_watchpoint_hit_condition(watchpoint, hit_condition)
                    .await
            }
            Err(condition) => {
                self.handle
                    .set_watchpoint_condition(watchpoint, condition)
                    .await
            }
        }
        .map_err(|error| protocol(format!("amending watchpoint {id} failed: {error}")))?;

        self.note(format!(
            "watchpoint {id} now {:?} {:?}",
            amended.hit_condition, amended.condition
        ));
        self.mark(Mark::WatchAmended);
        Ok(())
    }

    pub(super) async fn toggle_watch(&self, running: bool) -> Result<(), Failure> {
        let disabled = self
            .disabled_watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if !running && !disabled.is_empty() {
            let id = *self.choices.borrow_mut().pick(Stream::Control, &disabled);
            return self.enable_watch(id).await;
        }

        let ids = self
            .shared
            .watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }

        let id = *self.choices.borrow_mut().pick(Stream::Control, &ids);

        let intent = self
            .shared
            .watches
            .borrow_mut()
            .remove(&id)
            .expect("a known watch");
        let watchpoint = match self
            .handle
            .set_watchpoint_enabled(WatchpointId::new(id), false)
            .await
        {
            Ok(watchpoint) => watchpoint,
            Err(error) => {
                self.shared.watches.borrow_mut().insert(id, intent);
                return Err(protocol(format!(
                    "disabling watchpoint {id} failed: {error}"
                )));
            }
        };

        if watchpoint.enabled {
            return Err(protocol(format!(
                "disabling watchpoint {id} returned {watchpoint:?}"
            )));
        }

        self.disabled_watches
            .borrow_mut()
            .insert(id, (intent, watchpoint.hit_count));

        if running {
            self.released_while_running.set(true);
        }

        self.note(format!("disabled watchpoint {id}"));
        self.mark(Mark::WatchDisabled);
        Ok(())
    }

    async fn enable_watch(&self, id: u64) -> Result<(), Failure> {
        match self
            .handle
            .set_watchpoint_enabled(WatchpointId::new(id), true)
            .await
        {
            Ok(watchpoint) if watchpoint.enabled => {
                let (mut intent, _) = self
                    .disabled_watches
                    .borrow_mut()
                    .remove(&id)
                    .expect("a disabled watch");
                intent.policies.drain(..intent.policies.len() - 1);
                self.shared.watches.borrow_mut().insert(id, intent);
                self.note(format!("enabled watchpoint {id}"));
                self.mark(Mark::WatchEnabled);
            }
            Err(Error::WatchpointCapacity { .. }) => {
                self.note(format!("no slots to enable watchpoint {id}"));
            }

            Err(Error::HardwareWatchpointsUnavailable(description))
                if self.script.debug == DebugBehavior::Discarding =>
            {
                self.note(format!("enabling refused: {description}"));
            }

            Err(Error::WatchpointHardwareBusy { thread })
                if matches!(self.script.debug, DebugBehavior::Contended(_)) =>
            {
                self.note(format!("thread {thread}'s slots are busy"));
            }
            other => {
                return Err(protocol(format!(
                    "enabling watchpoint {id} returned {other:?}"
                )));
            }
        }

        Ok(())
    }

    fn judge_disabled_watches(&self, snapshot: &StateSnapshot) -> Result<(), Failure> {
        let mut disabled = self.disabled_watches.borrow_mut();
        disabled.retain(|id, _| {
            snapshot
                .watchpoints
                .iter()
                .any(|watchpoint| watchpoint.id.get() == *id)
        });

        let counted = disabled
            .iter()
            .map(|(&id, (_, counted))| (id, *counted))
            .collect();

        watches::judge_disabled(&counted, snapshot)
            .map_err(|message| Failure::debugger("watch accounting", message))
    }

    pub(super) async fn remove_watch(&self) -> Result<(), Failure> {
        let ids = self
            .shared
            .watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }

        let id = *self.choices.borrow_mut().pick(Stream::Client, &ids);

        self.shared.watches.borrow_mut().remove(&id);
        let id = crate::WatchpointId::new(id);
        self.handle
            .remove_watchpoint(id)
            .await
            .map_err(|error| protocol(format!("removing watchpoint {id} failed: {error}")))?;

        self.note(format!("removed watchpoint {id}"));
        Ok(())
    }

    fn choose_early_spec(&self) -> BreakpointSpec {
        let markers = self.script.markers.keys().copied().collect::<Vec<_>>();
        if markers.is_empty() || self.draw(4) == 0 {
            return self.choose_spec(Stream::Client);
        }

        let line = *self.choices.borrow_mut().pick(Stream::Client, &markers);
        BreakpointSpec::Source {
            path: self.script.source.clone(),
            line: LineNumber::new(line).expect("lines count from one"),
        }
    }

    pub(super) fn choose_spec(&self, stream: Stream) -> BreakpointSpec {
        let draw = |bound| self.choices.borrow_mut().below(stream, bound);
        if draw(2) == 0 {
            let function = self
                .choices
                .borrow_mut()
                .pick(stream, &self.script.functions)
                .clone();
            BreakpointSpec::Function(function)
        } else {
            let marker_lines = self.script.markers.keys().copied().collect::<Vec<_>>();
            let line = if !marker_lines.is_empty() && draw(2) == 0 {
                *self.choices.borrow_mut().pick(stream, &marker_lines)
            } else {
                draw(self.script.source_lines) + 1
            };

            BreakpointSpec::Source {
                path: self.script.source.clone(),
                line: LineNumber::new(line).expect("lines count from one"),
            }
        }
    }

    pub(super) async fn add_breakpoint(
        &self,
        breakpoints: &mut Vec<Added>,
        early: bool,
    ) -> Result<bool, Failure> {
        let spec = if early {
            self.choose_early_spec()
        } else {
            self.choose_spec(Stream::Client)
        };
        let marker_line = match &spec {
            BreakpointSpec::Source { line, .. } => {
                Some(line.get()).filter(|line| self.script.markers.contains_key(line))
            }
            _ => None,
        };

        let (options, condition) = self.choose_options(marker_line, early);
        let breakpoint = match self.handle.add_breakpoint_with(spec.clone(), options).await {
            Ok(breakpoint) => breakpoint,
            Err(error) if self.names_no_code(&spec, &error) => return Ok(false),
            Err(error) => {
                return Err(protocol(format!(
                    "adding a breakpoint at {spec} failed: {error}"
                )));
            }
        };

        self.note(format!(
            "breakpoint {} at {spec}: {} locations, {:?} {:?}{}",
            breakpoint.id,
            breakpoint.locations.len(),
            breakpoint.hit_condition,
            breakpoint.condition,
            if breakpoint.log_message.is_some() {
                ", logging"
            } else {
                ""
            }
        ));

        self.judge_locations(&breakpoint)?;
        let traps = image_traps(&breakpoint);

        if !breakpoint.enabled {
            self.shared
                .disabled
                .borrow_mut()
                .insert(breakpoint.id.get());
        } else if breakpoint.temporary {
            self.shared
                .temporaries
                .borrow_mut()
                .insert(breakpoint.id.get(), traps.iter().copied().collect());
        } else {
            self.shared
                .breakpoints
                .borrow_mut()
                .insert(breakpoint.id.get(), traps.iter().copied().collect());
        }

        let policy = Policy {
            hit_condition: breakpoint.hit_condition,
            condition: self.known(condition, marker_line, &traps),
            logs: breakpoint.log_message.is_some(),
            enabled: breakpoint.enabled,
        };

        if let Some(existing) = breakpoints
            .iter_mut()
            .find(|added| added.id == breakpoint.id)
        {
            existing.policies.push(policy);
        } else {
            breakpoints.push(Added {
                id: breakpoint.id,
                traps,
                enabled: breakpoint.enabled,
                temporary: breakpoint.temporary,
                marker_line,
                policies: vec![policy],
            });
        }

        if breakpoint.temporary {
            self.mark(Mark::TemporaryAdded);
        }

        Ok(true)
    }

    pub(super) fn names_no_code(&self, spec: &BreakpointSpec, error: &Error) -> bool {
        match error {
            Error::SourceLineUnavailable { line, .. }
                if matches!(spec, BreakpointSpec::Source { line: wanted, .. } if wanted.get() == *line)
                    && !self
                        .script
                        .statements
                        .values()
                        .any(|lines| lines.contains(line)) =>
            {
                self.note(format!("{spec} has no code"));
                true
            }

            Error::FunctionNotFound(name) | Error::SymbolNotFound(name)
                if matches!(spec, BreakpointSpec::Function(wanted) if wanted == name)
                    && !self.script.defined.contains(name) =>
            {
                self.note(format!("{name} is not in this variant"));
                true
            }
            _ => false,
        }
    }

    pub(super) async fn toggle_breakpoint(
        &self,
        breakpoints: &mut [Added],
    ) -> Result<bool, Failure> {
        if breakpoints.is_empty() {
            return Ok(false);
        }

        let index = usize::try_from(self.control(breakpoints.len() as u64)).expect("small");
        let added = &mut breakpoints[index];
        let (id, enabled) = (added.id, !added.enabled);

        if enabled {
            self.shared.disabled.borrow_mut().remove(&id.get());
        } else {
            self.shared.breakpoints.borrow_mut().remove(&id.get());
            self.shared.temporaries.borrow_mut().remove(&id.get());
        }

        let mut policy = *added.policies.last().expect("a breakpoint has a policy");
        policy.enabled = enabled;
        added.policies.push(policy);

        let breakpoint = match self.handle.set_breakpoint_enabled(id, enabled).await {
            Ok(breakpoint) => breakpoint,
            Err(error) if self.deleted_by_stop(added.temporary, &error) => return Ok(false),
            Err(error) => {
                policy.enabled = !enabled;
                added.policies.push(policy);
                if enabled {
                    self.shared.disabled.borrow_mut().insert(id.get());
                } else {
                    let enabled = if added.temporary {
                        &self.shared.temporaries
                    } else {
                        &self.shared.breakpoints
                    };
                    enabled
                        .borrow_mut()
                        .insert(id.get(), added.traps.iter().copied().collect());
                }

                return Err(protocol(format!(
                    "{} breakpoint {id} failed: {error}",
                    if enabled { "enabling" } else { "disabling" }
                )));
            }
        };

        if breakpoint.enabled != enabled {
            return Err(protocol(format!(
                "setting breakpoint {id} enabled {enabled} returned {breakpoint:?}"
            )));
        }

        added.enabled = enabled;
        if enabled {
            added.traps = image_traps(&breakpoint);
            let enabled = if added.temporary {
                &self.shared.temporaries
            } else {
                &self.shared.breakpoints
            };
            enabled
                .borrow_mut()
                .insert(id.get(), added.traps.iter().copied().collect());

            self.mark(Mark::BreakpointEnabled);
        } else {
            self.shared.disabled.borrow_mut().insert(id.get());
            self.mark(Mark::BreakpointDisabled);
        }

        self.note(format!(
            "breakpoint {id} {}",
            if enabled { "enabled" } else { "disabled" }
        ));
        Ok(true)
    }

    fn choose_hit_condition(&self) -> Option<HitCondition> {
        if self.draw(2) == 0 {
            return None;
        }

        let comparisons = [
            HitComparison::Equal,
            HitComparison::NotEqual,
            HitComparison::Less,
            HitComparison::LessOrEqual,
            HitComparison::Greater,
            HitComparison::GreaterOrEqual,
            HitComparison::Multiple,
        ];
        let comparison = *self.choices.borrow_mut().pick(Stream::Client, &comparisons);
        HitCondition::new(comparison, self.draw(4) + 1).ok()
    }

    fn choose_condition(&self, marker_line: Option<u64>) -> ConditionChoice {
        let choices = if marker_line.is_some() {
            &[
                ConditionChoice::None,
                ConditionChoice::True,
                ConditionChoice::False,
                ConditionChoice::Marker,
                ConditionChoice::NotMarker,
            ][..]
        } else {
            &[
                ConditionChoice::None,
                ConditionChoice::True,
                ConditionChoice::False,
            ][..]
        };
        *self.choices.borrow_mut().pick(Stream::Client, choices)
    }

    fn known(&self, condition: ConditionChoice, marker_line: Option<u64>, traps: &[u64]) -> Known {
        let at_marker = marker_line.is_some_and(|line| {
            !traps.is_empty()
                && traps
                    .iter()
                    .all(|trap| self.script.marker_rows.get(trap) == Some(&line))
        });
        match condition {
            ConditionChoice::None => Known::Absent,
            ConditionChoice::True => Known::Holds,
            ConditionChoice::False => Known::Fails,
            ConditionChoice::Marker if at_marker => Known::Holds,
            ConditionChoice::NotMarker if at_marker => Known::Fails,
            ConditionChoice::Marker | ConditionChoice::NotMarker => Known::Unknown,
        }
    }

    fn choose_options(
        &self,
        marker_line: Option<u64>,
        early: bool,
    ) -> (BreakpointOptions, ConditionChoice) {
        let (options, condition) = if self.draw(2) == 0 {
            (BreakpointOptions::default(), ConditionChoice::None)
        } else {
            let condition = self.choose_condition(marker_line);
            let options = BreakpointOptions {
                hit_condition: self.choose_hit_condition(),
                condition: condition.parse(&self.script.markers, marker_line),
                log_message: (self.draw(3) == 0)
                    .then(|| LogMessage::parse("hit").expect("a valid message")),
                ..BreakpointOptions::default()
            };

            (options, condition)
        };

        let options = BreakpointOptions {
            enabled: self.control(16) != 0,
            temporary: self.control(if early { 3 } else { 4 }) == 0,
            ..options
        };

        (options, condition)
    }

    pub(super) async fn amend_breakpoint(&self, breakpoints: &mut [Added]) -> Result<(), Failure> {
        if breakpoints.is_empty() {
            return Ok(());
        }

        let index = usize::try_from(self.draw(breakpoints.len() as u64)).expect("small");
        let added = &mut breakpoints[index];
        let id = added.id;
        let mut policy = *added.policies.last().expect("a breakpoint has a policy");
        let amended = if self.draw(2) == 0 {
            let hit_condition = self.choose_hit_condition();
            policy.hit_condition = hit_condition;
            self.handle
                .set_breakpoint_hit_condition(id, hit_condition)
                .await
        } else {
            let choice = self.choose_condition(added.marker_line);
            policy.condition = self.known(choice, added.marker_line, &added.traps);
            self.handle
                .set_breakpoint_condition(id, choice.parse(&self.script.markers, added.marker_line))
                .await
        };

        let breakpoint = match amended {
            Ok(breakpoint) => breakpoint,
            Err(error) if self.deleted_by_stop(added.temporary, &error) => return Ok(()),
            Err(error) => {
                return Err(protocol(format!(
                    "amending breakpoint {id} failed: {error}"
                )));
            }
        };

        self.note(format!(
            "breakpoint {id} now {:?} {:?}",
            breakpoint.hit_condition, breakpoint.condition
        ));
        added.policies.push(policy);
        self.mark(Mark::BreakpointAmended);
        Ok(())
    }

    fn deleted_by_stop(&self, temporary: bool, error: &Error) -> bool {
        let gone = temporary && matches!(error, Error::BreakpointNotFound(_));
        if gone {
            self.note(format!("{error}: a stop deleted it"));
        }

        gone
    }

    pub(super) fn judge_hits(
        &self,
        process: ProcessId,
        snapshot: &StateSnapshot,
        breakpoints: &mut Vec<Added>,
    ) -> Result<(), Failure> {
        let ending = self.ending(process);
        let published = self.shared.published.borrow().clone();
        let policies = breakpoints
            .iter()
            .map(|added| {
                (
                    added.id.get(),
                    hits::Kept {
                        versions: added.policies.clone(),
                        temporary: added.temporary,
                    },
                )
            })
            .collect();

        if !ending {
            let found = hits::judge(
                self.baseline.borrow().as_ref(),
                process,
                snapshot,
                &policies,
                &published,
            )
            .map_err(|message| Failure::debugger("breakpoint conditions", message))?;
            if found.declined {
                self.mark(Mark::HitDeclined);
            }

            if found.held {
                self.mark(Mark::ConditionHeld);
            }

            if found.logged {
                self.mark(Mark::HitLogged);
            }

            if found.temporary_stopped {
                self.mark(Mark::TemporaryStop);
            }

            if found.temporary_shared {
                self.mark(Mark::TemporaryCoHit);
            }

            self.judge_disabled_watches(snapshot)?;
        }

        for breakpoint in snapshot.breakpoints.iter() {
            self.judge_locations(breakpoint)?;
        }

        let remaining = |added: &Added| {
            snapshot
                .breakpoints
                .iter()
                .any(|breakpoint| breakpoint.id == added.id)
        };

        breakpoints.retain(|added| !added.temporary || remaining(added));
        self.forget_deleted(snapshot);

        *self.baseline.borrow_mut() = Some(Baseline {
            process,
            counts: snapshot
                .breakpoints
                .iter()
                .map(|breakpoint| (breakpoint.id.get(), breakpoint.hit_count))
                .collect(),
            published,
        });

        for added in breakpoints.iter_mut() {
            let current = *added.policies.last().expect("a breakpoint has a policy");
            added.policies = vec![current];
        }

        Ok(())
    }

    pub(super) async fn remove_breakpoint(
        &self,
        breakpoints: &mut Vec<Added>,
    ) -> Result<bool, Failure> {
        if breakpoints.is_empty() {
            return Ok(false);
        }

        let index = usize::try_from(self.draw(breakpoints.len() as u64)).expect("small");
        let removed = breakpoints.swap_remove(index);
        let id = removed.id;

        self.shared.breakpoints.borrow_mut().remove(&id.get());
        self.shared.temporaries.borrow_mut().remove(&id.get());
        self.shared.disabled.borrow_mut().remove(&id.get());

        match self.handle.remove_breakpoint(id).await {
            Ok(_) => {}
            Err(error) if self.deleted_by_stop(removed.temporary, &error) => return Ok(true),
            Err(error) => {
                return Err(protocol(format!(
                    "removing breakpoint {id} failed: {error}"
                )));
            }
        }

        self.note(format!("removed breakpoint {id}"));
        Ok(true)
    }

    pub(super) async fn resume_unarmed(&self) -> Result<(), Failure> {
        match self.handle.resume().await {
            Err(error) if self.refused_unarmed(&error).await? => Ok(()),
            other => Err(protocol(format!(
                "resuming with a thread that cannot be armed returned {other:?}"
            ))),
        }
    }

    pub(super) async fn refused_unarmed(&self, error: &Error) -> Result<bool, Failure> {
        let Error::WatchpointHardwareBusy { thread } = error else {
            return Ok(false);
        };

        if !matches!(self.script.debug, DebugBehavior::Contended(_)) {
            return Ok(false);
        }

        self.note(format!("running refused: thread {thread} cannot be armed"));
        self.mark(Mark::RunRefusedUnarmed);
        self.remove_watch().await?;
        Ok(true)
    }
}

const UNCONDITIONAL: Policy = Policy {
    hit_condition: None,
    condition: Known::Absent,
    logs: false,
    enabled: true,
};

fn image_traps(breakpoint: &crate::Breakpoint) -> Vec<u64> {
    breakpoint
        .locations
        .iter()
        .filter_map(|location| match location.location {
            BreakpointLocation::Image(address) => Some(address.get()),
            BreakpointLocation::Virtual(_) | BreakpointLocation::Managed(_) => None,
        })
        .collect()
}

impl Client {
    pub(super) fn forget_deleted(&self, snapshot: &StateSnapshot) {
        self.shared.temporaries.borrow_mut().retain(|&id, _| {
            snapshot
                .breakpoints
                .iter()
                .any(|breakpoint| breakpoint.id.get() == id)
        });
    }

    pub(super) fn judge_locations(&self, breakpoint: &crate::Breakpoint) -> Result<(), Failure> {
        misplaced(
            &breakpoint.spec,
            &breakpoint.locations,
            &self.script.statements,
            &self.script.call_sites,
            self.script.unoptimized_functions.as_deref(),
        )
        .map_or(Ok(()), |message| {
            Err(Failure::debugger(
                "breakpoint locations",
                format!(
                    "breakpoint {} at {}: {message}",
                    breakpoint.id, breakpoint.spec
                ),
            ))
        })
    }
}

fn misplaced(
    spec: &BreakpointSpec,
    locations: &[crate::ResolvedBreakpointLocation],
    statements: &BTreeMap<u64, BTreeSet<u64>>,
    call_sites: &BTreeMap<u64, BTreeSet<u64>>,
    functions: Option<&[(String, std::ops::Range<u64>)]>,
) -> Option<String> {
    let mut addresses = Vec::new();
    for location in locations {
        match &location.location {
            BreakpointLocation::Image(address) => addresses.push(address.get()),
            BreakpointLocation::Virtual(address) => {
                return Some(format!("{:#x} is in no image", address.get()));
            }

            BreakpointLocation::Managed(location) => {
                return Some(format!("a managed location {location:?}"));
            }
        }
    }

    match spec {
        BreakpointSpec::Source { line, .. } => {
            let line = line.get();
            let stops = statements
                .values()
                .flatten()
                .chain(call_sites.keys())
                .copied()
                .filter(|&known| known >= line)
                .min();
            let has_statements =
                |stops: u64| statements.values().any(|lines| lines.contains(&stops));

            addresses
                .iter()
                .find(|address| {
                    stops.is_none_or(|stops| {
                        let statement = statements
                            .get(address)
                            .is_some_and(|lines| lines.contains(&stops));
                        let call = !has_statements(stops)
                            && call_sites
                                .get(&stops)
                                .is_some_and(|entries| entries.contains(address));
                        !statement && !call
                    })
                })
                .map(|address| {
                    stops.map_or_else(
                        || format!("{address:#x} is past the source's last statement"),
                        |stops| {
                            format!(
                                "{address:#x} begins no statement row or inlined call of \
                                 line {stops}, the first at or after {line} with either"
                            )
                        },
                    )
                })
        }
        BreakpointSpec::Function(name) => {
            let functions = functions?;
            addresses
                .iter()
                .find(|&&address| {
                    !functions
                        .iter()
                        .any(|(known, range)| known == name && range.contains(&address))
                })
                .map(|address| format!("{address:#x} is outside every function named {name}"))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionChoice {
    None,
    True,
    False,

    Marker,

    NotMarker,
}

impl ConditionChoice {
    fn parse(self, markers: &BTreeMap<u64, Marker>, line: Option<u64>) -> Option<Condition> {
        let marker = || {
            line.and_then(|line| markers.get(&line))
                .expect("marker conditions are chosen at marker lines")
        };

        let text = match self {
            Self::None => return None,
            Self::True => "true".to_owned(),
            Self::False => "false".to_owned(),
            Self::Marker => marker().text.clone(),
            Self::NotMarker => format!("!({})", marker().text),
        };

        Some(Condition::parse(&text).expect("the client's conditions parse"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn at(address: u64) -> crate::ResolvedBreakpointLocation {
        crate::ResolvedBreakpointLocation {
            location: BreakpointLocation::Image(crate::ImageAddress::new(address)),
            code_instances: Arc::from([]),
            library: None,
        }
    }

    #[test]
    fn misplaced_breakpoints_are_named() {
        let statements = BTreeMap::from([
            (0x10, BTreeSet::from([4])),
            (0x20, BTreeSet::from([6, 2])),
            (0x30, BTreeSet::from([6])),
        ]);
        let call_sites = BTreeMap::from([(4, BTreeSet::from([0x18])), (7, BTreeSet::from([0x38]))]);
        let functions = [("f".to_owned(), 0x10..0x40)];
        let line = |line| BreakpointSpec::Source {
            path: "a.c".into(),
            line: LineNumber::new(line).expect("a line"),
        };
        let judge = |spec: &BreakpointSpec, addresses: &[u64]| {
            let locations = addresses
                .iter()
                .map(|&address| at(address))
                .collect::<Vec<_>>();
            misplaced(spec, &locations, &statements, &call_sites, Some(&functions))
        };

        assert_eq!(judge(&line(6), &[0x20, 0x30]), None);
        assert_eq!(judge(&line(2), &[0x20]), None);
        assert_eq!(judge(&line(5), &[0x30]), None, "5 has none, so 6's");
        assert!(judge(&line(6), &[0x10]).is_some(), "0x10 is line 4's");
        assert!(judge(&line(3), &[0x20]).is_some(), "3 slides to 4");
        assert_eq!(judge(&line(7), &[0x38]), None, "7 calls inlined code");
        assert!(judge(&line(7), &[0x30]).is_some(), "0x30 is line 6's");
        assert!(judge(&line(8), &[0x38]).is_some(), "nothing follows 7");
        assert!(judge(&line(4), &[0x18]).is_some(), "4 has a statement");

        let function = BreakpointSpec::Function("f".to_owned());
        assert_eq!(judge(&function, &[0x10]), None);
        assert!(judge(&function, &[0x40]).is_some());
        assert!(judge(&BreakpointSpec::Function("g".to_owned()), &[0x10]).is_some());

        let other = crate::ResolvedBreakpointLocation {
            location: BreakpointLocation::Virtual(crate::VirtualAddress::new(0x20)),
            ..at(0)
        };
        assert!(misplaced(&line(6), &[other], &statements, &call_sites, None).is_some());
    }
}
