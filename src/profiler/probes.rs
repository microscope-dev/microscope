use std::collections::BTreeMap;
use std::ops::Range;

use microscope_probes::abi;
use object::{Object as _, ObjectSection as _};

use crate::capture::CaptureBuilder;
use crate::capture::records::{
    MarkerKind, NONE, ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, ZONE_LOST_WITHIN, ZONE_MISMATCHED, Zone,
};

use crate::model::Tid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub control: u64,

    pub abi: u16,
    pub sites: Vec<SiteRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteRecord {
    pub key: u32,
    pub kind: u8,
    pub line: u32,
    pub name: String,
    pub file: String,

    pub state: u64,
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk)
        .copied()
        .unwrap_or([0; N])
}

const MOST_NAME: u32 = abi::MOST_SITE_TEXT;

pub const MOST_EVENT_RECORDS: u64 = 1 + abi::MOST_TEXT.div_ceil(15) as u64;

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
        .section_by_name(abi::SITE_SECTION)
        .ok_or("it has a control block but no sites")?;
    let data = sites
        .data()
        .map_err(|error| format!("its sites cannot be read: {error}"))?;
    if !data.len().is_multiple_of(abi::SITE_SIZE) {
        return Err("its site table is cut short".into());
    }

    let bytes = |address: u64, length: u32| {
        file.sections().find_map(|section| {
            section
                .data_range(address, u64::from(length))
                .ok()
                .flatten()
        })
    };

    let text = |at: u64, record: &[u8], what: &str| -> Result<String, String> {
        let offset = i32::from_le_bytes(field(record, 0));
        let length = u32::from_le_bytes(field(record, 4));
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
        let field = |offset: usize| u32::from_le_bytes(field(record, offset));

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

fn version(block: &[u8]) -> Result<u16, String> {
    if block.len() < abi::CONTROL_SIZE || block.first_chunk() != Some(&abi::MAGIC) {
        return Err("its control block is not one".into());
    }

    let version = u16::from_le_bytes(field(block, 8));
    if !(1..=abi::ABI).contains(&version) {
        return Err(format!(
            "its probes are ABI version {version}, which this microscope does not read"
        ));
    }

    Ok(version)
}

fn static_key(version: u16, offset: i64) -> Option<u32> {
    let near = version < 2 || (-abi::MOST_STATIC_OFFSET..abi::MOST_STATIC_OFFSET).contains(&offset);
    i32::try_from(offset)
        .ok()
        .filter(|_| near)
        .map(i32::cast_unsigned)
}

pub fn runtime_sites(bytes: &[u8]) -> Result<u32, &'static str> {
    if bytes.len() < abi::SITES_HEADER
        || bytes.first_chunk() != Some(&abi::SITES_MAGIC.to_le_bytes())
        || bytes.get(4) != Some(&abi::SITES_VERSION)
    {
        return Err("the run-time sites are not a table this microscope reads");
    }

    let word = |offset: usize| u32::from_le_bytes(field(bytes, offset));
    let most = u32::try_from(abi::MOST_RUNTIME_SITES).unwrap_or(u32::MAX);
    Ok(word(12).min(word(8)).min(most))
}

pub const fn runtime_site_address(table: u64, index: u32) -> u64 {
    table + (abi::SITES_HEADER + index as usize * abi::RUNTIME_SITE_SIZE) as u64
}

pub fn runtime_site(
    table: u64,
    index: u32,
    record: &[u8],
    mut text: impl FnMut(u64, u32) -> Option<Vec<u8>>,
) -> Result<SiteRecord, String> {
    if record.len() < abi::RUNTIME_SITE_SIZE || u64::from(index) >= abi::MOST_RUNTIME_SITES as u64 {
        return Err("a run-time site is cut short".into());
    }

    let [kind] = field(record, 0);
    if !(abi::SITE_ZONE..=abi::SITE_TRIGGER).contains(&kind) {
        return Err(format!("a run-time site has unknown kind {kind}"));
    }

    let word = |offset: usize| u32::from_le_bytes(field(record, offset));
    let long = |offset: usize| u64::from_le_bytes(field(record, offset));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    pub rings: u32,

    pub sites: u64,
    pub dropped: u64,
    pub exiting: bool,

    pub enable: u32,

    pub armed: bool,
}

