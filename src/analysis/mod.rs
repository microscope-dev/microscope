//! Queries of a capture, pure functions of its tables: the call tree a
//! selection of samples makes, the cost of each function, and how samples
//! fall over time. Every view and report reads a capture through here, so
//! none can disagree with another. A boundary test keeps process control,
//! I/O, clocks, threads, and debug information out.
//!
//! A frame stands for each function its code runs: its source lines, one
//! per inlined function and then the one it is compiled into. A frame
//! without symbols stands for itself, and a truncation for its reason.
//! Each is a dense key.

pub mod diff;
pub mod keys;
pub mod query;
#[cfg(test)]
mod tests;
pub mod zones;

pub use diff::{DiffCost, DiffTree, Side, diff_costs, diff_trees};
pub use keys::{Granularity, KeyKind, Keys};
pub use query::{
    CallTree, Cost, Costs, Lanes, Look, OffCpuLane, Query, Source, Transform, aggregate, density,
    functions, off_cpu,
};

pub use zones::{
    PlotBin, PlotSeries, ZoneAt, ZoneBlock, ZoneStats, plot_series, sites_named, zone_blocks,
    zone_stats,
};

/// A position in a capture's tables, which validation keeps within `u32`.
fn index(position: usize) -> u32 {
    let index = u32::try_from(position);
    debug_assert!(
        index.is_ok(),
        "a capture's tables are indexed by u32, not {position}"
    );
    index.unwrap_or(u32::MAX)
}
