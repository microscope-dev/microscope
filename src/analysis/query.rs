//! Aggregating a selection of samples into a call tree, function costs,
//! and densities over time.
//!
//! A query selects samples, or the waits task snapshots found, by time,
//! thread, task, and label, turns each one's stack into a path of keys,
//! outermost first or, inverted, innermost first, and reshapes the path by
//! its transforms in order. A logical query first shows each async task's
//! frames as a stack of their own, and a query by task or label roots each
//! path at its task or its label's value. A sample's labels are those
//! open on its thread at its time, joined in one pass over each thread's
//! samples and label spans, and its task's. Samples are counted per
//! distinct stack first, and group, so the work is linear in the samples
//! selected and the stacks they use.

use foldhash::HashMap;

use super::{Keys, index};
use crate::CodeRole;
use crate::capture::Capture;
use crate::capture::records::NONE;

/// What a query aggregates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    /// CPU samples, or a breakpoint's.
    #[default]
    Samples,

    /// Where task snapshots found tasks waiting, each weighted by the time
    /// it stands for.
    Waits,
}

/// Which samples, seen how.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub source: Source,

    /// Samples taken in `[start, end)`, or all.
    pub range: Option<(i64, i64)>,

    /// Samples of these threads, by their rows in the capture, or of all.
    /// Waits are of no thread, so this selects samples only.
    pub threads: Option<Vec<u32>>,

    /// Samples or waits of these tasks, by their rows, or of any: [`NONE`]
    /// is a sample no task snapshot named.
    pub tasks: Option<Vec<u32>>,

    /// Samples or waits that carry every one of these labels, each a key
    /// and value.
    pub labels: Vec<(String, String)>,
    pub transforms: Vec<Transform>,

    /// Paths from the innermost frame out, so that the tree is of callers.
    pub invert: bool,

    /// How each path's frames are seen, before any transform.
    pub look: Look,

    /// Roots each path, after its transforms, at the task it ran or waited
    /// in.
    pub by_task: bool,

    /// Roots each path, after its transforms and outside any task, at its
    /// label of this key, or at the key's absence.
    pub by_label: Option<String>,
}

/// How a path's frames are seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Look {
    /// The frames its thread ran, which an interpreter's frames are not:
    /// its own frames that run them stand for them.
    #[default]
    Native,

    /// Each async task's frames as a stack of their own (see [`logical`]),
    /// an interpreter's frames among the frames that run them.
    Logical,

    /// As [`Look::Logical`], and as its interpreted code sees it (see
    /// [`python`]).
    Python,
}

/// One step of reshaping each sample's path, applied in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// Keeps samples whose path has the key, from its first occurrence on:
    /// the key becomes the root, and its calls from everywhere merge.
    Focus(u32),

    /// Removes the key from every path, giving its cost to its caller.
    Merge(u32),

    /// Removes every sample whose path has the key.
    Drop(u32),

    /// Merges each run of the key calling itself into its first call.
    CollapseRecursion(u32),
}

/// The call tree of a query, its nodes in depth-first order with each
/// node's children by total, largest first; a node's subtree is the
/// nodes from it to its `end`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallTree {
    pub key: Vec<u32>,

    /// The caller's node, or [`NONE`] for a root.
    pub parent: Vec<u32>,
    pub depth: Vec<u32>,

    /// The weight of samples whose path ends at the node.
    pub own: Vec<u64>,

    /// The weight of samples whose path passes through the node.
    pub total: Vec<u64>,

    /// One past the node's last descendant.
    pub end: Vec<u32>,

    /// The weight of every sample selected and kept.
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

    /// Panics if the tree is not laid out depth first, each node's subtree
    /// ending inside its caller's, with each total its own weight and its
    /// children's.
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

/// What one key costs under a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub key: u32,

    /// Weight of samples whose path ends with it.
    pub own: u64,

    /// Weight of samples whose path has it, counted once per sample.
    pub total: u64,
}

/// One sample or wait a query selects.
#[derive(Debug, Clone, Copy)]
struct Selected {
    time: i64,

