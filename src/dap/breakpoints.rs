//! The client's breakpoints over the debugger's logical breakpoints.
//!
//! The client replaces whole groups of breakpoints at once: every
//! breakpoint of one source, or every function breakpoint. Each client
//! breakpoint keeps its id while it is re-sent, even with new conditions,
//! and the debugger's breakpoints are shared by reference count, since the
//! debugger merges equal requests into one breakpoint.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use microscope::{BreakpointHit, BreakpointId};

/// A set of breakpoints the client replaces as one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    /// The breakpoints of one source file, named by the client's path.
    Source(PathBuf),
    Functions,
    Instructions,

    /// Breakpoints made with console commands, which the client only hears
    /// about through events.
    Console,
}

/// What a client breakpoint is placed at, within its group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// A one-based source line.
    Line(u64),

    /// A function, or anything else the console's `break` accepts.
    Function(String),

    /// An address: a memory reference and a byte offset from it.
    Instruction(u64),

    /// A debugger breakpoint made from the console.
    Console(BreakpointId),

    /// A location the client named that names nothing, with the reason.
    Invalid(String),
}

/// One breakpoint as the client asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub key: Key,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

impl Want {
    /// A breakpoint with conditions, where blank ones count as none.
    pub fn new(
        key: Key,
        condition: Option<String>,
        hit_condition: Option<String>,
        log_message: Option<String>,
    ) -> Self {
        let given = |text: Option<String>| text.filter(|text| !text.trim().is_empty());
        Self {
            key,
            condition: given(condition),
            hit_condition: given(hit_condition),
            log_message,
        }
    }

    pub const fn at(key: Key) -> Self {
        Self {
            key,
            condition: None,
            hit_condition: None,
            log_message: None,
        }
    }
}

/// Where a breakpoint resolved, as the client is told.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placement {
    /// The local source file and one-based line of its first location.
    pub source: Option<(PathBuf, u64)>,

    /// The address of its first location.
    pub address: Option<u64>,
}

/// Whether a client breakpoint is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Resolved {
        breakpoint: BreakpointId,
        placement: Placement,
        message: Option<String>,
    },

    /// Configured in the debugger, but no location is ready: code has not
    /// loaded or its runtime has not accepted arming.
    Pending {
        breakpoint: BreakpointId,
        message: String,
    },

    /// Installed in the debugger but disabled there, from the console,
    /// whose `enable` is the only way back: the protocol has no enabled
    /// flag.
    Disabled { breakpoint: BreakpointId },
    Unresolved {
        message: String,

        /// Whether it may still resolve later, such as once the program
        /// is loaded, rather than having failed.
        pending: bool,
    },
}

/// One client breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub want: Want,
    pub state: State,
}

impl Entry {
    pub const fn breakpoint(&self) -> Option<BreakpointId> {
        match &self.state {
            State::Resolved { breakpoint, .. }
            | State::Pending { breakpoint, .. }
            | State::Disabled { breakpoint } => Some(*breakpoint),
            State::Unresolved { .. } => None,
        }
    }
}

/// What replacing a group takes: the entries to release, then each new
/// entry in request order, kept or to be resolved.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub release: Vec<Entry>,
    pub slots: Vec<Slot>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Slot {
    Keep(Entry),
    Resolve { id: i64, want: Want },
}

/// A change made outside the client's requests, such as from the console.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    New(Entry),
    Removed(Entry),
}

#[derive(Debug)]
pub struct Breakpoints {
    next_id: i64,
    groups: BTreeMap<Group, Vec<Entry>>,
    owners: HashMap<BreakpointId, usize>,
}

impl Default for Breakpoints {
    fn default() -> Self {
        Self {
            next_id: 1,
            groups: BTreeMap::new(),
            owners: HashMap::new(),
        }
    }
}

impl Breakpoints {
    /// Takes a group's entries out and plans replacing them with `wants`.
    /// An entry re-sent unchanged and resolved is kept; one re-sent with
    /// other conditions, or still unresolved, keeps its id and is resolved
    /// again; the rest are released.
    pub fn plan(&mut self, group: &Group, wants: Vec<Want>) -> Plan {
        let mut current = self
            .groups
            .remove(group)
            .unwrap_or_default()
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
        let mut release = Vec::new();
        let mut slots = Vec::with_capacity(wants.len());

        for want in wants {
            let matched = current
                .iter_mut()
                .find(|entry| {
                    entry
                        .as_ref()
                        .is_some_and(|entry| entry.want.key == want.key)
                })
                .and_then(Option::take);
            slots.push(match matched {
                Some(entry) if entry.want == want && entry.breakpoint().is_some() => {
                    Slot::Keep(entry)
                }
                Some(entry) => {
                    let id = entry.id;
                    release.push(entry);
                    Slot::Resolve { id, want }
                }
                None => Slot::Resolve {
                    id: self.allocate_id(),
                    want,
                },
            });
        }

        release.extend(current.into_iter().flatten());
        Plan { release, slots }
    }

