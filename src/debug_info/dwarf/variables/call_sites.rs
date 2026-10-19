//! The calls a module describes (DWARF 5 section 3.4.1), which recover the
//! values a function's parameters held on entry from the call that entered
//! it, and the tail calls that may have entered it since.

use std::sync::Arc;

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};
use gimli::{Location, Value};

use crate::debug_info::dwarf::{
    DieKey, DieMap, DwarfError, Reader, Units, die_reference, unit_dwarf,
};

use crate::debug_info::{
    CallSite, CallSiteId, CallTarget, EntryParameter, TailCallChain, TailJump, VariableRuntime,
    VariableRuntimeError,
};

use crate::{
    AddressRange, EntryValueUnavailableReason, ImageAddress, VariableUnavailableReason,
    VirtualAddress,
};

use super::codec::bytes_to_u64;
use super::die::{flag_with_origins, origin_chain, string_with_origins};
use super::evaluate::{EvaluateError, FrameBase, FrameBaseCache, FrameBaseContext, evaluate};
use super::location::{
    Expression, LocationListId, LocationSelectionError, LocationsBuilder, copy_expression,
    copy_optional_location, select,
};

use super::{DwarfVariableInfo, InspectionBudget, Metadata, MetadataAbsence};
use crate::image::calls::{
    self, CallView, CallingFunction, Calls, Site, SiteId, SiteParameter, SiteTarget,
};

use crate::image::locations::ExpressionId;
use crate::unwind::DwarfRegister;

/// How many functions a search for tail calls may visit.
const MAX_TAIL_CALL_FUNCTIONS: usize = 4_096;

/// What a call site entry says it calls: a function entry, which the
/// catalog resolves once it has seen every function, or a target.
enum Callee {
    Entry(DieKey),
    Target(SiteTarget),
}

/// Builds the catalog while the variable catalog walks every entry.
#[derive(Default)]
pub(super) struct CallSiteBuilder {
    /// Each site, with the function entry it calls until that is resolved.
    sites: Vec<(calls::CallSite, Option<DieKey>)>,
    functions: Vec<CallingFunction>,

    /// The first address of each function entry with code.
    starts: DieMap<ImageAddress>,

    /// Each function with code, by its first address.
    code: Vec<(ImageAddress, usize)>,

    /// The abstract origin of each function with code.
    origins: Vec<(DieKey, usize, ImageAddress)>,

    /// Functions with code that other units may call by linkage name.
    external: HashMap<Arc<str>, Vec<usize>>,

    /// The site whose parameters follow, with its entry's depth.
    open: Option<(usize, usize)>,
}

