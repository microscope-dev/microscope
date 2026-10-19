//! Where the functions that run coroutines go for each state, and which
//! variables of an async body still hold their values when it resumes.
//!
//! Resume points are decoded from each body's dispatch on its state
//! number, and held ranges by following the code from where each state
//! resumes; both are the module's machine code, read once as it loads.

use std::sync::Arc;

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::strings::{StrId, Strings, StringsBuilder};
use super::variables::CodeRange;
use super::{Builder, Image, NONE, Record, Span, TableKind};
use crate::{AddressRange, CodeInstanceId, ImageAddress, ResumePoint, ResumePoints};

/// Where a code instance that runs a coroutine goes for each state, or why
/// that is unknown.
pub type Decoded = (CodeInstanceId, Result<ResumePoints, Arc<str>>);

/// The code where the variable whose entry is at an offset still holds,
/// as its body resumes in a state, what it held before the state's await,
/// by the offset and the state.
pub type Held = ((u64, u64), Vec<AddressRange<ImageAddress>>);

/// What [`add_to`] encodes.
#[derive(Debug, Clone, Default)]
pub struct Resumes {
    /// Where each code instance that runs a coroutine goes for each
    /// state, or why that is unknown.
    pub points: Vec<Decoded>,

    /// For a variable of an async body, by its entry's offset, and each
    /// suspended state of the body's future: the code where the variable
    /// still holds what it held before the state's await. A later entry
    /// for a key replaces an earlier one.
    pub held: Vec<Held>,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ResumeRecord {
    pub instance: U32,
    pub dispatch: Span<CodeRange>,
    pub points: Span<ResumePointRecord>,

    /// Why the points are unknown, or [`NONE`].
    pub malformed: U32,
}

impl Record for ResumeRecord {
    const KIND: TableKind = TableKind::Resumes;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ResumePointRecord {
    pub state: U64,
    pub address: U64,
    pub resumption: Span<CodeRange>,
}

impl Record for ResumePointRecord {
    const KIND: TableKind = TableKind::ResumePoints;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct HeldRecord {
    pub offset: U64,
    pub state: U64,
    pub ranges: Span<CodeRange>,
}

impl Record for HeldRecord {
    const KIND: TableKind = TableKind::Held;
}

/// Why resume points could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the resume points do not fit an image")]
pub struct TooMany;

fn number(count: usize) -> Result<u32, TooMany> {
    u32::try_from(count)
        .ok()
        .filter(|count| *count < NONE)
        .ok_or(TooMany)
}

/// Adds `ranges` to `pool`, returning their span.
fn pooled(
    pool: &mut Vec<CodeRange>,
    ranges: &[AddressRange<ImageAddress>],
) -> Result<Span<CodeRange>, TooMany> {
    let first = number(pool.len())?;
    pool.extend(ranges.iter().map(|range| CodeRange {
        start: range.start.get().into(),
        end: range.end.get().into(),
    }));
    number(pool.len())?;
    Ok(Span::new(first, number(ranges.len())?))
}

/// Adds the resume points and held ranges to `builder`, pooling reasons in
/// `strings`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    resumes: &Resumes,
) -> Result<(), TooMany> {
    let mut ranges = Vec::new();
    let mut points = Vec::new();
    let mut sorted = resumes.points.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|(instance, _)| *instance);
    let mut records = Vec::with_capacity(sorted.len());
    for (instance, decoded) in sorted {
        let mut record = ResumeRecord {
            instance: instance.get().into(),
            dispatch: Span::EMPTY,
            points: Span::EMPTY,
            malformed: NONE.into(),
        };

        match decoded {
            Ok(decoded) => {
                record.dispatch = pooled(&mut ranges, &decoded.dispatch)?;
                record.points = Span::new(number(points.len())?, number(decoded.points.len())?);
                for point in decoded.points.iter() {
                    points.push(ResumePointRecord {
                        state: point.state.into(),
                        address: point.address.get().into(),
                        resumption: pooled(&mut ranges, &point.resumption)?,
                    });
                }
            }
            Err(why) => record.malformed = strings.push(why).ok_or(TooMany)?.0.into(),
        }

        records.push(record);
    }

    let mut held = resumes.held.iter().rev().collect::<Vec<_>>();
    held.sort_by_key(|(key, _)| *key);
    held.dedup_by_key(|(key, _)| *key);
    let held = held
        .into_iter()
        .map(|((offset, state), code)| {
            Ok(HeldRecord {
                offset: (*offset).into(),
                state: (*state).into(),
                ranges: pooled(&mut ranges, code)?,
            })
        })
        .collect::<Result<Vec<_>, TooMany>>()?;

