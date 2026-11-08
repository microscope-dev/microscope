//! Zones as views draw and measure them: each thread's tracks folded to
//! what a width of pixels can show, and the spread of one site's lengths.

use std::collections::BTreeMap;

use crate::capture::Capture;
use crate::capture::records::NONE;

/// What a view draws of one thread's zones at one depth: one zone, or a
/// run of zones too narrow to tell apart, drawn as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneBlock {
    pub thread: u32,
    pub depth: u32,
    pub start: i64,
    pub end: i64,

    /// The zone's site, or for a run, its zones' site, or [`NONE`] when
    /// they are of several.
    pub site: u32,

    /// A label's value, or [`NONE`].
    pub value: u32,

    /// How many zones it draws.
    pub count: u32,

    /// The `ZONE_` flags of any of them.
    pub flags: u32,
}

/// The zones in `range` of the threads `threads` selects, by thread and
/// depth, each run of zones narrower than `fold` with less than `fold`
/// between them drawn as one block.
pub fn zone_blocks(
    capture: &Capture,
    range: (i64, i64),
    threads: Option<&[u32]>,
    fold: i64,
) -> Vec<ZoneBlock> {
    let mut blocks = Vec::new();

    for zones in &capture.zone_ranges {
        let thread = zones.thread.get();
        if threads.is_some_and(|threads| !threads.contains(&thread)) {
            continue;
        }

        let row = capture.range_zones(zones);

        // One depth's zones do not overlap, so their ends are in order too.
        let first = row.partition_point(|zone| zone.end.get() <= range.0);
        let last = row.partition_point(|zone| zone.start.get() < range.1);
        #[expect(
            clippy::indexing_slicing,
            reason = "`partition_point` returns at most the length"
        )]
        let row = &row[first..last.max(first)];
        let mut run: Option<ZoneBlock> = None;
        for zone in row {
            let block = ZoneBlock {
                thread,
                depth: zones.depth.get(),
                start: zone.start.get(),
                end: zone.end.get(),
                site: zone.site.get(),
                value: zone.value.get(),
                count: 1,
                flags: zone.flags.get(),
            };

            if block.end - block.start >= fold {
                blocks.extend(run.take());
                blocks.push(block);
                continue;
            }

            match &mut run {
                Some(run) if block.start - run.end < fold => {
                    run.end = block.end;
                    run.count += 1;
                    run.flags |= block.flags;
                    if (run.site, run.value) != (block.site, block.value) {
                        (run.site, run.value) = (NONE, NONE);
                    }
                }
                _ => blocks.extend(run.replace(block)),
            }
        }

        blocks.extend(run);
    }

    blocks
}

/// One zone, where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneAt {
    pub thread: u32,
    pub start: i64,
    pub end: i64,
    pub value: u32,
}

/// The spread of one site's zone lengths, in nanoseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneStats {
    pub count: u64,
    pub total: u64,
    pub flagged: u64,
    pub min: u64,
    pub median: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,

    /// Lengths in buckets of equal ratio from the shortest to the longest:
    /// each its lowest length and count, the last bucket to `max`.
    pub histogram: Vec<(u64, u64)>,

    /// The longest in the band, longest first.
    pub longest: Vec<ZoneAt>,

    /// How many are in the band, and their time.
    pub banded: (u64, u64),

    /// How many are shorter than the length asked about.
    pub shorter: u64,

    /// Each label value's time and count, most time first: for a label's
    /// sites.
    pub values: Vec<(u32, u64, u64)>,
}

/// How many buckets a histogram has, and how many longest zones are kept.
const BUCKETS: usize = 32;
const LONGEST: usize = 10;

