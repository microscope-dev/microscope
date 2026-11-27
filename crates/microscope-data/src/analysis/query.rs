//! Filtering, stack rewriting, and aggregation queries over a `Capture`.
//!
//! Every query selects samples (or task waits, plus optional syscall leaves),
//! rewrites each stack according to `Look` and `Transform`, and aggregates into
//! a flat `CallTree`, per-key `Costs`, or time-bucketed timeline lanes.

use std::collections::BTreeMap;

use foldhash::HashMap;

use super::{Keys, index};
use crate::CodeRole;
use crate::capture::Capture;
use crate::capture::records::{NONE, UNSEEN};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    Samples,

    Waits,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub source: Source,

    pub range: Option<(i64, i64)>,

    pub threads: Option<Vec<u32>>,

    pub tasks: Option<Vec<u32>>,

    pub labels: Vec<(String, String)>,
    pub transforms: Vec<Transform>,

    pub invert: bool,

    pub look: Look,

    pub by_task: bool,

    pub by_label: Option<String>,

    pub zones: Option<ZoneScope>,

    pub during: Option<Vec<u32>>,

    pub calls: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneScope {
    pub sites: Vec<u32>,

    pub band: Option<(u64, u64)>,

    pub outside: bool,
}

impl ZoneScope {
    fn holds(&self, start: i64, end: i64) -> bool {
        let Some((shortest, longest)) = self.band else {
            return !self.outside;
        };

        let length = u64::try_from(end - start).unwrap_or(0);
        (shortest..=longest).contains(&length) != self.outside
    }

    fn spans(&self, capture: &Capture, thread: u32) -> Vec<(i64, i64)> {
        let mut spans: Vec<(i64, i64)> = Vec::new();
        for zones in capture
            .zone_ranges
            .iter()
            .filter(|zones| zones.thread.get() == thread)
        {
            for zone in capture.range_zones(zones) {
                let (start, end) = (zone.start.get(), zone.end.get());
                if self.sites.contains(&zone.site.get()) && end > start && self.holds(start, end) {
                    spans.push((start, end));
                }
            }
        }

        merged(spans)
    }
}

fn merged(mut spans: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    spans.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }

    debug_assert!(
        merged
            .windows(2)
            .all(|pair| matches!(pair, [a, b] if a.1 < b.0)),
        "merged spans overlap"
    );
    merged
}

fn call_spans(capture: &Capture, rows: &[u32]) -> HashMap<u32, Vec<(i64, i64)>> {
    let mut spans: HashMap<u32, Vec<(i64, i64)>> = HashMap::default();
    for call in rows
        .iter()
        .filter_map(|&row| capture.syscalls.get(row as usize))
    {
        let (entered, exited) = (call.entered.get(), call.exited.get());
        let start = if entered == UNSEEN { exited } else { entered };
        let end = if exited == UNSEEN { i64::MAX } else { exited };
        if start != UNSEEN && end > start {
            spans
                .entry(call.thread.get())
                .or_default()
                .push((start, end));
        }
    }

    spans
        .into_iter()
        .map(|(thread, spans)| (thread, merged(spans)))
        .collect()
}

struct Inside {
    spans: Vec<(i64, i64)>,
    next: usize,
}

