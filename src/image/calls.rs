//! What debug information says of calls: each call site, with what it
//! passed and where it returns, and what each function says of the calls
//! it makes, which entry values and tail-call chains are found from.
//!
//! Calling functions are one per function of [`super::variables`], in its
//! order; sites and functions name each other by row.

use std::sync::Arc;

use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::locations::{ExpressionId, LocationListId};
use super::strings::{StrId, Strings, StringsBuilder};
use super::types::Item;
use super::variables::{Keyed, Metadata};
use super::{Builder, Image, NONE, Record, Span, TableKind};
use crate::ImageAddress;
use crate::debug_info::TailJump;

/// What a function says of the calls it makes.
#[derive(Debug, Clone)]
pub struct CallingFunction {
    /// The linker name other modules may call it by.
    pub name: Option<Arc<str>>,
    pub frame_base: Metadata<LocationListId>,

    /// Whether the function describes every tail call it makes.
    pub tail_calls_described: bool,
    pub tail_calls: Vec<u32>,
}

/// What a call site calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteTarget {
    Code(ImageAddress),
    Symbol(Arc<str>),

    /// The location of the address it calls, or why it is malformed.
    Computed(Result<LocationListId, Arc<str>>),
    Unknown,
}

/// What a call passed in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiteParameter {
    /// The register the parameter is passed in.
    pub register: Option<u16>,

    /// The `.debug_info` offset of the callee's parameter entry.
    pub parameter: Option<u64>,
    pub value: Option<ExpressionId>,

    /// The value the passed address pointed at.
    pub data_value: Option<ExpressionId>,
}

/// One call site, as [`add_to`] takes it.
#[derive(Debug, Clone)]
pub struct CallSite {
    pub function: u32,

    /// The instruction after the call, which a frame it entered returns to.
    pub return_address: Option<ImageAddress>,
    pub target: SiteTarget,

    /// For a tail call, the function it enters in this module, if known.
    pub enters: Option<u32>,

    /// For a tail call, where it jumped from, if known.
    pub jump: Option<TailJump>,
    pub parameters: Vec<SiteParameter>,
    pub malformed: Option<Arc<str>>,
}

/// What [`add_to`] encodes.
#[derive(Debug, Clone, Default)]
pub struct Calls {
    pub functions: Vec<CallingFunction>,
    pub sites: Vec<CallSite>,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct CallingFunctionRecord {
    /// The linker name, or [`NONE`].
    pub name: U32,
    pub frame_base: U32,
    pub tail_calls: Span<Item>,
    pub frame_base_kind: u8,
    pub flags: u8,
}

impl Record for CallingFunctionRecord {
    const KIND: TableKind = TableKind::CallingFunctions;
}

pub mod calling_flags {
    pub const TAIL_CALLS_DESCRIBED: u8 = 1 << 0;

    pub const ALL: u8 = TAIL_CALLS_DESCRIBED;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct CallSiteRecord {
    pub function: U32,
    pub return_address: U64,

    /// What [`CallSiteRecord::target_kind`] says it is.
    pub target: U64,

    /// The function a tail call enters, or [`NONE`].
    pub enters: U32,
    pub jump_instruction: U64,
    pub jump_lookup: U64,
    pub parameters: Span<SiteParameterRecord>,

    /// Why the site is malformed, or [`NONE`].
    pub malformed: U32,
    pub target_kind: u8,
    pub flags: u8,
}

impl Record for CallSiteRecord {
    const KIND: TableKind = TableKind::CallSites;
}

pub mod site_flags {
    pub const RETURN_ADDRESS: u8 = 1 << 0;

    pub const JUMP: u8 = 1 << 1;

    pub const ALL: u8 = RETURN_ADDRESS | JUMP;
}

pub mod targets {
    pub const UNKNOWN: u8 = 0;

    pub const CODE: u8 = 1;

    pub const SYMBOL: u8 = 2;

    pub const COMPUTED: u8 = 3;

    pub const COMPUTED_MALFORMED: u8 = 4;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SiteParameterRecord {
    pub register: U16,
    pub parameter: U64,

    /// The value's expression, or [`NONE`].
    pub value: U32,
    pub data_value: U32,
    pub flags: u8,
}

impl Record for SiteParameterRecord {
    const KIND: TableKind = TableKind::SiteParameters;
}

pub mod parameter_flags {
    pub const REGISTER: u8 = 1 << 0;

