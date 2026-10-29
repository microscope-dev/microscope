use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Context as _, Result, ensure};
use microscope::profiler::{
    CallTree, Cost, Costs, DiffTree, Granularity, Keys, Look, NO_KEY, Profile, Query, Side, Source,
    Transform, diff_costs, diff_trees,
};

pub struct Options<'a> {
    pub tree: bool,
    pub limit: usize,
    pub min_percent: f64,
    pub granularity: Granularity,
    pub source: Source,
    pub look: Look,
    pub focus: Option<&'a str>,

    /// With `tree` and `focus`, the focused function's callers instead of
    /// its callees.
    pub invert: bool,

    /// How each stack is reshaped first, in order.
    pub reshapes: &'a [Reshape],
}

/// A change to each stack before a report adds them up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reshape {
    /// Removes the function, giving its cost to its caller.
    Merge(String),

    /// Removes every sample through the function.
    Drop(String),

    /// Merges each run of the function calling itself into its first call.
    Collapse(String),

    /// Cuts every cycle out of each stack.
    Cycles,
}

/// `reshapes` as transforms of the profile `keys` are of, each function
/// resolved as `--focus` resolves it. In a `base` profile, a function it
/// lacks changes nothing there.
pub fn transforms(keys: &Keys<'_>, reshapes: &[Reshape], base: bool) -> Result<Vec<Transform>> {
    let mut transforms = Vec::new();
    for reshape in reshapes {
        let (name, make): (&str, fn(u32) -> Transform) = match reshape {
            Reshape::Merge(name) => (name, Transform::Merge),
            Reshape::Drop(name) => (name, Transform::Drop),
            Reshape::Collapse(name) => (name, Transform::CollapseRecursion),
            Reshape::Cycles => {
                transforms.push(Transform::CollapseCycles);
                continue;
            }
        };
        let key = resolve(keys, name, base)?;
        if key != NO_KEY {
            transforms.push(make(key));
        }
    }

    Ok(transforms)
}

pub fn write(
    current: &Profile,
    base: Option<&Profile>,
    options: &Options<'_>,
    out: &mut impl Write,
) -> Result<()> {
    ensure!(
        options.min_percent.is_finite() && (0.0..=100.0).contains(&options.min_percent),
        "--min-percent must be between 0 and 100"
    );
    ensure!(
        !current.is_trace() && base.is_none_or(|profile| !profile.is_trace()),
        "a profile report needs samples, not a syscall trace alone"
    );
    let unit = unit(current, options.source);
    if let Some(base) = base {
        ensure!(
            unit == self::unit(base, options.source),
            "cannot compare profiles with different units: after is {unit}, before is {}",
            self::unit(base, options.source)
        );
    }
    if let Some(name) = options.focus {
        return focus(current, base, options, name, out);
    }
    let base = base.context("a comparison needs --base FILE")?;
    let keys = current.keys(options.granularity);
    let base_keys = base.keys(options.granularity);
    let query = Query {
        source: options.source,
        look: options.look,
        transforms: transforms(&keys, options.reshapes, false)?,
        ..Query::default()
    };
    let base_query = Query {
        transforms: transforms(&base_keys, options.reshapes, true)
            .context("resolving --base profile")?,
        ..query.clone()
    };
    let costs = keys.functions(&query);
    let base_costs = base_keys.functions(&base_query);
    total(base_costs.weight, costs.weight, unit, out)?;
    writeln!(out)?;

    if options.tree {
        let tree = keys.aggregate(&query);
        let base_tree = base_keys.aggregate(&base_query);
        compared_tree((&keys, &tree), (&base_keys, &base_tree), options, unit, out)?;
    } else {
        let mut rows = diff_costs((&keys, &costs), (&base_keys, &base_costs));
        rows.retain(|cost| cost.own > 0 || cost.base_own > 0);
        rows.sort_by_key(|cost| std::cmp::Reverse(cost.own.abs_diff(cost.base_own)));
        writeln!(out, "Own cost (after - before):")?;
        let rows = rows.iter().take(options.limit).map(|cost| {
            (
                comparison(cost.base_own, cost.own, unit).to_vec(),
                on_side(&keys, &base_keys, cost.side).name(cost.key),
            )
        });
        table(&COMPARISON, "function", rows, out)?;
    }
    Ok(())
}