    /// Stores a group's entries after its plan was carried out.
    pub fn install(&mut self, group: Group, entries: Vec<Entry>) {
        if entries.is_empty() {
            self.groups.remove(&group);
        } else {
            self.groups.insert(group, entries);
        }
    }

    /// Stores a group's entries after its plan failed part way: those it
    /// kept, and those it had yet to release, whose owners still count.
    /// One it released before is gone, as is one it had yet to resolve.
    pub fn abandon(
        &mut self,
        group: Group,
        slots: Vec<Slot>,
        unreleased: impl IntoIterator<Item = Entry>,
    ) {
        let entries = slots
            .into_iter()
            .filter_map(|slot| match slot {
                Slot::Keep(entry) => Some(entry),
                Slot::Resolve { .. } => None,
            })
            .chain(unreleased)
            .collect();
        self.install(group, entries);
    }

    /// Allocates an id from the one space every kind of breakpoint shares.
    pub const fn allocate_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Records one more owner of a debugger breakpoint.
    pub fn acquire(&mut self, breakpoint: BreakpointId) {
        *self.owners.entry(breakpoint).or_default() += 1;
    }

    /// Releases one owner and returns whether it was the last, so the
    /// debugger breakpoint should be removed.
    pub fn release(&mut self, breakpoint: BreakpointId) -> bool {
        let Some(count) = self.owners.get_mut(&breakpoint) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.owners.remove(&breakpoint);
            true
        } else {
            false
        }
    }

    /// Every entry, with its group.
    pub fn entries(&self) -> impl Iterator<Item = (&Group, &Entry)> {
        self.groups
            .iter()
            .flat_map(|(group, entries)| entries.iter().map(move |entry| (group, entry)))
    }

    /// A copy of every entry with its group, to update while iterating.
    pub fn owned_entries(&self) -> Vec<(Group, Entry)> {
        self.entries()
            .map(|(group, entry)| (group.clone(), entry.clone()))
            .collect()
    }

    /// Every unresolved entry with its group, to be resolved again and put
    /// back with [`Self::replace`].
    pub fn unresolved(&self) -> Vec<(Group, Entry)> {
        self.entries()
            .filter(|(_, entry)| entry.breakpoint().is_none())
            .map(|(group, entry)| (group.clone(), entry.clone()))
            .collect()
    }

    /// Replaces the entry with `id` in its group.
    pub fn replace(&mut self, group: &Group, replacement: Entry) {
        if let Some(entry) = self
            .groups
            .get_mut(group)
            .and_then(|entries| entries.iter_mut().find(|entry| entry.id == replacement.id))
        {
            *entry = replacement;
        }
    }

    /// The client's ids of the breakpoints a stop hit, and the stop reason
    /// they make: a function or instruction breakpoint when every one is of
    /// that kind.
    pub fn hit(&self, hits: &[BreakpointHit]) -> (Vec<i64>, &'static str) {
        let hit = hits
            .iter()
            .map(|hit| hit.breakpoint)
            .collect::<BTreeSet<_>>();
        let mut ids = Vec::new();
        let mut groups = BTreeSet::new();

        for (group, entry) in self.entries() {
            if entry
                .breakpoint()
                .is_some_and(|breakpoint| hit.contains(&breakpoint))
            {
                ids.push(entry.id);
                groups.insert(match group {
                    Group::Functions => "function breakpoint",
                    Group::Instructions => "instruction breakpoint",
                    Group::Source(_) | Group::Console => "breakpoint",
                });
            }
        }

        ids.sort_unstable();
        let reason = match groups.len() {
            1 => groups.pop_first().unwrap_or("breakpoint"),
            _ => "breakpoint",
        };

        (ids, reason)
    }

    /// Reconciles with the debugger's breakpoints after they changed:
    /// breakpoints nobody here owns, made from the console, become console
    /// entries, and entries whose breakpoint is gone are dropped.
    pub fn sync(&mut self, current: &[microscope::Breakpoint]) -> Vec<Change> {
        let existing = current
            .iter()
            .map(|breakpoint| breakpoint.id)
            .collect::<BTreeSet<_>>();
        let mut changes = Vec::new();

        for entries in self.groups.values_mut() {
            entries.retain(|entry| match entry.breakpoint() {
                Some(breakpoint) if !existing.contains(&breakpoint) => {
                    changes.push(Change::Removed(entry.clone()));
                    false
                }
                _ => true,
            });
        }

        for change in &changes {
            if let Change::Removed(entry) = change
                && let Some(breakpoint) = entry.breakpoint()
            {
                self.owners.remove(&breakpoint);
            }
        }

        self.groups.retain(|_, entries| !entries.is_empty());

        for breakpoint in current {
            if self.owners.contains_key(&breakpoint.id) {
                continue;
            }

            let entry = Entry {
                id: self.allocate_id(),
                want: Want::at(Key::Console(breakpoint.id)),
                state: State::Resolved {
                    breakpoint: breakpoint.id,
                    placement: Placement::default(),
                    message: None,
                },
            };

            self.acquire(breakpoint.id);
            self.groups
                .entry(Group::Console)
                .or_default()
                .push(entry.clone());
            changes.push(Change::New(entry));
        }

        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(number: u64) -> Want {
        Want::at(Key::Line(number))
    }

    fn resolved(id: i64, want: Want, breakpoint: u64) -> Entry {
        Entry {
            id,
            want,
            state: State::Resolved {
                breakpoint: BreakpointId::new(breakpoint),
                placement: Placement::default(),
                message: None,
            },
        }
    }

    /// Carries out a plan as the session does, resolving each line to a
    /// debugger breakpoint numbered after it.
    fn apply(breakpoints: &mut Breakpoints, group: &Group, wants: Vec<Want>) -> Vec<Entry> {
        let plan = breakpoints.plan(group, wants);
        for entry in &plan.release {
            if let Some(breakpoint) = entry.breakpoint() {
                breakpoints.release(breakpoint);
            }
        }

        let entries = plan
            .slots
            .into_iter()
            .map(|slot| match slot {
                Slot::Keep(entry) => entry,
                Slot::Resolve { id, want } => {
                    let Key::Line(number) = want.key else {
                        panic!("lines only")
                    };

                    breakpoints.acquire(BreakpointId::new(number));
                    resolved(id, want, number)
                }
            })
            .collect::<Vec<_>>();
        breakpoints.install(group.clone(), entries.clone());
        entries
    }

    #[test]
    fn ids_survive_resends_and_amendments_and_responses_follow_request_order() {
        let mut breakpoints = Breakpoints::default();
        let file = Group::Source("/a.c".into());
        let first = apply(&mut breakpoints, &file, vec![line(10), line(20)]);
        assert_eq!(
            first.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [1, 2]
        );

        // Re-sent in another order, with one amended and one added.
        let amended = Want {
            hit_condition: Some(">=2".to_owned()),
            ..line(10)
        };

        let plan = breakpoints.plan(&file, vec![line(20), amended.clone(), line(30)]);
        assert_eq!(plan.release, [first[0].clone()]);
        assert_eq!(
            plan.slots,
            [
                Slot::Keep(first[1].clone()),
                Slot::Resolve {
                    id: 1,
                    want: amended
                },
                Slot::Resolve {
                    id: 3,
                    want: line(30)
                },
            ]
        );
    }

    #[test]
    fn shared_debugger_breakpoints_are_removed_with_their_last_owner() {
        let mut breakpoints = Breakpoints::default();
        let (a, b) = (Group::Source("/a.c".into()), Group::Source("/b.c".into()));
        apply(&mut breakpoints, &a, vec![line(10)]);
        apply(&mut breakpoints, &b, vec![line(10)]);
        let plan = breakpoints.plan(&a, Vec::new());
        assert!(!breakpoints.release(plan.release[0].breakpoint().expect("resolved")));
        let plan = breakpoints.plan(&b, Vec::new());
        assert!(breakpoints.release(plan.release[0].breakpoint().expect("resolved")));
        assert_eq!(breakpoints.entries().count(), 0);
    }

    #[test]
    fn duplicate_lines_get_their_own_ids_and_unresolved_entries_are_retried() {
        let mut breakpoints = Breakpoints::default();
        let file = Group::Source("/a.c".into());
        let entries = apply(&mut breakpoints, &file, vec![line(5), line(5)]);
        assert_eq!(
            entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [1, 2]
        );

        let unresolved = Entry {
            id: 9,
            want: line(7),
            state: State::Unresolved {
                message: "not loaded".to_owned(),
                pending: true,
            },
        };

        breakpoints.install(file.clone(), vec![unresolved.clone()]);
        assert_eq!(breakpoints.unresolved(), [(file.clone(), unresolved)]);
        let plan = breakpoints.plan(&file, vec![line(7)]);
        assert_eq!(
            plan.slots,
            [Slot::Resolve {
                id: 9,
                want: line(7)
            }]
        );
    }

    #[test]
    fn stops_name_every_owner_and_take_a_reason_from_their_kind() {
        let mut breakpoints = Breakpoints::default();
        breakpoints.install(
            Group::Functions,
            vec![resolved(1, Want::at(Key::Function("f".into())), 7)],
        );
        breakpoints.install(
            Group::Source("/a.c".into()),
            vec![resolved(2, line(3), 7), resolved(3, line(4), 8)],
        );
        let hit = |ids: &[u64]| {
            ids.iter()
                .map(|id| BreakpointHit {
                    breakpoint: BreakpointId::new(*id),
                    hit_count: 1,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(breakpoints.hit(&hit(&[7])), (vec![1, 2], "breakpoint"));
        assert_eq!(breakpoints.hit(&hit(&[8])), (vec![3], "breakpoint"));
        breakpoints.install(Group::Source("/a.c".into()), Vec::new());
        assert_eq!(
            breakpoints.hit(&hit(&[7])),
            (vec![1], "function breakpoint")
        );
        assert_eq!(breakpoints.hit(&[]), (vec![], "breakpoint"));
    }

    #[test]
    fn console_breakpoints_are_adopted_and_deleted_ones_dropped() {
        let mut breakpoints = Breakpoints::default();
        let file = Group::Source("/a.c".into());
        apply(&mut breakpoints, &file, vec![line(4)]);
        let core = |id: u64| microscope::Breakpoint {
            pending_locations: std::sync::Arc::from([]),
            id: BreakpointId::new(id),
            spec: microscope::BreakpointSpec::Function("f".to_owned()),
            locations: std::sync::Arc::from([]),
            hit_condition: None,
            condition: None,
            log_message: None,
            hit_count: 0,
            enabled: true,
            temporary: false,
        };

        let changes = breakpoints.sync(&[core(4), core(9)]);
        assert_eq!(
            changes,
            [Change::New(Entry {
                id: 2,
                want: Want::at(Key::Console(BreakpointId::new(9))),
                state: State::Resolved {
                    breakpoint: BreakpointId::new(9),
                    placement: Placement::default(),
                    message: None,
                },
            })]
        );
        assert!(breakpoints.sync(&[core(4), core(9)]).is_empty());
        let changes = breakpoints.sync(&[core(9)]);
        assert!(matches!(&changes[..], [Change::Removed(entry)] if entry.id == 1));
        assert!(!breakpoints.groups.contains_key(&file));
    }

    /// A replacement that fails to release a breakpoint puts back every
    /// entry whose debugger breakpoint may still be installed, so their
    /// hits are still named and none is adopted as the console's.
    #[test]
    fn an_abandoned_replacement_keeps_the_entries_still_installed() {
        let mut breakpoints = Breakpoints::default();
        let file = Group::Source("/a.c".into());
        let first = apply(&mut breakpoints, &file, vec![line(10), line(20), line(30)]);

        // Keeps 10 and releases 20 and 30, but the debugger fails to remove
        // 20, whose owner the session takes back.
        let plan = breakpoints.plan(&file, vec![line(10)]);
        let mut release = plan.release.into_iter();
        let failed = release.next().expect("an entry to release");
        assert_eq!(failed, first[1]);
        assert!(breakpoints.release(BreakpointId::new(20)));
        breakpoints.acquire(BreakpointId::new(20));
        breakpoints.abandon(file, plan.slots, std::iter::once(failed).chain(release));

        let entries = breakpoints
            .entries()
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>();
        assert_eq!(entries, first);
        let hit = BreakpointHit {
            breakpoint: BreakpointId::new(20),
            hit_count: 1,
        };

        assert_eq!(breakpoints.hit(&[hit]), (vec![2], "breakpoint"));
        let core = |id: u64| microscope::Breakpoint {
            pending_locations: std::sync::Arc::from([]),
            id: BreakpointId::new(id),
            spec: microscope::BreakpointSpec::Function("f".to_owned()),
            locations: std::sync::Arc::from([]),
            hit_condition: None,
            condition: None,
            log_message: None,
            hit_count: 0,
            enabled: true,
            temporary: false,
        };

        assert!(breakpoints.sync(&[core(10), core(20), core(30)]).is_empty());
    }
}