impl Inside {
    const fn new(spans: Vec<(i64, i64)>) -> Self {
        Self { spans, next: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn holds(&mut self, time: i64) -> bool {
        while self
            .spans
            .get(self.next)
            .is_some_and(|&(_, end)| end <= time)
        {
            self.next += 1;
        }

        self.spans
            .get(self.next)
            .is_some_and(|&(start, _)| start <= time)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Look {
    #[default]
    Native,

    Tasks,

    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    Focus(u32),

    Merge(u32),

    Drop(u32),

    CollapseRecursion(u32),

    CollapseCycles,
}

/// Flat, structure-of-arrays call tree laid out in depth-first preorder.
///
/// Each node `i` owns the contiguous subtree `i..end[i]`, with sibling subtrees
/// ordered by descending `total` weight and `parent[i] == NONE` for roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallTree {
    pub key: Vec<u32>,

    pub parent: Vec<u32>,
    pub depth: Vec<u32>,

    pub own: Vec<u64>,

    pub total: Vec<u64>,

    pub end: Vec<u32>,

    pub weight: u64,
    pub samples: u64,
}

impl CallTree {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.key.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.key.is_empty()
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`aggregate` gives every column an entry per node, and a node's caller precedes it"
    )]
    fn check_invariants(&self) {
        let mut children = vec![0_u64; self.len()];
        let mut roots = 0_u64;
        for position in 0..self.len() {
            let (parent, end) = (self.parent[position], self.end[position] as usize);
            debug_assert!(
                position < end && end <= self.len(),
                "node {position} ends at {end}, outside the {} nodes",
                self.len()
            );
            if parent == NONE {
                debug_assert_eq!(self.depth[position], 0, "root {position} is not at depth 0");
                roots += self.total[position];
                continue;
            }

            let caller = parent as usize;
            debug_assert!(
                caller < position && end <= self.end[caller] as usize,
                "node {position}, ending at {end}, lies outside its caller {caller}'s subtree"
            );
            debug_assert_eq!(
                self.depth[position],
                self.depth[caller] + 1,
                "node {position} is not one deeper than its caller {caller}"
            );
            children[caller] += self.total[position];
        }

        for (position, ((&total, &own), &children)) in
            self.total.iter().zip(&self.own).zip(&children).enumerate()
        {
            debug_assert_eq!(
                total,
                own + children,
                "node {position}'s total is not its own weight and its children's"
            );
        }

        debug_assert!(
            roots <= self.weight,
            "the roots total {roots}, more than the {} the query kept",
            self.weight
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub key: u32,

    pub own: u64,

    pub total: u64,
}

#[derive(Debug, Clone, Copy)]
struct Selected {
    time: i64,

    thread: u32,
    task: u32,

    label: u32,
    stack: u32,
    weight: u64,
}

struct OpenLabels<'s> {
    spans: &'s [(i64, i64, u32)],
    next: usize,
    open: Vec<(i64, u32)>,
}

impl<'s> OpenLabels<'s> {
    const fn new(spans: &'s [(i64, i64, u32)]) -> Self {
        Self {
            spans,
            next: 0,
            open: Vec::new(),
        }
    }

    fn at(&mut self, time: i64) -> impl Iterator<Item = u32> + '_ {
        while let Some(&(start, end, label)) = self.spans.get(self.next)
            && start <= time
        {
            self.open.push((end, label));
            self.next += 1;
        }

        self.open.retain(|(end, _)| *end > time);
        self.open.iter().map(|(_, label)| *label)
    }
}

struct LabelQuery<'a> {
    required: Vec<u32>,
    by: Option<&'a str>,

    carried: Vec<u32>,
}

impl<'a> LabelQuery<'a> {
    fn new(keys: &Keys<'_>, query: &'a Query) -> Option<Self> {
        let required = query
            .labels
            .iter()
            .map(|(key, value)| keys.label(key, Some(value)))
            .collect::<Option<Vec<u32>>>()?;
        Some(Self {
            required,
            by: query.by_label.as_deref(),
            carried: Vec::new(),
        })
    }

    const fn active(&self) -> bool {
        !self.required.is_empty() || self.by.is_some()
    }

    fn judge(&self, keys: &Keys<'_>) -> Option<u32> {
        if !self
            .required
            .iter()
            .all(|label| self.carried.contains(label))
        {
            return None;
        }

        Some(self.by.map_or(NONE, |by| {
            let label = self
                .carried
                .iter()
                .copied()
                .rfind(|&label| keys.label_of(label).0 == by)
                .or_else(|| keys.label(by, None));
            keys.label_key(label.unwrap_or(NONE))
        }))
    }
}