impl CallSiteBuilder {
    /// Records the subprogram that the next cataloged function describes.
    pub(super) fn function<'data>(
        &mut self,
        dwarf: &gimli::Dwarf<Reader<'data>>,
        units: &Units<'data>,
        unit_index: usize,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        ranges: &[AddressRange<ImageAddress>],
        frame_base: Metadata<LocationListId>,
    ) {
        let index = self.functions.len();
        let start = ranges.iter().map(|range| range.start).min();
        let mut name = None;
        let described = [
            gimli::DW_AT_call_all_calls,
            gimli::DW_AT_call_all_tail_calls,
            gimli::DW_AT_GNU_all_call_sites,
            gimli::DW_AT_GNU_all_tail_call_sites,
        ]
        .into_iter()
        .any(|attribute| {
            matches!(
                entry.attr_value(attribute),
                Some(gimli::AttributeValue::Flag(true))
            )
        });

        if let Some(start) = start {
            let key = DieKey {
                unit: unit_index,
                offset: entry.offset().0,
            };

            self.starts.insert(key, start);
            self.code.push((start, index));
            if let Ok(Some(origin)) = die_reference(
                entry.attr_value(gimli::DW_AT_abstract_origin),
                unit_index,
                units,
            ) {
                self.origins.push((origin, index, start));
            }

            name = external_name(dwarf, units, unit_index, entry);
            if let Some(name) = &name {
                self.external
                    .entry(Arc::clone(name))
                    .or_default()
                    .push(index);
            }
        }

        self.functions.push(CallingFunction {
            name,
            frame_base,
            tail_calls_described: described,
            tail_calls: Vec::new(),
        });
    }

    /// Records a call site of the cataloged function `function`.
    #[expect(
        clippy::too_many_arguments,
        reason = "a site is one entry of one unit, read in one walk into one pool"
    )]
    pub(super) fn site<'data>(
        &mut self,
        dwarf: &gimli::Dwarf<Reader<'data>>,
        pool: &mut LocationsBuilder,
        units: &Units<'data>,
        unit_index: usize,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        function: usize,
        depth: usize,
    ) {
        let unit = &units[unit_index];
        let mut site = calls::CallSite {
            function: super::row(function),
            return_address: None,
            target: SiteTarget::Unknown,
            enters: None,
            jump: None,
            parameters: Vec::new(),
            malformed: None,
        };

        let mut entry_called = None;
        match site_attributes(dwarf, pool, units, unit_index, unit, entry) {
            Ok(SiteAttributes {
                return_address,
                call,
                tail,
                callee,
            }) => {
                match callee {
                    Callee::Entry(key) => entry_called = Some(key),
                    Callee::Target(target) => site.target = target,
                }

                // A tail call returns nowhere, whatever address follows it.
                site.return_address = return_address.filter(|_| !tail);
                if tail {
                    site.jump = tail_jump(call, return_address);
                    self.functions[function]
                        .tail_calls
                        .push(super::row(self.sites.len()));
                }
            }
            Err(error) => site.malformed = Some(error.to_string().into()),
        }

        self.open = Some((depth, self.sites.len()));
        self.sites.push((site, entry_called));
    }

    /// Records a parameter of the site whose entry contains it.
    pub(super) fn parameter(
        &mut self,
        dwarf: &gimli::Dwarf<Reader<'_>>,
        pool: &mut LocationsBuilder,
        units: &Units<'_>,
        unit_index: usize,
        entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
        depth: usize,
    ) {
        let Some((site_depth, site)) = self.open else {
            return;
        };

        if depth != site_depth + 1 {
            return;
        }

        let (site, _) = &mut self.sites[site];

        match site_parameter(dwarf, pool, units, unit_index, entry) {
            Ok(parameter) => site.parameters.push(parameter),
            Err(error) => {
                site.malformed
                    .get_or_insert_with(|| error.to_string().into());
            }
        }
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.sites.is_empty() && self.functions.is_empty()
    }

    /// Forgets the site the parameters that follow belong to: a unit's
    /// parameters belong to its own sites.
    pub(super) const fn begin_unit(&mut self) {
        self.open = None;
    }

    /// Adds what `other` recorded of a later unit, with its functions and
    /// sites after these, and each list and expression it pooled as
    /// `pooled` has it here.
    pub(super) fn absorb(&mut self, other: Self, pooled: &super::merge::Pooled) {
        let functions = self.functions.len();
        let sites = self.sites.len();
        let function = |index: usize| index + functions;
        self.functions
            .extend(other.functions.into_iter().map(|mut calling| {
                pooled.frame_base(&mut calling.frame_base);
                for site in &mut calling.tail_calls {
                    *site += super::row(sites);
                }

                calling
            }));
        self.sites
            .extend(other.sites.into_iter().map(|(mut site, entry)| {
                site.function += super::row(functions);
                if let SiteTarget::Computed(Ok(list)) = &mut site.target {
                    *list = pooled.list(*list);
                }

                for parameter in &mut site.parameters {
                    for expression in [&mut parameter.value, &mut parameter.data_value]
                        .into_iter()
                        .flatten()
                    {
                        *expression = pooled.expression(*expression);
                    }
                }

                (site, entry)
            }));
        self.starts.extend(other.starts);
        self.code.extend(
            other
                .code
                .into_iter()
                .map(|(start, index)| (start, function(index))),
        );
        self.origins.extend(
            other
                .origins
                .into_iter()
                .map(|(origin, index, start)| (origin, function(index), start)),
        );

        #[expect(
            clippy::iter_over_hash_type,
            reason = "each name's functions are appended in order, whatever order the names come in"
        )]
        for (name, indices) in other.external {
            self.external
                .entry(name)
                .or_default()
                .extend(indices.into_iter().map(function));
        }
    }

    pub(super) fn finish(mut self) -> Calls {
        let mut code = std::mem::take(&mut self.code);
        code.sort_unstable();
        let mut concrete = HashMap::<DieKey, Vec<ImageAddress>>::new();
        for (origin, _, start) in &self.origins {
            concrete.entry(*origin).or_default().push(*start);
        }

        for (site, entry) in &mut self.sites {
            if let Some(key) = entry {
                // A function with code is entered there; an abstract one
                // where its one out-of-line instance is.
                site.target = match (self.starts.get(key), concrete.get(key).map(Vec::as_slice)) {
                    (Some(start), _) | (_, Some([start])) => SiteTarget::Code(*start),
                    _ => SiteTarget::Unknown,
                };
            }
        }

        // What each tail call enters, when it stays in this module.
        let function_at = |address: ImageAddress| {
            code.binary_search_by_key(&address, |(start, _)| *start)
                .ok()
                .map(|position| super::row(code[position].1))
        };

        for function in &self.functions {
            for &site in &function.tail_calls {
                let (site, _) = &mut self.sites[site as usize];
                site.enters = match &site.target {
                    SiteTarget::Code(address) => function_at(*address),
                    SiteTarget::Symbol(name) => match self.external.get(name).map(Vec::as_slice) {
                        Some([only]) => Some(super::row(*only)),
                        _ => None,
                    },
                    _ => None,
                };
            }
        }

        Calls {
            functions: self.functions,
            sites: self.sites.into_iter().map(|(site, _)| site).collect(),
        }
    }
}

