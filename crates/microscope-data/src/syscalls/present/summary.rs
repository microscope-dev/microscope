use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::syscalls::{Abi, SyscallId, SyscallStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    TimePercent,
    TotalTime,
    AvgTime,
    Calls,
    Errors,
    P99,
    Name,
}

impl Column {
    pub const DEFAULT: [Self; 6] = [
        Self::TimePercent,
        Self::TotalTime,
        Self::AvgTime,
        Self::Calls,
        Self::Errors,
        Self::Name,
    ];

    const fn header(self) -> &'static str {
        match self {
            Self::TimePercent => "% time",
            Self::TotalTime => "seconds",
            Self::AvgTime => "usecs/call",
            Self::Calls => "calls",
            Self::Errors => "errors",
            Self::P99 => "p99 usecs",
            Self::Name => "syscall",
        }
    }

    const fn width(self) -> usize {
        match self {
            Self::TimePercent => 6,
            Self::Calls | Self::Errors => 9,
            Self::Name => 16,
            _ => 11,
        }
    }
}

fn seconds(nanoseconds: u64) -> String {
    format!(
        "{}.{:06}",
        nanoseconds / 1_000_000_000,
        nanoseconds % 1_000_000_000 / 1_000
    )
}

fn name(id: SyscallId) -> String {
    let name = id.display_name();
    match id.abi {
        Abi::X86_64 => name.into_owned(),
        abi => format!("{name}@{}", abi.suffix()),
    }
}

fn cell(column: Column, stats: &SyscallStats, label: &str, all_time: u64) -> String {
    match column {
        #[expect(clippy::cast_precision_loss, reason = "a percentage")]
        Column::TimePercent => {
            let percent = if all_time == 0 {
                0.0
            } else {
                stats.total_ns as f64 * 100.0 / all_time as f64
            };

            format!("{percent:.2}")
        }
        Column::TotalTime => seconds(stats.total_ns),
        Column::AvgTime => (stats.mean_ns().unwrap_or(0) / 1_000).to_string(),
        Column::Calls => stats.calls.to_string(),
        Column::Errors if stats.errors == 0 => String::new(),
        Column::Errors => stats.errors.to_string(),
        Column::Name => label.to_owned(),
        Column::P99 => stats
            .quantile_ns(0.99)
            .map_or_else(String::new, |value| (value / 1_000).to_string()),
    }
}

#[must_use]
pub fn table(calls: &BTreeMap<SyscallId, SyscallStats>, columns: &[Column]) -> Vec<String> {
    let rows = calls.iter().map(|(&id, stats)| (name(id), stats)).collect();
    labelled(rows, columns, "syscall", usize::MAX)
}

#[must_use]
pub fn labelled(
    mut rows: Vec<(String, &SyscallStats)>,
    columns: &[Column],
    heading: &str,
    limit: usize,
) -> Vec<String> {
    rows.sort_by_cached_key(|(label, stats)| (std::cmp::Reverse(stats.total_ns), label.clone()));

    let mut total = SyscallStats::default();

    for (_, stats) in &rows {
        total.merge(stats);
    }

    let all_time = total.total_ns;
    let cell = |column, stats: &SyscallStats, label: &str| cell(column, stats, label, all_time);

    let line = |cells: &mut dyn Iterator<Item = (Column, String)>| -> String {
        let mut text = String::new();

        for (index, (column, value)) in cells.enumerate() {
            if index > 0 {
                text.push(' ');
            }

            let width = column.width();
            if column == Column::Name {
                let _ = write!(text, "{value:<width$}");
            } else {
                let _ = write!(text, "{value:>width$}");
            }
        }

        text.trim_end().to_owned()
    };

    let rule = line(
        &mut columns
            .iter()
            .map(|&column| (column, "-".repeat(column.width().max(6)))),
    );

    let mut lines = vec![
        line(&mut columns.iter().map(|&column| {
            let header = if column == Column::Name {
                heading
            } else {
                column.header()
            };
            (column, header.to_owned())
        })),
        rule.clone(),
    ];

    for (label, stats) in rows.iter().take(limit) {
        lines.push(line(
            &mut columns
                .iter()
                .map(|&column| (column, cell(column, stats, label))),
        ));
    }

    lines.push(rule);
    lines.push(line(
        &mut columns
            .iter()
            .map(|&column| (column, cell(column, &total, "total"))),
    ));
    lines
}

#[cfg(test)]
mod tests {
    use crate::syscalls::Outcome;

    use super::*;

    #[test]
    fn the_table_matches_strace_and_adds_a_percentile() {
        let mut calls = BTreeMap::new();
        let id = |name| SyscallId::named(Abi::X86_64, name).expect(name);
        let mut write = SyscallStats::default();

        for _ in 0..3 {
            write.add(Outcome::Returned(1), Some(57_000), 1);
        }

        let mut open = SyscallStats::default();
        open.add(Outcome::Returned(3), Some(10_000), 0);
        open.add(Outcome::of_return(-2), Some(20_000), 0);
        calls.insert(id("write"), write);
        calls.insert(id("openat"), open);

        let lines = table(&calls, &Column::DEFAULT);
        assert_eq!(
            lines,
            [
                "% time     seconds  usecs/call     calls    errors syscall",
                "------ ----------- ----------- --------- --------- ----------------",
                " 85.07    0.000171          57         3           write",
                " 14.93    0.000030          15         2         1 openat",
                "------ ----------- ----------- --------- --------- ----------------",
                "100.00    0.000201          40         5         1 total",
            ]
        );

        let errors = [Column::Name, Column::Calls, Column::P99, Column::Errors];
        let lines = table(&calls, &errors);
        assert_eq!(
            lines[3].split_whitespace().collect::<Vec<_>>(),
            ["openat", "2", "18", "1"]
        );
    }
}