fn each(capture: &Capture, keys: &Keys<'_>, query: &Query, mut take: impl FnMut(Selected)) {
    let Some(mut labels) = LabelQuery::new(keys, query) else {
        return;
    };

    let task_kept = |task: u32| {
        query
            .tasks
            .as_ref()
            .is_none_or(|tasks| tasks.contains(&task))
    };

    let during = query.during.as_ref().map(|rows| call_spans(capture, rows));
    match query.source {
        Source::Samples => {
            for range in &capture.sample_ranges {
                let thread = range.thread.get();
                if query
                    .threads
                    .as_ref()
                    .is_some_and(|threads| !threads.contains(&thread))
                {
                    continue;
                }

                let mut open = OpenLabels::new(keys.spans(thread));

                let mut inside = query
                    .zones
                    .as_ref()
                    .map(|scope| Inside::new(scope.spans(capture, thread)));
                let mut calling = during
                    .as_ref()
                    .map(|spans| Inside::new(spans.get(&thread).cloned().unwrap_or_default()));
                if inside.as_ref().is_some_and(Inside::is_empty)
                    || calling.as_ref().is_some_and(Inside::is_empty)
                {
                    continue;
                }

                let samples = capture.thread_samples(range);
                for sample in in_range(samples, |sample| sample.time.get(), query.range) {
                    let (stack, task, time) =
                        (sample.stack.get(), sample.task.get(), sample.time.get());
                    if stack == NONE || !task_kept(task) {
                        continue;
                    }

                    if inside.as_mut().is_some_and(|inside| !inside.holds(time))
                        || calling.as_mut().is_some_and(|calling| !calling.holds(time))
                    {
                        continue;
                    }

                    let label = if labels.active() {
                        labels.carried.clear();
                        labels.carried.extend_from_slice(keys.task_labels(task));
                        labels.carried.extend(open.at(time));
                        let Some(judged) = labels.judge(keys) else {
                            continue;
                        };

                        judged
                    } else {
                        NONE
                    };

                    take(Selected {
                        time,
                        thread,
                        task,
                        label,
                        stack,
                        weight: sample.weight.get(),
                    });
                }
            }
        }
        Source::Waits => {
            if query.zones.is_some() || during.is_some() {
                return;
            }

            let waits = &capture.task_waits;
            for wait in in_range(waits, |wait| wait.time.get(), query.range) {
                let (stack, task) = (wait.stack.get(), wait.task.get());
                if stack == NONE || !task_kept(task) {
                    continue;
                }

                let label = if labels.active() {
                    labels.carried.clear();
                    labels.carried.extend_from_slice(keys.task_labels(task));
                    let Some(judged) = labels.judge(keys) else {
                        continue;
                    };

                    judged
                } else {
                    NONE
                };

                take(Selected {
                    time: wait.time.get(),
                    thread: NONE,
                    task,
                    label,
                    stack,
                    weight: wait.weight.get(),
                });
            }
        }
    }
}