/// What a call site entry says of its call.
struct SiteAttributes {
    /// The instruction after the call.
    return_address: Option<ImageAddress>,

    /// The call instruction itself.
    call: Option<ImageAddress>,
    tail: bool,
    callee: Callee,
}

/// Where a tail call jumped from: its instruction, or else within the
/// instruction before the address after it.
fn tail_jump(call: Option<ImageAddress>, after: Option<ImageAddress>) -> Option<TailJump> {
    if let Some(call) = call {
        return Some(TailJump {
            instruction: call,
            lookup: call,
        });
    }

    let after = after?;
    Some(TailJump {
        instruction: after,
        lookup: ImageAddress::new(after.get().checked_sub(1)?),
    })
}

fn site_attributes<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    pool: &mut LocationsBuilder,
    units: &Units<'data>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Result<SiteAttributes, DwarfError> {
    let address = |attributes: &[gimli::DwAt]| -> Result<Option<ImageAddress>, DwarfError> {
        match attributes
            .iter()
            .find_map(|attribute| entry.attr_value(*attribute))
        {
            Some(value) => Ok(unit_dwarf(dwarf, unit)
                .attr_address(unit, value)?
                .map(ImageAddress::new)),
            None => Ok(None),
        }
    };

    let return_address = address(&[gimli::DW_AT_call_return_pc, gimli::DW_AT_low_pc])?;
    let call = address(&[gimli::DW_AT_call_pc])?;
    let tail = [gimli::DW_AT_call_tail_call, gimli::DW_AT_GNU_tail_call]
        .into_iter()
        .any(|attribute| {
            matches!(
                entry.attr_value(attribute),
                Some(gimli::AttributeValue::Flag(true))
            )
        });
    let origin = entry
        .attr_value(gimli::DW_AT_call_origin)
        .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin));

    let callee = if let Some(key) = die_reference(origin, unit_index, units)? {
        let origin_unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        let origin = origin_unit.entry(gimli::UnitOffset(key.offset))?;
        if matches!(
            origin.attr_value(gimli::DW_AT_declaration),
            Some(gimli::AttributeValue::Flag(true))
        ) {
            Callee::Target(
                linkage_name(dwarf, units, key.unit, &origin)?
                    .map_or(SiteTarget::Unknown, SiteTarget::Symbol),
            )
        } else {
            Callee::Entry(key)
        }
    } else if let Some(value) = entry
        .attr_value(gimli::DW_AT_call_target)
        .or_else(|| entry.attr_value(gimli::DW_AT_GNU_call_site_target))
    {
        Callee::Target(SiteTarget::Computed(
            match copy_optional_location(
                dwarf,
                pool,
                unit_index,
                unit,
                Some(value),
                MetadataAbsence::NotApplicable,
            ) {
                Metadata::Value(location) => Ok(location),
                Metadata::Malformed(description) => Err(description),
                Metadata::Absent(_) => Err("a computed call target has no location".into()),
            },
        ))
    } else {
        Callee::Target(SiteTarget::Unknown)
    };

    Ok(SiteAttributes {
        return_address,
        call,
        tail,
        callee,
    })
}