    /// Its thread's row, or [`NONE`] for a wait.
    thread: u32,
    task: u32,

    /// The key of its label a query by label roots it at, or [`NONE`].
    label: u32,
    stack: u32,
    weight: u64,
}

/// What a query asks of labels: the rows a sample must carry, and the key
/// it is grouped by.
struct LabelQuery<'a> {
    required: Vec<u32>,
    by: Option<&'a str>,

    /// The labels a sample carries.
    carried: Vec<u32>,
}

impl<'a> LabelQuery<'a> {
    /// `None` when a required label is nowhere, so nothing is selected.
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

    /// Whether a sample that carries `self.carried`, its task's labels
    /// and then its thread's spans by start, is kept, and the key of the
    /// label it is grouped by: of several of one key, the innermost span's.
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
                .rfind(|&label| keys.labels()[label as usize].0 == by)
                .or_else(|| keys.label(by, None));
            keys.label_key(label.unwrap_or(NONE))
        }))
    }
}

/// Each sample or wait `query` selects, in time order per thread, but for
/// its transforms.
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

                // The thread's label spans that began by a sample's time
                // and have not ended.
                let spans = keys.spans(thread);
                let (mut next, mut open) = (0, Vec::new());
                let samples = capture.thread_samples(range);
                for sample in &samples[in_range(samples, |sample| sample.time.get(), query.range)] {
                    let (stack, task, time) =
                        (sample.stack.get(), sample.task.get(), sample.time.get());
                    if stack == NONE || !task_kept(task) {
                        continue;
                    }

                    let label = if labels.active() {
                        while let Some(&(start, end, label)) = spans.get(next)
                            && start <= time
                        {
                            open.push((end, label));
                            next += 1;
                        }

                        open.retain(|(end, _)| *end > time);
                        labels.carried.clear();
                        labels.carried.extend_from_slice(keys.task_labels(task));
                        labels.carried.extend(open.iter().map(|(_, label)| *label));
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
            let waits = &capture.task_waits;
            for wait in &waits[in_range(waits, |wait| wait.time.get(), query.range)] {
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

/// One stack's selection: its stack, and the task and label key a query
/// by task or label roots it at, or [`NONE`], with its weight and count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Group {
    stack: u32,
    task: u32,
    label: u32,
    weight: u64,
    count: u32,
}

/// The weight and count of what `query` selects on each stack and, by
/// task or label, of each task and label on it.
fn select(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Vec<Group> {
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
                weight,
                count,
            })
            .collect();
        selected.sort_unstable();
        return selected;
    }

    let mut weights = vec![0_u64; capture.stacks.len()];
    let mut counts = vec![0_u32; capture.stacks.len()];
    each(capture, keys, query, |selected| {
        weights[selected.stack as usize] += selected.weight;
        counts[selected.stack as usize] += 1;
    });
    weights
        .into_iter()
        .zip(counts)
        .enumerate()
        .filter(|(_, (weight, _))| *weight > 0)
        .map(|(stack, (weight, count))| Group {
            stack: index(stack),
            task: NONE,
            label: NONE,
            weight,
            count,
        })
        .collect()
}

/// The positions of a time-ordered run's rows in `range`.
fn in_range<T>(
    rows: &[T],
    time: impl Fn(&T) -> i64,
    range: Option<(i64, i64)>,
) -> std::ops::Range<usize> {
    let Some((start, end)) = range else {
        return 0..rows.len();
    };

    let first = rows.partition_point(|row| time(row) < start);
    let last = rows.partition_point(|row| time(row) < end);
    first..last.max(first)
}

/// Turns stacks into transformed paths.
struct Paths<'a> {
    capture: &'a Capture,
    keys: &'a Keys<'a>,
    query: &'a Query,
    path: Vec<u32>,
}

impl<'a> Paths<'a> {
    const fn new(capture: &'a Capture, keys: &'a Keys<'a>, query: &'a Query) -> Self {
        Self {
            capture,
            keys,
            query,
            path: Vec::new(),
        }
    }