pub fn syscalls(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Vec<bool> {
    let _span = crate::span!("analysis syscalls");
    let mut kept = vec![false; capture.syscalls.len()];
    kept_calls(capture, keys, query, |row, _| {
        if let Some(slot) = kept.get_mut(row) {
            *slot = true;
        }
    });
    kept
}

fn kept_calls(capture: &Capture, keys: &Keys<'_>, query: &Query, mut take: impl FnMut(usize, u32)) {
    let Some(mut labels) = LabelQuery::new(keys, query) else {
        return;
    };

    let paths_of = Query {
        by_task: false,
        by_label: None,
        ..query.clone()
    };
    let mut paths = Paths::new(capture, keys, &paths_of);

    let mut threads: BTreeMap<u32, Vec<(i64, usize)>> = BTreeMap::new();
    for (row, call) in capture.syscalls.iter().enumerate() {
        let time = if call.exited.get() == UNSEEN {
            call.entered.get()
        } else {
            call.exited.get()
        };
        let thread = call.thread.get();
        let shown = query
            .threads
            .as_ref()
            .is_none_or(|threads| threads.contains(&thread));
        let inside = query
            .range
            .is_none_or(|(start, end)| (start..end).contains(&time));
        let named = query.during.as_ref().is_none_or(|rows| {
            u32::try_from(row).is_ok_and(|row| rows.binary_search(&row).is_ok())
        });
        if shown && inside && named && time != UNSEEN {
            threads.entry(thread).or_default().push((time, row));
        }
    }

    for (thread, mut calls) in threads {
        calls.sort_unstable();
        let mut open = OpenLabels::new(keys.spans(thread));
        let mut inside = query
            .zones
            .as_ref()
            .map(|scope| Inside::new(scope.spans(capture, thread)));
        for (time, row) in calls {
            if inside.as_mut().is_some_and(|inside| !inside.holds(time)) {
                continue;
            }

            let label = if labels.active() {
                labels.carried.clear();
                labels.carried.extend(open.at(time));
                let Some(judged) = labels.judge(keys) else {
                    continue;
                };
                judged
            } else {
                NONE
            };

            let Some(call) = capture.syscalls.get(row) else {
                continue;
            };
            let leaf = keys.syscall(call.abi, call.number.get());
            if paths.of(call.stack.get(), NONE, NONE, leaf).is_some() {
                take(row, label);
            }
        }
    }
}

/// Turns kept syscalls into synthetic leaf groups weighted by uncovered wall time.
///
/// Any sample weight recorded on the same thread during `[entered, exited)` is
/// subtracted so on-CPU time already attributed to samples is not double-counted.
fn call_groups(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Vec<Group> {
    let by_thread: HashMap<u32, &[crate::capture::records::Sample]> = capture
        .sample_ranges
        .iter()
        .map(|range| (range.thread.get(), capture.thread_samples(range)))
        .collect();
    let mut groups = Vec::new();
    kept_calls(capture, keys, query, |row, label| {
        let Some(call) = capture.syscalls.get(row) else {
            return;
        };

        let (entered, exited) = (call.entered.get(), call.exited.get());
        if entered == UNSEEN || exited == UNSEEN || exited <= entered {
            return;
        }

        let sampled: u64 = by_thread
            .get(&call.thread.get())
            .map_or(&[][..], |samples| {
                in_range(samples, |sample| sample.time.get(), Some((entered, exited)))
            })
            .iter()
            .map(|sample| sample.weight.get())
            .sum();
        let weight = (exited - entered).unsigned_abs().saturating_sub(sampled);
        if weight > 0 {
            groups.push(Group {
                stack: call.stack.get(),
                task: NONE,
                label,
                leaf: keys.syscall(call.abi, call.number.get()),
                weight,
                count: 0,
            });
        }
    });

    groups
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Group {
    stack: u32,
    task: u32,
    label: u32,

    leaf: u32,
    weight: u64,
    count: u32,
}

fn select(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Vec<Group> {
    let mut selected = select_samples(capture, keys, query);
    if query.calls && query.source == Source::Samples {
        selected.extend(call_groups(capture, keys, query));
    }

    selected
}

fn select_samples(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Vec<Group> {
    if query.by_task || query.by_label.is_some() {
        let mut by: HashMap<(u32, u32, u32), (u64, u32)> = HashMap::default();
        each(capture, keys, query, |selected| {
            let task = if query.by_task { selected.task } else { NONE };
            let entry = by
                .entry((selected.stack, task, selected.label))
                .or_default();
            entry.0 += selected.weight;
            entry.1 += 1;
        });
        let mut selected: Vec<Group> = by
            .into_iter()
            .map(|((stack, task, label), (weight, count))| Group {
                stack,
                task,
                label,
                leaf: NONE,
                weight,
                count,
            })
            .collect();
        selected.sort_unstable();
        return selected;
    }

    let mut weights = vec![0_u64; capture.stacks.len()];
    let mut counts = vec![0_u32; capture.stacks.len()];
    #[expect(
        clippy::indexing_slicing,
        reason = "`each` passes only stacks other than `NONE`, which `validate` checks"
    )]
    let tally = |selected: Selected| {
        weights[selected.stack as usize] += selected.weight;
        counts[selected.stack as usize] += 1;
    };
    each(capture, keys, query, tally);
    weights
        .into_iter()
        .zip(counts)
        .enumerate()
        .filter(|(_, (weight, _))| *weight > 0)
        .map(|(stack, (weight, count))| Group {
            stack: index(stack),
            task: NONE,
            label: NONE,
            leaf: NONE,
            weight,
            count,
        })
        .collect()
}

#[expect(
    clippy::indexing_slicing,
    reason = "`partition_point` returns at most the length"
)]
fn in_range<T>(rows: &[T], time: impl Fn(&T) -> i64, range: Option<(i64, i64)>) -> &[T] {
    let Some((start, end)) = range else {
        return rows;
    };

    let first = rows.partition_point(|row| time(row) < start);
    let last = rows.partition_point(|row| time(row) < end);
    &rows[first..last.max(first)]
}

struct Paths<'a> {
    capture: &'a Capture,
    keys: &'a Keys<'a>,
    query: &'a Query,
    path: Vec<u32>,

    on_path: Vec<u32>,
}

