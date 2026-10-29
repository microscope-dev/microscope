//! What a profile's instrumentation recorded: zones and labels, plotted
//! values, the program's markers, and what was lost.

use super::{PlotSeries, Profile, ZoneBlock, ZoneStats};
use crate::capture::records::{AccountKind, MarkerKind, NONE};

pub use crate::capture::records::{
    ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, ZONE_LOST_WITHIN, ZONE_MISMATCHED,
};

/// One instrumented zone, or one label's span, of a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileZone {
    pub pid: u32,
    pub tid: u32,

    /// How many of the thread's zones hold it.
    pub depth: u32,

    /// Its site's name: a zone's name, or a label's key.
    pub name: String,

    /// A label's value; `None` for a zone.
    pub value: Option<String>,
    pub start: i64,
    pub end: i64,

    /// `ZONE_` flags: what of it was not seen.
    pub flags: u32,

    /// Whether the recorder gave the thread this label from what its
    /// runtime says of it, as whether it holds the GIL, rather than the
    /// program's instrumentation.
    pub runtime: bool,
}

/// A value of a plotted series.
#[derive(Debug, Clone, PartialEq)]
pub struct PlotValue {
    pub name: String,
    pub tid: u32,
    pub time: i64,
    pub value: f64,
}

/// An instant a program marked on a thread: a frame's end, a message, or
/// a trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramMarker {
    pub kind: MarkerKind,

    /// The row of the thread that marked it, by which a [`Query`] selects
    /// the thread, if the record names one.
    ///
    /// [`Query`]: super::Query
    pub thread: Option<u32>,
    pub pid: u32,
    pub tid: u32,
    pub time: i64,

    /// Its site's name.
    pub name: String,

    /// A message's text.
    pub text: Option<String>,
}

/// What a recording read of a program's instrumentation, and lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbeCounts {
    pub events: u64,

    /// Records threads wrote over before microscope read them.
    pub overwritten: u64,

    /// Events no ring could take.
    pub no_ring: u64,

    /// Events a signal handler raised while its thread was writing.
    pub interrupted: u64,

    /// Records, and rings, that broke the ABI.
    pub malformed: u64,

    /// Values plotted.
    pub plotted: u64,
}

/// Every zone of one site name, or one label key, over a whole profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneSite {
    pub name: String,

    /// Whether its sites are labels.
    pub label: bool,
    pub count: u64,

    /// Nanoseconds, all of them and the longest.
    pub total: u64,
    pub longest: u64,
    pub flagged: u64,
}

impl ProbeCounts {
    /// Whether anything was lost.
    #[must_use]
    pub const fn lost(&self) -> bool {
        self.overwritten + self.no_ring + self.interrupted + self.malformed > 0
    }
}

impl Profile {
    /// Every zone and label span, by thread, depth, and start.
    #[must_use]
    pub fn zones(&self) -> Vec<ProfileZone> {
        let capture = &self.capture;
        let mut zones = Vec::with_capacity(capture.zones.len());

        for range in &capture.zone_ranges {
            let thread = &capture.threads[range.thread.get() as usize];
            let pid = capture.processes[thread.process.get() as usize].pid.get();
            let start = range.start.get() as usize;

            for zone in &capture.zones[start..start + range.count.get() as usize] {
                let value = zone.value.get();
                let site = capture.sites.get(zone.site.get() as usize);
                zones.push(ProfileZone {
                    pid,
                    tid: thread.tid.get(),
                    depth: range.depth.get(),
                    name: self.site_name(zone.site.get()),
                    value: (value != NONE).then(|| capture.string(value).to_owned()),
                    start: zone.start.get(),
                    end: zone.end.get(),
                    flags: zone.flags.get(),
                    runtime: site.is_some_and(|site| site.module.get() == NONE),
                });
            }
        }

        zones
    }

    /// Every plotted value, by series and time.
    #[must_use]
    pub fn plots(&self) -> Vec<PlotValue> {
        let capture = &self.capture;
        capture
            .plots
            .iter()
            .map(|plot| PlotValue {
                name: self.site_name(plot.site.get()),
                tid: self.tid(plot.thread.get()),
                time: plot.time.get(),
                value: f64::from_bits(plot.value.get()),
            })
            .collect()
    }

    /// Every instant the program marked, by time.
    #[must_use]
    pub fn program_markers(&self) -> Vec<ProgramMarker> {
        let capture = &self.capture;
        capture
            .markers
            .iter()
            .filter_map(|marker| {
                let kind = MarkerKind::from_number(marker.kind)?;
                matches!(
                    kind,
                    MarkerKind::Frame | MarkerKind::Message | MarkerKind::Trigger
                )
                .then(|| {
                    let thread = capture.threads.get(marker.thread.get() as usize);
                    ProgramMarker {
                        kind,
                        thread: thread.map(|_| marker.thread.get()),
                        pid: thread.map_or(0, |thread| {
                            capture.processes[thread.process.get() as usize].pid.get()
                        }),
                        tid: thread.map_or(0, |thread| thread.tid.get()),
                        time: marker.time.get(),
                        name: self.site_name(marker.site.get()),
                        text: (marker.text.get() != NONE)
                            .then(|| capture.string(marker.text.get()).to_owned()),
                    }
                })
            })
            .collect()
    }

