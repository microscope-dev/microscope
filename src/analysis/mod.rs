pub mod diff;
pub mod kernel;
pub mod keys;
pub mod query;
#[cfg(test)]
mod tests;
pub mod zones;

pub use diff::{DiffCost, DiffTree, Side, diff_costs, diff_trees};
pub use kernel::{KernelLane, kernel_lanes};
pub use keys::{Granularity, KeyKind, Keys};
pub use query::{
    CallTree, Cost, Costs, Lanes, Look, OffCpuLane, Query, Source, Transform, ZoneScope, aggregate,
    density, functions, off_cpu, syscalls,
};

pub use zones::{
    ChosenZones, PlotBin, PlotSeries, ZoneAt, ZoneBlock, ZoneStats, plot_series, sites_named,
    zone_blocks, zone_stats,
};

fn index(position: usize) -> u32 {
    let index = u32::try_from(position);
    debug_assert!(
        index.is_ok(),
        "a capture's tables are indexed by u32, not {position}"
    );
    index.unwrap_or(u32::MAX)
}