fn site_parameter(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    pool: &mut LocationsBuilder,
    units: &Units<'_>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Result<SiteParameter, DwarfError> {
    let unit = &units[unit_index];
    let mut expression =
        |attributes: [gimli::DwAt; 2]| -> Result<Option<ExpressionId>, DwarfError> {
            attributes
                .into_iter()
                .find_map(|attribute| entry.attr_value(attribute))
                .and_then(|value| value.exprloc_value())
                .map(|expression| copy_expression(dwarf, pool, unit_index, unit, expression))
                .transpose()
        };
    let register = match entry
        .attr_value(gimli::DW_AT_location)
        .and_then(|value| value.exprloc_value())
    {
        Some(location) => {
            let mut operations = location.operations(unit.encoding());
            match (operations.next()?, operations.next()?) {
                (Some(gimli::Operation::Register { register }), None) => Some(register.0),
                _ => None,
            }
        }
        None => None,
    };

    let parameter = die_reference(
        entry
            .attr_value(gimli::DW_AT_call_parameter)
            .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin)),
        unit_index,
        units,
    )?
    .and_then(|key| units.debug_info_offset(key));

    Ok(SiteParameter {
        register,
        parameter,
        value: expression([gimli::DW_AT_call_value, gimli::DW_AT_GNU_call_site_value])?,
        data_value: expression([
            gimli::DW_AT_call_data_value,
            gimli::DW_AT_GNU_call_site_data_value,
        ])?,
    })
}

/// The name a linker symbol gives a function: its linkage name, or else
/// its name.
fn linkage_name<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Result<Option<Arc<str>>, DwarfError> {
    let chain = origin_chain(units, unit_index, entry)?;
    for attribute in [
        gimli::DW_AT_linkage_name,
        gimli::DW_AT_MIPS_linkage_name,
        gimli::DW_AT_name,
    ] {
        if let Some(name) =
            string_with_origins(dwarf, units, &units[unit_index], entry, &chain, attribute)?
        {
            return Ok(Some(name));
        }
    }

    Ok(None)
}

/// The linkage name of a function other units may call.
fn external_name<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Option<Arc<str>> {
    let chain = origin_chain(units, unit_index, entry).ok()?;
    if flag_with_origins(entry, &chain, gimli::DW_AT_external) != Some(true) {
        return None;
    }

    linkage_name(dwarf, units, unit_index, entry).ok().flatten()
}

const fn unavailable(reason: EntryValueUnavailableReason) -> VariableRuntimeError {
    VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(reason))
}

/// What a call passed for `parameter`: the value, or for a referent the
/// value at the address passed.
fn passed(site: Site<'_>, parameter: EntryParameter) -> Result<ExpressionId, VariableRuntimeError> {
    let mut entries = site.parameters().filter(|passed| match parameter {
        EntryParameter::Register(register) | EntryParameter::Referent(register) => {
            passed.register.map(DwarfRegister::new) == Some(register)
        }
        EntryParameter::Parameter(offset) => passed.parameter == Some(offset),
    });
    let not_passed = || unavailable(EntryValueUnavailableReason::NoParameter);
    let passed = entries.next().ok_or_else(not_passed)?;
    if entries.next().is_some() {
        return Err(VariableRuntimeError::Malformed(
            "a call site describes one parameter twice".into(),
        ));
    }

    match parameter {
        EntryParameter::Referent(_) => passed.data_value,
        EntryParameter::Register(_) | EntryParameter::Parameter(_) => passed.value,
    }
    .ok_or_else(not_passed)
}