    /// Why microscope could not read a module's instrumentation.
    #[must_use]
    pub fn probe_problems(&self) -> Vec<(i64, String)> {
        self.markers(MarkerKind::Probes)
    }

    /// What the recording read of the program's instrumentation, and lost.
    #[must_use]
    pub fn probe_counts(&self) -> ProbeCounts {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        ProbeCounts {
            events: account(AccountKind::ProbeEvents),
            overwritten: account(AccountKind::ProbeOverwritten),
            no_ring: account(AccountKind::ProbeNoRing),
            interrupted: account(AccountKind::ProbeInterrupted),
            malformed: account(AccountKind::ProbeMalformed),
            plotted: self.capture.plots.len() as u64,
        }
    }

    /// The span the program's instrumentation covers: from the first
    /// zone's start or mark to the last zone's end or mark, if any.
    #[must_use]
    pub fn zone_extent(&self) -> Option<(i64, i64)> {
        let capture = &self.capture;
        let marks = capture
            .markers
            .iter()
            .filter(|marker| marker.site.get() != NONE)
            .map(|marker| (marker.time.get(), marker.time.get()));
        capture
            .zones
            .iter()
            .map(|zone| (zone.start.get(), zone.end.get()))
            .chain(marks)
            .reduce(|(start, end), (from, to)| (start.min(from), end.max(to)))
    }

    /// What each site name's zones, or label key's spans, add up to, most
    /// time first.
    #[must_use]
    pub fn zone_sites(&self) -> Vec<ZoneSite> {
        let capture = &self.capture;
        let mut sites: Vec<ZoneSite> = Vec::new();

        for zone in &capture.zones {
            let site = &capture.sites[zone.site.get() as usize];
            let name = capture.string(site.name.get());
            let label = site.kind == microscope_probes::abi::SITE_LABEL;
            let position = sites
                .iter()
                .position(|known| known.name == name && known.label == label)
                .unwrap_or_else(|| {
                    sites.push(ZoneSite {
                        name: name.to_owned(),
                        label,
                        count: 0,
                        total: 0,
                        longest: 0,
                        flagged: 0,
                    });
                    sites.len() - 1
                });

            let length = u64::try_from(zone.end.get() - zone.start.get()).unwrap_or(0);
            let entry = &mut sites[position];
            entry.count += 1;
            entry.total += length;
            entry.longest = entry.longest.max(length);
            entry.flagged += u64::from(zone.flags.get() != 0);
        }

        sites.sort_by(|a, b| (b.total, &a.name).cmp(&(a.total, &b.name)));
        sites
    }

    /// The zones in `range` of the threads `threads` names by row, each
    /// run narrower than `fold` nanoseconds drawn as one.
    #[must_use]
    pub fn zone_blocks(
        &self,
        range: (i64, i64),
        threads: Option<&[u32]>,
        fold: i64,
    ) -> Vec<ZoneBlock> {
        crate::analysis::zone_blocks(&self.capture, range, threads, fold)
    }

    /// The spread of the lengths of the zones a site name, or label key,
    /// names that overlap `range`.
    #[must_use]
    pub fn zone_stats(&self, name: &str, range: (i64, i64), threads: Option<&[u32]>) -> ZoneStats {
        let sites = crate::analysis::sites_named(&self.capture, name);
        crate::analysis::zone_stats(&self.capture, &sites, range, threads)
    }

    /// Each plotted series' values in `range`, of the threads `threads`
    /// names by row, in `bins` bins of equal time.
    #[must_use]
    pub fn plot_series(
        &self,
        range: (i64, i64),
        threads: Option<&[u32]>,
        bins: usize,
    ) -> Vec<PlotSeries> {
        crate::analysis::plot_series(&self.capture, range, threads, bins)
    }

    /// A string of the profile's, by index.
    #[must_use]
    pub fn string(&self, string: u32) -> &str {
        self.capture.string(string)
    }

    /// How views name a zone of `site` with the label value `value`: its
    /// name, or `key=value`; a run of several sites is `zones`.
    #[must_use]
    pub fn zone_name(&self, site: u32, value: u32) -> String {
        match (site, value) {
            (NONE, _) => "zones".to_owned(),
            (site, NONE) => self.site_name(site),
            (site, value) => format!("{}={}", self.site_name(site), self.capture.string(value)),
        }
    }

    /// A label value's text, if the zone has one.
    #[must_use]
    pub fn label_value(&self, value: u32) -> Option<&str> {
        (value != NONE).then(|| self.capture.string(value))
    }

    fn site_name(&self, site: u32) -> String {
        self.capture
            .sites
            .get(site as usize)
            .map(|site| self.capture.string(site.name.get()).to_owned())
            .unwrap_or_default()
    }

    fn tid(&self, thread: u32) -> u32 {
        self.capture
            .threads
            .get(thread as usize)
            .map_or(0, |thread| thread.tid.get())
    }
}
