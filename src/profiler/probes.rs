//! Instrumentation: the sites a program compiled in with microscope's probes
//! SDK, and the records its threads write to rings in its own memory.
//!
//! The ABI is the SDK's `abi` module. This module is pure: it parses a
//! module's sites from its file, validates what the collector reads of a
//! program's memory, and turns records into zones, labels, plots, and
//! markers. A program can make its own instrumentation wrong, and every
//! record that breaks the ABI is counted, but nothing it writes can crash
//! or stall microscope.

use std::collections::BTreeMap;
use std::ops::Range;

use microscope_probes::abi;
use object::{Object as _, ObjectSection as _};

use crate::capture::CaptureBuilder;
use crate::capture::records::{
    MarkerKind, NONE, ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, ZONE_MISMATCHED, Zone,
};

use crate::model::Tid;

/// A module's instrumentation: where its control block links, and its
/// sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub control: u64,

    /// Its ABI version, from 2 of which the program may define sites as it
    /// runs.
    pub abi: u16,
    pub sites: Vec<SiteRecord>,
}

/// One site, as its module's file describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteRecord {
    /// What records call it: a static site's state byte's offset from the
    /// control block, or a run-time site's key.
    pub key: u32,
    pub kind: u8,
    pub line: u32,
    pub name: String,
    pub file: String,

    /// Where its state byte links.
    pub state: u64,
}

/// The longest name or file a site may have.
const MOST_NAME: u32 = abi::MOST_SITE_TEXT;

/// The most records one event takes: a longest text and its record.
pub const MOST_EVENT_RECORDS: u64 = 1 + abi::MOST_TEXT.div_ceil(15) as u64;

/// A module's instrumentation, if its file has any.
pub fn table(file: &object::File<'_>) -> Result<Option<Table>, String> {
    let Some(control) = file.section_by_name("microscope_probe_control") else {
        return Ok(None);
    };

    let block = control
        .data()
        .ok()
        .filter(|data| data.len() >= abi::CONTROL_SIZE)
        .ok_or("its control block is cut short")?;
    let version = version(block)?;

    let sites = file
        .section_by_name("microscope_sites")
        .ok_or("it has a control block but no sites")?;
    let data = sites
        .data()
        .map_err(|error| format!("its sites cannot be read: {error}"))?;
    if !data.len().is_multiple_of(abi::SITE_SIZE) {
        return Err("its site table is cut short".into());
    }

    // The bytes a site names, from whichever section holds them.
    let bytes = |address: u64, length: u32| {
        file.sections().find_map(|section| {
            section
                .data_range(address, u64::from(length))
                .ok()
                .flatten()
        })
    };

    let text = |at: u64, record: &[u8], what: &str| -> Result<String, String> {
        let offset = i32::from_le_bytes(record[..4].try_into().unwrap_or_default());
        let length = u32::from_le_bytes(record[4..8].try_into().unwrap_or_default());
        if length > MOST_NAME {
            return Err(format!("a site's {what} is too long"));
        }

        let bytes = bytes(at.wrapping_add_signed(i64::from(offset)), length)
            .ok_or_else(|| format!("a site's {what} is not in the file"))?;
        String::from_utf8(bytes.to_vec()).map_err(|_| format!("a site's {what} is not UTF-8"))
    };

    let mut table = Table {
        control: control.address(),
        abi: version,
        sites: Vec::new(),
    };

    let (records, _) = data.as_chunks::<{ abi::SITE_SIZE }>();

    for (index, record) in records.iter().enumerate() {
        let at = sites.address() + (index * abi::SITE_SIZE) as u64;
        let field = |offset: usize| {
            u32::from_le_bytes(record[offset..offset + 4].try_into().unwrap_or_default())
        };

        if record[0] != abi::SITE_VERSION {
            return Err(format!(
                "a site is version {}, which this microscope does not read",
                record[0]
            ));
        }

        let kind = record[1];
        if kind == abi::SITE_NONE {
            continue;
        }

        if kind > abi::SITE_TRIGGER {
            return Err(format!("a site has unknown kind {kind}"));
        }

        let state = (at + 24).wrapping_add_signed(i64::from(field(24).cast_signed()));
        let key = static_key(version, state.wrapping_sub(table.control).cast_signed())
            .ok_or("a site's state is too far from its control block")?;
        table.sites.push(SiteRecord {
            key,
            kind,
            line: field(4),
            name: text(at + 8, &record[8..16], "name")?,
            file: text(at + 16, &record[16..24], "file")?,
            state,
        });
    }

    Ok(Some(table))
}