    pub const PARAMETER: u8 = 1 << 1;

    pub const ALL: u8 = REGISTER | PARAMETER;
}

/// Why calls could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the call sites do not fit an image")]
pub struct TooMany;

fn number(count: usize) -> Result<u32, TooMany> {
    u32::try_from(count)
        .ok()
        .filter(|count| *count < NONE)
        .ok_or(TooMany)
}

fn text(strings: &mut StringsBuilder, text: &str) -> Result<U32, TooMany> {
    strings.push(text).map(|id| id.0.into()).ok_or(TooMany)
}

fn optional(id: Option<u32>) -> U32 {
    id.unwrap_or(NONE).into()
}

const fn some(value: U32) -> Option<u32> {
    match value.get() {
        NONE => None,
        value => Some(value),
    }
}

/// Adds the calling functions, call sites, and the index of where calls
/// return to `builder`, pooling names in `strings`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    calls: &Calls,
) -> Result<(), TooMany> {
    let mut functions = Vec::with_capacity(calls.functions.len());
    let mut tail_calls = Vec::new();
    for function in &calls.functions {
        let (frame_base_kind, frame_base) =
            super::variables::location(strings, &function.frame_base).map_err(|_| TooMany)?;
        functions.push(CallingFunctionRecord {
            name: match &function.name {
                Some(name) => text(strings, name)?,
                None => NONE.into(),
            },
            frame_base,
            tail_calls: Span::new(
                number(tail_calls.len())?,
                number(function.tail_calls.len())?,
            ),
            frame_base_kind,
            flags: if function.tail_calls_described {
                calling_flags::TAIL_CALLS_DESCRIBED
            } else {
                0
            },
        });
        tail_calls.extend(function.tail_calls.iter().map(|site| Item {
            value: (*site).into(),
        }));
    }

    let mut sites = Vec::with_capacity(calls.sites.len());
    let mut parameters = Vec::new();
    let mut returns = Vec::new();
    for (index, site) in calls.sites.iter().enumerate() {
        let mut flags = 0;
        if let Some(address) = site.return_address {
            flags |= site_flags::RETURN_ADDRESS;
            returns.push((address.get(), number(index)?));
        }

        if site.jump.is_some() {
            flags |= site_flags::JUMP;
        }

        let (target_kind, target) = match &site.target {
            SiteTarget::Unknown => (targets::UNKNOWN, 0),
            SiteTarget::Code(address) => (targets::CODE, address.get()),
            SiteTarget::Symbol(name) => (targets::SYMBOL, u64::from(text(strings, name)?.get())),
            SiteTarget::Computed(Ok(list)) => (targets::COMPUTED, u64::from(list.0)),
            SiteTarget::Computed(Err(why)) => (
                targets::COMPUTED_MALFORMED,
                u64::from(text(strings, why)?.get()),
            ),
        };

        sites.push(CallSiteRecord {
            function: site.function.into(),
            return_address: site.return_address.map_or(0, ImageAddress::get).into(),
            target: target.into(),
            enters: optional(site.enters),
            jump_instruction: site.jump.map_or(0, |jump| jump.instruction.get()).into(),
            jump_lookup: site.jump.map_or(0, |jump| jump.lookup.get()).into(),
            parameters: Span::new(number(parameters.len())?, number(site.parameters.len())?),
            malformed: match &site.malformed {
                Some(why) => text(strings, why)?,
                None => NONE.into(),
            },
            target_kind,
            flags,
        });
        parameters.extend(site.parameters.iter().map(|parameter| {
            let mut flags = 0;
            if parameter.register.is_some() {
                flags |= parameter_flags::REGISTER;
            }

            if parameter.parameter.is_some() {
                flags |= parameter_flags::PARAMETER;
            }

            SiteParameterRecord {
                register: parameter.register.unwrap_or(0).into(),
                parameter: parameter.parameter.unwrap_or(0).into(),
                value: optional(parameter.value.map(|id| id.0)),
                data_value: optional(parameter.data_value.map(|id| id.0)),
                flags,
            }
        }));
        number(parameters.len())?;
    }

    returns.sort_unstable();
    let returns = returns
        .into_iter()
        .map(|(address, site)| Keyed {
            key: address.into(),
            value: site.into(),
        })
        .collect::<Vec<_>>();
    builder
        .owned_table(functions)
        .owned_shared(TableKind::TailCalls, tail_calls)
        .owned_table(sites)
        .owned_table(parameters)
        .owned_shared(TableKind::CallReturns, returns);

    Ok(())
}

/// A call site, by its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SiteId(pub u32);

/// The calls of an image.
#[derive(Debug, Clone, Copy)]
pub struct CallView<'a> {
    strings: Strings<'a>,
    functions: &'a [CallingFunctionRecord],
    tail_calls: &'a [Item],
    sites: &'a [CallSiteRecord],
    parameters: &'a [SiteParameterRecord],
    returns: &'a [Keyed],
}

impl<'a> CallView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            functions: image.table(),
            tail_calls: image.shared(TableKind::TailCalls),
            sites: image.table(),
            parameters: image.table(),
            returns: image.shared(TableKind::CallReturns),
        }
    }

    /// The calling function `function` numbers, which must be one of these.
    pub const fn function(self, function: u32) -> Calling<'a> {
        Calling {
            view: self,
            record: &self.functions[function as usize],
        }
    }

    /// The site `id` names, when it is one of these.
    pub fn site(self, id: SiteId) -> Option<Site<'a>> {
        Some(Site {
            view: self,
            record: self.sites.get(id.0 as usize)?,
        })
    }

    /// The sites whose calls return to `address`; more than one is
    /// malformed.
    pub fn returning_to(self, address: ImageAddress) -> impl Iterator<Item = SiteId> + 'a {
        let first = self
            .returns
            .partition_point(|entry| entry.key.get() < address.get());
        self.returns[first..]
            .iter()
            .take_while(move |entry| entry.key.get() == address.get())
            .map(|entry| SiteId(entry.value.get()))
    }

    fn text(self, id: u32) -> Arc<str> {
        self.strings.get(StrId(id)).into()
    }
}

