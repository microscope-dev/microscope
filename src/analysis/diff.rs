//! Two aggregates compared: call trees merged path by path, and costs row
//! by row, matched by the names views show, so that a base may come from
//! another profile whose keys are its own.

use std::collections::{BTreeMap, VecDeque};

use super::{CallTree, Costs, Keys, index};
use crate::capture::records::NONE;

/// Which aggregate a node's or row's key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Current,

    /// A path or key only the base has.
    Base,
}

/// The paths of two call trees, each with both trees' weights, in the
/// current tree's order and then the base's: parents before children, a
/// node's descendants up to its `end`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffTree {
    pub key: Vec<u32>,
    pub side: Vec<Side>,
    pub parent: Vec<u32>,
    pub depth: Vec<u32>,
    pub end: Vec<u32>,
    pub own: Vec<u64>,
    pub total: Vec<u64>,
    pub base_own: Vec<u64>,
    pub base_total: Vec<u64>,
}

impl DiffTree {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.key.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.key.is_empty()
    }
}

/// A tree's children of `node`, or its roots for [`NONE`].
fn children(tree: &CallTree, node: u32) -> Vec<u32> {
    let (mut at, end) = match node {
        NONE => (0, index(tree.len())),
        node => (node + 1, tree.end[node as usize]),
    };

    let mut found = Vec::new();
    while at < end {
        found.push(at);
        at = tree.end[at as usize];
    }

    found
}

/// One node of the merged tree: its node in each tree, if any, and the key
/// and side it shows.
struct Pair {
    key: u32,
    side: Side,
    current: Option<u32>,
    base: Option<u32>,
    parent: u32,
    depth: u32,
}

/// Merges two trees path by path, a path matching where its names do. Of
/// siblings that share a name, the first of each tree's are paired, then
/// the second.
#[must_use]
pub fn diff_trees(current: (&Keys<'_>, &CallTree), base: (&Keys<'_>, &CallTree)) -> DiffTree {
    let mut names = Names::default();
    let mut out = DiffTree::default();
    let mut pending = pairs(&mut names, current, base, (Some(NONE), Some(NONE)), NONE, 0);
    pending.reverse();

    while let Some(pair) = pending.pop() {
        let node = index(out.len());
        let weights = |tree: &CallTree, at: Option<u32>| {
            at.map_or((0, 0), |at| {
                (tree.own[at as usize], tree.total[at as usize])
            })
        };

        let (own, total) = weights(current.1, pair.current);
        let (base_own, base_total) = weights(base.1, pair.base);
        out.key.push(pair.key);
        out.side.push(pair.side);
        out.parent.push(pair.parent);
        out.depth.push(pair.depth);
        out.end.push(node + 1);
        out.own.push(own);
        out.total.push(total);
        out.base_own.push(base_own);
        out.base_total.push(base_total);

        let mut next = pairs(
            &mut names,
            current,
            base,
            (pair.current, pair.base),
            node,
            pair.depth + 1,
        );
        next.reverse();
        pending.extend(next);
    }

    for node in (0..out.len()).rev() {
        let parent = out.parent[node];
        if parent != NONE {
            out.end[parent as usize] = out.end[parent as usize].max(out.end[node]);
        }
    }

    out
}

/// The merged children of a node of each tree, current's first.
fn pairs(
    names: &mut Names,
    current: (&Keys<'_>, &CallTree),
    base: (&Keys<'_>, &CallTree),
    nodes: (Option<u32>, Option<u32>),
    parent: u32,
    depth: u32,
) -> Vec<Pair> {
    let ours = nodes
        .0
        .map(|node| children(current.1, node))
        .unwrap_or_default();
    let theirs = nodes
        .1
        .map(|node| children(base.1, node))
        .unwrap_or_default();

    let mut unmatched: BTreeMap<String, VecDeque<u32>> = BTreeMap::new();
    for &node in &theirs {
        let name = names.of(Side::Base, base, node);
        unmatched.entry(name).or_default().push_back(node);
    }

    let mut paired = Vec::with_capacity(ours.len() + theirs.len());
    for &node in &ours {
        let name = names.of(Side::Current, current, node);
        let matched = unmatched.get_mut(&name).and_then(VecDeque::pop_front);
        paired.push(Pair {
            key: current.1.key[node as usize],
            side: Side::Current,
            current: Some(node),
            base: matched,
            parent,
            depth,
        });
    }

    let left: std::collections::BTreeSet<u32> = unmatched.into_values().flatten().collect();
    paired.extend(
        theirs
            .into_iter()
            .filter(|node| left.contains(node))
            .map(|node| Pair {
                key: base.1.key[node as usize],
                side: Side::Base,
                current: None,
                base: Some(node),
                parent,
                depth,
            }),
    );
    paired
}

/// Each key's name, by side, as views show it, found once.
#[derive(Default)]
struct Names {
    known: BTreeMap<(bool, u32), String>,
}

impl Names {
    fn of(&mut self, side: Side, (keys, tree): (&Keys<'_>, &CallTree), node: u32) -> String {
        let key = tree.key[node as usize];
        self.known
            .entry((side == Side::Base, key))
            .or_insert_with(|| keys.name(key))
            .clone()
    }
}

/// One key's cost now and in the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffCost {
    pub key: u32,
    pub side: Side,
    pub own: u64,
    pub total: u64,
    pub base_own: u64,
    pub base_total: u64,
}

/// Merges two aggregates' costs by name, current's rows first, then the
/// base's that it lacks. Keys that share a name pair in order.
#[must_use]
pub fn diff_costs(current: (&Keys<'_>, &Costs), base: (&Keys<'_>, &Costs)) -> Vec<DiffCost> {
    let mut unmatched: BTreeMap<String, VecDeque<usize>> = BTreeMap::new();
    for (row, cost) in base.1.rows.iter().enumerate() {
        unmatched
            .entry(base.0.name(cost.key))
            .or_default()
            .push_back(row);
    }

    let mut rows = Vec::with_capacity(current.1.rows.len() + base.1.rows.len());
    for cost in &current.1.rows {
        let matched = unmatched
            .get_mut(&current.0.name(cost.key))
            .and_then(VecDeque::pop_front)
            .map(|row| base.1.rows[row]);
        rows.push(DiffCost {
            key: cost.key,
            side: Side::Current,
            own: cost.own,
            total: cost.total,
            base_own: matched.map_or(0, |cost| cost.own),
            base_total: matched.map_or(0, |cost| cost.total),
        });
    }

    let mut left: Vec<usize> = unmatched.into_values().flatten().collect();
    left.sort_unstable();
    rows.extend(left.into_iter().map(|row| {
        let cost = base.1.rows[row];
        DiffCost {
            key: cost.key,
            side: Side::Base,
            own: 0,
            total: 0,
            base_own: cost.own,
            base_total: cost.total,
        }
    }));
    rows
}
