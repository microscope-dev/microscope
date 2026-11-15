use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use microscope::{BreakpointHit, BreakpointId};

use super::protocol::ErrorBody;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Source(PathBuf),
    Functions,
    Instructions,

    Console,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Line(u64),

    Function(String),

    Instruction(u64),

    Console(BreakpointId),

    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub key: Key,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

impl Want {
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placement {
    pub source: Option<(PathBuf, u64)>,

    pub address: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Resolved {
        breakpoint: BreakpointId,
        placement: Placement,
        message: Option<String>,
    },

    Pending {
        breakpoint: BreakpointId,
        message: String,
    },

    /// whose `enable` is the only way back: the protocol has no enabled
    Disabled { breakpoint: BreakpointId },
    Unresolved {
        message: String,

        /// is loaded, rather than having failed.
        pending: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub want: Want,
    pub state: State,
}

impl State {
    pub const fn breakpoint(&self) -> Option<BreakpointId> {
        match self {
            Self::Resolved { breakpoint, .. }
            | Self::Pending { breakpoint, .. }
            | Self::Disabled { breakpoint } => Some(*breakpoint),
            Self::Unresolved { .. } => None,
        }
    }
}

impl Entry {
    pub const fn breakpoint(&self) -> Option<BreakpointId> {
        self.state.breakpoint()
    }
}

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

    pub fn install(&mut self, group: Group, entries: Vec<Entry>) {
        if entries.is_empty() {
            self.groups.remove(&group);
        } else {
            self.groups.insert(group, entries);
        }
    }

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

    pub const fn allocate_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn acquire(&mut self, breakpoint: BreakpointId) {
        *self.owners.entry(breakpoint).or_default() += 1;
    }

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

    pub fn entries(&self) -> impl Iterator<Item = (&Group, &Entry)> {
        self.groups
            .iter()
            .flat_map(|(group, entries)| entries.iter().map(move |entry| (group, entry)))
    }

    pub fn owned_entries(&self) -> Vec<(Group, Entry)> {
        self.entries()
            .map(|(group, entry)| (group.clone(), entry.clone()))
            .collect()
    }

    pub fn unresolved(&self) -> Vec<(Group, Entry)> {
        self.entries()
            .filter(|(_, entry)| entry.breakpoint().is_none())
            .map(|(group, entry)| (group.clone(), entry.clone()))
            .collect()
    }

    pub fn replace(&mut self, group: &Group, replacement: Entry) {
        if let Some(entry) = self
            .groups
            .get_mut(group)
            .and_then(|entries| entries.iter_mut().find(|entry| entry.id == replacement.id))
        {
            *entry = replacement;
        }
    }

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

pub(super) trait Debugger {
    fn breakpoints(&mut self) -> &mut Breakpoints;

    async fn remove(&mut self, breakpoint: BreakpointId) -> Result<(), ErrorBody>;

    async fn add(&mut self, group: &Group, want: &Want) -> State;
}

pub(super) async fn replace_group(
    debugger: &mut impl Debugger,
    group: Group,
    wants: Vec<Want>,
) -> Result<Vec<Entry>, ErrorBody> {
    let plan = debugger.breakpoints().plan(&group, wants);
    let mut release = plan.release.into_iter();
    while let Some(entry) = release.next() {
        if let Err(error) = release_breakpoint(debugger, &entry).await {
            debugger.breakpoints().abandon(
                group,
                plan.slots,
                std::iter::once(entry).chain(release),
            );
            return Err(error);
        }
    }

    let mut entries = Vec::with_capacity(plan.slots.len());

    for slot in plan.slots {
        entries.push(match slot {
            Slot::Keep(entry) => entry,
            Slot::Resolve { id, want } => {
                let state = resolve(debugger, &group, &want).await;
                Entry { id, want, state }
            }
        });
    }

    debugger.breakpoints().install(group, entries.clone());
    Ok(entries)
}

async fn release_breakpoint(debugger: &mut impl Debugger, entry: &Entry) -> Result<(), ErrorBody> {
    let Some(breakpoint) = entry.breakpoint() else {
        return Ok(());
    };

    if debugger.breakpoints().release(breakpoint)
        && let Err(error) = debugger.remove(breakpoint).await
    {
        debugger.breakpoints().acquire(breakpoint);
        return Err(error);
    }

    Ok(())
}

pub(super) async fn resolve(debugger: &mut impl Debugger, group: &Group, want: &Want) -> State {
    let state = debugger.add(group, want).await;
    if let Some(breakpoint) = state.breakpoint() {
        debugger.breakpoints().acquire(breakpoint);
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    #[derive(Default)]
    struct Fake {
        table: Breakpoints,
        installed: BTreeMap<BreakpointId, (String, Option<String>)>,
        next: u64,
        refuse: bool,
    }

    impl Fake {
        fn add_spec(&mut self, spec: (String, Option<String>)) -> BreakpointId {
            if let Some((&id, _)) = self
                .installed
                .iter()
                .find(|(_, installed)| **installed == spec)
            {
                return id;
            }

            self.next += 1;
            let id = BreakpointId::new(self.next);
            self.installed.insert(id, spec);
            id
        }

        fn sync(&mut self) {
            let current = self
                .installed
                .keys()
                .map(|&id| microscope::Breakpoint {
                    pending_locations: std::sync::Arc::from([]),
                    id,
                    spec: microscope::BreakpointSpec::Function("f".to_owned()),
                    locations: std::sync::Arc::from([]),
                    hit_condition: None,
                    condition: None,
                    log_message: None,
                    hit_count: 0,
                    enabled: true,
                    temporary: false,
                })
                .collect::<Vec<_>>();
            self.table.sync(&current);
        }
    }

    impl Debugger for Fake {
        fn breakpoints(&mut self) -> &mut Breakpoints {
            &mut self.table
        }

        fn remove(
            &mut self,
            breakpoint: BreakpointId,
        ) -> impl std::future::Future<Output = Result<(), ErrorBody>> {
            let removed = if std::mem::take(&mut self.refuse) {
                Err(ErrorBody::new("refused".to_owned()))
            } else {
                self.installed.remove(&breakpoint);
                Ok(())
            };

            std::future::ready(removed)
        }

        fn add(&mut self, group: &Group, want: &Want) -> impl std::future::Future<Output = State> {
            let location = match (group, &want.key) {
                (Group::Source(path), Key::Line(line)) => {
                    Some(format!("{}:{line}", path.display()))
                }
                (_, Key::Function(name)) => Some(name.clone()),
                _ => None,
            };

            std::future::ready(location.map_or_else(
                || State::Unresolved {
                    message: format!("{:?} cannot be placed", want.key),
                    pending: false,
                },
                |location| State::Resolved {
                    breakpoint: self.add_spec((location, want.condition.clone())),
                    placement: Placement::default(),
                    message: None,
                },
            ))
        }
    }

    fn now<T>(future: impl std::future::Future<Output = T>) -> T {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match std::pin::pin!(future).poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("the fake never waits"),
        }
    }

    #[derive(Debug, Clone)]
    enum Operation {
        Set {
            group: u8,
            wants: Vec<(u8, bool, Option<u8>)>,
        },

        /// The debugger refuses the next removal.
        Refuse,

        /// The console adds a function breakpoint, which may equal one the
        ConsoleAdd { name: u8, condition: Option<u8> },

        /// The console deletes any breakpoint.
        ConsoleDelete(prop::sample::Index),

        /// A stop hits one breakpoint.
        Hit(prop::sample::Index),
    }

    fn operation() -> impl Strategy<Value = Operation> {
        let want = (
            0..4_u8,
            prop::bool::weighted(0.1),
            proptest::option::of(0..2_u8),
        );
        prop_oneof![
            4 => (0..3_u8, prop::collection::vec(want, 0..5))
                .prop_map(|(group, wants)| Operation::Set { group, wants }),
            1 => Just(Operation::Refuse),
            1 => (0..4_u8, proptest::option::of(0..2_u8))
                .prop_map(|(name, condition)| Operation::ConsoleAdd { name, condition }),
            1 => any::<prop::sample::Index>().prop_map(Operation::ConsoleDelete),
            2 => any::<prop::sample::Index>().prop_map(Operation::Hit),
        ]
    }

    fn group(group: u8) -> Group {
        match group {
            0 => Group::Source("/a.c".into()),
            1 => Group::Source("/b c.c".into()),
            _ => Group::Functions,
        }
    }

    fn want(group: &Group, (key, invalid, condition): (u8, bool, Option<u8>)) -> Want {
        let key = match group {
            _ if invalid => Key::Invalid(format!("invalid {key}")),
            Group::Source(_) => Key::Line(u64::from(key) + 1),
            _ => Key::Function(format!("f{key}")),
        };

        Want::new(
            key,
            condition.map(|condition| format!("x == {condition}")),
            None,
            None,
        )
    }

    fn references(table: &Breakpoints) -> HashMap<BreakpointId, usize> {
        let mut references = HashMap::new();
        for (_, entry) in table.entries() {
            if let Some(breakpoint) = entry.breakpoint() {
                *references.entry(breakpoint).or_default() += 1;
            }
        }

        references
    }

    proptest! {
        /// Every debugger breakpoint is owned once for each entry that
        /// refers to it, and exactly the owned ones are installed, whatever
        /// the client sends, the console changes, and the debugger refuses.
        /// Entries keep their ids while re-sent, answer in request order,
        /// and a stop names exactly the entries of what it hit.
        #[test]
        fn breakpoint_owners_match_the_entries_through_any_requests(
            operations in prop::collection::vec(operation(), 1..24),
        ) {
            let mut fake = Fake::default();
            let mut issued = BTreeSet::new();

            for operation in operations {
                match operation {
                    Operation::Set { group: index, wants } => {
                        let group = group(index);
                        let wants = wants.into_iter().map(|key| want(&group, key)).collect::<Vec<_>>();
                        let before = fake
                            .table
                            .groups
                            .get(&group)
                            .cloned()
                            .unwrap_or_default();

                        if let Ok(entries) = now(replace_group(&mut fake, group.clone(), wants.clone())) {
                                let keys = |entries: &[Entry]| entries.iter().map(|entry| entry.want.key.clone()).collect::<Vec<_>>();
                                prop_assert_eq!(
                                    keys(&entries),
                                    wants.iter().map(|want| want.key.clone()).collect::<Vec<_>>()
                                );



                                let mut old = before.clone();
                                for entry in &entries {
                                    let kept = old.iter().position(|old| old.want.key == entry.want.key);
                                    match kept {
                                        Some(index) => prop_assert_eq!(entry.id, old.remove(index).id),
                                        None => prop_assert!(!issued.contains(&entry.id), "{} reused", entry.id),
                                    }
                                }
                        } else {
                                let after = fake.table.groups.get(&group).cloned().unwrap_or_default();
                                prop_assert!(after.iter().all(|entry| before.contains(entry)), "{:?} came from nowhere", after);
                        }
                    }
                    Operation::Refuse => fake.refuse = true,
                    Operation::ConsoleAdd { name, condition } => {
                        fake.add_spec((format!("f{name}"), condition.map(|condition| format!("x == {condition}"))));
                        fake.sync();
                    }
                    Operation::ConsoleDelete(index) => {
                        let installed = fake.installed.keys().copied().collect::<Vec<_>>();
                        if !installed.is_empty() {
                            fake.installed.remove(&installed[index.index(installed.len())]);
                        }

                        fake.sync();
                    }
                    Operation::Hit(index) => {
                        let installed = fake.installed.keys().copied().collect::<Vec<_>>();
                        if !installed.is_empty() {
                            let breakpoint = installed[index.index(installed.len())];
                            let mut expected = fake
                                .table
                                .entries()
                                .filter(|(_, entry)| entry.breakpoint() == Some(breakpoint))
                                .map(|(_, entry)| entry.id)
                                .collect::<Vec<_>>();
                            expected.sort_unstable();
                            let hit = BreakpointHit { breakpoint, hit_count: 1 };
                            prop_assert_eq!(fake.table.hit(&[hit]).0, expected);
                        }
                    }
                }

                let ids = fake.table.entries().map(|(_, entry)| entry.id).collect::<Vec<_>>();
                prop_assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len(), "ids repeat: {:?}", ids);
                issued.extend(ids);

                prop_assert_eq!(&fake.table.owners, &references(&fake.table));
                prop_assert_eq!(fake.installed.len(), fake.table.owners.len());
                prop_assert!(
                    fake.installed.keys().all(|breakpoint| fake.table.owners.contains_key(breakpoint)),
                    "{:?} are installed, but {:?} are owned", fake.installed, fake.table.owners
                );
            }
        }
    }
}