/// The spread of the lengths of the zones of `sites` that overlap
/// `range`, of the threads `threads` selects: those around a span chosen
/// in it too. The longest and `banded` are of those whose lengths are in
/// `band`, or of all; `shorter` counts those shorter than `rank_of`.
pub fn zone_stats(
    capture: &Capture,
    sites: &[u32],
    (range, threads): ((i64, i64), Option<&[u32]>),
    band: Option<(u64, u64)>,
    rank_of: Option<u64>,
) -> ZoneStats {
    let _span = crate::span!("analysis zone stats");
    let mut found = Vec::new();
    let mut flagged = 0;

    for zones in &capture.zone_ranges {
        let thread = zones.thread.get();
        if threads.is_some_and(|threads| !threads.contains(&thread)) {
            continue;
        }

        for zone in capture.range_zones(zones) {
            let end = zone.end.get();
            if sites.contains(&zone.site.get()) && range.0 <= end && zone.start.get() < range.1 {
                flagged += u64::from(zone.flags.get() != 0);
                found.push(ZoneAt {
                    thread,
                    start: zone.start.get(),
                    end,
                    value: zone.value.get(),
                });
            }
        }
    }

    let length = |zone: &ZoneAt| u64::try_from(zone.end - zone.start).unwrap_or(0);
    found.sort_by_key(|zone| std::cmp::Reverse(length(zone)));
    let lengths: Vec<u64> = found.iter().rev().map(length).collect();
    let Some((&min, &max)) = lengths.first().zip(lengths.last()) else {
        return ZoneStats::default();
    };

    // The length below which `share` of them are.
    #[expect(
        clippy::indexing_slicing,
        reason = "`lengths` is not empty, and the rank is clamped to its length"
    )]
    let at = |share: f64| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a rank within the zones"
        )]
        let rank = ((lengths.len() as f64 * share).ceil() as usize).clamp(1, lengths.len());
        lengths[rank - 1]
    };

    let mut values: Vec<(u32, u64, u64)> = Vec::new();
    for zone in found.iter().filter(|zone| zone.value != NONE) {
        match values.iter_mut().find(|(value, _, _)| *value == zone.value) {
            Some(entry) => {
                entry.1 += length(zone);
                entry.2 += 1;
            }
            None => values.push((zone.value, length(zone), 1)),
        }
    }

    values.sort_by(|a, b| (b.1, a.0).cmp(&(a.1, b.0)));
    let in_band =
        |zone: &&ZoneAt| band.is_none_or(|(low, high)| (low..=high).contains(&length(zone)));
    let banded = found
        .iter()
        .filter(in_band)
        .fold((0, 0), |(count, total), zone| {
            (count + 1, total + length(zone))
        });
    let shorter = rank_of.map_or(0, |of| lengths.partition_point(|&each| each < of) as u64);
    let longest = found
        .iter()
        .filter(in_band)
        .take(LONGEST)
        .copied()
        .collect();
    ZoneStats {
        count: lengths.len() as u64,
        total: lengths.iter().sum(),
        flagged,
        min,
        median: at(0.5),
        p90: at(0.9),
        p99: at(0.99),
        max,
        histogram: histogram(&lengths, min, max),
        longest,
        banded,
        shorter,
        values,
    }
}

/// Sorted lengths in [`BUCKETS`] buckets of equal ratio from `min` to
/// `max`, or one bucket when they are all one length.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "bucket edges are approximate by nature"
)]
#[expect(
    clippy::indexing_slicing,
    reason = "`buckets` has `BUCKETS` entries, and a bucket stays below that"
)]
fn histogram(lengths: &[u64], min: u64, max: u64) -> Vec<(u64, u64)> {
    if min == max {
        return vec![(min, lengths.len() as u64)];
    }

    let (low, high) = ((min.max(1) as f64).ln(), (max as f64).ln());
    let edge = |bucket: usize| {
        if bucket == 0 {
            min
        } else {
            (low + (high - low) * bucket as f64 / BUCKETS as f64).exp() as u64
        }
    };

    let mut buckets: Vec<(u64, u64)> = (0..BUCKETS).map(|bucket| (edge(bucket), 0)).collect();
    let mut bucket = 0;
    for &length in lengths {
        while bucket + 1 < BUCKETS && length >= buckets[bucket + 1].0 {
            bucket += 1;
        }

        buckets[bucket].1 += 1;
    }

    buckets
}

/// One plotted series in a range: every plot site of one name.
#[derive(Debug, Clone, PartialEq)]
pub struct PlotSeries {
    /// Its name's string.
    pub name: u32,

    /// How many values it drew, and how many were not finite.
    pub values: u64,
    pub unshown: u64,