    /// The path of `stack`, run or waited on in `task` under the label key
    /// `label`, under the query, or `None` when a transform removes it.
    fn of(&mut self, stack: u32, task: u32, label: u32) -> Option<&[u32]> {
        let path = &mut self.path;
        path.clear();
        for frame in self.capture.stack_frames(stack) {
            path.extend_from_slice(self.keys.frame(frame));
        }

        match self.query.look {
            Look::Native => path.retain(|key| !self.keys.interpreted(*key)),
            Look::Logical => logical(self.keys, path),
            Look::Python => {
                logical(self.keys, path);
                python(self.keys, path);
            }
        }

        // Innermost first, as stacks are; a tree from the roots starts
        // with the outermost.
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
            }
        }

        let groups = [
            self.query.by_task.then(|| self.keys.group(task)),
            (label != NONE).then_some(label),
        ];

        // The label outermost, then the task.
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

/// Makes an innermost-first path logical. The runtime gives the thread to
/// a task at the innermost `Dispatch` frame, so the frames inside it are
/// the task's and the scheduler's outside it are dropped. The task's own
/// code begins at its outermost frame that is neither runtime machinery
/// nor a wrapper, which is rooted under a key for the task; the machinery
/// between the dispatch and it goes. Wrappers only forward, so they fold
/// away everywhere. A path with no dispatch, such as a goroutine's, which
/// runs on a stack of its own, keeps its frames.
fn logical(keys: &Keys<'_>, path: &mut Vec<u32>) {
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

        if let Some(root) = path[..dispatch].iter().rposition(|key| !machinery(key))
            && let Some(task) = keys.task(path[root])
        {
            path.splice(root + 1..dispatch, [task]);
        }
    }

    path.retain(|key| keys.role(*key) != CodeRole::Wrapper);
}

/// Makes an innermost-first path the one its interpreted code sees: from
/// its outermost interpreted frame in, without the interpreter's machinery
/// that runs them, so that the interpreted frames each activation of the
/// interpreter runs join, and native code they call stays where it was
/// called. A path with no interpreted frame keeps its frames.
fn python(keys: &Keys<'_>, path: &mut Vec<u32>) {
    let Some(outermost) = path.iter().rposition(|key| keys.interpreted(*key)) else {
        return;
    };

    path.truncate(outermost + 1);
    path.retain(|key| keys.role(*key) != CodeRole::Interpreter);
}