/// The ABI version of the control block `block`: 1, without run-time
/// sites, or 2.
fn version(block: &[u8]) -> Result<u16, String> {
    if block.len() < abi::CONTROL_SIZE || block[..8] != abi::MAGIC {
        return Err("its control block is not one".into());
    }

    let version = u16::from_le_bytes([block[8], block[9]]);
    if !(1..=abi::ABI).contains(&version) {
        return Err(format!(
            "its probes are ABI version {version}, which this microscope does not read"
        ));
    }

    Ok(version)
}

/// The key of a static site of ABI `version` whose state byte is `offset`
/// from its control block. From ABI 2 it leaves run-time sites theirs.
fn static_key(version: u16, offset: i64) -> Option<u32> {
    let near = version < 2 || (-abi::MOST_STATIC_OFFSET..abi::MOST_STATIC_OFFSET).contains(&offset);
    i32::try_from(offset)
        .ok()
        .filter(|_| near)
        .map(i32::cast_unsigned)
}

/// How many run-time sites the table whose header is `bytes` counts.
pub fn runtime_sites(bytes: &[u8]) -> Result<u32, &'static str> {
    if bytes.len() < abi::SITES_HEADER
        || bytes[..4] != abi::SITES_MAGIC.to_le_bytes()
        || bytes[4] != abi::SITES_VERSION
    {
        return Err("the run-time sites are not a table this microscope reads");
    }

    let word = |offset: usize| {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default())
    };
    let most = u32::try_from(abi::MOST_RUNTIME_SITES).unwrap_or(u32::MAX);
    Ok(word(12).min(word(8)).min(most))
}

/// Where the run-time site `index` of the table at `table` lies.
pub const fn runtime_site_address(table: u64, index: u32) -> u64 {
    table + (abi::SITES_HEADER + index as usize * abi::RUNTIME_SITE_SIZE) as u64
}

/// The run-time site `index` of the table at `table`, from its `record`;
/// `text` reads the bytes of its name and file from the program.
pub fn runtime_site(
    table: u64,
    index: u32,
    record: &[u8],
    mut text: impl FnMut(u64, u32) -> Option<Vec<u8>>,
) -> Result<SiteRecord, String> {
    if record.len() < abi::RUNTIME_SITE_SIZE || u64::from(index) >= abi::MOST_RUNTIME_SITES as u64 {
        return Err("a run-time site is cut short".into());
    }

    let kind = record[0];
    if !(abi::SITE_ZONE..=abi::SITE_TRIGGER).contains(&kind) {
        return Err(format!("a run-time site has unknown kind {kind}"));
    }

    let word = |offset: usize| {
        u32::from_le_bytes(record[offset..offset + 4].try_into().unwrap_or_default())
    };
    let long = |offset: usize| {
        u64::from_le_bytes(record[offset..offset + 8].try_into().unwrap_or_default())
    };
    let mut read = |address: u64, length: u32, what: &str| -> Result<String, String> {
        if length > MOST_NAME {
            return Err(format!("a run-time site's {what} is too long"));
        }

        let bytes = text(address, length)
            .ok_or_else(|| format!("a run-time site's {what} cannot be read"))?;
        String::from_utf8(bytes).map_err(|_| format!("a run-time site's {what} is not UTF-8"))
    };

    Ok(SiteRecord {
        key: abi::RUNTIME_KEY | index,
        kind,
        line: word(4),
        name: read(long(8), word(16), "name")?,
        file: read(long(24), word(20), "file")?,
        state: runtime_site_address(table, index) + 1,
    })
}

/// What a control block says, past its magic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    /// How many of the registry's slots threads took.
    pub rings: u32,

    /// Where its run-time sites are, or zero.
    pub sites: u64,
    pub dropped: u64,
    pub exiting: bool,

    /// What microscope last set its sites to, unless the program switched
    /// them off: zero is off.
    pub enable: u32,
}

/// Reads a control block's first [`abi::CONTROL_SIZE`] bytes.
pub fn control(bytes: &[u8]) -> Result<Control, &'static str> {
    if bytes.len() < abi::CONTROL_SIZE || bytes[..8] != abi::MAGIC {
        return Err("the control block is not one");
    }

    let word = |offset: usize| {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default())
    };

    let capacity = word(40);
    Ok(Control {
        rings: word(20)
            .min(capacity)
            .min(u32::try_from(abi::MOST_RINGS).unwrap_or(u32::MAX)),
        sites: u64::from_le_bytes(bytes[56..64].try_into().unwrap_or_default()),
        dropped: u64::from_le_bytes(bytes[32..40].try_into().unwrap_or_default()),
        exiting: word(44) != 0,
        enable: word(12),
    })
}

/// A ring's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingHeader {
    /// Its size, as a power of two of records.
    pub size: u8,
    pub clock: u16,

    /// The thread that took it.
    pub thread: Tid,
    pub state: u32,
    pub interrupted: u64,
    pub head: u64,
}

impl RingHeader {
    pub const fn capacity(self) -> u64 {
        1 << self.size
    }
}