/// The function a tail call enters, which a chain was checked to stay in
/// the module for.
fn entered(view: CallView<'_>, site: SiteId) -> Result<u32, VariableRuntimeError> {
    view.site(site)
        .and_then(Site::enters)
        .ok_or_else(|| VariableRuntimeError::Fatal("unchecked tail call site".into()))
}

/// The chain of tail calls from function `from`, entered by a call, to
/// function `to`, when exactly one is possible.
fn tail_path(view: CallView<'_>, from: u32, to: u32) -> Result<Vec<SiteId>, VariableRuntimeError> {
    // Every function a chain from `from` may reach must describe its tail
    // calls, each of which must stay within this module.
    let mut reached = vec![from];
    let mut seen = HashSet::from_iter([from]);
    let mut next = 0;

    while let Some(&function) = reached.get(next) {
        next += 1;
        let calling = view.function(function);
        if !calling.tail_calls_described() {
            return Err(unavailable(EntryValueUnavailableReason::TailCalls));
        }

        for site in calling.tail_calls() {
            let enters = view
                .site(site)
                .and_then(Site::enters)
                .ok_or_else(|| unavailable(EntryValueUnavailableReason::TailCalls))?;
            if seen.insert(enters) {
                if reached.len() == MAX_TAIL_CALL_FUNCTIONS {
                    return Err(VariableUnavailableReason::EvaluationLimit.into());
                }

                reached.push(enters);
            }
        }
    }

    if !seen.contains(&to) {
        return Err(unavailable(EntryValueUnavailableReason::TargetMismatch));
    }

    // The functions that lead to `to`, by walking tail calls backwards.
    let mut callers = HashMap::<u32, Vec<u32>>::new();

    for &function in &reached {
        for site in view.function(function).tail_calls() {
            callers
                .entry(entered(view, site)?)
                .or_default()
                .push(function);
        }
    }

    let mut leads = HashSet::from_iter([to]);
    let mut pending = vec![to];

    while let Some(function) = pending.pop() {
        for &caller in callers.get(&function).into_iter().flatten() {
            if leads.insert(caller) {
                pending.push(caller);
            }
        }
    }

    // Count the chains to `to`; a cycle among the functions leading to it
    // allows endlessly many.
    let mut chains = HashMap::<u32, u64>::new();
    let mut active = HashSet::new();
    count_chains(view, from, to, &leads, &mut chains, &mut active)?;
    if chains[&from] != 1 {
        return Err(unavailable(EntryValueUnavailableReason::TailCalls));
    }

    let mut path = Vec::new();
    let mut function = from;

    while function != to {
        let mut next = None;
        for site in view.function(function).tail_calls() {
            let enters = entered(view, site)?;
            if chains.get(&enters) == Some(&1) {
                next = Some((site, enters));
                break;
            }
        }

        let Some((site, enters)) = next else {
            return Err(VariableRuntimeError::Fatal(
                "the chain to the target does not continue".into(),
            ));
        };

        path.push(site);
        function = enters;
    }

    Ok(path)
}

fn count_chains(
    view: CallView<'_>,
    function: u32,
    to: u32,
    leads: &HashSet<u32>,
    chains: &mut HashMap<u32, u64>,
    active: &mut HashSet<u32>,
) -> Result<u64, VariableRuntimeError> {
    if let Some(count) = chains.get(&function) {
        return Ok(*count);
    }

    if !active.insert(function) {
        return Err(unavailable(EntryValueUnavailableReason::TailCalls));
    }

    let mut count = u64::from(function == to);
    for site in view.function(function).tail_calls() {
        let enters = entered(view, site)?;
        if leads.contains(&enters) {
            count = count.saturating_add(count_chains(view, enters, to, leads, chains, active)?);
        }
    }

    active.remove(&function);
    chains.insert(function, count);
    Ok(count)
}