/// Collapses call cycles by rewinding to each repeated key's first index,
/// resetting only touched `on_path` slots to stay `O(path.len())`.
#[expect(
    clippy::indexing_slicing,
    reason = "every key on the path is below `keys`, which `on_path` covers, and a \
              position on it is below `kept`, which never passes `at`"
)]
fn erase_cycles(path: &mut Vec<u32>, on_path: &mut Vec<u32>, keys: usize) {
    if on_path.len() < keys {
        on_path.resize(keys, NONE);
    }

    let mut kept = 0;
    for at in 0..path.len() {
        let key = path[at];
        match on_path[key as usize] {
            NONE => {
                on_path[key as usize] = index(kept);
                path[kept] = key;
                kept += 1;
            }
            earlier => {
                for &cut in &path[earlier as usize + 1..kept] {
                    on_path[cut as usize] = NONE;
                }
                kept = earlier as usize + 1;
            }
        }
    }

    for &key in &path[..kept] {
        on_path[key as usize] = NONE;
    }
    path.truncate(kept);
}

impl<'a> Paths<'a> {
    const fn new(capture: &'a Capture, keys: &'a Keys<'a>, query: &'a Query) -> Self {
        Self {
            capture,
            keys,
            query,
            path: Vec::new(),
            on_path: Vec::new(),
        }
    }

    fn of(&mut self, stack: u32, task: u32, label: u32, leaf: u32) -> Option<&[u32]> {
        let path = &mut self.path;
        path.clear();

        for frame in self.capture.stack_frames(stack) {
            path.extend_from_slice(self.keys.frame(frame));
        }

        match self.query.look {
            Look::Native => path.retain(|key| !self.keys.interpreted(*key)),
            Look::Tasks => tasks(self.keys, path),
            Look::Code => {
                tasks(self.keys, path);
                code(self.keys, path);
            }
        }

        if leaf != NONE {
            path.insert(0, leaf);
        }

        if !self.query.invert {
            path.reverse();
        }

        for transform in &self.query.transforms {
            match *transform {
                Transform::Focus(key) => {
                    let first = path.iter().position(|candidate| *candidate == key)?;
                    path.drain(..first);
                }
                Transform::Merge(key) => path.retain(|candidate| *candidate != key),
                Transform::Drop(key) => {
                    if path.contains(&key) {
                        return None;
                    }
                }
                Transform::CollapseRecursion(key) => {
                    let mut previous = NONE;
                    path.retain(|candidate| {
                        let repeat = *candidate == key && previous == key;
                        previous = *candidate;
                        !repeat
                    });
                }
                Transform::CollapseCycles => {
                    if self.query.invert {
                        path.reverse();
                    }
                    erase_cycles(path, &mut self.on_path, self.keys.count());
                    if self.query.invert {
                        path.reverse();
                    }
                }
            }
        }

        let groups = [
            self.query.by_task.then(|| self.keys.group(task)),
            (label != NONE).then_some(label),
        ];

        for group in groups.into_iter().flatten() {
            if self.query.invert {
                path.push(group);
            } else {
                path.insert(0, group);
            }
        }

        Some(path)
    }
}