/// The call tree of the samples `query` selects.
pub fn aggregate(capture: &Capture, keys: &Keys<'_>, query: &Query) -> CallTree {
    let selected = select(capture, keys, query);
    let mut paths = Paths::new(capture, keys, query);

    // The tree in the order nodes are first reached.
    let mut key = Vec::new();
    let mut parent = Vec::new();
    let mut own = Vec::new();
    let mut children: HashMap<(u32, u32), u32> = HashMap::default();
    let mut tree = CallTree::default();
    for group in &selected {
        let Some(path) = paths.of(group.stack, group.task, group.label) else {
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

    // A node is reached after its parent, so a reverse pass totals it.
    let mut total = own.clone();
    for node in (0..key.len()).rev() {
        if parent[node] != NONE {
            total[parent[node] as usize] += total[node];
        }
    }

    // Lay the tree out depth first, largest child first.
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

    // Each subtree ends where the next node of its depth or less begins.
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

/// What each key costs under a query, most own weight first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Costs {
    pub rows: Vec<Cost>,

    /// The weight of every sample selected and kept.
    pub weight: u64,
}

/// What each key costs under `query`.
pub fn functions(capture: &Capture, keys: &Keys<'_>, query: &Query) -> Costs {
    let selected = select(capture, keys, query);
    let mut paths = Paths::new(capture, keys, query);
    let mut own = vec![0_u64; keys.count()];
    let mut total = vec![0_u64; keys.count()];

    // The entry that last counted a key, so recursion counts once.
    let mut counted = vec![u32::MAX; keys.count()];
    let mut kept = 0;
    for (entry, group) in selected.iter().enumerate() {
        let Some(path) = paths.of(group.stack, group.task, group.label) else {
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

    let mut costs: Vec<Cost> = (0..keys.count())
        .filter(|&key| total[key] > 0)
        .map(|key| Cost {
            key: index(key),
            own: own[key],
            total: total[key],
        })
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

/// One thread's nanoseconds off its CPU in each slice of a range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffCpuLane {
    pub thread: u32,
    pub blocked: Vec<u64>,
    pub preempted: Vec<u64>,
}

/// How long each selected thread was off its CPU, blocked and preempted,
/// in each of `buckets` equal slices of the query's range, per thread
/// with any. Intervals have no stacks, so transforms do not apply.
pub fn off_cpu(capture: &Capture, query: &Query, buckets: usize) -> Vec<OffCpuLane> {
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };

    let span = u128::try_from(end - start).unwrap_or(1);
    let count = buckets as u128;

    // Slice `i` is [edge(i), edge(i + 1)).
    let edge = |slice: usize| {
        start + i64::try_from(slice as u128 * span / count).unwrap_or(i64::MAX - start)
    };

    let mut lanes = Vec::new();
    for range in &capture.off_cpu_ranges {
        let thread = range.thread.get();
        if query
            .threads
            .as_ref()
            .is_some_and(|threads| !threads.contains(&thread))
        {
            continue;
        }

        let mut lane = OffCpuLane {
            thread,
            blocked: vec![0; buckets],
            preempted: vec![0; buckets],
        };

        let mut any = false;
        for interval in capture.thread_off_cpu(range) {
            let left = interval.time.get();
            let back = left.saturating_add_unsigned(interval.duration.get());
            let (from, to) = (left.max(start), back.min(end));
            if from >= to {
                continue;
            }

            let totals = if interval.preempted == 0 {
                &mut lane.blocked
            } else {
                &mut lane.preempted
            };

            let offset = u128::try_from(from - start).unwrap_or(0);
            let mut slice = usize::try_from(offset * count / span)
                .unwrap_or(0)
                .min(buckets - 1);
            while slice < buckets && edge(slice) < to {
                let overlap = to.min(edge(slice + 1)) - from.max(edge(slice));
                totals[slice] += u64::try_from(overlap).unwrap_or(0);
                slice += 1;
            }

            any = true;
        }

        if any {
            lanes.push(lane);
        }
    }

    lanes
}

/// What a timeline's lanes are of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lanes {
    /// A lane per thread, by its row.
    #[default]
    Threads,

    /// A lane per task, by its row, and one, [`NONE`], for what no task
    /// snapshot named.
    Tasks,
}

/// The weight of what `query` keeps in each of `buckets` equal slices of
/// its range, per lane with any. Waits are of no thread, so their lanes
/// are always by task.
pub fn density(
    capture: &Capture,
    keys: &Keys<'_>,
    query: &Query,
    buckets: usize,
    lanes: Lanes,
) -> Vec<(u32, Vec<u64>)> {
    let Some((start, end)) = query
        .range
        .filter(|(start, end)| end > start && buckets > 0)
    else {
        return Vec::new();
    };

    let by_task = lanes == Lanes::Tasks || query.source == Source::Waits;
    let span = u128::try_from(end - start).unwrap_or(1);
    let mut paths = Paths::new(capture, keys, query);

    // Whether each stack's samples are kept, decided once per stack and
    // task, and each lane, the last for no task.
    let mut kept: HashMap<(u32, u32, u32), bool> = HashMap::default();
    let count = if by_task {
        capture.tasks.len() + 1
    } else {
        capture.threads.len()
    };

    let mut slices: Vec<Option<Vec<u64>>> = vec![None; count];
    each(capture, keys, query, |selected| {
        let task = if query.by_task { selected.task } else { NONE };
        let keep = *kept
            .entry((selected.stack, task, selected.label))
            .or_insert_with(|| paths.of(selected.stack, task, selected.label).is_some());
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
    });
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