impl DwarfVariableInfo {
    /// The image's calls, once [`Self::bind`] has given the image.
    pub(super) fn calls(&self) -> CallView<'_> {
        CallView::new(self.types.tables())
    }

    pub(super) fn described_call_site(
        &self,
        return_address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<CallSite>, VariableRuntimeError> {
        let calls = self.calls();
        let mut sites = calls.returning_to(return_address);
        let Some(id) = sites.next() else {
            return Ok(None);
        };

        if sites.next().is_some() {
            return Err(VariableRuntimeError::Malformed(
                format!("several call sites return to {return_address}").into(),
            ));
        }

        let Some(site) = calls.site(id) else {
            return Err(VariableRuntimeError::Fatal(
                "a call site index is out of range".into(),
            ));
        };

        if let Some(description) = site.malformed() {
            return Err(VariableRuntimeError::Malformed(description));
        }

        let target = match site.target() {
            SiteTarget::Code(address) => CallTarget::Code(address),
            SiteTarget::Symbol(name) => CallTarget::Symbol(name),
            SiteTarget::Computed(Err(description)) => {
                return Err(VariableRuntimeError::Malformed(description));
            }
            SiteTarget::Computed(Ok(location)) => {
                // A target the caller can no longer compute is unknown.
                let list = self.locations().list(location);

                match self.site_word(site, |address| select(list, address), runtime, budget) {
                    Ok(address) => CallTarget::Computed(VirtualAddress::new(address)),
                    Err(VariableRuntimeError::Unavailable(
                        reason @ (VariableUnavailableReason::EvaluationLimit
                        | VariableUnavailableReason::InspectionLimit(_)),
                    )) => return Err(reason.into()),
                    Err(VariableRuntimeError::Unavailable(_)) => CallTarget::Unknown,
                    Err(error) => return Err(error),
                }
            }
            SiteTarget::Unknown => CallTarget::Unknown,
        };

        Ok(Some(CallSite {
            id: CallSiteId(id.0 as usize),
            target,
        }))
    }

    pub(super) fn tail_call_path(
        &self,
        from: ImageAddress,
        to: ImageAddress,
    ) -> Result<TailCallChain, VariableRuntimeError> {
        let function = |address| {
            self.function_at(address)
                .map(|function| function.id().0)
                .ok_or_else(|| unavailable(EntryValueUnavailableReason::UnknownTarget))
        };

        let (from, to) = (function(from)?, function(to)?);
        let calls = self.calls();
        let links = tail_path(calls, from, to)?;
        let entered = links
            .iter()
            .map(|site| entered(calls, *site))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(TailCallChain {
            functions: std::iter::once(from)
                .chain(entered)
                .map(|function| calls.function(function).name())
                .collect(),
            jumps: links
                .iter()
                .map(|site| calls.site(*site).and_then(Site::jump))
                .collect(),
            links: links
                .into_iter()
                .map(|site| CallSiteId(site.0 as usize))
                .collect(),
        })
    }

    pub(super) fn site_parameter_value(
        &self,
        site: CallSiteId,
        parameter: EntryParameter,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let site = u32::try_from(site.0)
            .ok()
            .and_then(|id| self.calls().site(SiteId(id)))
            .ok_or_else(|| VariableRuntimeError::Fatal("unknown call site".into()))?;
        if let Some(description) = site.malformed() {
            return Err(VariableRuntimeError::Malformed(description));
        }

        let expression = self.locations().expression(passed(site, parameter)?);
        self.site_word(site, |_| Ok(Some(expression)), runtime, budget)
    }

    /// Evaluates one of a call site's expressions to a word, in the state of
    /// the frame that made the call: the value it computes, or the address
    /// it describes.
    fn site_word<'a>(
        &'a self,
        site: Site<'a>,
        expression: impl FnOnce(
            Option<ImageAddress>,
        ) -> Result<Option<Expression<'a>>, LocationSelectionError>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        // Within the call instruction, which the caller's frame describes.
        let address = site
            .return_address()
            .and_then(|address| address.get().checked_sub(1))
            .map(ImageAddress::new);
        let expression = expression(address)
            .map_err(|error| match error {
                LocationSelectionError::Unavailable(reason) => {
                    VariableRuntimeError::Unavailable(reason)
                }
                LocationSelectionError::Malformed(description) => {
                    VariableRuntimeError::Malformed(description)
                }
            })?
            .ok_or(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::UnavailableAtInstruction,
            ))?;

        let mut cache = FrameBaseCache::Empty;
        let location = self.calls().function(site.function()).frame_base();
        let mut frame_base = FrameBase::Lazy(FrameBaseContext {
            location: &location,
            tables: self.locations(),
            address,
            cache: &mut cache,
        });
        let pieces = evaluate(
            expression,
            self.endian,
            address,
            &mut frame_base,
            runtime,
            budget,
        )
        .map_err(runtime_error)?;
        let [piece] = pieces.as_slice() else {
            return Err(VariableRuntimeError::Malformed(
                "a call site's value has several pieces".into(),
            ));
        };

        if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
            return Err(VariableRuntimeError::Malformed(
                "a call site's value is a partial piece".into(),
            ));
        }

        match &piece.location {
            // Without DW_OP_stack_value, the address an expression computes
            // is its value.
            Location::Address { address } => Ok(*address),
            Location::Value { value } => Ok(word(*value)),
            Location::Register { register } => {
                let register = runtime.register((*register).into())?;
                bytes_to_u64(&register.bytes, self.endian).map_err(runtime_error)
            }
            Location::Bytes { value } => {
                bytes_to_u64(value.slice(), self.endian).map_err(runtime_error)
            }
            _ => Err(VariableRuntimeError::Malformed(
                "a call site's value is not a value".into(),
            )),
        }
    }
}