/// Expects `path` in callee-to-caller order; truncates above the first
/// `CodeRole::Dispatch` frame (keeping `Dispatch` as the outer caller),
/// collapses intermediate runtime frames into a synthetic `[task]` frame, and
/// strips `CodeRole::Wrapper` frames from all stacks.
fn tasks(keys: &Keys<'_>, path: &mut Vec<u32>) {
    if let Some(dispatch) = path
        .iter()
        .position(|key| keys.role(*key) == CodeRole::Dispatch)
    {
        path.truncate(dispatch + 1);
        let machinery = |key: &u32| {
            matches!(
                keys.role(*key),
                CodeRole::RuntimeInternal | CodeRole::Wrapper
            )
        };

        if let Some((root, &key)) = path
            .iter()
            .enumerate()
            .take(dispatch)
            .rfind(|(_, key)| !machinery(key))
            && let Some(task) = keys.task(key)
        {
            path.splice(root + 1..dispatch, [task]);
        }
    }

    path.retain(|key| keys.role(*key) != CodeRole::Wrapper);
}

fn code(keys: &Keys<'_>, path: &mut Vec<u32>) {
    let Some(outermost) = path.iter().rposition(|key| keys.interpreted(*key)) else {
        return;
    };

    path.truncate(outermost + 1);
    path.retain(|key| keys.role(*key) != CodeRole::Interpreter);
}