    builder
        .owned_shared(TableKind::ResumeRanges, ranges)
        .owned_table(records)
        .owned_table(points)
        .owned_table(held);
    Ok(())
}

/// The resume points and held ranges of an image.
#[derive(Debug, Clone, Copy)]
pub struct ResumeView<'a> {
    strings: Strings<'a>,
    ranges: &'a [CodeRange],
    records: &'a [ResumeRecord],
    points: &'a [ResumePointRecord],
    held: &'a [HeldRecord],
}

impl<'a> ResumeView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            ranges: image.shared(TableKind::ResumeRanges),
            records: image.table(),
            points: image.table(),
            held: image.table(),
        }
    }

    fn ranges(
        self,
        ranges: Span<CodeRange>,
    ) -> impl ExactSizeIterator<Item = AddressRange<ImageAddress>> + 'a {
        ranges.rows(self.ranges).iter().map(CodeRange::get)
    }

    /// Where the code instance `instance`, which runs a coroutine, goes
    /// for each state, or why that is unknown; `None` for an instance that
    /// runs none.
    pub fn resume_points(self, instance: CodeInstanceId) -> Option<Result<ResumePoints, Arc<str>>> {
        let found = self
            .records
            .binary_search_by_key(&instance.get(), |record| record.instance.get())
            .ok()?;
        let record = &self.records[found];
        if record.malformed.get() != NONE {
            return Some(Err(self.strings.get(StrId(record.malformed.get())).into()));
        }

        Some(Ok(ResumePoints {
            dispatch: self.ranges(record.dispatch).collect(),
            points: record
                .points
                .rows(self.points)
                .iter()
                .map(|point| ResumePoint {
                    state: point.state.get(),
                    address: ImageAddress::new(point.address.get()),
                    resumption: self.ranges(point.resumption).collect(),
                })
                .collect(),
        }))
    }

    /// The code that runs on every resumption and is no statement of the
    /// program's: each coroutine's dispatch on its state, and the code
    /// leading from it into each state, with the instance it is in.
    pub fn resume_code(
        self,
    ) -> impl Iterator<Item = (AddressRange<ImageAddress>, CodeInstanceId)> + 'a {
        self.records.iter().flat_map(move |record| {
            let instance = CodeInstanceId::new(record.instance.get());
            let points = if record.malformed.get() == NONE {
                record.points.rows(self.points)
            } else {
                &[]
            };

            self.ranges(record.dispatch)
                .chain(
                    points
                        .iter()
                        .flat_map(move |point| self.ranges(point.resumption)),
                )
                .filter(|range| range.start < range.end)
                .map(move |range| (range, instance))
        })
    }

    /// The code where the variable whose entry is at `offset` still holds,
    /// as the body resumes in `state`, what it held before the state's
    /// await; `None` when that was not followed.
    pub fn held(
        self,
        offset: u64,
        state: u64,
    ) -> Option<impl ExactSizeIterator<Item = AddressRange<ImageAddress>> + 'a> {
        let found = self
            .held
            .binary_search_by_key(&(offset, state), |held| {
                (held.offset.get(), held.state.get())
            })
            .ok()?;
        let held = &self.held[found];
        Some(self.ranges(held.ranges))
    }
}

/// Checks the resume points and held ranges.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let view = ResumeView::new(image);
    let instances = image.table::<super::functions::InstanceRecord>().len();
    let ranges = view.ranges.len();
    if !view.records.iter().all(|record| {
        (record.instance.get() as usize) < instances
            && if record.malformed.get() == NONE {
                record.dispatch.within(ranges) && record.points.within(view.points.len())
            } else {
                view.strings.contains(StrId(record.malformed.get()))
                    && record.dispatch == Span::EMPTY
                    && record.points == Span::EMPTY
            }
    }) || !view
        .records
        .is_sorted_by(|earlier, later| earlier.instance.get() < later.instance.get())
    {
        return Err("a coroutine's resume points are malformed".into());
    }

    if !view
        .points
        .iter()
        .all(|point| point.resumption.within(ranges))
    {
        return Err("a resume point is malformed".into());
    }

    if !view.held.iter().all(|held| held.ranges.within(ranges))
        || !view.held.is_sorted_by(|earlier, later| {
            (earlier.offset.get(), earlier.state.get()) < (later.offset.get(), later.state.get())
        })
    {
        return Err("a held range is malformed".into());
    }

    Ok(())
}
