//! strace's `-c` table, with microscope's columns beside its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use microscope::syscalls::{Abi, SyscallId, SyscallStats};

/// A column of the table, by its `-U` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    TimePercent,
    TotalTime,
    MinTime,
    MaxTime,
    AvgTime,
    Calls,
    Errors,
    Name,
    P50,
    P90,
    P99,
    Bytes,
    Errnos,
}

impl Column {
    const NAMED: [(&'static str, Self); 21] = [
        ("time-percent", Self::TimePercent),
        ("time", Self::TimePercent),
        ("total-time", Self::TotalTime),
        ("time-total", Self::TotalTime),
        ("min-time", Self::MinTime),
        ("shortest", Self::MinTime),
        ("max-time", Self::MaxTime),
        ("longest", Self::MaxTime),
        ("max", Self::MaxTime),
        ("avg-time", Self::AvgTime),
        ("time-avg", Self::AvgTime),
        ("calls", Self::Calls),
        ("count", Self::Calls),
        ("errors", Self::Errors),
        ("name", Self::Name),
        ("syscall", Self::Name),
        ("p50", Self::P50),
        ("p90", Self::P90),
        ("p99", Self::P99),
        ("bytes", Self::Bytes),
        ("errnos", Self::Errnos),
    ];

    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::NAMED
            .iter()
            .find(|&&(candidate, _)| candidate == name)
            .map(|&(_, column)| column)
    }

    /// strace's columns, in its order.
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
            Self::MinTime => "min usecs",
            Self::MaxTime => "max usecs",
            Self::AvgTime => "usecs/call",
            Self::Calls => "calls",
            Self::Errors => "errors",
            Self::Name => "syscall",
            Self::P50 => "p50 usecs",
            Self::P90 => "p90 usecs",
            Self::P99 => "p99 usecs",
            Self::Bytes => "bytes",
            Self::Errnos => "errnos",
        }
    }

    const fn width(self) -> usize {
        match self {
            Self::TimePercent => 6,
            Self::Calls | Self::Errors => 9,
            Self::Name => 16,
            Self::Errnos => 0,
            _ => 11,
        }
    }
}

/// Parses a `-U` list.
pub fn columns(text: &str) -> Result<Vec<Column>, String> {
    text.split(',')
        .map(|name| Column::named(name.trim()).ok_or_else(|| format!("unknown column {name:?}")))
        .collect()
}

/// Parses a `-S` key; `None` for strace's `nothing`.
pub fn sort_key(text: &str) -> Result<Option<Column>, String> {
    match text {
        "nothing" | "none" => Ok(None),
        "avg-time" | "time-avg" => Ok(Some(Column::AvgTime)),
        other => Column::named(other)
            .map(|column| {
                Some(if column == Column::TimePercent {
                    Column::TotalTime
                } else {
                    column
                })
            })
            .ok_or_else(|| format!("unknown sort key {other:?}")),
    }
}

const fn microseconds(nanoseconds: u64) -> u64 {
    nanoseconds / 1_000
}

fn name(id: SyscallId) -> String {
    let name = id.display_name();
    match id.abi {
        Abi::X86_64 => name.into_owned(),
        abi => format!("{name}@{}", abi.suffix()),
    }
}

/// The value a row sorts by, largest first except for names.
fn key(column: Column, id: SyscallId, stats: &SyscallStats) -> (u64, String) {
    let number = match column {
        Column::TimePercent | Column::TotalTime => stats.total_ns,
        Column::MinTime => stats.min_ns,
        Column::MaxTime => stats.max_ns,
        Column::AvgTime => stats.mean_ns().unwrap_or(0),
        Column::Calls => stats.calls,
        Column::Errors => stats.errors,
        Column::P50 => stats.quantile_ns(0.5).unwrap_or(0),
        Column::P90 => stats.quantile_ns(0.9).unwrap_or(0),
        Column::P99 => stats.quantile_ns(0.99).unwrap_or(0),
        Column::Bytes => stats.bytes,
        Column::Name | Column::Errnos => 0,
    };

    (u64::MAX - number, name(id))
}