#[expect(
    clippy::indexing_slicing,
    reason = "nodes index the columns built beside them, and a node's parent precedes it"
)]
pub fn aggregate(capture: &Capture, keys: &Keys<'_>, query: &Query) -> CallTree {
    let _span = crate::span!("analysis aggregate");
    let selected = select(capture, keys, query);
    let mut paths = Paths::new(capture, keys, query);

    let mut key = Vec::new();
    let mut parent = Vec::new();
    let mut own = Vec::new();
    let mut children: HashMap<(u32, u32), u32> = HashMap::default();
    let mut tree = CallTree::default();

    for group in &selected {
        let Some(path) = paths.of(group.stack, group.task, group.label, group.leaf) else {
            continue;
        };

        let (weight, count) = (group.weight, group.count);
        tree.weight += weight;
        tree.samples += u64::from(count);
        let mut node = NONE;
        for &step in path {
            node = *children.entry((node, step)).or_insert_with(|| {
                key.push(step);
                parent.push(node);
                own.push(0);
                index(key.len() - 1)
            });
        }

        if node != NONE {
            own[node as usize] += weight;
        }
    }

    let mut total = own.clone();

    for node in (0..key.len()).rev() {
        if parent[node] != NONE {
            total[parent[node] as usize] += total[node];
        }
    }

    let mut kids: Vec<Vec<u32>> = vec![Vec::new(); key.len()];
    let mut roots = Vec::new();

    for (node, caller) in parent.iter().enumerate() {
        match *caller {
            NONE => roots.push(index(node)),
            caller => kids[caller as usize].push(index(node)),
        }
    }

    let largest_first = |nodes: &mut Vec<u32>| {
        nodes.sort_by(|a, b| {
            total[*b as usize]
                .cmp(&total[*a as usize])
                .then(key[*a as usize].cmp(&key[*b as usize]))
        });
    };

    largest_first(&mut roots);
    for list in &mut kids {
        largest_first(list);
    }

    let mut placed = vec![NONE; key.len()];
    let mut pending: Vec<(u32, u32)> = roots.iter().rev().map(|root| (*root, 0)).collect();

    while let Some((node, depth)) = pending.pop() {
        let position = index(tree.key.len());
        placed[node as usize] = position;
        tree.key.push(key[node as usize]);
        tree.parent.push(match parent[node as usize] {
            NONE => NONE,
            caller => placed[caller as usize],
        });
        tree.depth.push(depth);
        tree.own.push(own[node as usize]);
        tree.total.push(total[node as usize]);
        tree.end.push(position + 1);
        pending.extend(
            kids[node as usize]
                .iter()
                .rev()
                .map(|kid| (*kid, depth + 1)),
        );
    }

    for position in (0..tree.len()).rev() {
        let caller = tree.parent[position];
        if caller != NONE {
            let end = tree.end[position];
            let caller_end = &mut tree.end[caller as usize];
            *caller_end = (*caller_end).max(end);
        }
    }

    if cfg!(debug_assertions) {
        tree.check_invariants();
    }

    tree
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Costs {
    pub rows: Vec<Cost>,

    pub weight: u64,
}

#[expect(
    clippy::indexing_slicing,
    reason = "a path holds only keys of `keys`, which the columns have an entry for each of"
)]
pub fn functions(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Costs {
    let _span = crate::span!("analysis functions");
    let selected = select(capture, keys, query);
    let mut paths = Paths::new(capture, keys, query);
    let mut own = vec![0_u64; keys.count()];
    let mut total = vec![0_u64; keys.count()];

    let mut counted = vec![u32::MAX; keys.count()];
    let mut kept = 0;
    for (entry, group) in selected.iter().enumerate() {
        let Some(path) = paths.of(group.stack, group.task, group.label, group.leaf) else {
            continue;
        };

        let weight = group.weight;
        kept += weight;
        let innermost = if query.invert {
            path.first()
        } else {
            path.last()
        };

        if let Some(&innermost) = innermost {
            own[innermost as usize] += weight;
        }

        for &key in path {
            if counted[key as usize] != index(entry) {
                counted[key as usize] = index(entry);
                total[key as usize] += weight;
            }
        }
    }

    let mut costs: Vec<Cost> = (0..)
        .zip(own.into_iter().zip(total))
        .filter(|(_, (_, total))| *total > 0)
        .map(|(key, (own, total))| Cost { key, own, total })
        .collect();

    for cost in &costs {
        debug_assert!(
            cost.own <= cost.total && cost.total <= kept,
            "key {} costs {} own and {} total, of {kept} kept",
            cost.key,
            cost.own,
            cost.total
        );
    }

    costs.sort_by(|a, b| (b.own, b.total, a.key).cmp(&(a.own, a.total, b.key)));
    Costs {
        rows: costs,
        weight: kept,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffCpuLane {
    pub thread: u32,
    pub blocked: Vec<u64>,
    pub preempted: Vec<u64>,
}

#[must_use]
pub fn off_cpu(capture: &Capture, query: &Query, buckets: usize) -> Vec<OffCpuLane> {
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };

    let span = u128::try_from(i128::from(end) - i128::from(start)).unwrap_or(1);
    let count = buckets as u128;

    let edge = |slice: usize| {
        let delta = i128::try_from(slice as u128 * span / count).unwrap_or(i128::MAX);
        i64::try_from(i128::from(start).saturating_add(delta)).unwrap_or(i64::MAX)
    };

    let mut lanes: std::collections::BTreeMap<u32, OffCpuLane> = std::collections::BTreeMap::new();
    for range in &capture.off_cpu_ranges {
        let thread = range.thread.get();
        if query
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&thread))
        {
            continue;
        }

        for interval in capture.thread_off_cpu(range) {
            let left = interval.time.get();
            let back = left.saturating_add_unsigned(interval.duration.get());
            let (from, to) = (left.max(start), back.min(end));
            if from >= to {
                continue;
            }

            let lane = lanes.entry(thread).or_insert_with(|| OffCpuLane {
                thread,
                blocked: vec![0; buckets],
                preempted: vec![0; buckets],
            });
            let totals = if interval.preempted == 0 {
                &mut lane.blocked
            } else {
                &mut lane.preempted
            };

            let offset = u128::try_from(i128::from(from) - i128::from(start)).unwrap_or(0);
            let first = usize::try_from(offset * count / span)
                .unwrap_or(0)
                .min(buckets - 1);
            for (slice, total) in totals.iter_mut().enumerate().skip(first) {
                if edge(slice) >= to {
                    break;
                }

                let overlap = to.min(edge(slice + 1)) - from.max(edge(slice));
                *total += u64::try_from(overlap).unwrap_or(0);
            }
        }
    }

    lanes.into_values().collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lanes {
    #[default]
    Threads,

    Tasks,
}

#[must_use]
pub fn off_cpu_lanes(capture: &Capture, query: &Query) -> Vec<u32> {
    let Some((start, end)) = query.range.filter(|(start, end)| end > start) else {
        return Vec::new();
    };
    let mut active = Vec::new();
    for range in &capture.off_cpu_ranges {
        let thread = range.thread.get();
        if query
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&thread))
        {
            continue;
        }
        let any = capture.thread_off_cpu(range).iter().any(|interval| {
            let left = interval.time.get();
            let back = left.saturating_add_unsigned(interval.duration.get());
            left.max(start) < back.min(end)
        });
        if any {
            active.push(thread);
        }
    }
    active
}