/// Reads a ring's header.
pub fn ring_header(bytes: &[u8]) -> Result<RingHeader, &'static str> {
    if bytes.len() < abi::RING_HEADER {
        return Err("a ring's header is cut short");
    }

    let word = |offset: usize| {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default())
    };

    let long = |offset: usize| {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap_or_default())
    };

    if word(0) != abi::RING_MAGIC || bytes[4] != abi::RING_VERSION {
        return Err("a ring is not one");
    }

    let size = bytes[5];
    if !(abi::LEAST_RING_SIZE..=abi::MOST_RING_SIZE).contains(&u32::from(size)) {
        return Err("a ring has an unknown size");
    }

    let state = word(12);
    if state > abi::RING_CLAIMED {
        return Err("a ring has an unknown state");
    }

    // A thread sets its ring's clock as it takes the ring, so only a ring
    // whose records are read has one.
    let clock = u16::from(bytes[6]);
    if matches!(state, abi::RING_ACTIVE | abi::RING_RETIRED)
        && clock != abi::CLOCK_MONOTONIC
        && clock != abi::CLOCK_TSC
    {
        return Err("a ring has an unknown clock");
    }

    Ok(RingHeader {
        size,
        clock,
        thread: Tid::new(word(8)),
        state,
        interrupted: long(16),
        head: long(64),
    })
}

/// The positions of a ring to copy, from `tail` to the head, and how many
/// before them the ring no longer holds.
pub const fn copy_span(tail: u64, header: RingHeader) -> (Range<u64>, u64) {
    let start = if header.head.saturating_sub(tail) > header.capacity() {
        header.head - header.capacity()
    } else {
        tail
    };

    // A head that went back is not the ring's: nothing is copied.
    let end = if header.head < start {
        start
    } else {
        header.head
    };

    (start..end, start.saturating_sub(tail))
}

/// The first position still whole once the head has moved to `head`
/// while the ring was copied: the thread may be writing over every slot
/// below it.
pub const fn first_whole(head: u64, size: u8) -> u64 {
    (head + MOST_EVENT_RECORDS).saturating_sub(1 << size)
}

/// Where positions `span` lie in a ring's records, in bytes from the
/// first record: one run, or two when it wraps.
pub fn slots(span: &Range<u64>, size: u8) -> [Range<usize>; 2] {
    let capacity = 1_u64 << size;
    let count = (span.end - span.start).min(capacity);
    if count == 0 {
        return [0..0, 0..0];
    }

    let first = record_bytes(span.start % capacity);
    let wrap = record_bytes(capacity);
    let length = record_bytes(count);
    if first + length <= wrap {
        [first..first + length, 0..0]
    } else {
        [first..wrap, 0..first + length - wrap]
    }
}

/// The bytes of `records` records, as a ring holds no more than fit.
pub fn record_bytes(records: u64) -> usize {
    usize::try_from(records)
        .unwrap_or(usize::MAX)
        .saturating_mul(abi::RECORD)
}

/// One event a ring held.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    /// In the ring's clock.
    pub time: u64,
    pub key: u32,
    pub kind: u8,

    /// A plot's value.
    pub value: f64,

    /// A message's or label's text, in [`Decoder::text`].
    pub text: Range<usize>,
}

/// Decodes rings' records into events, counting what breaks the ABI.
#[derive(Debug, Default)]
pub struct Decoder {
    pub events: Vec<Decoded>,
    pub text: Vec<u8>,

    /// Records that broke the ABI.
    pub malformed: u64,

    /// Records that continued an event whose first record was overwritten.
    pub orphaned: u64,
}

impl Decoder {
    pub fn clear(&mut self) {
        self.events.clear();
        self.text.clear();
    }