/// A DWARF value as the register-sized word that holds it.
fn word(value: Value) -> u64 {
    match value {
        Value::Generic(value) | Value::U64(value) => value,
        Value::U8(value) => u64::from(value),
        Value::U16(value) => u64::from(value),
        Value::U32(value) => u64::from(value),
        Value::I8(value) => i64::from(value).cast_unsigned(),
        Value::I16(value) => i64::from(value).cast_unsigned(),
        Value::I32(value) => i64::from(value).cast_unsigned(),
        Value::I64(value) => value.cast_unsigned(),
        Value::F32(value) => u64::from(value.to_bits()),
        Value::F64(value) => value.to_bits(),
    }
}

pub(super) fn runtime_error(error: EvaluateError) -> VariableRuntimeError {
    match error {
        EvaluateError::Unavailable(reason) => VariableRuntimeError::Unavailable(reason),
        EvaluateError::Malformed(description) => VariableRuntimeError::Malformed(description),
        EvaluateError::Fatal(description) => VariableRuntimeError::Fatal(description),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image holding `calls`, whose expressions `pool` holds.
    fn sealed(calls: &Calls, pool: &LocationsBuilder) -> crate::image::Image {
        let mut builder = crate::image::Builder::new(crate::TargetDescription::X86_64);
        let mut strings = crate::image::StringsBuilder::default();
        pool.add_to(&mut builder);
        calls::add_to(&mut builder, &mut strings, calls).expect("the calls fit");
        builder.bytes(crate::image::TableKind::Strings, strings.into_bytes());

        builder
            .seal(crate::image::Limits::default())
            .expect("the calls are valid")
    }

    /// The calls of functions `0..described.len()`, of which those
    /// `described` say what tail calls they make: `tail_calls`, from one
    /// function to another or to code outside the module.
    fn catalog(described: &[bool], tail_calls: &[(u32, Option<u32>)]) -> crate::image::Image {
        let mut functions = described
            .iter()
            .map(|&tail_calls_described| CallingFunction {
                name: None,
                frame_base: Metadata::Absent(MetadataAbsence::NotApplicable),
                tail_calls_described,
                tail_calls: Vec::new(),
            })
            .collect::<Vec<_>>();
        let sites = (0_u32..)
            .zip(tail_calls)
            .map(|(site, &(function, enters))| {
                functions[function as usize].tail_calls.push(site);
                calls::CallSite {
                    function,
                    return_address: None,
                    target: SiteTarget::Unknown,
                    enters,
                    jump: None,
                    parameters: Vec::new(),
                    malformed: None,
                }
            })
            .collect();

        sealed(&Calls { functions, sites }, &LocationsBuilder::default())
    }

    /// What a call passed is what the one parameter entry naming the place
    /// says; a call that describes one place twice could have passed either.
    #[test]
    fn a_call_passes_what_its_one_parameter_entry_says() {
        let mut pool = super::super::tests::Pool::new([]);
        let (first, second) = (pool.add(&[1]), pool.add(&[2]));
        let site = |parameters| calls::CallSite {
            function: 0,
            return_address: None,
            target: SiteTarget::Unknown,
            enters: None,
            jump: None,
            parameters,
            malformed: None,
        };

        let in_register = |register, value| SiteParameter {
            register: Some(register),
            parameter: None,
            value: Some(value),
            data_value: None,
        };

        let twice = site(vec![in_register(5, first), in_register(5, second)]);
        let one = site(vec![in_register(5, first), in_register(4, second)]);

        let functions = vec![CallingFunction {
            name: None,
            frame_base: Metadata::Absent(MetadataAbsence::NotApplicable),
            tail_calls_described: false,
            tail_calls: Vec::new(),
        }];

        let image = sealed(
            &Calls {
                functions,
                sites: vec![one, twice],
            },
            &pool.0,
        );
        let view = CallView::new(&image);
        let (one, twice) = (SiteId(0), SiteId(1));

        let passed = |site, parameter| match passed(view.site(site).unwrap(), parameter) {
            Ok(expression) => Ok(pool.get(expression).bytes().to_vec()),
            Err(VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(
                EntryValueUnavailableReason::NoParameter,
            ))) => Err("not passed"),
            Err(VariableRuntimeError::Malformed(_)) => Err("malformed"),
            Err(VariableRuntimeError::Unavailable(_) | VariableRuntimeError::Fatal(_)) => {
                panic!("refused for another reason")
            }
        };

        assert_eq!(
            passed(one, EntryParameter::Register(DwarfRegister::new(4))),
            Ok(vec![2])
        );
        assert_eq!(
            passed(one, EntryParameter::Register(DwarfRegister::new(1))),
            Err("not passed")
        );
        assert_eq!(
            passed(one, EntryParameter::Referent(DwarfRegister::new(4))),
            Err("not passed")
        );
        assert_eq!(
            passed(twice, EntryParameter::Register(DwarfRegister::new(5))),
            Err("malformed")
        );
    }

    /// A frame's function was entered by the call its caller made, or by
    /// the one chain of tail calls from that call's target that reaches it.
    /// Any doubt about which chain ran refuses.
    #[test]
    fn only_one_possible_chain_of_tail_calls_is_followed() {
        use EntryValueUnavailableReason::{TailCalls, TargetMismatch};

        let all = [true; 4];
        let cases: Vec<(&str, crate::image::Image, u32, Result<Vec<u32>, _>)> = vec![
            ("entered directly", catalog(&all, &[]), 0, Ok(vec![])),
            (
                "tail calls that lead elsewhere",
                catalog(&all, &[(0, Some(1)), (1, Some(1))]),
                0,
                Ok(vec![]),
            ),
            (
                "a chain",
                catalog(&all, &[(0, Some(1)), (1, Some(2)), (0, Some(3))]),
                2,
                Ok(vec![0, 1]),
            ),
            (
                "two chains",
                catalog(
                    &all,
                    &[(0, Some(1)), (0, Some(2)), (1, Some(3)), (2, Some(3))],
                ),
                3,
                Err(TailCalls),
            ),
            (
                "a cycle through the frame's function",
                catalog(&all, &[(0, Some(1)), (1, Some(0))]),
                0,
                Err(TailCalls),
            ),
            (
                "a cycle on the way",
                catalog(&all, &[(0, Some(1)), (1, Some(0)), (1, Some(2))]),
                2,
                Err(TailCalls),
            ),
            (
                "a tail call out of the module",
                catalog(&all, &[(0, Some(1)), (1, None)]),
                1,
                Err(TailCalls),
            ),
            (
                "a function silent about its tail calls",
                catalog(&[true, false, true, true], &[(0, Some(1))]),
                1,
                Err(TailCalls),
            ),
            (
                "an unreachable function",
                catalog(&all, &[(0, Some(1))]),
                2,
                Err(TargetMismatch),
            ),
        ];

        for (case, catalog, to, expected) in cases {
            let path = tail_path(CallView::new(&catalog), 0, to)
                .map(|path| path.into_iter().map(|site| site.0).collect())
                .map_err(|error| match error {
                    VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(
                        reason,
                    )) => reason,
                    _ => panic!("{case}: refused for no reason of entry values"),
                });
            assert_eq!(path, expected, "{case}");
        }
    }
}