/// One cell of the table.
fn cell(
    column: Column,
    stats: &SyscallStats,
    label: &str,
    all_time: u64,
    errno: &dyn Fn(u16) -> String,
) -> String {
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
        Column::TotalTime => format!(
            "{}.{:06}",
            stats.total_ns / 1_000_000_000,
            stats.total_ns % 1_000_000_000 / 1_000
        ),
        Column::MinTime => microseconds(stats.min_ns).to_string(),
        Column::MaxTime => microseconds(stats.max_ns).to_string(),
        Column::AvgTime => microseconds(stats.mean_ns().unwrap_or(0)).to_string(),
        Column::Calls => stats.calls.to_string(),
        Column::Errors if stats.errors == 0 => String::new(),
        Column::Errors => stats.errors.to_string(),
        Column::Name => label.to_owned(),
        Column::P50 => stats
            .quantile_ns(0.5)
            .map_or_else(String::new, |value| microseconds(value).to_string()),
        Column::P90 => stats
            .quantile_ns(0.9)
            .map_or_else(String::new, |value| microseconds(value).to_string()),
        Column::P99 => stats
            .quantile_ns(0.99)
            .map_or_else(String::new, |value| microseconds(value).to_string()),
        Column::Bytes if stats.bytes == 0 => String::new(),
        Column::Bytes => stats.bytes.to_string(),
        Column::Errnos => {
            let mut errnos: Vec<(u64, u16)> = stats
                .errors_by_errno
                .iter()
                .map(|(&errno, &count)| (count, errno))
                .collect();
            errnos.sort_by(|a, b| b.cmp(a));
            errnos
                .iter()
                .map(|&(count, number)| format!("{}×{count}", errno(number)))
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

/// The table, one line each, with errno names from `errno`.
#[must_use]
pub fn table(
    calls: &BTreeMap<SyscallId, SyscallStats>,
    columns: &[Column],
    sort: Option<Column>,
    errno: &dyn Fn(u16) -> String,
) -> Vec<String> {
    let mut rows: Vec<(SyscallId, &SyscallStats)> =
        calls.iter().map(|(&id, stats)| (id, stats)).collect();

    if let Some(sort) = sort {
        rows.sort_by_key(|&(id, stats)| {
            if sort == Column::Name {
                (0, name(id))
            } else {
                key(sort, id, stats)
            }
        });
    }

    let mut total = SyscallStats::default();

    for (_, stats) in &rows {
        total.merge(stats);
    }

    let all_time = total.total_ns;
    let cell =
        |column, stats: &SyscallStats, label: &str| cell(column, stats, label, all_time, errno);

    let line = |cells: &mut dyn Iterator<Item = (Column, String)>| -> String {
        let mut text = String::new();

        for (index, (column, value)) in cells.enumerate() {
            if index > 0 {
                text.push(' ');
            }

            let width = column.width();
            if matches!(column, Column::Name | Column::Errnos) {
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
        line(
            &mut columns
                .iter()
                .map(|&column| (column, column.header().to_owned())),
        ),
        rule.clone(),
    ];

    for (id, stats) in rows {
        let label = name(id);
        lines.push(line(
            &mut columns
                .iter()
                .map(|&column| (column, cell(column, stats, &label))),
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
    use microscope::syscalls::Outcome;

    use super::*;

    #[test]
    fn the_table_matches_strace_and_adds_percentiles() {
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

        let errno = |number: u16| {
            if number == 2 {
                "ENOENT".to_owned()
            } else {
                number.to_string()
            }
        };

        let lines = table(&calls, &Column::DEFAULT, Some(Column::TotalTime), &errno);
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

        let extra = columns("name,calls,p99,errnos").expect("columns");
        let lines = table(&calls, &extra, Some(Column::Name), &errno);
        assert_eq!(
            lines[2].split_whitespace().collect::<Vec<_>>(),
            ["openat", "2", "18", "ENOENT×1"]
        );
        assert!(columns("nonsense").is_err());
        assert_eq!(sort_key("nothing"), Ok(None));
    }
}
