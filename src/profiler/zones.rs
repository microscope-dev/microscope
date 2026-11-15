use super::{PlotSeries, Profile, ZoneBlock, ZoneStats};
use crate::capture::records::{AccountKind, MarkerKind, NONE};

pub use crate::capture::records::{
    ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, ZONE_LOST_WITHIN, ZONE_MISMATCHED,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileZone {
    pub pid: u32,
    pub tid: u32,

    pub depth: u32,

    pub name: String,

    pub value: Option<String>,
    pub start: i64,
    pub end: i64,

    pub flags: u32,

    pub runtime: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlotValue {
    pub name: String,
    pub tid: u32,
    pub time: i64,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramMarker {
    pub kind: MarkerKind,

    pub thread: Option<u32>,
    pub pid: u32,
    pub tid: u32,
    pub time: i64,

    pub name: String,

    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbeCounts {
    pub events: u64,

    pub overwritten: u64,

    pub no_ring: u64,

    pub interrupted: u64,

    pub malformed: u64,

    pub unread: u64,

    pub read: u64,

    pub plotted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneSite {
    pub name: String,

    pub label: bool,
    pub count: u64,

    pub total: u64,
    pub longest: u64,
    pub flagged: u64,
}

impl ProbeCounts {
    #[must_use]
    pub const fn lost(&self) -> bool {
        self.overwritten + self.no_ring + self.interrupted + self.malformed + self.unread > 0
    }
}

impl Profile {
    #[must_use]
    pub fn zones(&self) -> Vec<ProfileZone> {
        let capture = &self.capture;
        let mut zones = Vec::with_capacity(capture.zones.len());

        for range in &capture.zone_ranges {
            let thread = capture.thread(range.thread.get());
            let pid = capture.process(thread.process.get()).pid.get();
            let tid = thread.tid.get();

            for zone in capture.range_zones(range) {
                let value = zone.value.get();
                let site = capture.sites.get(zone.site.get() as usize);
                zones.push(ProfileZone {
                    pid,
                    tid,
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
                        pid: thread
                            .map_or(0, |thread| capture.process(thread.process.get()).pid.get()),
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

    #[must_use]
    pub fn probe_problems(&self) -> Vec<(i64, String)> {
        self.markers(MarkerKind::Probes)
    }

    #[must_use]
    pub fn probe_counts(&self) -> ProbeCounts {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        ProbeCounts {
            events: account(AccountKind::ProbeEvents),
            overwritten: account(AccountKind::ProbeOverwritten),
            no_ring: account(AccountKind::ProbeNoRing),
            interrupted: account(AccountKind::ProbeInterrupted),
            malformed: account(AccountKind::ProbeMalformed),
            unread: account(AccountKind::ProbesUnread),
            read: account(AccountKind::ProbesRead),
            plotted: self.capture.plots.len() as u64,
        }
    }

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

    #[must_use]
    pub fn zone_sites(&self) -> Vec<ZoneSite> {
        let capture = &self.capture;
        let mut sites: Vec<ZoneSite> = Vec::new();

        for zone in &capture.zones {
            let site = capture.site(zone.site.get());
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
            let Some(entry) = sites.get_mut(position) else {
                continue;
            };
            entry.count += 1;
            entry.total += length;
            entry.longest = entry.longest.max(length);
            entry.flagged += u64::from(zone.flags.get() != 0);
        }

        sites.sort_by(|a, b| (b.total, &a.name).cmp(&(a.total, &b.name)));
        sites
    }

    #[must_use]
    pub fn zone_blocks(
        &self,
        range: (i64, i64),
        threads: Option<&[u32]>,
        fold: i64,
        chosen: Option<(&str, Option<(u64, u64)>)>,
    ) -> Vec<ZoneBlock> {
        let sites = chosen
            .map(|(name, _)| self.sites_named(name))
            .unwrap_or_default();
        let chosen = chosen.map(|(_, band)| crate::analysis::ChosenZones {
            sites: &sites,
            band,
        });
        crate::analysis::zone_blocks(&self.capture, range, threads, fold, chosen)
    }

    #[must_use]
    pub fn zone_stats(
        &self,
        name: &str,
        of: ((i64, i64), Option<&[u32]>),
        band: Option<(u64, u64)>,
        rank_of: Option<u64>,
    ) -> ZoneStats {
        let sites = crate::analysis::sites_named(&self.capture, name);
        crate::analysis::zone_stats(&self.capture, &sites, of, band, rank_of)
    }

    #[must_use]
    pub fn sites_named(&self, name: &str) -> Vec<u32> {
        crate::analysis::sites_named(&self.capture, name)
    }

    #[must_use]
    pub fn plot_series(
        &self,
        range: (i64, i64),
        threads: Option<&[u32]>,
        bins: usize,
    ) -> Vec<PlotSeries> {
        crate::analysis::plot_series(&self.capture, range, threads, bins)
    }

    #[must_use]
    pub fn string(&self, string: u32) -> &str {
        self.capture.string(string)
    }

    #[must_use]
    pub fn zone_name(&self, site: u32, value: u32) -> String {
        match (site, value) {
            (NONE, _) => "zones".to_owned(),
            (site, NONE) => self.site_name(site),
            (site, value) => format!("{}={}", self.site_name(site), self.capture.string(value)),
        }
    }

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