    /// By time, each a bin's values.
    pub bins: Vec<PlotBin>,

    /// The last value drawable before the range, which holds into it.
    pub held: Option<f64>,

    /// Whether a value follows the range, so the last one holds to its end.
    pub continues: bool,
}

/// The values a series plotted in one bin of time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotBin {
    /// Its first value's time.
    pub time: i64,
    pub first: f64,
    pub last: f64,
    pub low: f64,
    pub high: f64,
}

/// Each series' values in `range`, of the threads `threads` selects, by
/// name, in `bins` bins of equal time, with what holds at either end. A
/// series with no values in `range` is there when one holds into it.
pub fn plot_series(
    capture: &Capture,
    range: (i64, i64),
    threads: Option<&[u32]>,
    bins: usize,
) -> Vec<PlotSeries> {
    let chosen = || {
        capture
            .plots
            .iter()
            .filter(|plot| threads.is_none_or(|threads| threads.contains(&plot.thread.get())))
    };

    // A plot of no site is of no name.
    let name_of = |site: u32| {
        if site == NONE {
            NONE
        } else {
            capture.site(site).name.get()
        }
    };

    // By name: the last drawable value before the range, and whether any
    // value follows it.
    let mut edges: BTreeMap<u32, (Option<(i64, f64)>, bool)> = BTreeMap::new();

    for plot in chosen() {
        let (time, value) = (plot.time.get(), f64::from_bits(plot.value.get()));
        if time < range.0 && value.is_finite() {
            let held = &mut edges.entry(name_of(plot.site.get())).or_default().0;
            if held.is_none_or(|(at, _)| at <= time) {
                *held = Some((time, value));
            }
        } else if time >= range.1 {
            edges.entry(name_of(plot.site.get())).or_default().1 = true;
        }
    }

    let mut values: Vec<(&str, u32, i64, f64)> = chosen()
        .filter(|plot| (range.0..range.1).contains(&plot.time.get()))
        .map(|plot| {
            let name = name_of(plot.site.get());
            let value = f64::from_bits(plot.value.get());
            (capture.string(name), name, plot.time.get(), value)
        })
        .collect();
    values.sort_by(|a, b| (a.0, a.2).cmp(&(b.0, b.2)));

    let width = i128::from((range.1 - range.0).max(1));
    let bin_of = |time: i64| i128::from(time - range.0) * bins.max(1) as i128 / width;
    let mut series: Vec<PlotSeries> = Vec::new();
    let mut last_bin = 0;
    for (_, name, time, value) in values {
        if series.last().is_none_or(|series| series.name != name) {
            series.push(PlotSeries {
                name,
                values: 0,
                unshown: 0,
                bins: Vec::new(),
                held: None,
                continues: false,
            });
        }

        let Some(series) = series.last_mut() else {
            continue;
        };

        if !value.is_finite() {
            series.unshown += 1;
            continue;
        }

        series.values += 1;
        let bin = bin_of(time);
        match series.bins.last_mut() {
            Some(open) if bin == last_bin => {
                open.last = value;
                open.low = open.low.min(value);
                open.high = open.high.max(value);
            }
            _ => series.bins.push(PlotBin {
                time,
                first: value,
                last: value,
                low: value,
                high: value,
            }),
        }

        last_bin = bin;
    }

    for (&name, &(held, _)) in &edges {
        if held.is_some() && series.iter().all(|series| series.name != name) {
            series.push(PlotSeries {
                name,
                values: 0,
                unshown: 0,
                bins: Vec::new(),
                held: None,
                continues: false,
            });
        }
    }

    for series in &mut series {
        if let Some(&(held, continues)) = edges.get(&series.name) {
            series.held = held.map(|(_, value)| value);
            series.continues = continues;
        }
    }

    series.sort_by(|a, b| capture.string(a.name).cmp(capture.string(b.name)));
    series
}

/// The sites `name` names, of zones or labels.
pub fn sites_named(capture: &Capture, name: &str) -> Vec<u32> {
    (0..)
        .zip(&capture.sites)
        .filter(|(_, site)| capture.string(site.name.get()) == name)
        .map(|(row, _)| row)
        .collect()
}