pub fn control(bytes: &[u8]) -> Result<Control, &'static str> {
    if bytes.len() < abi::CONTROL_SIZE || bytes.first_chunk() != Some(&abi::MAGIC) {
        return Err("the control block is not one");
    }

    let word = |offset: usize| u32::from_le_bytes(field(bytes, offset));

    let capacity = word(40);
    Ok(Control {
        rings: word(20)
            .min(capacity)
            .min(u32::try_from(abi::MOST_RINGS).unwrap_or(u32::MAX)),
        sites: u64::from_le_bytes(field(bytes, 56)),
        dropped: u64::from_le_bytes(field(bytes, 32)),
        exiting: word(44) != 0,
        enable: word(12),
        armed: word(12) != 0 && word(52) == 0,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingHeader {
    pub size: u8,
    pub clock: u16,

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

pub fn ring_header(bytes: &[u8]) -> Result<RingHeader, &'static str> {
    if bytes.len() < abi::RING_HEADER {
        return Err("a ring's header is cut short");
    }

    let word = |offset: usize| u32::from_le_bytes(field(bytes, offset));

    let long = |offset: usize| u64::from_le_bytes(field(bytes, offset));

    if word(0) != abi::RING_MAGIC || bytes.get(4) != Some(&abi::RING_VERSION) {
        return Err("a ring is not one");
    }

    let [size] = field(bytes, 5);
    if !(abi::LEAST_RING_SIZE..=abi::MOST_RING_SIZE).contains(&u32::from(size)) {
        return Err("a ring has an unknown size");
    }

    let state = word(12);
    if state > abi::RING_CLAIMED {
        return Err("a ring has an unknown state");
    }

    let [clock] = field(bytes, 6);
    let clock = u16::from(clock);
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

pub const fn copy_span(tail: u64, header: RingHeader) -> (Range<u64>, u64) {
    let start = if header.head.saturating_sub(tail) > header.capacity() {
        header.head - header.capacity()
    } else {
        tail
    };

    let end = if header.head < start {
        start
    } else {
        header.head
    };

    (start..end, start.saturating_sub(tail))
}

pub const fn first_whole(head: u64, size: u8) -> u64 {
    (head + MOST_EVENT_RECORDS).saturating_sub(1 << size)
}

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

pub fn record_bytes(records: u64) -> usize {
    usize::try_from(records)
        .unwrap_or(usize::MAX)
        .saturating_mul(abi::RECORD)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub time: u64,
    pub key: u32,
    pub kind: u8,

    pub value: f64,

    pub text: Range<usize>,
}

#[derive(Debug, Default)]
pub struct Decoder {
    pub events: Vec<Decoded>,
    pub text: Vec<u8>,

    pub malformed: u64,

    pub orphaned: u64,
}

impl Decoder {
    pub fn clear(&mut self) {
        self.events.clear();
        self.text.clear();
    }

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

            let continued = records
                .get(index..)
                .unwrap_or_default()
                .iter()
                .take(more)
                .take_while(|record| kind(record) == abi::CONTINUED)
                .count();
            if continued < more {
                self.malformed += 1 + continued as u64;
                index += continued;
                continue;
            }

            let start = self.text.len();
            for record in records.get(index..index + more).unwrap_or_default() {
                self.text.extend_from_slice(&record[..12]);
                self.text.extend_from_slice(&record[13..]);
            }

            index += more;
            if event.kind == abi::PLOT {
                event.value = f64::from_le_bytes(field(&self.text, start));
                self.text.truncate(start);
            } else if more > 0 {
                self.text.truncate(start + argument as usize);
                if self
                    .text
                    .get(start..)
                    .is_none_or(|text| std::str::from_utf8(text).is_err())
                {
                    self.text.truncate(start);
                    self.malformed += 1 + more as u64;
                    continue;
                }

                event.text = start..self.text.len();
            }

            self.events.push(event);
        }
    }

    pub fn text(&self, event: &Decoded) -> &str {
        self.text
            .get(event.text.clone())
            .and_then(|text| std::str::from_utf8(text).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchors {
    previous: (u64, u64),
    latest: (u64, u64),
}

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

#[derive(Debug, Clone, Copy)]
pub struct Event<'a> {
    pub time: i64,
    pub site: u32,
    pub kind: u8,
    pub value: f64,
    pub text: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct Open {
    site: u32,
    start: i64,
    value: u32,

    lost_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
struct Thread {
    open: Vec<Open>,

    lost: bool,

    unknown_since: Option<i64>,
    latest: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Stacks {
    threads: BTreeMap<u32, Thread>,
}

impl Stacks {
    pub fn lost(&mut self, thread: u32) {
        self.threads.entry(thread).or_default().lost = true;
    }

    pub fn apply(&mut self, builder: &mut CaptureBuilder, thread: u32, event: &Event<'_>) {
        let state = self.threads.entry(thread).or_default();
        let time = event.time.max(state.latest);
        state.latest = time;
        if std::mem::take(&mut state.lost) {
            for open in &mut state.open {
                open.lost_at.get_or_insert(time);
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
                    lost_at: None,
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

    pub fn ended(&mut self, builder: &mut CaptureBuilder, thread: u32) {
        if let Some(state) = self.threads.remove(&thread) {
            for open in state.open {
                builder.zone(thread, unended(open, state.latest, 0));
            }
        }
    }

    pub fn close(&self, builder: &mut CaptureBuilder, time: i64) {
        for (&thread, state) in &self.threads {
            for &open in &state.open {
                builder.zone(thread, unended(open, time.max(state.latest), 0));
            }
        }
    }
}

fn end(builder: &mut CaptureBuilder, thread: u32, state: &mut Thread, site: u32, time: i64) {
    if let Some(position) = state.open.iter().rposition(|open| open.site == site) {
        for open in state.open.drain(position + 1..).rev() {
            let flags = if open.lost_at.is_some() {
                0
            } else {
                ZONE_MISMATCHED
            };
            builder.zone(thread, unended(open, time, flags));
        }

        if let Some(open) = state.open.pop() {
            let flags = if open.lost_at.is_some() {
                ZONE_LOST_WITHIN
            } else {
                0
            };
            builder.zone(thread, zone(open, time, flags));
        }

        return;
    }

    let began = Open {
        site,
        start: time,
        value: NONE,
        lost_at: None,
    };

    match state.unknown_since {
        Some(since) => {
            while let Some(open) = state.open.pop_if(|open| open.lost_at.is_none()) {
                builder.zone(thread, unended(open, time, ZONE_MISMATCHED));
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

fn unended(open: Open, end: i64, flags: u32) -> Zone {
    let end = open.lost_at.map_or(end, |lost| lost.min(end));
    zone(open, end, flags | ZONE_ENDED_UNSEEN)
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
    clippy::cast_sign_loss,
    reason = "a test's counts and times are small and not negative"
)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::capture::records::ZONE_FLAGS;

    #[derive(Debug, Clone)]
    struct Written {
        kind: u8,
        key: u32,
        value: f64,
        text: Vec<u8>,
    }

    fn records(index: usize, event: &Written) -> Vec<[u8; abi::RECORD]> {
        let (argument, more) = match event.kind {
            abi::PLOT => (0, event.value.to_le_bytes().to_vec()),
            abi::MESSAGE | abi::LABEL_BEGIN => (event.text.len() as u32, event.text.clone()),
            _ => (0, Vec::new()),
        };

        let mut first = [0; abi::RECORD];
        first[..8].copy_from_slice(&(index as u64).to_le_bytes());
        first[8..12].copy_from_slice(&event.key.to_le_bytes());
        first[12..].copy_from_slice(&(u32::from(event.kind) | argument << 8).to_le_bytes());
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

    fn text() -> impl Strategy<Value = Vec<u8>> {
        let text = prop_oneof![
            3 => "[a-z]{0,20}",
            1 => "[a-zé€😀]{0,90}",
        ];
        text.prop_map(|text: String| {
            let mut end = text.len().min(abi::MOST_TEXT);
            while !text.is_char_boundary(end) {
                end -= 1;
            }

            text.as_bytes()[..end].to_vec()
        })
    }

    fn written() -> impl Strategy<Value = Written> {
        let key = crate::test_strategies::u32_edges();
        let bare = prop::sample::select(vec![
            abi::ZONE_BEGIN,
            abi::ZONE_END,
            abi::LABEL_END,
            abi::FRAME,
            abi::TRIGGER,
        ]);
        prop_oneof![
            (bare, key.clone()).prop_map(|(kind, key)| Written {
                kind,
                key,
                value: 0.0,
                text: Vec::new()
            }),
            ((-1e9..1e9_f64), key.clone()).prop_map(|(value, key)| Written {
                kind: abi::PLOT,
                key,
                value,
                text: Vec::new()
            }),
            (
                prop::sample::select(vec![abi::MESSAGE, abi::LABEL_BEGIN]),
                key,
                text()
            )
                .prop_map(|(kind, key, text)| Written {
                    kind,
                    key,
                    value: 0.0,
                    text
                }),
            Just(Written {
                kind: abi::MESSAGE,
                key: 1,
                value: 0.0,
                text: vec![b'x'; abi::MOST_TEXT]
            }),
        ]
    }

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
        /// interleave, yields every event of the span it reads that the
        /// thread cannot have reached by the time the head is read again,
        /// and only events whose slots it did not write over while they
        /// were copied, each as it was written; every position it does not
        /// yield is counted lost.
        #[test]
        fn a_ring_read_as_it_is_written_yields_whole_events_and_counts_the_rest(
            events in proptest::collection::vec(written(), 0..120),
            schedule in proptest::collection::vec((0..80_usize, 0..4_usize), 1..40),
        ) {
            let size = abi::LEAST_RING_SIZE as u8;
            let capacity = 1_u64 << size;
            let encoded: Vec<_> = events.iter().enumerate().map(|(index, event)| records(index, event)).collect();
            let starts: Vec<u64> = encoded.iter().scan(0, |at, records| {
                let start = *at;
                *at += records.len() as u64;
                Some(start)
            }).collect();
            let total = encoded.iter().map(Vec::len).sum();
            let mut writer = Writer {
                slots: vec![[0; abi::RECORD]; 1 << size],
                pending: std::collections::VecDeque::new(),
                events: encoded.clone().into_iter(),
                written: 0,
                head: 0,
            };

            let (mut tail, mut decoder) = (0, Decoder::default());
            let rounds = schedule.iter().copied().chain(std::iter::once((usize::MAX, 0)));

            for (before, during) in rounds {
                for _ in 0..before.min(total) {
                    writer.step();
                }

                let header = RingHeader { size, clock: 0, thread: Tid::new(0), state: abi::RING_ACTIVE, interrupted: 0, head: writer.head };
                let (span, mut lost) = copy_span(tail, header);
                let mut copied = Vec::new();
                let mut intact = Vec::new();

                for run in slots(&span, size) {
                    for slot in run.step_by(abi::RECORD) {
                        let position = span.start + intact.len() as u64;
                        intact.push(writer.written <= position + capacity);
                        copied.extend_from_slice(&writer.slots[slot / abi::RECORD]);
                        for _ in 0..during {
                            writer.step();
                        }
                    }
                }

                prop_assert_eq!(intact.len() as u64, span.end - span.start);
                let first = first_whole(writer.head, size).max(span.start).min(span.end);
                lost += first - span.start;
                let orphaned = decoder.orphaned;
                decoder.clear();
                decoder.decode(&copied[(first - span.start) as usize * abi::RECORD..]);
                prop_assert_eq!(decoder.malformed, 0);

                let whole = |index: usize| {
                    let mut positions = starts[index]..starts[index] + encoded[index].len() as u64;
                    span.contains(&positions.start)
                        && positions.end <= span.end
                        && positions.all(|position| intact[(position - span.start) as usize])
                };

                for event in &decoder.events {
                    let index = event.time as usize;
                    let written = &events[index];
                    prop_assert!(whole(index), "event {} was written over while it was copied", index);
                    prop_assert_eq!((event.kind, event.key), (written.kind, written.key), "event {}", index);
                    match written.kind {
                        abi::PLOT => prop_assert_eq!(event.value.to_bits(), written.value.to_bits()),
                        abi::MESSAGE | abi::LABEL_BEGIN => prop_assert_eq!(decoder.text(event).as_bytes(), written.text.as_slice()),
                        _ => {}
                    }
                }

                let reach = (writer.head + MOST_EVENT_RECORDS).saturating_sub(capacity);
                let yielded: Vec<usize> = decoder.events.iter().map(|event| event.time as usize).collect();
                let expected: Vec<usize> = (0..events.len())
                    .filter(|&index| starts[index] >= reach && whole(index))
                    .collect();
                prop_assert!(expected.iter().all(|index| yielded.contains(index)), "{:?} of {:?}", yielded, expected);
                prop_assert!(yielded.windows(2).all(|pair| pair[0] < pair[1]));

                let read: u64 = yielded.iter().map(|&index| encoded[index].len() as u64).sum();
                lost += decoder.orphaned - orphaned;
                prop_assert_eq!(read + lost, span.end - tail);
                tail = span.end;
            }

            prop_assert_eq!(tail, starts.last().map_or(0, |start| start + encoded[encoded.len() - 1].len() as u64));
        }
    }

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
        /// end were read with nothing lost between them. Where two ends
        /// came in the wrong order, a zone is whole only where a begin and
        /// an end of its site were read with nothing lost between them.
        #[test]
        fn zones_are_whole_exactly_where_nothing_between_was_lost(
            roots in proptest::collection::vec(trace(), 1..4),
            lost in prop_oneof![
                1 => Just(Vec::new()),
                3 => proptest::collection::vec(proptest::bool::weighted(0.1), 0..200),
            ],
            swap in proptest::option::weighted(0.3, any::<proptest::sample::Index>()),
        ) {
            let mut events: Vec<(bool, u32)> = roots.into_iter().flatten().collect();
            let ends: Vec<usize> = (0..events.len().saturating_sub(1))
                .filter(|&at| !events[at].0 && !events[at + 1].0)
                .collect();
            let swapped = swap.filter(|_| !ends.is_empty()).map(|swap| *swap.get(&ends));
            if let Some(at) = swapped {
                events.swap(at, at + 1);
            }

            let lost_at = |index: usize| lost.get(index).copied().unwrap_or(false);

            let mut builder = CaptureBuilder::new();
            let thread = builder.thread(crate::model::Pid::new(1), Tid::new(1), 0);
            let sites: Vec<u32> = (0..4).map(|site| builder.site(&site.to_string(), "", 0, NONE, abi::SITE_ZONE)).collect();
            let mut stacks = Stacks::default();
            let mut read = 0;

            let mut whole = std::collections::BTreeSet::new();
            let mut open: Vec<(u32, i64, bool)> = Vec::new();

            for (index, &(begin, site)) in events.iter().enumerate() {
                let time = index as i64 * 10;
                if lost_at(index) {
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
            let flagless: std::collections::BTreeSet<_> = zones
                .iter()
                .filter(|zone| zone.flags.get() == 0)
                .map(|zone| (zone.site.get(), zone.start.get(), zone.end.get()))
                .collect();
            if swapped.is_none() {
                prop_assert_eq!(flagless, whole);
            } else {
                for &(site, start, end) in &flagless {
                    let (begin, end) = ((start / 10) as usize, (end / 10) as usize);
                    let site = sites.iter().position(|known| *known == site).unwrap_or_default() as u32;
                    prop_assert!(events[begin] == (true, site) && events[end] == (false, site), "{:?}", (site, begin, end));
                    prop_assert!((begin..=end).all(|index| !lost_at(index)), "{:?}", (site, begin, end));
                }
            }
        }
    }

    #[test]
    fn a_zone_open_across_a_loss_is_one_zone() {
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(crate::model::Pid::new(1), Tid::new(1), 0);
        let outer = builder.site("outer", "", 0, NONE, abi::SITE_ZONE);
        let inner = builder.site("inner", "", 0, NONE, abi::SITE_ZONE);
        let mut stacks = Stacks::default();
        let apply = |stacks: &mut Stacks, builder: &mut CaptureBuilder, time, site, kind| {
            stacks.apply(
                builder,
                thread,
                &Event {
                    time,
                    site,
                    kind,
                    value: 0.0,
                    text: "",
                },
            );
        };

        apply(&mut stacks, &mut builder, 10, outer, abi::ZONE_BEGIN);
        apply(&mut stacks, &mut builder, 20, inner, abi::ZONE_BEGIN);
        stacks.lost(thread);
        apply(&mut stacks, &mut builder, 50, inner, abi::ZONE_BEGIN);
        apply(&mut stacks, &mut builder, 60, inner, abi::ZONE_END);
        apply(&mut stacks, &mut builder, 90, outer, abi::ZONE_END);

        let capture = builder.finish();
        assert_eq!(capture.validate(), Ok(()));
        let mut zones: Vec<_> = capture
            .zones
            .iter()
            .map(|zone| {
                (
                    zone.site.get(),
                    zone.start.get(),
                    zone.end.get(),
                    zone.flags.get(),
                )
            })
            .collect();
        zones.sort_unstable();
        let mut expected = vec![
            (outer, 10, 90, ZONE_LOST_WITHIN),
            (inner, 20, 50, ZONE_ENDED_UNSEEN),
            (inner, 50, 60, 0),
        ];
        expected.sort_unstable();
        assert_eq!(zones, expected);
    }

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

            for clock in [0, 3] {
                assert_eq!(header(state, clock), Err("a ring has an unknown clock"));
            }
        }

        for state in [abi::RING_FREE, abi::RING_CLAIMED] {
            assert_eq!(header(state, 0), Ok(0));
        }
    }

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