#[must_use]
pub fn density_lanes(capture: &Capture, keys: &Keys<'_>, query: &Query, lanes: Lanes) -> Vec<u32> {
    let Some((start, end)) = query.range.filter(|(start, end)| end > start) else {
        return Vec::new();
    };
    let _ = (start, end);
    let by_task = lanes == Lanes::Tasks || query.source == Source::Waits;
    let mut paths = Paths::new(capture, keys, query);
    let mut kept: HashMap<(u32, u32, u32), bool> = HashMap::default();
    let count = if by_task {
        capture.tasks.len() + 1
    } else {
        capture.threads.len()
    };
    let mut seen = vec![false; count];
    each(capture, keys, query, |selected| {
        let task = if query.by_task { selected.task } else { NONE };
        let keep = *kept
            .entry((selected.stack, task, selected.label))
            .or_insert_with(|| {
                paths
                    .of(selected.stack, task, selected.label, NONE)
                    .is_some()
            });
        if !keep {
            return;
        }
        let lane = match (by_task, selected.task) {
            (true, NONE) => capture.tasks.len(),
            (true, task) => task as usize,
            (false, _) => selected.thread as usize,
        };
        if let Some(slot) = seen.get_mut(lane) {
            *slot = true;
        }
    });
    seen.into_iter()
        .enumerate()
        .filter_map(|(lane, active)| {
            if !active {
                return None;
            }
            Some(match (by_task, lane == capture.tasks.len()) {
                (true, true) => NONE,
                _ => index(lane),
            })
        })
        .collect()
}

pub fn density(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &Query,
    buckets: usize,
    lanes: Lanes,
) -> Vec<(u32, Vec<u64>)> {
    let _span = crate::span!("analysis density");
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };

    let by_task = lanes == Lanes::Tasks || query.source == Source::Waits;
    let span = u128::try_from(end - start).unwrap_or(1);
    let mut paths = Paths::new(capture, keys, query);

    let mut kept: HashMap<(u32, u32, u32), bool> = HashMap::default();
    let count = if by_task {
        capture.tasks.len() + 1
    } else {
        capture.threads.len()
    };

    let mut slices: Vec<Option<Vec<u64>>> = vec![None; count];

    #[expect(
        clippy::indexing_slicing,
        reason = "a lane is a task `validate` checked, the last for none, or a sample's thread, \
                  which `validate` checks too, and a bucket is capped below `buckets`"
    )]
    let tally = |selected: Selected| {
        let task = if query.by_task { selected.task } else { NONE };
        let keep = *kept
            .entry((selected.stack, task, selected.label))
            .or_insert_with(|| {
                paths
                    .of(selected.stack, task, selected.label, NONE)
                    .is_some()
            });
        if !keep {
            return;
        }

        let lane = match (by_task, selected.task) {
            (true, NONE) => capture.tasks.len(),
            (true, task) => task as usize,
            (false, _) => selected.thread as usize,
        };

        let offset = u128::try_from(selected.time - start).unwrap_or(0);
        let bucket = usize::try_from(offset * buckets as u128 / span).unwrap_or(0);
        slices[lane].get_or_insert_with(|| vec![0; buckets])[bucket.min(buckets - 1)] +=
            selected.weight;
    };
    each(capture, keys, query, tally);

    slices
        .into_iter()
        .enumerate()
        .filter_map(|(lane, slices)| {
            let lane = match (by_task, lane == capture.tasks.len()) {
                (true, true) => NONE,
                _ => index(lane),
            };

            Some((lane, slices?))
        })
        .collect()
}