/// What one function says of the calls it makes.
#[derive(Debug, Clone, Copy)]
pub struct Calling<'a> {
    view: CallView<'a>,
    record: &'a CallingFunctionRecord,
}

impl<'a> Calling<'a> {
    /// The linker name other modules may call it by.
    pub fn name(self) -> Option<Arc<str>> {
        some(self.record.name).map(|name| self.view.text(name))
    }

    pub fn frame_base(self) -> Metadata<LocationListId> {
        super::variables::decode_location(
            self.view.strings,
            self.record.frame_base_kind,
            self.record.frame_base,
        )
    }

    /// Whether the function describes every tail call it makes.
    pub const fn tail_calls_described(self) -> bool {
        self.record.flags & calling_flags::TAIL_CALLS_DESCRIBED != 0
    }

    /// The tail calls it makes, in the order it describes them.
    pub fn tail_calls(self) -> impl ExactSizeIterator<Item = SiteId> + 'a {
        self.record
            .tail_calls
            .rows(self.view.tail_calls)
            .iter()
            .map(|item| SiteId(item.value.get()))
    }
}

/// One call site of a [`CallView`].
#[derive(Debug, Clone, Copy)]
pub struct Site<'a> {
    view: CallView<'a>,
    record: &'a CallSiteRecord,
}

impl<'a> Site<'a> {
    /// The function making the call.
    pub const fn function(self) -> u32 {
        self.record.function.get()
    }

    /// The instruction after the call, which a frame it entered returns to.
    pub const fn return_address(self) -> Option<ImageAddress> {
        if self.record.flags & site_flags::RETURN_ADDRESS == 0 {
            None
        } else {
            Some(ImageAddress::new(self.record.return_address.get()))
        }
    }

    pub fn target(self) -> SiteTarget {
        let value = self.record.target.get();

        // Validation keeps a string's or a list's number within u32.
        let row = u32::try_from(value).ok();
        match self.record.target_kind {
            targets::CODE => SiteTarget::Code(ImageAddress::new(value)),
            targets::SYMBOL => row.map_or(SiteTarget::Unknown, |row| {
                SiteTarget::Symbol(self.view.text(row))
            }),
            targets::COMPUTED => row.map_or(SiteTarget::Unknown, |row| {
                SiteTarget::Computed(Ok(LocationListId(row)))
            }),
            targets::COMPUTED_MALFORMED => row.map_or(SiteTarget::Unknown, |row| {
                SiteTarget::Computed(Err(self.view.text(row)))
            }),
            _ => SiteTarget::Unknown,
        }
    }

    /// For a tail call, the function it enters in this module, if known.
    pub const fn enters(self) -> Option<u32> {
        some(self.record.enters)
    }

