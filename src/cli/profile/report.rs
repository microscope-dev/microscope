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
        ..Query::default()
    };
    let costs = keys.functions(&query);
    let base_costs = base_keys.functions(&query);
    writeln!(
        out,
        "Before: {}",
        weight(base_costs.weight.into(), unit, false)
    )?;
    writeln!(out, "After: {}", weight(costs.weight.into(), unit, false))?;
    writeln!(out)?;

    if options.tree {
        let tree = keys.aggregate(&query);
        let base_tree = base_keys.aggregate(&query);
        compared_tree((&keys, &tree), (&base_keys, &base_tree), options, unit, out)?;
    } else {
        let mut rows = diff_costs((&keys, &costs), (&base_keys, &base_costs));
        rows.sort_by_key(|cost| std::cmp::Reverse(cost.own.abs_diff(cost.base_own)));
        writeln!(out, "Own cost (after - before):")?;
        writeln!(
            out,
            "{:>18} {:>18} {:>18}  function",
            "before", "after", "change"
        )?;
        for cost in rows.iter().take(options.limit) {
            comparison(out, cost.base_own, cost.own, unit)?;
            writeln!(
                out,
                "  {}",
                on_side(&keys, &base_keys, cost.side).name(cost.key)
            )?;
        }
    }
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
    writeln!(
        out,
        "{:>18} {:>18} {:>18} {:>18} {:>18} {:>18}  function",
        "before total", "after total", "change", "before own", "after own", "own change"
    )?;
    for node in tree_order(&diff) {
        if percent(diff.total[node], tree.weight)
            .max(percent(diff.base_total[node], base_tree.weight))
            < options.min_percent
        {
            continue;
        }
        comparison(out, diff.base_total[node], diff.total[node], unit)?;
        write!(out, " ")?;
        comparison(out, diff.base_own[node], diff.own[node], unit)?;
        writeln!(
            out,
            "  {}{}",
            "  ".repeat(diff.depth[node] as usize),
            on_side(keys, base_keys, diff.side[node]).name(diff.key[node])
        )?;
    }
    Ok(())
}

fn resolve(keys: &Keys<'_>, name: &str, base: bool) -> Result<u32> {
    let mut named = keys.named(name);
    if named.is_empty() {
        named = (0..u32::try_from(keys.count()).unwrap_or(NO_KEY))
            .filter(|&key| qualified(keys, key) == name)
            .collect();
    }
    if named.is_empty() && base {
        return Ok(NO_KEY);
    }
    ensure!(!named.is_empty(), "nothing in the profile is named {name}");
    if named.len() > 1 {
        let candidates = named
            .iter()
            .map(|&key| format!("  {}", qualified(keys, key)))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "{} things are named {name}; use a more exact name:\n{candidates}",
            named.len()
        );
    }
    Ok(named[0])
}

fn qualified(keys: &Keys<'_>, key: u32) -> String {
    let mut parts = Vec::new();
    if let Some(module) = keys.module(key) {
        parts.push(module.to_owned());
    }
    if let Some((file, line)) = keys.location(key) {
        parts.push(format!("{file}:{line}"));
    }
    format!("{} [{}]", keys.name(key), parts.join(", "))
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
        ..Query::default()
    };
    let unit = unit(current, options.source);
    let focused = butterfly(&keys, &query, key);
    let before = base_keys
        .as_ref()
        .zip(base_key)
        .map(|(keys, key)| butterfly(keys, &query, key));
    if let Some(base_keys) = &base_keys {
        writeln!(
            out,
            "Before: {}",
            weight(base_keys.functions(&query).weight.into(), unit, false)
        )?;
        writeln!(
            out,
            "After: {}",
            weight(keys.functions(&query).weight.into(), unit, false)
        )?;
    }
    writeln!(out, "Focus: {name}")?;
    if let Some(before) = &before {
        writeln!(
            out,
            "{:>18} {:>18} {:>18}  cost",
            "before", "after", "change"
        )?;
        comparison(out, before.total, focused.total, unit)?;
        writeln!(out, "  total")?;
        comparison(out, before.own, focused.own, unit)?;
        writeln!(out, "  own")?;
    } else {
        writeln!(out, "Total: {}", weight(focused.total.into(), unit, false))?;
        writeln!(out, "Own: {}", weight(focused.own.into(), unit, false))?;
    }
    if options.tree {
        let mut selected = query;
        selected.transforms.push(Transform::Focus(key));
        let tree = keys.aggregate(&selected);
        if let Some((base_keys, base_key)) = base_keys.as_ref().zip(base_key) {
            selected.transforms = vec![Transform::Focus(base_key)];
            let base_tree = base_keys.aggregate(&selected);
            return compared_tree((&keys, &tree), (base_keys, &base_tree), options, unit, out);
        }
        writeln!(out, "{:>18} {:>18}  function", "total", "own")?;
        for node in 0..tree.len() {
            if percent(tree.total[node], tree.weight) < options.min_percent {
                continue;
            }
            writeln!(
                out,
                "{:>18} {:>18}  {}{}",
                weight(tree.total[node].into(), unit, false),
                weight(tree.own[node].into(), unit, false),
                "  ".repeat(tree.depth[node] as usize),
                keys.name(tree.key[node])
            )?;
        }
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
        writeln!(
            out,
            "{:>18} {:>18} {:>18}  function",
            "before", "after", "change"
        )?;
        for cost in rows.iter().take(options.limit) {
            comparison(out, cost.base_own, cost.own, unit)?;
            writeln!(
                out,
                "  {}",
                on_side(keys, base_keys, cost.side).name(cost.key)
            )?;
        }
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

fn comparison(out: &mut impl Write, before: u64, after: u64, unit: &str) -> Result<()> {
    write!(
        out,
        "{:>18} {:>18} {:>18}",
        weight(before.into(), unit, false),
        weight(after.into(), unit, false),
        weight(i128::from(after) - i128::from(before), unit, true)
    )?;
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
