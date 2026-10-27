use std::io::Write;

use anyhow::{Context as _, Result, ensure};
use microscope::profiler::{
    DiffTree, Granularity, Keys, Look, NO_KEY, Profile, Query, Side, Source, diff_costs, diff_trees,
};

pub struct Options {
    pub tree: bool,
    pub limit: usize,
    pub min_percent: f64,
    pub granularity: Granularity,
    pub source: Source,
    pub look: Look,
}

pub fn write(
    current: &Profile,
    base: Option<&Profile>,
    options: &Options,
    out: &mut impl Write,
) -> Result<()> {
    ensure!(
        options.min_percent.is_finite() && (0.0..=100.0).contains(&options.min_percent),
        "--min-percent must be between 0 and 100"
    );
    let base = base.context("a comparison needs --base FILE")?;
    ensure!(
        !current.is_trace() && !base.is_trace(),
        "a profile comparison needs samples, not a syscall trace alone"
    );
    let unit = unit(current, options.source);
    ensure!(
        unit == self::unit(base, options.source),
        "cannot compare profiles with different units: after is {unit}, before is {}",
        self::unit(base, options.source)
    );
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
        let diff = diff_trees((&keys, &tree), (&base_keys, &base_tree));
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
                on_side(&keys, &base_keys, diff.side[node]).name(diff.key[node])
            )?;
        }
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