    /// Decodes `records`, in position order: the first may continue an
    /// event whose start the ring no longer holds.
    pub fn decode(&mut self, records: &[u8]) {
        let (records, _) = records.as_chunks::<{ abi::RECORD }>();
        let kind = |record: &[u8; abi::RECORD]| record[12];
        let mut index = records
            .iter()
            .position(|record| kind(record) != abi::CONTINUED)
            .unwrap_or(records.len());
        self.orphaned += index as u64;

        while let Some(record) = records.get(index) {
            index += 1;
            let argument = u32::from_le_bytes(record[12..16].try_into().unwrap_or_default()) >> 8;
            let mut event = Decoded {
                time: u64::from_le_bytes(record[..8].try_into().unwrap_or_default()),
                key: u32::from_le_bytes(record[8..12].try_into().unwrap_or_default()),
                kind: kind(record),
                value: 0.0,
                text: 0..0,
            };

            // The records that continue it.
            let more = match event.kind {
                abi::ZONE_BEGIN | abi::ZONE_END | abi::FRAME | abi::LABEL_END | abi::TRIGGER => 0,
                abi::PLOT => 1,
                abi::MESSAGE | abi::LABEL_BEGIN if argument as usize <= abi::MOST_TEXT => {
                    (argument as usize).div_ceil(15)
                }
                _ => {
                    self.malformed += 1;
                    continue;
                }
            };

            let continued = records[index..]
                .iter()
                .take(more)
                .take_while(|record| kind(record) == abi::CONTINUED)
                .count();
            if continued < more {
                // Its continuation is not there; what follows is another.
                self.malformed += 1 + continued as u64;
                index += continued;
                continue;
            }

            let start = self.text.len();
            for record in &records[index..index + more] {
                self.text.extend_from_slice(&record[..12]);
                self.text.extend_from_slice(&record[13..]);
            }

            index += more;
            if event.kind == abi::PLOT {
                event.value =
                    f64::from_le_bytes(self.text[start..start + 8].try_into().unwrap_or_default());
                self.text.truncate(start);
            } else if more > 0 {
                self.text.truncate(start + argument as usize);
                if std::str::from_utf8(&self.text[start..]).is_err() {
                    self.text.truncate(start);
                    self.malformed += 1 + more as u64;
                    continue;
                }

                event.text = start..self.text.len();
            }

            self.events.push(event);
        }
    }

    /// An event's text.
    pub fn text(&self, event: &Decoded) -> &str {
        std::str::from_utf8(&self.text[event.text.clone()]).unwrap_or_default()
    }
}

/// Pairs of the time-stamp counter and `CLOCK_MONOTONIC`, which turn the
/// one into the other between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchors {
    previous: (u64, u64),
    latest: (u64, u64),
}

/// How far apart anchors are kept: far enough that their readings' noise
/// is a small part of the rate, near enough that the clock's adjustment
/// is not.
const ANCHOR_SPACING: u64 = 50_000_000;

impl Anchors {
    pub const fn new(anchor: (u64, u64)) -> Self {
        Self {
            previous: anchor,
            latest: anchor,
        }
    }

    pub const fn push(&mut self, anchor: (u64, u64)) {
        if self.latest.1.saturating_sub(self.previous.1) < ANCHOR_SPACING {
            self.latest = anchor;
        } else if anchor.1.saturating_sub(self.latest.1) >= ANCHOR_SPACING {
            self.previous = self.latest;
            self.latest = anchor;
        }
    }

    /// The monotonic time of a counter's reading.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a time near the anchors fits, and is not before the clock began"
    )]
    pub fn monotonic(&self, tsc: u64) -> u64 {
        let ticks = i128::from(self.latest.0) - i128::from(self.previous.0);
        let nanoseconds = i128::from(self.latest.1) - i128::from(self.previous.1);
        if ticks <= 0 {
            return self.latest.1;
        }

        let since = i128::from(tsc) - i128::from(self.latest.0);
        (i128::from(self.latest.1) + since * nanoseconds / ticks).max(0) as u64
    }
}

/// One event, as a capture's rows name it.
#[derive(Debug, Clone, Copy)]
pub struct Event<'a> {
    pub time: i64,
    pub site: u32,
    pub kind: u8,
    pub value: f64,
    pub text: &'a str,
}

/// A zone that began and has not ended.
#[derive(Debug, Clone, Copy)]
struct Open {
    site: u32,
    start: i64,
    value: u32,
}

/// One thread's open zones, and what is known of its records.
#[derive(Debug, Clone, Default)]
struct Thread {
    open: Vec<Open>,

    /// Whether records were lost before its next event.
    lost: bool,

    /// When its records were last lost: a zone that ends with no begin
    /// began before then.
    unknown_since: Option<i64>,
    latest: i64,
}

/// Every thread's open zones and labels: what turns begins and ends into
/// spans. It never repairs what does not nest; it flags it.
#[derive(Debug, Clone, Default)]
pub struct Stacks {
    threads: BTreeMap<u32, Thread>,
}

impl Stacks {
    /// Says that the thread's records were lost before the next it writes:
    /// overwritten, or written before microscope began reading them.
    pub fn lost(&mut self, thread: u32) {
        self.threads.entry(thread).or_default().lost = true;
    }