    /// For a tail call, where it jumped from, if known.
    pub const fn jump(self) -> Option<TailJump> {
        if self.record.flags & site_flags::JUMP == 0 {
            None
        } else {
            Some(TailJump {
                instruction: ImageAddress::new(self.record.jump_instruction.get()),
                lookup: ImageAddress::new(self.record.jump_lookup.get()),
            })
        }
    }

    pub fn parameters(self) -> impl ExactSizeIterator<Item = SiteParameter> + 'a {
        self.record
            .parameters
            .rows(self.view.parameters)
            .iter()
            .map(|parameter| SiteParameter {
                register: (parameter.flags & parameter_flags::REGISTER != 0)
                    .then(|| parameter.register.get()),
                parameter: (parameter.flags & parameter_flags::PARAMETER != 0)
                    .then(|| parameter.parameter.get()),
                value: some(parameter.value).map(ExpressionId),
                data_value: some(parameter.data_value).map(ExpressionId),
            })
    }

    pub fn malformed(self) -> Option<Arc<str>> {
        some(self.record.malformed).map(|why| self.view.text(why))
    }
}

/// Checks the calls.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let view = CallView::new(image);
    let lists = image.table::<super::locations::LocationListRecord>().len();
    let expressions = image.table::<super::locations::ExpressionRecord>().len();
    let strings = view.strings;
    let string = |id: u64| u32::try_from(id).is_ok_and(|id| strings.contains(StrId(id)));
    let optional_string = |id: U32| id.get() == NONE || string(id.get().into());
    let functions = view.functions.len();
    let sites = view.sites.len();
    if !view.functions.iter().all(|function| {
        optional_string(function.name)
            && function.flags & !calling_flags::ALL == 0
            && function.frame_base_kind != super::variables::value_kinds::CONSTANT
            && super::variables::valid_location(
                function.frame_base_kind,
                function.frame_base,
                lists,
                strings,
            )
            && function.tail_calls.within(view.tail_calls.len())
    }) {
        return Err("a calling function is malformed".into());
    }

    if !view
        .tail_calls
        .iter()
        .all(|item| (item.value.get() as usize) < sites)
    {
        return Err("a tail call names no site".into());
    }

    let expression = |id: U32| id.get() == NONE || (id.get() as usize) < expressions;
    if !view.parameters.iter().all(|parameter| {
        parameter.flags & !parameter_flags::ALL == 0
            && (parameter.flags & parameter_flags::REGISTER != 0 || parameter.register.get() == 0)
            && (parameter.flags & parameter_flags::PARAMETER != 0 || parameter.parameter.get() == 0)
            && expression(parameter.value)
            && expression(parameter.data_value)
    }) {
        return Err("a call site's parameter is malformed".into());
    }

    if !view.sites.iter().all(|site| {
        (site.function.get() as usize) < functions
            && (site.enters.get() == NONE || (site.enters.get() as usize) < functions)
            && site.flags & !site_flags::ALL == 0
            && (site.flags & site_flags::RETURN_ADDRESS != 0 || site.return_address.get() == 0)
            && (site.flags & site_flags::JUMP != 0
                || (site.jump_instruction.get() == 0 && site.jump_lookup.get() == 0))
            && site.parameters.within(view.parameters.len())
            && optional_string(site.malformed)
            && match site.target_kind {
                targets::UNKNOWN => site.target.get() == 0,
                targets::CODE => true,
                targets::SYMBOL | targets::COMPUTED_MALFORMED => string(site.target.get()),
                targets::COMPUTED => {
                    usize::try_from(site.target.get()).is_ok_and(|list| list < lists)
                }
                _ => false,
            }
    }) {
        return Err("a call site is malformed".into());
    }

    if !view.returns.is_sorted_by(|earlier, later| {
        (earlier.key.get(), earlier.value.get()) < (later.key.get(), later.value.get())
    }) || !view.returns.iter().all(|entry| {
        view.sites
            .get(entry.value.get() as usize)
            .is_some_and(|site| {
                site.flags & site_flags::RETURN_ADDRESS != 0
                    && site.return_address.get() == entry.key.get()
            })
    }) || view
        .sites
        .iter()
        .filter(|site| site.flags & site_flags::RETURN_ADDRESS != 0)
        .count()
        != view.returns.len()
    {
        return Err("the index of where calls return is malformed".into());
    }

    Ok(())
}
