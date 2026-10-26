//! strace's `-c` table, with microscope's columns beside its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use microscope::syscalls::{Abi, SyscallId, SyscallStats};

use super::{Precision, seconds};

/// The unit of the table's times: strace's microseconds, or nanoseconds
/// with `--syscall-times=ns`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Unit {
    #[default]
    Microseconds,
    Nanoseconds,
}

impl Unit {
    const fn of(self, nanoseconds: u64) -> u64 {
        match self {
            Self::Microseconds => nanoseconds / 1_000,
            Self::Nanoseconds => nanoseconds,
        }
    }

    const fn precision(self) -> Precision {
        match self {
            Self::Microseconds => Precision::Microseconds,
            Self::Nanoseconds => Precision::Nanoseconds,
        }
    }
}

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

    const fn header(self, unit: Unit) -> &'static str {
        let nanoseconds = matches!(unit, Unit::Nanoseconds);
        match self {
            Self::TimePercent => "% time",
            Self::TotalTime => "seconds",
            Self::MinTime if nanoseconds => "min nsecs",
            Self::MinTime => "min usecs",
            Self::MaxTime if nanoseconds => "max nsecs",
            Self::MaxTime => "max usecs",
            Self::AvgTime if nanoseconds => "nsecs/call",
            Self::AvgTime => "usecs/call",
            Self::Calls => "calls",
            Self::Errors => "errors",
            Self::Name => "syscall",
            Self::P50 if nanoseconds => "p50 nsecs",
            Self::P50 => "p50 usecs",
            Self::P90 if nanoseconds => "p90 nsecs",
            Self::P90 => "p90 usecs",
            Self::P99 if nanoseconds => "p99 nsecs",
            Self::P99 => "p99 usecs",
            Self::Bytes => "bytes",
            Self::Errnos => "errnos",
        }
    }

    const fn width(self, unit: Unit) -> usize {
        match self {
            Self::TimePercent => 6,
            // Room for as many whole seconds as strace's column has.
            Self::TotalTime if matches!(unit, Unit::Nanoseconds) => 14,
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

fn name(id: SyscallId) -> String {
    let name = id.display_name();
    match id.abi {
        Abi::X86_64 => name.into_owned(),
        abi => format!("{name}@{}", abi.suffix()),
    }
}

/// The value a row sorts by, largest first except for names.
fn key(column: Column, label: &str, stats: &SyscallStats) -> (u64, String) {
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

    (u64::MAX - number, label.to_owned())
}

/// One cell of the table.
fn cell(
    column: Column,
    stats: &SyscallStats,
    label: &str,
    all_time: u64,
    unit: Unit,
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
        Column::TotalTime => seconds(stats.total_ns, unit.precision()),
        Column::MinTime => unit.of(stats.min_ns).to_string(),
        Column::MaxTime => unit.of(stats.max_ns).to_string(),
        Column::AvgTime => unit.of(stats.mean_ns().unwrap_or(0)).to_string(),
        Column::Calls => stats.calls.to_string(),
        Column::Errors if stats.errors == 0 => String::new(),
        Column::Errors => stats.errors.to_string(),
        Column::Name => label.to_owned(),
        Column::P50 => stats
            .quantile_ns(0.5)
            .map_or_else(String::new, |value| unit.of(value).to_string()),
        Column::P90 => stats
            .quantile_ns(0.9)
            .map_or_else(String::new, |value| unit.of(value).to_string()),
        Column::P99 => stats
            .quantile_ns(0.99)
            .map_or_else(String::new, |value| unit.of(value).to_string()),
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

/// The table, one line each, with times in `unit` and errno names from
/// `errno`.
#[must_use]
pub fn table(
    calls: &BTreeMap<SyscallId, SyscallStats>,
    columns: &[Column],
    sort: Option<Column>,
    unit: Unit,
    errno: &dyn Fn(u16) -> String,
) -> Vec<String> {
    let rows = calls.iter().map(|(&id, stats)| (name(id), stats)).collect();
    labelled(rows, columns, sort, "syscall", usize::MAX, unit, errno)
}

/// The table of rows each named by its label, the column of labels
/// headed `heading`, showing the first `limit` rows after sorting; the
/// total counts every row.
#[must_use]
pub fn labelled(
    mut rows: Vec<(String, &SyscallStats)>,
    columns: &[Column],
    sort: Option<Column>,
    heading: &str,
    limit: usize,
    unit: Unit,
    errno: &dyn Fn(u16) -> String,
) -> Vec<String> {
    if let Some(sort) = sort {
        rows.sort_by_cached_key(|(label, stats)| {
            if sort == Column::Name {
                (0, label.clone())
            } else {
                key(sort, label, stats)
            }
        });
    }

    let mut total = SyscallStats::default();

    for (_, stats) in &rows {
        total.merge(stats);
    }

    let all_time = total.total_ns;
    let cell = |column, stats: &SyscallStats, label: &str| {
        cell(column, stats, label, all_time, unit, errno)
    };

    let line = |cells: &mut dyn Iterator<Item = (Column, String)>| -> String {
        let mut text = String::new();

        for (index, (column, value)) in cells.enumerate() {
            if index > 0 {
                text.push(' ');
            }

            let width = column.width(unit);
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
            .map(|&column| (column, "-".repeat(column.width(unit).max(6)))),
    );

    let mut lines = vec![
        line(&mut columns.iter().map(|&column| {
            let header = if column == Column::Name {
                heading
            } else {
                column.header(unit)
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

        let lines = table(
            &calls,
            &Column::DEFAULT,
            Some(Column::TotalTime),
            Unit::Microseconds,
            &errno,
        );
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
        let lines = table(
            &calls,
            &extra,
            Some(Column::Name),
            Unit::Microseconds,
            &errno,
        );
        assert_eq!(
            lines[2].split_whitespace().collect::<Vec<_>>(),
            ["openat", "2", "18", "ENOENT×1"]
        );
        assert!(columns("nonsense").is_err());
        assert_eq!(sort_key("nothing"), Ok(None));
    }

    /// A call under a microsecond is 0 in strace's units and itself in
    /// nanoseconds, and the total keeps every nanosecond.
    #[test]
    fn nanoseconds_show_what_microseconds_cut_to_zero() {
        let mut getpid = SyscallStats::default();
        for duration in [180, 240, 600] {
            getpid.add(Outcome::Returned(1), Some(duration), 0);
        }

        let calls = BTreeMap::from([(
            SyscallId::named(Abi::X86_64, "getpid").expect("getpid"),
            getpid,
        )]);
        let columns = columns("total-time,min-time,max-time,avg-time,calls,name").expect("columns");
        let lines = |unit| {
            table(&calls, &columns, None, unit, &|number: u16| {
                number.to_string()
            })
        };

        assert_eq!(
            lines(Unit::Microseconds)[2],
            "   0.000001           0           0           0         3 getpid"
        );
        assert_eq!(
            lines(Unit::Nanoseconds),
            [
                "       seconds   min nsecs   max nsecs  nsecs/call     calls syscall",
                "-------------- ----------- ----------- ----------- --------- ----------------",
                "   0.000001020         180         600         340         3 getpid",
                "-------------- ----------- ----------- ----------- --------- ----------------",
                "   0.000001020         180         600         340         3 total",
            ]
        );
    }
}