    /// Applies one of a thread's events, in its order.
    pub fn apply(&mut self, builder: &mut CaptureBuilder, thread: u32, event: &Event<'_>) {
        let state = self.threads.entry(thread).or_default();
        let time = event.time.max(state.latest);
        state.latest = time;
        if std::mem::take(&mut state.lost) {
            for open in state.open.drain(..) {
                builder.zone(thread, zone(open, time, ZONE_ENDED_UNSEEN));
            }

            state.unknown_since = Some(time);
        }

        let at = (thread, event.site);
        match event.kind {
            abi::ZONE_BEGIN | abi::LABEL_BEGIN => {
                let value = if event.kind == abi::LABEL_BEGIN {
                    builder.string(event.text)
                } else {
                    NONE
                };

                state.open.push(Open {
                    site: event.site,
                    start: time,
                    value,
                });
            }
            abi::ZONE_END | abi::LABEL_END => end(builder, thread, state, event.site, time),
            abi::FRAME => builder.thread_marker(at, time, MarkerKind::Frame, None),
            abi::MESSAGE => builder.thread_marker(at, time, MarkerKind::Message, Some(event.text)),
            abi::TRIGGER => builder.thread_marker(at, time, MarkerKind::Trigger, None),
            abi::PLOT => builder.plot(thread, event.site, time, event.value),
            _ => {}
        }
    }

    /// Ends a thread's zones where its records end: it exited, or its ring
    /// went to another.
    pub fn ended(&mut self, builder: &mut CaptureBuilder, thread: u32) {
        if let Some(state) = self.threads.remove(&thread) {
            for open in state.open {
                builder.zone(thread, zone(open, state.latest, ZONE_ENDED_UNSEEN));
            }
        }
    }

    /// Ends every open zone at `time` in `builder`, which the recording
    /// ends at, leaving them open here.
    pub fn close(&self, builder: &mut CaptureBuilder, time: i64) {
        for (&thread, state) in &self.threads {
            for &open in &state.open {
                builder.zone(
                    thread,
                    zone(open, time.max(state.latest), ZONE_ENDED_UNSEEN),
                );
            }
        }
    }
}

/// Ends the zone of `site` at `time`. Zones it holds that are still open
/// end with it, flagged; an end with no begin is a zone that began before
/// the thread's records were lost, or, if none were, a flagged instant.
fn end(builder: &mut CaptureBuilder, thread: u32, state: &mut Thread, site: u32, time: i64) {
    if let Some(position) = state.open.iter().rposition(|open| open.site == site) {
        for open in state.open.drain(position + 1..).rev() {
            builder.zone(
                thread,
                zone(open, time, ZONE_ENDED_UNSEEN | ZONE_MISMATCHED),
            );
        }

        if let Some(open) = state.open.pop() {
            builder.zone(thread, zone(open, time, 0));
        }

        return;
    }

    let began = Open {
        site,
        start: time,
        value: NONE,
    };

    match state.unknown_since {
        Some(since) => {
            for open in state.open.drain(..).rev() {
                builder.zone(
                    thread,
                    zone(open, time, ZONE_ENDED_UNSEEN | ZONE_MISMATCHED),
                );
            }

            let began = Open {
                start: since,
                ..began
            };

            builder.zone(thread, zone(began, time, ZONE_BEGAN_UNSEEN));
        }
        None => builder.zone(
            thread,
            zone(began, time, ZONE_BEGAN_UNSEEN | ZONE_MISMATCHED),
        ),
    }
}