/// Both profiles' total cost, and how it changed.
fn total(before: u64, after: u64, unit: &str, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "Total: {} → {} ({})",
        weight(before.into(), unit, false),
        weight(after.into(), unit, false),
        relative(before, after)
    )?;
    Ok(())
}

fn compared_tree(
    (keys, tree): (&Keys<'_>, &CallTree),
    (base_keys, base_tree): (&Keys<'_>, &CallTree),
    options: &Options<'_>,
    unit: &str,
    out: &mut impl Write,
) -> Result<()> {
    let diff = diff_trees((keys, tree), (base_keys, base_tree));
    let rows = tree_order(&diff).into_iter().filter_map(|node| {
        let shown = percent(diff.total[node], tree.weight)
            .max(percent(diff.base_total[node], base_tree.weight))
            >= options.min_percent;
        shown.then(|| {
            let [before, after, change, relative] =
                comparison(diff.base_total[node], diff.total[node], unit);
            let [own_before, own_after, own_change, _] =
                comparison(diff.base_own[node], diff.own[node], unit);
            (
                vec![
                    before, after, change, relative, own_before, own_after, own_change,
                ],
                format!(
                    "{}{}",
                    "  ".repeat(diff.depth[node] as usize),
                    on_side(keys, base_keys, diff.side[node]).name(diff.key[node])
                ),
            )
        })
    });
    writeln!(
        out,
        "Total cost through each path, then own cost (after - before):"
    )?;
    table(
        &[
            "before",
            "after",
            "change",
            "",
            "own before",
            "own after",
            "own change",
        ],
        "function",
        rows,
        out,
    )
}

fn resolve(keys: &Keys<'_>, name: &str, base: bool) -> Result<u32> {
    let named = keys.named(name);
    if named.is_empty() && base {
        return Ok(NO_KEY);
    }
    ensure!(!named.is_empty(), "nothing in the profile is named {name}");
    if named.len() > 1 {
        let candidates = named
            .iter()
            .map(|&key| format!("  {}", keys.qualified(key)))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "{} things are named {name}; use a more exact name:\n{candidates}",
            named.len()
        );
    }
    Ok(named[0])
}

struct Butterfly {
    total: u64,
    own: u64,
    callers: Costs,
    callees: Costs,
}

fn butterfly(keys: &Keys<'_>, query: &Query, key: u32) -> Butterfly {
    let costs = keys.functions(query);
    let cost = costs.rows.iter().find(|cost| cost.key == key);
    let total = cost.map_or(0, |cost| cost.total);
    let own = cost.map_or(0, |cost| cost.own);
    let tree = keys.aggregate(query);
    let mut incoming = BTreeMap::<u32, u64>::new();
    let mut outgoing = BTreeMap::<u32, u64>::new();
    let mut node = 0;
    while node < tree.len() {
        if tree.key[node] != key {
            node += 1;
            continue;
        }
        let parent = tree.parent[node];
        if parent != NO_KEY {
            *incoming.entry(tree.key[parent as usize]).or_default() += tree.total[node];
        }
        let mut child = node + 1;
        while child < tree.end[node] as usize {
            *outgoing.entry(tree.key[child]).or_default() += tree.total[child];
            child = tree.end[child] as usize;
        }
        // Skip recursive occurrences: focus starts at the outermost one.
        node = tree.end[node] as usize;
    }
    let costs = |entries: BTreeMap<u32, u64>| Costs {
        weight: total,
        rows: entries
            .into_iter()
            .map(|(key, weight)| Cost {
                key,
                own: weight,
                total: weight,
            })
            .collect(),
    };
    Butterfly {
        total,
        own,
        callers: costs(incoming),
        callees: costs(outgoing),
    }
}

fn focus(
    current: &Profile,
    base: Option<&Profile>,
    options: &Options<'_>,
    name: &str,
    out: &mut impl Write,
) -> Result<()> {
    let keys = current.keys(options.granularity);
    // A name that only inlined frames carry is folded away, not missing.
    ensure!(
        options.granularity != Granularity::PhysicalFunction
            || !keys.named(name).is_empty()
            || current.keys(Granularity::Function).named(name).is_empty(),
        "{name} is only ever inlined, so --fold-inlines folds it into its callers; \
         focus on it without --fold-inlines"
    );
    let key = resolve(&keys, name, false)?;
    let base_keys = base.map(|base| base.keys(options.granularity));
    let base_key = base_keys
        .as_ref()
        .map(|keys| resolve(keys, name, true))
        .transpose()
        .context("resolving focus in --base profile")?;
    let query = Query {
        source: options.source,
        look: options.look,
        transforms: transforms(&keys, options.reshapes, false)?,
        ..Query::default()
    };
    let base_query = base_keys
        .as_ref()
        .map(|keys| -> Result<Query> {
            Ok(Query {
                transforms: transforms(keys, options.reshapes, true)
                    .context("resolving --base profile")?,
                ..query.clone()
            })
        })
        .transpose()?;
    let unit = unit(current, options.source);
    let focused = butterfly(&keys, &query, key);
    let before = base_keys
        .as_ref()
        .zip(base_key)
        .zip(base_query.as_ref())
        .map(|((keys, key), query)| butterfly(keys, query, key));
    if let Some((base_keys, base_query)) = base_keys.as_ref().zip(base_query.as_ref()) {
        let before = base_keys.functions(base_query).weight;
        total(before, keys.functions(&query).weight, unit, out)?;
    }
    writeln!(out, "Focus: {name}")?;
    if let Some(before) = &before {
        let rows = [
            (before.total, focused.total, "total"),
            (before.own, focused.own, "own"),
        ]
        .map(|(before, after, name)| (comparison(before, after, unit).to_vec(), name.to_owned()));
        table(&COMPARISON, "cost", rows, out)?;
    } else {
        writeln!(out, "Total: {}", weight(focused.total.into(), unit, false))?;
        writeln!(out, "Own: {}", weight(focused.own.into(), unit, false))?;
    }
    if options.tree {
        let mut selected = query;
        selected.transforms.push(Transform::Focus(key));
        selected.invert = options.invert;
        let tree = keys.aggregate(&selected);
        if let Some(((base_keys, base_key), mut base_query)) =
            base_keys.as_ref().zip(base_key).zip(base_query)
        {
            base_query.transforms.push(Transform::Focus(base_key));
            base_query.invert = options.invert;
            let base_tree = base_keys.aggregate(&base_query);
            return compared_tree((&keys, &tree), (base_keys, &base_tree), options, unit, out);
        }
        focused_tree(&keys, &tree, options, unit, out)?;
    } else {
        writeln!(out, "Callers (outermost occurrence):")?;
        relations(
            (&keys, &focused.callers),
            base_keys
                .as_ref()
                .zip(before.as_ref())
                .map(|(keys, before)| (keys, &before.callers)),
            options,
            unit,
            out,
        )?;
        writeln!(out, "Callees (outermost occurrence):")?;
        relations(
            (&keys, &focused.callees),
            base_keys
                .as_ref()
                .zip(before.as_ref())
                .map(|(keys, before)| (keys, &before.callees)),
            options,
            unit,
            out,
        )?;
    }
    Ok(())
}

/// A focused function's subtree, or with `invert` its callers, which have
/// no own cost of their own to show.
fn focused_tree(
    keys: &Keys<'_>,
    tree: &CallTree,
    options: &Options<'_>,
    unit: &str,
    out: &mut impl Write,
) -> Result<()> {
    if options.invert {
        writeln!(out, "{:>18}  function", "total")?;
    } else {
        writeln!(out, "{:>18} {:>18}  function", "total", "own")?;
    }
    for node in 0..tree.len() {
        if percent(tree.total[node], tree.weight) < options.min_percent {
            continue;
        }
        write!(out, "{:>18}", weight(tree.total[node].into(), unit, false))?;
        if !options.invert {
            write!(out, " {:>18}", weight(tree.own[node].into(), unit, false))?;
        }
        writeln!(
            out,
            "  {}{}",
            "  ".repeat(tree.depth[node] as usize),
            keys.name(tree.key[node])
        )?;
    }
    Ok(())
}

fn relations(
    (keys, costs): (&Keys<'_>, &Costs),
    base: Option<(&Keys<'_>, &Costs)>,
    options: &Options<'_>,
    unit: &str,
    out: &mut impl Write,
) -> Result<()> {
    if costs.rows.is_empty() && base.is_none_or(|(_, costs)| costs.rows.is_empty()) {
        writeln!(out, "  (none)")?;
    } else if let Some((base_keys, before)) = base {
        let mut rows = diff_costs((keys, costs), (base_keys, before));
        rows.sort_by_key(|cost| std::cmp::Reverse(cost.own.abs_diff(cost.base_own)));
        let rows = rows.iter().take(options.limit).map(|cost| {
            (
                comparison(cost.base_own, cost.own, unit).to_vec(),
                on_side(keys, base_keys, cost.side).name(cost.key),
            )
        });
        table(&COMPARISON, "function", rows, out)?;
    } else {
        let mut rows: Vec<_> = costs.rows.iter().collect();
        rows.sort_by_key(|cost| std::cmp::Reverse(cost.own));
        for cost in rows.iter().take(options.limit) {
            writeln!(
                out,
                "{:>18}  {}",
                weight(cost.own.into(), unit, false),
                keys.name(cost.key)
            )?;
        }
    }
    Ok(())
}

fn unit(profile: &Profile, source: Source) -> &str {
    match source {
        Source::Waits => "nanoseconds",
        Source::Samples => profile.setting("weight").unwrap_or("nanoseconds"),
    }
}

fn weight(value: i128, unit: &str, signed: bool) -> String {
    let sign = if value < 0 {
        "-"
    } else if signed && value > 0 {
        "+"
    } else {
        ""
    };
    let value = value.unsigned_abs();
    if unit == "nanoseconds" {
        let millis = (value + 500_000) / 1_000_000;
        format!("{sign}{}.{:03} s", millis / 1000, millis % 1000)
    } else {
        format!("{sign}{value} {unit}")
    }
}

/// The columns of [`comparison`].
const COMPARISON: [&str; 4] = ["before", "after", "change", ""];

/// A cost before and after, how much it changed, and by what share.
fn comparison(before: u64, after: u64, unit: &str) -> [String; 4] {
    [
        weight(before.into(), unit, false),
        weight(after.into(), unit, false),
        weight(i128::from(after) - i128::from(before), unit, true),
        relative(before, after),
    ]
}

/// How `after` differs from `before` as a share of it: `new` when there
/// was nothing before, and `gone` when there is nothing after.
#[expect(
    clippy::cast_precision_loss,
    reason = "percentages need no more precision than f64's"
)]
fn relative(before: u64, after: u64) -> String {
    match (before, after) {
        (0, 0) => "=".to_owned(),
        (0, _) => "new".to_owned(),
        (_, 0) => "gone".to_owned(),
        _ => format!(
            "{:+.1}%",
            (after as f64 - before as f64) * 100.0 / before as f64
        ),
    }
}

/// Rows of right-aligned cells, each column as wide as its widest cell or
/// heading, then each row's name.
fn table(
    headings: &[&str],
    name: &str,
    rows: impl IntoIterator<Item = (Vec<String>, String)>,
    out: &mut impl Write,
) -> Result<()> {
    let rows: Vec<_> = rows.into_iter().collect();
    let widths: Vec<usize> = headings
        .iter()
        .enumerate()
        .map(|(column, heading)| {
            rows.iter()
                .map(|(cells, _)| cells[column].len())
                .fold(heading.len(), usize::max)
        })
        .collect();
    let line = |cells: &mut dyn Iterator<Item = &str>, name: &str| {
        let cells: Vec<_> = cells
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:>width$}"))
            .collect();
        format!("{}  {name}", cells.join("  "))
    };
    writeln!(out, "{}", line(&mut headings.iter().copied(), name))?;
    for (cells, name) in &rows {
        writeln!(out, "{}", line(&mut cells.iter().map(String::as_str), name))?;
    }
    Ok(())
}

const fn on_side<'a, 'b>(current: &'a Keys<'b>, base: &'a Keys<'b>, side: Side) -> &'a Keys<'b> {
    match side {
        Side::Current => current,
        Side::Base => base,
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "percentages are display thresholds"
)]
fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 * 100.0 / total as f64
    }
}

fn tree_order(tree: &DiffTree) -> Vec<usize> {
    let mut children = vec![Vec::new(); tree.len() + 1];
    for (node, &parent) in tree.parent.iter().enumerate() {
        let parent = if parent == NO_KEY {
            tree.len()
        } else {
            parent as usize
        };
        children[parent].push(node);
    }
    for siblings in &mut children {
        siblings.sort_by_key(|&node| {
            std::cmp::Reverse(tree.total[node].abs_diff(tree.base_total[node]))
        });
    }
    let mut pending: Vec<_> = children[tree.len()].iter().rev().copied().collect();
    let mut ordered = Vec::with_capacity(tree.len());
    while let Some(node) = pending.pop() {
        ordered.push(node);
        pending.extend(children[node].iter().rev().copied());
    }
    ordered
}