fn zone(open: Open, end: i64, flags: u32) -> Zone {
    Zone {
        start: open.start.into(),
        end: end.into(),
        site: open.site.into(),
        flags: flags.into(),
        value: open.value.into(),
        reserved: 0.into(),
    }
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::float_cmp,
    reason = "a test's counts are small, and a value read back is the bits written"
)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::capture::records::ZONE_FLAGS;

    /// One event a reference writer of the ABI writes, by its index.
    #[derive(Debug, Clone)]
    enum Written {
        Zone(bool),
        Plot(f64),
        Message(Vec<u8>),
    }

    /// The records of event `index`, as the ABI lays them out.
    fn records(index: usize, event: &Written) -> Vec<[u8; abi::RECORD]> {
        let (kind, argument, more): (u8, u32, Vec<u8>) = match event {
            Written::Zone(begin) => (
                if *begin {
                    abi::ZONE_BEGIN
                } else {
                    abi::ZONE_END
                },
                0,
                Vec::new(),
            ),
            Written::Plot(value) => (abi::PLOT, 0, value.to_le_bytes().to_vec()),
            Written::Message(text) => (abi::MESSAGE, text.len() as u32, text.clone()),
        };

        let mut first = [0; abi::RECORD];
        first[..8].copy_from_slice(&(index as u64).to_le_bytes());
        first[8..12].copy_from_slice(&7_u32.to_le_bytes());
        first[12..].copy_from_slice(&(u32::from(kind) | argument << 8).to_le_bytes());
        let mut all = vec![first];
        for chunk in more.chunks(15) {
            let mut record = [0; abi::RECORD];
            let (low, high) = chunk.split_at(chunk.len().min(12));
            record[..low.len()].copy_from_slice(low);
            record[12] = abi::CONTINUED;
            record[13..13 + high.len()].copy_from_slice(high);
            all.push(record);
        }

        all
    }

    fn written() -> impl Strategy<Value = Written> {
        prop_oneof![
            any::<bool>().prop_map(Written::Zone),
            (-1e9..1e9_f64).prop_map(Written::Plot),
            proptest::collection::vec(b'a'..=b'z', 0..=abi::MOST_TEXT).prop_map(Written::Message),
        ]
    }

    /// A writer that publishes each event's records by moving the head
    /// past them, one record a step, and so may be caught between.
    struct Writer {
        slots: Vec<[u8; abi::RECORD]>,
        pending: std::collections::VecDeque<[u8; abi::RECORD]>,
        events: std::vec::IntoIter<Vec<[u8; abi::RECORD]>>,
        written: u64,
        head: u64,
    }

    impl Writer {
        fn step(&mut self) {
            if self.pending.is_empty() {
                match self.events.next() {
                    Some(records) => self.pending.extend(records),
                    None => return,
                }
            }

            if let Some(record) = self.pending.pop_front() {
                let capacity = self.slots.len() as u64;
                self.slots[(self.written % capacity) as usize] = record;
                self.written += 1;
                if self.pending.is_empty() {
                    self.head = self.written;
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// A ring read while its thread writes it, however the two
        /// interleave, yields only whole events, each as it was written,
        /// and every position is read or counted lost.
        #[test]
        fn a_ring_read_as_it_is_written_yields_whole_events_and_counts_the_rest(
            events in proptest::collection::vec(written(), 0..120),
            schedule in proptest::collection::vec((0..80_usize, 0..4_usize), 1..40),
        ) {
            let size = abi::LEAST_RING_SIZE as u8;
            let encoded: Vec<_> = events.iter().enumerate().map(|(index, event)| records(index, event)).collect();
            let starts: Vec<u64> = encoded.iter().scan(0, |at, records| {
                let start = *at;
                *at += records.len() as u64;
                Some(start)
            }).collect();
            let total: u64 = encoded.iter().map(|records| records.len() as u64).sum();
            let mut writer = Writer {
                slots: vec![[0; abi::RECORD]; 1 << size],
                pending: std::collections::VecDeque::new(),
                events: encoded.clone().into_iter(),
                written: 0,
                head: 0,
            };

            let (mut tail, mut accounted, mut decoder) = (0, 0, Decoder::default());
            let rounds = schedule.iter().copied().chain(std::iter::once((usize::MAX, 0)));

            for (before, during) in rounds {
                for _ in 0..before.min(1 << 20) {
                    writer.step();
                }

                let header = RingHeader { size, clock: 0, thread: Tid::new(0), state: abi::RING_ACTIVE, interrupted: 0, head: writer.head };
                let (span, mut lost) = copy_span(tail, header);
                let mut copied = Vec::new();

                for position in span.clone() {
                    copied.extend_from_slice(&writer.slots[(position % (1 << size)) as usize]);
                    for _ in 0..during {
                        writer.step();
                    }
                }

                let first = first_whole(writer.head, size).max(span.start).min(span.end);
                lost += first - span.start;
                decoder.clear();
                decoder.decode(&copied[(first - span.start) as usize * abi::RECORD..]);
                prop_assert_eq!(decoder.malformed, 0);
                let expected: Vec<usize> = (0..events.len())
                    .filter(|&index| (first..span.end).contains(&starts[index]))
                    .collect();
                let found: Vec<usize> = decoder.events.iter().map(|event| event.time as usize).collect();
                prop_assert_eq!(&found, &expected);

                for (event, &index) in decoder.events.iter().zip(&expected) {
                    match &events[index] {
                        Written::Plot(value) => prop_assert_eq!(event.value, *value),
                        Written::Message(text) => prop_assert_eq!(decoder.text(event).as_bytes(), text.as_slice()),
                        Written::Zone(_) => {}
                    }
                }

                accounted += lost + expected.iter().map(|&index| encoded[index].len() as u64).sum::<u64>();
                tail = span.end;
            }

            prop_assert_eq!(accounted + decoder.orphaned, total);
        }
    }

    /// One thread's trace: a tree of zones, written as begins and ends.
    fn trace() -> impl Strategy<Value = Vec<(bool, u32)>> {
        let leaf = (0..4_u32).prop_map(|site| vec![(true, site), (false, site)]);
        leaf.prop_recursive(4, 64, 4, |inner| {
            ((0..4_u32), proptest::collection::vec(inner, 0..4)).prop_map(|(site, children)| {
                let mut events = vec![(true, site)];
                events.extend(children.into_iter().flatten());
                events.push((false, site));
                events
            })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Whatever a thread's records lose, and however its ends fail to
        /// nest, its zones are a valid capture's: each event read is in
        /// exactly one zone, and a zone is whole exactly when its begin and
        /// end were read with nothing lost between them.
        #[test]
        fn zones_are_whole_exactly_where_nothing_between_was_lost(
            roots in proptest::collection::vec(trace(), 1..4),
            lost in proptest::collection::vec(any::<bool>(), 0..200),
            swap in proptest::option::of(0..200_usize),
        ) {
            let mut events: Vec<(bool, u32)> = roots.into_iter().flatten().collect();
            let swapped = swap.filter(|&at| at + 1 < events.len() && !events[at].0 && !events[at + 1].0);
            if let Some(at) = swapped {
                events.swap(at, at + 1);
            }

            let mut builder = CaptureBuilder::new();
            let thread = builder.thread(crate::model::Pid::new(1), Tid::new(1), 0);
            let sites: Vec<u32> = (0..4).map(|site| builder.site(&site.to_string(), "", 0, NONE, abi::SITE_ZONE)).collect();
            let mut stacks = Stacks::default();
            let mut read = 0;

            // The true zones whose begin and end were both read with no loss
            // between them, by site, start, and end.
            let mut whole = std::collections::BTreeSet::new();
            let mut open: Vec<(u32, i64, bool)> = Vec::new();

            for (index, &(begin, site)) in events.iter().enumerate() {
                let time = index as i64 * 10;
                if lost.get(index).copied().unwrap_or(false) {
                    stacks.lost(thread);
                    for zone in &mut open {
                        zone.2 = false;
                    }

                    if begin {
                        open.push((site, time, false));
                    } else {
                        open.pop();
                    }

                    continue;
                }

                read += 1;
                if begin {
                    open.push((site, time, true));
                } else if let Some((_, start, seen)) = open.pop()
                    && seen
                {
                    whole.insert((sites[site as usize], start, time));
                }

                let kind = if begin { abi::ZONE_BEGIN } else { abi::ZONE_END };
                stacks.apply(&mut builder, thread, &Event { time, site: sites[site as usize], kind, value: 0.0, text: "" });
            }

            stacks.close(&mut builder, events.len() as i64 * 10);
            let capture = builder.finish();
            prop_assert_eq!(capture.validate(), Ok(()));
            let zones = &capture.zones;
            let halves: u64 = zones.iter().map(|zone| {
                let flags = zone.flags.get();
                prop_assert_eq!(flags & !ZONE_FLAGS, 0);
                Ok(u64::from(flags & ZONE_BEGAN_UNSEEN == 0) + u64::from(flags & ZONE_ENDED_UNSEEN == 0))
            }).sum::<Result<u64, TestCaseError>>()?;
            prop_assert_eq!(halves, read);
            if swapped.is_none() {
                let flagless: std::collections::BTreeSet<_> = zones
                    .iter()
                    .filter(|zone| zone.flags.get() == 0)
                    .map(|zone| (zone.site.get(), zone.start.get(), zone.end.get()))
                    .collect();
                prop_assert_eq!(flagless, whole);
            }
        }
    }

    /// Times between two anchors are the line between them.
    #[test]
    fn counter_readings_become_the_clock_between_anchors() {
        let mut anchors = Anchors::new((1_000, 5_000_000_000));
        anchors.push((1_000 + 3 * ANCHOR_SPACING, 5_000_000_000 + ANCHOR_SPACING));
        assert_eq!(
            anchors.monotonic(1_000 + 3 * ANCHOR_SPACING / 2),
            5_000_000_000 + ANCHOR_SPACING / 2
        );
        assert_eq!(
            anchors.monotonic(1_000 + 6 * ANCHOR_SPACING),
            5_000_000_000 + 2 * ANCHOR_SPACING
        );
    }

    /// A ring whose records are on a clock the ABI does not name breaks it,
    /// as a ring of an unknown size or state does: its times are not read
    /// as some other clock's. A ring no thread has taken has no clock yet.
    #[test]
    fn a_ring_on_an_unknown_clock_breaks_the_abi() {
        let header = |state: u32, clock: u8| {
            let mut bytes = [0; abi::RING_HEADER];
            bytes[..4].copy_from_slice(&abi::RING_MAGIC.to_le_bytes());
            bytes[4] = abi::RING_VERSION;
            bytes[5] = abi::LEAST_RING_SIZE as u8;
            bytes[6] = clock;
            bytes[12..16].copy_from_slice(&state.to_le_bytes());
            ring_header(&bytes).map(|header| header.clock)
        };

        for state in [abi::RING_ACTIVE, abi::RING_RETIRED] {
            for clock in [abi::CLOCK_MONOTONIC, abi::CLOCK_TSC] {
                assert_eq!(header(state, clock as u8), Ok(clock));
            }

            for clock in [0, 3, u8::MAX] {
                assert_eq!(header(state, clock), Err("a ring has an unknown clock"));
            }
        }

        for state in [abi::RING_FREE, abi::RING_CLAIMED] {
            assert_eq!(header(state, 0), Ok(0));
        }
    }

    /// A run-time site reads as the ABI lays it out, by its key, its state
    /// byte inside its record; what breaks the ABI is refused, never read
    /// past or guessed at.
    #[test]
    fn run_time_sites_read_as_the_abi_lays_them_out_or_are_refused() {
        let mut header = [0; abi::SITES_HEADER];
        header[..4].copy_from_slice(&abi::SITES_MAGIC.to_le_bytes());
        header[4] = abi::SITES_VERSION;
        header[8..12].copy_from_slice(&4_u32.to_le_bytes());
        header[12..16].copy_from_slice(&9_u32.to_le_bytes());
        assert_eq!(runtime_sites(&header), Ok(4), "never more than it holds");
        header[4] = abi::SITES_VERSION + 1;
        assert!(runtime_sites(&header).is_err());
        assert!(runtime_sites(&header[..8]).is_err());

        let mut record = [0; abi::RUNTIME_SITE_SIZE];
        record[0] = abi::SITE_ZONE;
        record[4..8].copy_from_slice(&7_u32.to_le_bytes());
        record[8..16].copy_from_slice(&0x5000_u64.to_le_bytes());
        record[16..20].copy_from_slice(&4_u32.to_le_bytes());
        record[20..24].copy_from_slice(&6_u32.to_le_bytes());
        record[24..32].copy_from_slice(&0x6000_u64.to_le_bytes());
        let texts = |address: u64, length: u32| match (address, length) {
            (0x5000, 4) => Some(b"load".to_vec()),
            (0x6000, 6) => Some(b"app.py".to_vec()),
            (0x7000, _) => Some(vec![0xff]),
            _ => None,
        };
        let site = runtime_site(0x1000, 2, &record, texts).unwrap();
        assert_eq!(
            site,
            SiteRecord {
                key: abi::RUNTIME_KEY | 2,
                kind: abi::SITE_ZONE,
                line: 7,
                name: "load".into(),
                file: "app.py".into(),
                state: 0x1000 + 16 + 2 * 48 + 1,
            }
        );

        let broken = |change: &dyn Fn(&mut [u8; abi::RUNTIME_SITE_SIZE])| {
            let mut record = record;
            change(&mut record);
            runtime_site(0x1000, 2, &record, texts).unwrap_err()
        };
        assert!(broken(&|record| record[0] = abi::SITE_NONE).contains("kind"));
        assert!(
            broken(&|record| record[16..20].copy_from_slice(&5000_u32.to_le_bytes()))
                .contains("too long")
        );
        assert!(
            broken(&|record| record[16..20].copy_from_slice(&5_u32.to_le_bytes()))
                .contains("cannot be read")
        );
        assert!(
            broken(&|record| record[8..16].copy_from_slice(&0x7000_u64.to_le_bytes()))
                .contains("UTF-8")
        );
        assert!(runtime_site(0x1000, 2, &record[..40], texts).is_err());
        let last = u32::try_from(abi::MOST_RUNTIME_SITES).unwrap();
        assert!(runtime_site(0x1000, last, &record, texts).is_err());
    }

    /// A control block of ABI 1, which programs built before run-time sites
    /// carry, reads as one of ABI 2 does; no other version does.
    #[test]
    fn control_blocks_of_abi_1_and_2_are_read() {
        let block = |version: u16| {
            let mut block = [0; abi::CONTROL_SIZE];
            block[..8].copy_from_slice(&abi::MAGIC);
            block[8..10].copy_from_slice(&version.to_le_bytes());
            block
        };
        assert_eq!(version(&block(1)), Ok(1));
        assert_eq!(version(&block(2)), Ok(2));
        assert!(version(&block(0)).unwrap_err().contains("ABI version 0"));
        assert!(version(&block(3)).is_err());
        assert!(version(&block(2)[..32]).is_err());
    }

    /// From ABI 2 a static site's state byte lies within 2^30 of its control
    /// block, so its key never meets a run-time site's; ABI 1 keeps the
    /// whole `i32`.
    #[test]
    fn static_keys_leave_run_time_sites_their_keys_from_abi_2() {
        let far = abi::MOST_STATIC_OFFSET;
        assert_eq!(static_key(2, -8), Some((-8_i32).cast_unsigned()));
        assert_eq!(
            static_key(2, far - 1),
            Some(u32::try_from(far - 1).unwrap())
        );
        assert_eq!(static_key(2, far), None);
        assert_eq!(static_key(2, -far - 1), None);
        assert_eq!(static_key(1, far), Some(u32::try_from(far).unwrap()));
        assert_eq!(static_key(1, 1 << 31), None);
        for offset in [-far, far - 1] {
            assert_ne!(
                static_key(2, offset).unwrap() & 0xc000_0000,
                abi::RUNTIME_KEY
            );
        }
    }
}
