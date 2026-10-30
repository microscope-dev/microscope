//! DWARF variable inspection.
//!
//! Loading builds an immutable catalog of data objects, their scopes, and a
//! normalized type graph; inspection then evaluates locations against a
//! stopped thread through [`VariableRuntime`](crate::debug_info::VariableRuntime).
//!
//! - [`types`]: normalizing type DIEs; [`variant`]: discriminated variants.
//! - [`globals`]: the global catalog; [`die`] and [`location`]: reading DIE
//!   attributes and location descriptions.
//! - [`shape`]: how a type's bytes decode; [`codec`]: the byte-level decoding.
//! - [`evaluate`]: DWARF expression evaluation; [`inspect`]: value paths and
//!   materializing values, children, and dereferences.
//! - [`pieces`]: the storage location pieces describe; [`storage`]: reading
//!   and selecting storage of every form.
//! - [`call_sites`]: the calls that recover parameters' entry values.

use crate::debug_info::dwarf::DwarfSet;
use crate::image::lines::Files;
use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};

use foldhash::{HashMap, HashMapExt};
use gimli::RunTimeEndian;

use crate::debug_info::{
    Accessed, CallSite, CallSiteId, EntryParameter, Located, ObjectKey, ObjectStorage, PlannedStep,
    Step, TextLocation, VariableContext, VariableInfo, VariableRuntime, VariableRuntimeError,
};

use crate::inspection::InspectionBudget;
use crate::{
    AddressRange, ByteOrder, CodeInstanceId, DereferenceReference, DereferencedValue, Error,
    GlobalVariableId, ImageAddress, InspectedValue, ModuleImageId, Result, SourceFileId,
    SourceLanguage, SourceLocation, TargetDescription, TypeId, TypeInfo, TypeNode, TypeReference,
    ValueChildPage, ValueChildrenReference, Variable, VariableKind, VariableMalformedKind,
    VariableMalformedReason, VariableQuery, VariableState,
};

use super::{
    DieKey, DieWalk, DwarfError, Reader, UnitCatalog, Units, die_code_ranges, die_reference,
    is_type_unit, unit_dwarf,
};

use crate::image::variables::{
    Capture, ConstantValue, DataObject, Function, Metadata, MetadataAbsence, Object, ObjectId,
    ValueDescription, VariableView,
};

use die::{
    copy_name, data_object_scope_ranges, debug_info_offset, declaration_with_origins,
    is_type_scope, origin_chain, strict_flag, string_with_origins, type_with_origins,
    variable_order_key,
};

use evaluate::FrameBaseCache;
use globals::load_globals;
pub(in crate::debug_info) use identity::{produced_language, source_language};

pub(in crate::debug_info) use inspect::{PathStep, array_byte_offset};
use inspect::{data_object, evaluate_error_state, inspected_value};
use location::{
    LocationListId, LocationTables, LocationsBuilder, copy_data_object_value,
    copy_optional_location, load_evaluation_units,
};

use types::{
    DynamicAggregateChild, TypeArenaBuilder, TypeEntry, TypeMetadataEntry, TypeResolution,
};

mod call_sites;
mod codec;
pub(super) mod coroutine;

mod dedup;
mod die;
mod evaluate;
mod generic;
mod globals;
mod identity;
mod in_order;
mod inspect;
mod layered;
mod location;
mod merge;
mod pieces;
mod returns;
mod runtime_array;
mod shape;
mod storage;
mod text;
mod types;
mod variant;
mod visibility;

const MAX_SCALAR_BYTES: u64 = 16;
const MAX_EVALUATION_ITERATIONS: u32 = 10_000;
const MAX_EVALUATION_MEMORY_BYTES: usize = 1_024;
const MAX_LOCATION_PIECES: usize = 64;
const MAX_TYPE_RESOLUTION_DEPTH: usize = 256;
const MAX_RECORD_CHILDREN: usize = 4_096;
const MAX_VARIANT_METADATA: usize = 4_096;
const MAX_AGGREGATE_DEPTH: usize = 64;

/// Types past this many are the work limit's type, so that every type,
/// with the few made once each, has a `u32` id.
const MAX_BUILT_TYPES: usize = u32::MAX as usize - 4;

/// The row `index` numbers, in a table no larger than a module's entries.
fn row(index: usize) -> u32 {
    let row = u32::try_from(index);
    debug_assert!(row.is_ok(), "row {index} is past a module's entries");
    row.unwrap_or(u32::MAX)
}

const fn malformed_reason(
    kind: VariableMalformedKind,
    description: Arc<str>,
) -> VariableMalformedReason {
    VariableMalformedReason { kind, description }
}

/// What kind of Rust scope a scope is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RustScope {
    /// The outermost scope of the body of an `async fn`, where rustc
    /// declares the fields of its future that captured its arguments, which
    /// the body moves into variables of its own.
    AsyncCaptures,
    Other,
}

impl RustScope {
    /// The scope of a subprogram or inline instance of a Rust unit.
    fn routine<'data>(
        dwarf: &DwarfSet<'data>,
        units: &Units<'data>,
        unit_index: usize,
        unit: &gimli::Unit<Reader<'data>>,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    ) -> Self {
        if async_fn_body(dwarf, units, unit_index, unit, entry) {
            Self::AsyncCaptures
        } else {
            Self::Other
        }
    }
}

#[derive(Clone)]
struct Scope {
    ranges: Arc<[AddressRange<ImageAddress>]>,
    lexical_depth: u32,
    frame_base: Metadata<LocationListId>,

    /// True within subprograms and inlined subroutines, whose formal
    /// parameters some producers, such as Zig, nest in lexical blocks.
    routine: bool,
    function: usize,

    /// The innermost containing inline instance, or `None` when the scope
    /// belongs directly to the physical frame.
    instance: Option<CodeInstanceId>,

    /// The code instance whose code the scope is in: `instance`, or the
    /// physical frame's.
    code_instance: Option<CodeInstanceId>,

    /// The file declaring a Go function, and so its variables, whose
    /// declarations give only their line.
    go_file: Option<SourceFileId>,

    /// For a Rust scope, whose compiler names variables of its own, what
    /// kind it is.
    rust: Option<RustScope>,

    /// The line of the await whose future this scope, or one enclosing it,
    /// declares as `__awaitee`: rustc binds the value it gives as `result`
    /// in a block within.
    awaitee: Option<crate::LineNumber>,
    malformed: Option<Arc<str>>,

    /// Whether the scope is in a subprogram's definition, abstract or
    /// concrete, rather than in a declaration inside a type.
    defined: bool,
}

/// Go's `DW_AT_go_closure_offset`: where in a closure's context a captured
/// variable is.
const DW_AT_GO_CLOSURE_OFFSET: gimli::DwAt = gimli::DwAt(0x2907);

pub(super) struct DwarfVariableInfo {
    /// The image's types, whose tables hold everything else it reads.
    types: Arc<crate::image::types::TypeTable>,
    target: TargetDescription,
    endian: RunTimeEndian,
}

pub(super) struct LoadedVariables {
    pub types: Arc<[crate::TypeNode]>,

    /// The data objects and the functions whose frames show them.
    pub variables: crate::image::variables::Variables,

    /// The calls the functions make.
    pub calls: crate::image::calls::Calls,

    /// What reading values of some types takes beyond their layout.
    pub type_facts: crate::image::type_facts::TypeFacts,

    /// Every location the variables and types name.
    pub locations: LocationsBuilder,

    /// Integer constants the units declare at their top level, by name,
    /// and Rust trait objects' vtables, by address, with the concrete type
    /// each is for.
    pub declarations: crate::image::declarations::Declarations,

    /// The coroutine each code instance that runs one is passed, as the
    /// body of an `async fn` is passed its future.
    pub coroutine_bodies: BTreeMap<CodeInstanceId, TypeId>,

    /// Every type named as a coroutine, with what it is or why its layout
    /// cannot be read as one.
    pub coroutines: BTreeMap<TypeId, std::result::Result<crate::CoroutineInfo, Arc<str>>>,

    /// The generic type arguments of each code instance of a generic
    /// function, by their parameters' names.
    pub function_generics: BTreeMap<CodeInstanceId, crate::FunctionGenerics>,
}

/// The producer a unit names.
fn unit_producer(
    dwarf: &DwarfSet<'_>,
    unit: &gimli::Unit<Reader<'_>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    let mut entries = unit.entries();
    let Some(root) = entries.next_dfs()? else {
        return Ok(None);
    };

    super::string_attribute(dwarf, unit, root, gimli::DW_AT_producer)
}

/// What a function returns by the System V convention: one value of its
/// type, named for it, unless it returns nothing.
fn system_v_returns<'data>(
    dwarf: &DwarfSet<'data>,
    units: &Units<'data>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    language: SourceLanguage,
    types: &mut TypeArenaBuilder<'_, 'data>,
) -> Option<returns::ReturnConvention> {
    let chain = origin_chain(units, unit_index, entry).unwrap_or_default();
    let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
    let type_value = type_value?;
    let name = string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name)
        .ok()
        .flatten()
        .unwrap_or_else(|| Arc::from("returned"));

    // Named for the function, as C's are.
    let name = super::own_name(language, &name).map_or(name, Arc::from);
    let rewritten = std::iter::once(entry)
        .chain(chain.iter().map(|(_, origin)| origin))
        .any(|entry| {
            entry.attr_value(gimli::DW_AT_calling_convention)
                == Some(gimli::AttributeValue::CallingConvention(
                    gimli::DW_CC_nocall,
                ))
        });
    Some(returns::ReturnConvention::SystemV(Box::new(
        returns::SystemV {
            name,
            ty: types.variable_type(type_unit, Some(type_value)),
            language,
            rewritten,
        },
    )))
}

/// The file declaring a function or inlined call's function, if known.
fn declared_file<'data>(
    dwarf: &DwarfSet<'data>,
    units: &Units<'data>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    files: &mut Files,
) -> Option<SourceFileId> {
    let chain = origin_chain(units, unit_index, entry).ok()?;
    declaration_with_origins(dwarf, units, &units[unit_index], entry, &chain, files)
        .ok()
        .flatten()
        .map(|declaration| declaration.file)
}

/// Each Go lexical block's code fused with its nested blocks' and inlined
/// calls', as Delve reads them: Go may place a nested block's code
/// outside its parent's ranges. Only blocks the fusion changes are listed.
fn fused_block_ranges(
    dwarf: &DwarfSet<'_>,
    unit: &gimli::Unit<Reader<'_>>,
    code: &super::CodeRanges,
) -> std::result::Result<HashMap<usize, Vec<AddressRange<ImageAddress>>>, DwarfError> {
    struct Open {
        depth: isize,
        offset: usize,
        block: bool,

        /// Its own ranges, fused; a block with none takes its parent's.
        own: Vec<AddressRange<ImageAddress>>,
        ranges: Vec<AddressRange<ImageAddress>>,
    }

    fn close(
        open: Open,
        stack: &mut [Open],
        fused: &mut HashMap<usize, Vec<AddressRange<ImageAddress>>>,
    ) {
        let ranges = visibility::fused(open.ranges);
        if open.block && !open.own.is_empty() && ranges != open.own {
            fused.insert(open.offset, ranges.clone());
        }

        if let Some(parent) = stack.last_mut() {
            parent.ranges.extend(ranges);
        }
    }

    let mut fused = HashMap::new();
    let mut stack = Vec::<Open>::new();
    let mut entries = unit.entries();

    while let Some(entry) = entries.next_dfs()? {
        let depth = entry.depth();
        while let Some(open) = stack.pop_if(|open| open.depth >= depth) {
            close(open, &mut stack, &mut fused);
        }

        let ranges = match entry.tag() {
            gimli::DW_TAG_subprogram
            | gimli::DW_TAG_lexical_block
            | gimli::DW_TAG_inlined_subroutine => die_code_ranges(dwarf, unit, entry, code)?,
            _ => continue,
        };

        stack.push(Open {
            depth,
            offset: entry.offset().0,
            block: entry.tag() == gimli::DW_TAG_lexical_block,
            own: visibility::fused(ranges.clone()),
            ranges,
        });
    }

    while let Some(open) = stack.pop() {
        close(open, &mut stack, &mut fused);
    }

    Ok(fused)
}

/// Whether a load reads the locals and parameters of functions, or leaves
/// them out, and the types only they name, so that the rest fits its
/// budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Locals {
    Read,
    LeftOut,
}

/// What the image knows of its code: the instance each function DIE
/// becomes.
#[derive(Clone, Copy)]
pub(super) struct CodeMetadata<'a> {
    pub(super) instance_ids: &'a super::DieMap<CodeInstanceId>,
}

#[expect(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "one depth-first DIE walk must keep scope, variable, and parameter state synchronized"
)]
pub(super) fn load_variable_info<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
    target: TargetDescription,
    image_id: ModuleImageId,
    code: CodeMetadata<'_>,
    files: &mut Files,
    locals: Locals,
    budget: super::budget::Meter,
    walk_ahead: usize,
) -> std::result::Result<LoadedVariables, DwarfError> {
    let units = &catalog.units;
    let instance_ids = code.instance_ids;
    let mut walked = Walked::default();
    let phase = crate::span!("variables.evaluation_units");

    // Every location is pooled once, in the tables the image will hold.
    let pool = std::sync::Mutex::new(LocationsBuilder::default());
    load_evaluation_units(
        units,
        &mut pool.lock().unwrap_or_else(PoisonError::into_inner),
    )?;
    let unit_count = u32::try_from(units.len()).map_err(|_| DwarfError::TooMany("units"))?;
    let languages = (0..unit_count)
        .map(|unit| {
            pool.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .tables()
                .unit_language(unit)
        })
        .collect::<Vec<_>>();
    drop(phase);
    let phase = crate::span!("variables.type_arena");
    let die_buffers = types::DieBuffers::default();
    let mut types = TypeArenaBuilder::new(
        dwarf,
        units,
        &catalog.type_signatures,
        image_id,
        target.byte_order,
        &pool,
        &die_buffers,
        budget,
    );
    drop(phase);
    let phase = crate::span!("variables.globals");
    let globals = load_globals(
        dwarf,
        units,
        &mut walked.objects,
        &mut walked.order,
        files,
        &mut types,
        &pool,
    )?;
    drop(phase);

    types.freeze();
    let phase = crate::span!("variables.main_walk");

    // The file of each declaration, which the walk would otherwise spell
    // again for each variable.
    let mut declared_files = die::DeclaredFiles::default();
    let cx = WalkContext {
        dwarf,
        catalog,
        target,
        languages: &languages,
        instance_ids,
        locals,
    };

    if !merge::walk_units_apart(&cx, &mut walked, &mut types, files, &pool, walk_ahead) {
        crate::count!("units_walked_in_order", units.len());
        for (unit_index, unit) in units.iter().enumerate() {
            if is_type_unit(unit) {
                continue;
            }

            walk_unit(
                &cx,
                unit_index,
                &mut walked,
                &mut types,
                files,
                &mut declared_files,
            )?;
        }
    }

    let Walked {
        mut objects,
        mut functions,
        calls,
        procedures,
        mut vtables,
        go_function_entries,
        mut unnamed_parameters,
        mut abstract_bodies,
        mut function_generics,
        order: _,
        names: _,
    } = walked;

    for function in &mut functions {
        function
            .objects
            .sort_by_key(|index| variable_order_key(&objects[*index as usize]));
    }

    drop(phase);
    let phase = crate::span!("variables.constants_and_members");
    let constants = types.named_constants();
    types.populate_go_named_constants();
    types.populate_record_member_declarations(files);
    drop(phase);
    let phase = crate::span!("variables.finalize_types");
    types.finalize_type_graph();
    drop(phase);

    // Types and symbolic names past the budget stand in for what could not
    // be built, which fails the load once nothing more is built.
    types.budget.check()?;
    crate::count!("budget_spent", types.budget.spent());
    let mut types = types.finish();
    let phase = crate::span!("types.deduplicate");

    if let Some(remap) = types.deduplicate() {
        remap_catalog(&remap, &mut objects, &mut functions);
        for (_, ty) in &mut vtables {
            *ty = remap.id(*ty);
        }

        for (_, ty, _) in &mut unnamed_parameters {
            *ty = remap.id(*ty);
        }

        for (_, ty) in &mut abstract_bodies {
            *ty = remap.id(*ty);
        }

        for generics in function_generics.values_mut() {
            *generics = generics
                .iter()
                .map(|(name, ty)| (Arc::clone(name), remap.id(*ty)))
                .collect();
        }
    }

    drop(phase);
    let _phase = crate::span!("variables.catalog");
    let finalized_types = std::mem::take(&mut types.entries)
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let description = match entry {
                TypeEntry::Resolved(info) => {
                    debug_assert_eq!(
                        info.reference.id.index(),
                        index,
                        "the type at {index} names another position as its own"
                    );
                    return TypeNode::Resolved(info);
                }
                TypeEntry::Malformed(description) => description,
                TypeEntry::Building => "type graph did not finish building".into(),
            };

            TypeNode::Malformed {
                reference: TypeReference {
                    image: image_id,
                    id: TypeId::new(row(index)),
                },
                description,
            }
        })
        .collect::<Arc<[_]>>();
    let coroutines = crate::debug_info::coroutines::normalize(&finalized_types);
    let mut coroutine_bodies = BTreeMap::new();

    for (instance, ty, object) in unnamed_parameters {
        if let Some(coroutine) =
            crate::debug_info::coroutines::pinned_coroutine(&finalized_types, ty)
        {
            coroutine_bodies.insert(instance, coroutine);

            // The body's future is what the parameter points to.
            if let Some(object) = object {
                let future = &mut objects[object];
                future.name = "$future".into();
                future.type_info = TypeResolution::Resolved(coroutine);
                future.escaped = Some(ty);
                future.coroutine = Some(coroutine);
            }
        }
    }

    for (instance, ty) in abstract_bodies {
        if let Some(coroutine) =
            crate::debug_info::coroutines::pinned_coroutine(&finalized_types, ty)
        {
            coroutine_bodies.entry(instance).or_insert(coroutine);
        }
    }

    Ok(LoadedVariables {
        coroutines,
        coroutine_bodies,
        function_generics,
        types: finalized_types,
        calls: calls.finish(),
        type_facts: crate::image::type_facts::TypeFacts {
            dictionary_indices: types.go_dict_indices.into_iter().collect(),
            passed_by_value: types.passed_by_value.into_iter().collect(),
            complex_parts: types
                .complex_parts
                .into_iter()
                .map(|((name, size), ty)| (name, size, ty))
                .collect(),
            dynamic_layouts: types
                .dynamic_record_layouts
                .into_iter()
                .filter_map(|(key, expression)| {
                    Some((key.aggregate, layout_child(key.child)?, expression))
                })
                .collect(),
        },
        variables: crate::image::variables::Variables {
            objects,
            functions,
            globals,
            go_entries: go_function_entries,
            procedures,
        },
        locations: pool.into_inner().unwrap_or_else(PoisonError::into_inner),
        declarations: crate::image::declarations::Declarations {
            constants: constants.into_iter().collect(),
            vtables: vtables
                .into_iter()
                .map(|(address, id)| (ImageAddress::new(address), id))
                .collect(),
            producers: Vec::new(),
        },
    })
}

/// A child whose place an expression computes, as the image names it;
/// `None` for one whose index no image row can hold.
fn layout_child(child: DynamicAggregateChild) -> Option<crate::image::type_facts::LayoutChild> {
    use crate::image::type_facts::LayoutChild;
    let index = |index: usize| u32::try_from(index).ok();
    Some(match child {
        DynamicAggregateChild::Member(member) => LayoutChild::Member(index(member)?),
        DynamicAggregateChild::Base(base) => LayoutChild::Base(index(base)?),
        DynamicAggregateChild::Discriminant => LayoutChild::Discriminant,
        DynamicAggregateChild::VariantMember { variant, member } => LayoutChild::VariantMember {
            variant: index(variant)?,
            member: index(member)?,
        },
        DynamicAggregateChild::Bound { dimension, part } => LayoutChild::Bound {
            dimension: index(dimension)?,
            part,
        },
        DynamicAggregateChild::DataLocation => LayoutChild::DataLocation,
        DynamicAggregateChild::Allocated => LayoutChild::Allocated,
        DynamicAggregateChild::Associated => LayoutChild::Associated,
    })
}

/// What the main walk reads every unit with.
struct WalkContext<'w, 'data> {
    dwarf: &'w DwarfSet<'data>,
    catalog: &'w UnitCatalog<'data>,
    target: TargetDescription,
    languages: &'w [Option<gimli::DwLang>],
    instance_ids: &'w super::DieMap<CodeInstanceId>,
    locals: Locals,
}

/// What the main walk records, in the order it reads the DIEs.
#[derive(Default)]
struct Walked {
    objects: Vec<DataObject>,
    functions: Vec<Function>,
    calls: call_sites::CallSiteBuilder,
    procedures: Vec<(u64, Metadata<LocationListId>)>,
    vtables: Vec<(u64, TypeId)>,
    go_function_entries: Vec<(ImageAddress, u32)>,

    /// The instances passed an unnamed parameter, as an `async fn`'s body
    /// is passed its future, its type, and its data object, when locals
    /// are read.
    unnamed_parameters: Vec<(CodeInstanceId, TypeId, Option<usize>)>,
    abstract_bodies: Vec<(CodeInstanceId, TypeId)>,
    function_generics: BTreeMap<CodeInstanceId, crate::FunctionGenerics>,
    order: u64,
    names: Names,
}

/// Data objects' names, each held once: a large program names a few
/// hundred thousand variables millions of times.
#[derive(Default)]
struct Names(foldhash::HashSet<Arc<str>>);

impl Names {
    fn intern(&mut self, name: Arc<str>) -> Arc<str> {
        if let Some(held) = self.0.get(&*name) {
            return Arc::clone(held);
        }

        self.0.insert(Arc::clone(&name));
        name
    }
}

/// Records what one unit's DIEs describe: its functions, scopes, call
/// sites, and data objects, and the types they reach.
#[expect(
    clippy::too_many_lines,
    reason = "one depth-first DIE walk must keep scope, variable, and parameter state synchronized"
)]
fn walk_unit<'data>(
    cx: &WalkContext<'_, 'data>,
    unit_index: usize,
    walked: &mut Walked,
    types: &mut TypeArenaBuilder<'_, 'data>,
    files: &mut Files,
    declared_files: &mut die::DeclaredFiles,
) -> std::result::Result<(), DwarfError> {
    let WalkContext {
        dwarf,
        catalog,
        target,
        languages,
        instance_ids,
        locals,
    } = *cx;
    let Walked {
        objects,
        functions,
        calls,
        procedures,
        vtables,
        go_function_entries,
        unnamed_parameters,
        abstract_bodies,
        function_generics,
        order,
        names,
    } = walked;
    let units = &catalog.units;
    let unit = &units[unit_index];
    calls.begin_unit();
    let go = languages[unit_index] == Some(gimli::DW_LANG_Go);
    let fortran = source_language(languages[unit_index], None) == SourceLanguage::Fortran;
    let d = languages[unit_index] == Some(gimli::DW_LANG_D);
    let nim = types.produced_language(unit_index) == Some(SourceLanguage::Nim);
    let ada = source_language(languages[unit_index], None) == SourceLanguage::Ada;
    let rust = languages[unit_index] == Some(gimli::DW_LANG_Rust);

    // Go names the register ABI its x86-64 code calls with among the
    // flags of each unit's producer, as `go1.27.1; -N -l regabi`.
    let go_registers = go
        && target.architecture == crate::Architecture::X86_64
        && unit_producer(dwarf, unit)?.is_some_and(|producer| {
            producer
                .split_once(';')
                .is_some_and(|(_, flags)| flags.split_whitespace().any(|flag| flag == "regabi"))
        });

    // Other languages' x86-64 code returns as the System V convention
    // says, or, for the languages that leave theirs unspecified, as it
    // for scalars.
    let language = source_language(languages[unit_index], types.produced_language(unit_index));
    let system_v = target.architecture == crate::Architecture::X86_64
        && matches!(
            language,
            SourceLanguage::C
                | SourceLanguage::Cpp
                | SourceLanguage::Rust
                | SourceLanguage::Zig
                | SourceLanguage::Nim
                | SourceLanguage::Odin
                | SourceLanguage::Fortran
                | SourceLanguage::D
                | SourceLanguage::Ada
        );
    let fused_blocks = if go {
        fused_block_ranges(dwarf, unit, &catalog.code)?
    } else {
        HashMap::new()
    };

    let mut walk = DieWalk::new(unit)?;
    let mut scopes = Vec::<Option<Scope>>::new();

    // The concrete instances of abstract functions open at this point
    // of the walk, innermost last.
    let mut concrete = Vec::<ConcreteRoutine>::new();

    while let Some(die) = walk.next()? {
        let depth = usize::try_from(die.depth).map_err(|_| DwarfError::InvalidEntryDepth)?;
        scopes.truncate(depth);
        while let Some(routine) = concrete.pop_if(|routine| routine.depth >= depth) {
            add_abstract_only_variables(
                dwarf,
                units,
                &routine,
                &mut AbstractTargets {
                    objects,
                    functions,
                    order,
                    names,
                    types,
                    files,
                    bodies: abstract_bodies,
                    locals,
                },
            )?;
        }

        if !main_walk_reads(die.tag, depth) {
            // Nothing here reads such a DIE's attributes; all it has is
            // its scope for the DIEs within it: its parent's, or none
            // within a type. Most DIEs are types' members, parameters,
            // and arguments, which are not decoded.
            let scope = if is_type_scope(die.tag) {
                None
            } else {
                scopes.last().and_then(Clone::clone)
            };

            scopes.push(scope);
            continue;
        }

        let entry = walk.decode()?;
        let parent = scopes.last().and_then(Clone::clone);

        // A concrete DIE standing for an abstract one covers it.
        if let Some(routine) = concrete.last_mut()
            && matches!(
                entry.tag(),
                gimli::DW_TAG_variable | gimli::DW_TAG_formal_parameter
            )
            && let Some(origin) = die_reference(
                entry.attr_value(gimli::DW_AT_abstract_origin),
                unit_index,
                units,
            )?
        {
            routine.covered.insert(origin);
        }

        let scope = match entry.tag() {
            gimli::DW_TAG_subprogram => {
                let defined = strict_flag(entry, gimli::DW_AT_declaration) == Ok(false);

                // The types a function's code uses are the program's
                // types too, though no data holds them: a view may name
                // the type only an inlined function returns.
                if defined {
                    types.reach(unit_index, entry.attr_value(gimli::DW_AT_type));
                }

                let ranges =
                    die_code_ranges(dwarf, unit, entry, &catalog.code).map(Arc::<[_]>::from)?;
                let key = DieKey {
                    unit: unit_index,
                    offset: entry.offset().0,
                };

                if rust
                    && defined
                    && !ranges.is_empty()
                    && let Some(instance) = instance_ids.get(&key)
                {
                    let generics = types.function_generics(key);
                    if !generics.is_empty() {
                        function_generics.insert(*instance, Arc::from(generics));
                    }
                }

                let function = functions.len();

                if go && defined {
                    // A func value holds the address its code begins at.
                    let entry_address = entry
                        .attr_value(gimli::DW_AT_low_pc)
                        .map(|value| unit_dwarf(dwarf, unit).attr_address(unit, value))
                        .transpose()?
                        .flatten()
                        .map(ImageAddress::new)
                        .filter(|address| ranges.iter().any(|range| range.contains(*address)));
                    if let Some(address) = entry_address {
                        go_function_entries.push((address, row(function)));
                    }
                }

                functions.push(Function {
                    ranges: Arc::clone(&ranges),
                    objects: Vec::new(),
                    // An optimized closure's out-of-line code names
                    // its abstract origin.
                    name: if go {
                        origin_chain(units, unit_index, entry)
                            .ok()
                            .and_then(|chain| {
                                string_with_origins(
                                    dwarf,
                                    units,
                                    unit,
                                    entry,
                                    &chain,
                                    gimli::DW_AT_name,
                                )
                                .ok()
                            })
                            .flatten()
                    } else {
                        None
                    },
                    captures: Ok(Vec::new()),
                    returns: if go_registers && defined {
                        Some(returns::ReturnConvention::GoRegisters)
                    } else if system_v && defined {
                        system_v_returns(dwarf, units, unit_index, unit, entry, language, types)
                    } else {
                        None
                    },
                });
                let frame_base = copy_optional_location(
                    dwarf,
                    &mut types.pool.lock().unwrap_or_else(PoisonError::into_inner),
                    unit_index,
                    unit,
                    entry.attr_value(gimli::DW_AT_frame_base),
                    MetadataAbsence::NoFrameBase,
                );
                calls.function(dwarf, units, unit_index, entry, &ranges, frame_base.clone());

                Some(Scope {
                    ranges,
                    lexical_depth: 0,
                    frame_base,
                    routine: true,
                    function,
                    instance: None,
                    code_instance: instance_ids
                        .get(&DieKey {
                            unit: unit_index,
                            offset: entry.offset().0,
                        })
                        .copied(),
                    go_file: if go {
                        declared_file(dwarf, units, unit_index, entry, files)
                    } else {
                        None
                    },
                    malformed: None,
                    defined,
                    rust: rust.then(|| RustScope::routine(dwarf, units, unit_index, unit, entry)),
                    awaitee: None,
                })
            }
            gimli::DW_TAG_lexical_block => parent.as_ref().map(|parent| {
                // A Go block's code includes its nested blocks'.
                let own = fused_blocks.get(&entry.offset().0).map_or_else(
                    || die_code_ranges(dwarf, unit, entry, &catalog.code),
                    |fused| Ok(fused.clone()),
                );
                let (ranges, malformed) = match own.map(Arc::<[_]>::from) {
                    Ok(ranges) if !ranges.is_empty() => (ranges, None),
                    Ok(_) => (Arc::clone(&parent.ranges), None),
                    Err(error) => (Arc::clone(&parent.ranges), Some(error.to_string().into())),
                };

                Scope {
                    ranges,
                    lexical_depth: parent.lexical_depth.saturating_add(1),
                    frame_base: parent.frame_base.clone(),
                    routine: parent.routine,
                    function: parent.function,
                    instance: parent.instance,
                    code_instance: parent.code_instance,
                    go_file: parent.go_file,
                    rust: parent.rust.map(|_| RustScope::Other),
                    awaitee: if parent.rust.is_some() {
                        origin_awaitee(dwarf, units, unit_index, entry).or(parent.awaitee)
                    } else {
                        None
                    },
                    malformed: malformed.or_else(|| parent.malformed.clone()),
                    defined: parent.defined,
                }
            }),
            // An inline instance keeps the caller's frame base and function
            // but only its own code ranges; one without usable ranges gets
            // none, unlike a lexical block, so its locals never match.
            gimli::DW_TAG_inlined_subroutine => parent.as_ref().map(|parent| {
                let instance = instance_ids
                    .get(&DieKey {
                        unit: unit_index,
                        offset: entry.offset().0,
                    })
                    .copied();
                let (ranges, malformed) = match die_code_ranges(dwarf, unit, entry, &catalog.code)
                    .map(Arc::<[_]>::from)
                {
                    Ok(ranges) if ranges.is_empty() => (
                        Vec::new().into(),
                        Some(Arc::from("inlined subroutine has no address ranges")),
                    ),
                    // A ranged instance must be identified so lookups can
                    // scope to it; without an identity its contents could
                    // only be misattributed.
                    Ok(_) if instance.is_none() => (
                        Vec::new().into(),
                        Some(Arc::from("inlined subroutine has no code instance")),
                    ),
                    Ok(ranges) => (ranges, None),
                    Err(error) => (Vec::new().into(), Some(error.to_string().into())),
                };

                Scope {
                    ranges,
                    lexical_depth: parent.lexical_depth.saturating_add(1),
                    frame_base: parent.frame_base.clone(),
                    routine: true,
                    function: parent.function,
                    instance,
                    code_instance: instance,
                    go_file: if go {
                        declared_file(dwarf, units, unit_index, entry, files)
                    } else {
                        None
                    },
                    rust: parent
                        .rust
                        .map(|_| RustScope::routine(dwarf, units, unit_index, unit, entry)),
                    awaitee: None,
                    malformed: malformed.or_else(|| parent.malformed.clone()),
                    defined: parent.defined,
                }
            }),
            tag if is_type_scope(tag) => None,
            _ => parent.clone(),
        };

        // An empty extent is deliberate containment (a rangeless inline
        // instance) and must stay empty through every descendant scope;
        // only a nested subprogram starts an independent extent.
        let scope = if entry.tag() != gimli::DW_TAG_subprogram
            && parent
                .as_ref()
                .is_some_and(|parent| parent.ranges.is_empty())
        {
            scope.map(|mut scope| {
                scope.ranges = Vec::new().into();
                scope
            })
        } else {
            scope
        };

        if matches!(
            entry.tag(),
            gimli::DW_TAG_subprogram
                | gimli::DW_TAG_inlined_subroutine
                | gimli::DW_TAG_lexical_block
        ) && let Some(routine) = scope.as_ref().filter(|scope| !scope.ranges.is_empty())
            && let Some(origin) = die_reference(
                entry.attr_value(gimli::DW_AT_abstract_origin),
                unit_index,
                units,
            )?
        {
            concrete.push(ConcreteRoutine {
                depth,
                origin,
                scope: routine.clone(),
                covered: std::collections::HashSet::new(),
            });
        }

        match entry.tag() {
            gimli::DW_TAG_call_site | gimli::DW_TAG_GNU_call_site => {
                if let Some(parent) = parent.as_ref().filter(|parent| parent.defined) {
                    calls.site(
                        dwarf,
                        &mut types.pool.lock().unwrap_or_else(PoisonError::into_inner),
                        units,
                        unit_index,
                        entry,
                        parent.function,
                        depth,
                    );
                }
            }
            gimli::DW_TAG_call_site_parameter | gimli::DW_TAG_GNU_call_site_parameter => {
                calls.parameter(
                    dwarf,
                    &mut types.pool.lock().unwrap_or_else(PoisonError::into_inner),
                    units,
                    unit_index,
                    entry,
                    depth,
                );
            }
            gimli::DW_TAG_dwarf_procedure => {
                if let Some(offset) = debug_info_offset(units, unit_index, entry) {
                    let location = copy_optional_location(
                        dwarf,
                        &mut types.pool.lock().unwrap_or_else(PoisonError::into_inner),
                        unit_index,
                        unit,
                        entry.attr_value(gimli::DW_AT_location),
                        MetadataAbsence::NoLocation,
                    );
                    procedures.push((offset, location));
                }
            }
            _ => {}
        }

        if depth == 1
            && entry.tag() == gimli::DW_TAG_variable
            && let Some((address, ty)) = rust_vtable(dwarf, unit_index, unit, entry, types)
        {
            vtables.push((address, ty));
        }

        // A Go interface may hold a value of any type the runtime
        // describes, which no data need mention.
        if depth == 1
            && types::is_type_die_tag(entry.tag())
            && identity::go_runtime_type(entry).is_some()
        {
            types.resolve(DieKey {
                unit: unit_index,
                offset: entry.offset().0,
            });
        }

        let kind = match entry.tag() {
            gimli::DW_TAG_variable => Some(VariableKind::Local),
            gimli::DW_TAG_formal_parameter => Some(VariableKind::Parameter),
            _ => None,
        };

        if entry.tag() == gimli::DW_TAG_variable
            && let Some(scope) = parent.as_ref().filter(|scope| scope.routine)
            && let Some(offset) = entry.attr_value(DW_AT_GO_CLOSURE_OFFSET)
        {
            let (type_unit, type_value) = type_with_origins(unit_index, entry, &[]);
            let capture = match (offset.udata_value(), copy_name(dwarf, unit, entry)) {
                (Some(offset), Ok(Some(name))) => Ok(Capture {
                    name,
                    offset,
                    type_info: types.variable_type(type_unit, type_value),
                }),
                _ => Err(Arc::from("a closure's captured variable is malformed")),
            };

            // One capture it cannot describe leaves them all unknown,
            // rather than the closure seeming to capture less.
            let captures = &mut functions[scope.function].captures;

            match capture {
                Ok(capture) => {
                    if let Ok(captures) = captures {
                        captures.push(capture);
                    }
                }
                Err(reason) => *captures = Err(reason),
            }
        }

        let owning_scope = kind.and_then(|kind| {
            parent.as_ref().filter(|scope| {
                !scope.ranges.is_empty() && (kind == VariableKind::Local || scope.routine)
            })
        });

        if kind.is_some() && locals == Locals::LeftOut {
            // rustc passes the body of an `async fn` its future as an
            // unnamed parameter, which says what coroutine the body runs.
            if kind == Some(VariableKind::Parameter)
                && let Some(instance) = owning_scope.and_then(|scope| scope.code_instance)
            {
                let chain = origin_chain(units, unit_index, entry).unwrap_or_default();
                let named = matches!(
                    string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name),
                    Ok(Some(_))
                );
                let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
                if !named
                    && let TypeResolution::Resolved(ty) = types.variable_type(type_unit, type_value)
                {
                    unnamed_parameters.push((instance, ty, None));
                }
            }
        } else if let Some(kind) = kind {
            // A variable of code with no address of its own, such as an
            // abstract inline instance, names no value, but its type is
            // the program's.
            if owning_scope.is_none() && parent.as_ref().is_some_and(|scope| scope.defined) {
                types.reach(unit_index, entry.attr_value(gimli::DW_AT_type));
            }

            if let Some(scope) = owning_scope {
                // Concrete inline-instance entries reference their
                // abstract origin for descriptive metadata.
                let (chain, chain_error) = match origin_chain(units, unit_index, entry) {
                    Ok(chain) => (chain, None),
                    Err(error) => (Vec::new(), Some(Arc::from(error.to_string()))),
                };

                // Go marks its results as variable parameters.
                let kind = if kind == VariableKind::Parameter
                    && go
                    && entry
                        .attr_value(gimli::DW_AT_variable_parameter)
                        .or_else(|| {
                            chain.iter().find_map(|(_, origin)| {
                                origin.attr_value(gimli::DW_AT_variable_parameter)
                            })
                        })
                        .is_some_and(|value| {
                            matches!(value, gimli::AttributeValue::Flag(true))
                                || value.udata_value() == Some(1)
                        }) {
                    VariableKind::Result
                } else {
                    kind
                };

                let object_name = match kind {
                    VariableKind::Parameter => "parameter",
                    VariableKind::Result | VariableKind::Returned => "result",
                    VariableKind::Local
                    | VariableKind::Captured
                    | VariableKind::Module
                    | VariableKind::Unnamed => "variable",
                    VariableKind::Global => "global",
                };

                let mut nameless = false;
                let (name, name_error) =
                    match string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name)
                    {
                        Ok(Some(name)) => (name, None),
                        Ok(None) => {
                            nameless = true;
                            (
                                format!("<anonymous {object_name} at {:#x}>", entry.offset().0)
                                    .into(),
                                Some(Arc::from(format!("{object_name} has no name"))),
                            )
                        }
                        Err(error) => (
                            format!("<malformed {object_name} at {:#x}>", entry.offset().0).into(),
                            Some(error.to_string().into()),
                        ),
                    };

                *order = order.checked_add(1).ok_or_else(|| {
                    DwarfError::MalformedVariable("data-object DIE order overflows".into())
                })?;
                let declaration = die::declaration_remembering_files(
                    dwarf,
                    units,
                    unit,
                    entry,
                    &chain,
                    files,
                    declared_files,
                )
                .map(|declaration| {
                    // Go gives a variable's line alone: its file is
                    // its function's.
                    declaration.or_else(|| {
                        let line = entry
                            .attr(gimli::DW_AT_decl_line)
                            .and_then(gimli::Attribute::udata_value)
                            .and_then(crate::LineNumber::new)?;
                        Some(SourceLocation {
                            file: scope.go_file?,
                            line,
                            column: None,
                        })
                    })
                });
                let (ranges, scope_error) = data_object_scope_ranges(scope, entry);

                // A Go local exists from the line after its declaration.
                let go_declaration = match (go, kind, &declaration) {
                    (true, VariableKind::Local, Ok(Some(declared))) => {
                        Some(Box::new(visibility::GoDeclaration {
                            location: declared.clone(),
                            instance: scope.code_instance,
                        }))
                    }
                    _ => None,
                };

                let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
                let type_info = types.variable_type(type_unit, type_value);

                // Go names a variable it moved to the heap `&name`, and
                // describes the pointer to it.
                let (name, type_info, escaped) = match (go, name.strip_prefix('&')) {
                    (true, Some(variable)) => {
                        let variable = Arc::from(variable);
                        match type_info {
                            TypeResolution::Resolved(pointer) => match types.pointee(pointer) {
                                Some(target) => (
                                    variable,
                                    TypeResolution::Resolved(target),
                                    Some(pointer),
                                ),
                                None => (
                                    variable,
                                    TypeResolution::Malformed(
                                        "a variable Go moved to the heap is not described by a pointer"
                                            .into(),
                                    ),
                                    None,
                                ),
                            },
                            malformed @ TypeResolution::Malformed(_) => {
                                (variable, malformed, None)
                            }
                        }
                    }
                    _ => (name, type_info, None),
                };

                // A variable with no name that says it is the
                // compiler's own, as gfortran's temporaries do, is no
                // defect. Go and gfortran start the names of their own
                // variables with characters no identifier of their
                // languages can begin with, D with the `__` it reserves
                // for them, Nim with a trailing or doubled `_` no Nim
                // identifier has, and GNAT, which writes every Ada
                // identifier in lower case, with a capital letter, as
                // `C173b`. rustc's own are an async body's temporaries and unnamed parameters, the
                // `result` an await binds, and, in an `async fn`'s
                // body, the fields of its future that captured its
                // arguments, which the body moves into variables of
                // its own.
                let rust_unnamed =
                    scope.rust.is_some() && kind == VariableKind::Parameter && name_error.is_some();
                let declared_line = declaration
                    .as_ref()
                    .ok()
                    .and_then(|declared| declared.as_ref().map(|declared| declared.line));
                let compilers = (nameless
                    && strict_flag(entry, gimli::DW_AT_artificial) == Ok(true))
                    || (go && name.starts_with(['.', '#']))
                    || (fortran && !name.starts_with(|c: char| c.is_ascii_alphabetic()))
                    || (d && name.starts_with("__"))
                    || (nim && (name.ends_with('_') || name.contains("__")))
                    || (ada && name.contains(|c: char| c.is_ascii_uppercase()));
                let written = nim
                    .then(|| nim_name(&name, kind).map(Arc::<str>::from))
                    .flatten()
                    .filter(|_| !compilers);
                let name = written.unwrap_or(name);
                let hidden = compilers
                    || (scope.rust.is_some()
                        && (rust_temporary(&name, rust_unnamed)
                            || (scope.rust == Some(RustScope::AsyncCaptures)
                                && kind == VariableKind::Local)
                            || (&*name == "result"
                                && declared_line.is_some()
                                && declared_line == scope.awaitee)));

                if scope.rust.is_some()
                    && &*name == "__awaitee"
                    && let Some(Some(enclosing)) = scopes.last_mut()
                {
                    enclosing.awaitee = declared_line;
                }

                // rustc passes the body of an `async fn` its future
                // as an unnamed parameter.
                if kind == VariableKind::Parameter
                    && name_error.is_some()
                    && let (Some(instance), TypeResolution::Resolved(ty)) =
                        (scope.code_instance, &type_info)
                {
                    unnamed_parameters.push((instance, *ty, Some(objects.len())));
                }

                types
                    .budget
                    .charge("data objects", size_of::<DataObject>())?;
                functions[scope.function].objects.push(row(objects.len()));
                objects.push(DataObject {
                    debug_info_offset: debug_info_offset(units, unit_index, entry),
                    kind,
                    name: names.intern(name),
                    declaration: declaration.as_ref().ok().cloned().flatten(),
                    ranges,
                    go_declaration,
                    instance: scope.instance,
                    lexical_depth: scope.lexical_depth,
                    order: *order,
                    type_info,
                    escaped,
                    hidden,
                    coroutine: None,
                    value: copy_data_object_value(
                        dwarf,
                        &mut types.pool.lock().unwrap_or_else(PoisonError::into_inner),
                        unit_index,
                        unit,
                        entry,
                    ),
                    frame_base: scope.frame_base.clone(),
                    malformed: declaration
                        .err()
                        .map(|error| error.to_string().into())
                        .or(scope_error)
                        .or_else(|| scope.malformed.clone())
                        .or(chain_error)
                        .or_else(|| name_error.filter(|_| !rust_unnamed && !compilers)),
                });
            }
        }

        scopes.push(scope);
    }

    while let Some(routine) = concrete.pop() {
        add_abstract_only_variables(
            dwarf,
            units,
            &routine,
            &mut AbstractTargets {
                objects,
                functions,
                order,
                names,
                types,
                files,
                bodies: abstract_bodies,
                locals,
            },
        )?;
    }

    Ok(())
}

/// Points the catalog's types where deduplication moved them.
fn remap_catalog(remap: &dedup::Remap, objects: &mut [DataObject], functions: &mut [Function]) {
    for object in objects {
        remap.resolution(&mut object.type_info);
        for ty in [&mut object.escaped, &mut object.coroutine]
            .into_iter()
            .flatten()
        {
            *ty = remap.id(*ty);
        }
    }

    for function in functions {
        if let Ok(captures) = &mut function.captures {
            for capture in captures {
                remap.resolution(&mut capture.type_info);
            }
        }

        if let Some(returns::ReturnConvention::SystemV(convention)) = &mut function.returns {
            remap.resolution(&mut convention.ty);
        }
    }
}

/// A concrete instance of an abstract function, inlined or out of line,
/// and the abstract variables its own DIEs stand for.
struct ConcreteRoutine {
    depth: usize,
    origin: DieKey,
    scope: Scope,
    covered: std::collections::HashSet<DieKey>,
}

/// Where the variables an instance leaves out are added.
struct AbstractTargets<'a, 'data, 'units> {
    objects: &'a mut Vec<DataObject>,
    functions: &'a mut Vec<Function>,
    order: &'a mut u64,
    names: &'a mut Names,
    types: &'a mut TypeArenaBuilder<'units, 'data>,
    files: &'a mut Files,

    /// The instances whose abstract function takes an unnamed parameter,
    /// as an `async fn`'s body takes its future, and its type.
    bodies: &'a mut Vec<(CodeInstanceId, TypeId)>,
    locals: Locals,
}

/// Adds the named variables and parameters of a concrete instance's
/// abstract function, or of a concrete block's abstract block, that it has
/// no DIE for: the compiler kept no trace of them there, so they exist in
/// its code without a location, as gdb shows them, rather than not at all.
/// A nested abstract block is its own concrete block's to add; one with
/// none has no code, so its variables are nowhere in scope.
fn add_abstract_only_variables<'data>(
    dwarf: &DwarfSet<'data>,
    units: &Units<'data>,
    routine: &ConcreteRoutine,
    targets: &mut AbstractTargets<'_, 'data, '_>,
) -> std::result::Result<(), DwarfError> {
    let Some(unit) = units.get(routine.origin.unit) else {
        return Ok(());
    };

    let mut tree = unit.entries_tree(Some(gimli::UnitOffset(routine.origin.offset)))?;
    let mut children = tree.root()?.children();
    let mut awaitee = routine.scope.awaitee;

    while let Some(child) = children.next()? {
        let entry = child.entry();
        let key = DieKey {
            unit: routine.origin.unit,
            offset: entry.offset().0,
        };

        let kind = match entry.tag() {
            gimli::DW_TAG_variable => VariableKind::Local,
            gimli::DW_TAG_formal_parameter => VariableKind::Parameter,
            _ => continue,
        };

        if routine.covered.contains(&key) {
            continue;
        }

        let Some(name) = string_attribute_of(dwarf, unit, entry)? else {
            // An inlined `async fn` body may keep no DIE for the future
            // its abstract function takes, which still says what it runs.
            if kind == VariableKind::Parameter
                && routine.scope.rust.is_some()
                && let Some(instance) = routine.scope.code_instance
                && let TypeResolution::Resolved(ty) = targets
                    .types
                    .variable_type(routine.origin.unit, entry.attr_value(gimli::DW_AT_type))
            {
                targets.bodies.push((instance, ty));
            }

            continue;
        };

        if targets.locals == Locals::LeftOut {
            continue;
        }

        let declaration = declaration_with_origins(dwarf, units, unit, entry, &[], targets.files)
            .ok()
            .flatten();
        let declared_line = declaration.as_ref().map(|declared| declared.line);

        if &*name == "__awaitee" {
            awaitee = declared_line;
        }

        let type_info = targets
            .types
            .variable_type(routine.origin.unit, entry.attr_value(gimli::DW_AT_type));
        *targets.order = targets.order.checked_add(1).ok_or_else(|| {
            DwarfError::MalformedVariable("data-object DIE order overflows".into())
        })?;
        let hidden = routine.scope.rust.is_some()
            && (rust_temporary(&name, false)
                || (routine.scope.rust == Some(RustScope::AsyncCaptures)
                    && kind == VariableKind::Local)
                || (&*name == "result" && declared_line.is_some() && declared_line == awaitee));
        targets
            .types
            .budget
            .charge("data objects", size_of::<DataObject>())?;
        targets.functions[routine.scope.function]
            .objects
            .push(row(targets.objects.len()));
        targets.objects.push(DataObject {
            debug_info_offset: None,
            kind,
            name: targets.names.intern(name),
            declaration,
            ranges: Arc::clone(&routine.scope.ranges),
            go_declaration: None,
            instance: routine.scope.instance,
            lexical_depth: routine.scope.lexical_depth,
            order: *targets.order,
            type_info,
            escaped: None,
            hidden,
            coroutine: None,
            value: Metadata::Absent(MetadataAbsence::NoLocation),
            frame_base: routine.scope.frame_base.clone(),
            malformed: routine.scope.malformed.clone(),
        });
    }

    Ok(())
}

/// The line of the await whose future an abstract block's origin declares
/// as `__awaitee`, which a concrete copy of the block may leave out.
fn origin_awaitee<'data>(
    dwarf: &DwarfSet<'data>,
    units: &Units<'data>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Option<crate::LineNumber> {
    let origin = die_reference(
        entry.attr_value(gimli::DW_AT_abstract_origin),
        unit_index,
        units,
    )
    .ok()??;
    let unit = units.get(origin.unit)?;
    let mut tree = unit
        .entries_tree(Some(gimli::UnitOffset(origin.offset)))
        .ok()?;
    let mut children = tree.root().ok()?.children();

    while let Ok(Some(child)) = children.next() {
        let entry = child.entry();
        if entry.tag() == gimli::DW_TAG_variable
            && string_attribute_of(dwarf, unit, entry)
                .ok()
                .flatten()
                .is_some_and(|name| &*name == "__awaitee")
        {
            return entry
                .attr(gimli::DW_AT_decl_line)
                .and_then(gimli::Attribute::udata_value)
                .and_then(crate::LineNumber::new);
        }
    }

    None
}

/// Whether a subprogram or inline instance is the body of a Rust `async
/// fn`, by its name or its abstract origin's.
fn async_fn_body<'data>(
    dwarf: &DwarfSet<'data>,
    units: &Units<'data>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> bool {
    // Most routines name themselves, and their origins are read only for
    // those that do not; the name is only inspected, never kept.
    if let Ok(Some(name)) = super::str_attribute(dwarf, unit, entry, gimli::DW_AT_name) {
        return crate::debug_info::coroutines::is_async_fn_body(&name);
    }

    origin_chain(units, unit_index, entry)
        .ok()
        .and_then(|chain| {
            string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name)
                .ok()
                .flatten()
        })
        .is_some_and(|name| crate::debug_info::coroutines::is_async_fn_body(&name))
}

/// Whether the main walk reads anything of a DIE at `depth` with `tag`
/// but its place: what builds scopes, call sites, procedures, and data
/// objects, and the types a module declares at its top level, which a
/// vtable or a Go interface may name though no data does.
const fn main_walk_reads(tag: gimli::DwTag, depth: usize) -> bool {
    matches!(
        tag,
        gimli::DW_TAG_subprogram
            | gimli::DW_TAG_lexical_block
            | gimli::DW_TAG_inlined_subroutine
            | gimli::DW_TAG_call_site
            | gimli::DW_TAG_GNU_call_site
            | gimli::DW_TAG_call_site_parameter
            | gimli::DW_TAG_GNU_call_site_parameter
            | gimli::DW_TAG_dwarf_procedure
            | gimli::DW_TAG_variable
            | gimli::DW_TAG_formal_parameter
    ) || (depth == 1 && types::is_type_die_tag(tag))
}

/// The name a Nim programmer wrote for a variable Nim 2 named in C: a
/// local's name numbered within its procedure, as `small_1`, or a
/// parameter's numbered by its position, as `value_p0`. Nim drops an
/// underscore before a digit from the names it writes, and Nim reads `x_1`
/// and `x1` as one name, so what precedes the number is the name exactly.
/// A name Nim had to encode ends in an underscore, and stays as it is.
fn nim_name(name: &str, kind: VariableKind) -> Option<&str> {
    let (base, number) = name.rsplit_once('_')?;
    let number = match kind {
        VariableKind::Parameter => number.strip_prefix('p')?,
        VariableKind::Local => number,
        _ => return None,
    };

    let plain = base.starts_with(|first: char| first.is_ascii_alphabetic())
        && !base.ends_with('_')
        && base
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !base
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0] == b'_' && (pair[1] == b'_' || pair[1].is_ascii_digit()));
    (plain && !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(base)
}

/// Whether rustc made a variable for its own use: an async body's
/// `_task_context`, an await's `__awaitee`, numbered temporaries, and
/// the parameters it leaves unnamed.
fn rust_temporary(name: &str, unnamed_parameter: bool) -> bool {
    unnamed_parameter
        || name == "_task_context"
        || name == "__awaitee"
        || name
            .strip_prefix("__")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

fn string_attribute_of<'data>(
    dwarf: &DwarfSet<'data>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    entry
        .attr_value(gimli::DW_AT_name)
        .map(|value| unit_dwarf(dwarf, unit).attr_string(unit, value))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(|value| Arc::<str>::from(value.to_string_lossy().as_ref())))
}

/// A Rust trait object's vtable, `<C as Trait>::{vtable}`: a variable at a
/// fixed address, whose type names `C` as its containing type.
fn rust_vtable<'data>(
    dwarf: &DwarfSet<'data>,
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    types: &mut TypeArenaBuilder<'_, 'data>,
) -> Option<(u64, TypeId)> {
    let name = die::copy_name(dwarf, unit, entry).ok()??;
    if !name.ends_with("::{vtable}") {
        return None;
    }

    let gimli::AttributeValue::Exprloc(expression) = entry.attr_value(gimli::DW_AT_location)?
    else {
        return None;
    };

    let mut operations = expression.operations(unit.encoding());
    let address = match operations.next().ok()?? {
        gimli::Operation::Address { address } => address,
        gimli::Operation::AddressIndex { index } => {
            unit_dwarf(dwarf, unit).address(unit, index).ok()?
        }
        _ => return None,
    };

    if operations.next().ok()?.is_some() {
        return None;
    }

    let vtable_type = types.reference(unit_index, entry.attr_value(gimli::DW_AT_type))?;
    let vtable_entry = types.units[vtable_type.unit]
        .entry(gimli::UnitOffset(vtable_type.offset))
        .ok()?;
    let concrete = types.reference(
        vtable_type.unit,
        vtable_entry.attr_value(gimli::DW_AT_containing_type),
    )?;

    Some((address, types.resolve(concrete)))
}

impl VariableInfo for DwarfVariableInfo {
    fn inspect(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        query: &VariableQuery,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>> {
        let objects = match query {
            VariableQuery::All => {
                self.locals_read()?;
                self.function_at(address).map_or_else(Vec::new, |function| {
                    function
                        .objects()
                        .filter(|object| {
                            object.instance() == selected
                                && !object.hidden()
                                && self.visible_at(*object, address)
                        })
                        .collect()
                })
            }
            VariableQuery::Name(name) => vec![self.visible_object(address, selected, name)?],
            VariableQuery::Global(global) => {
                return Err(Error::VariableNotFound(global.variable.to_string()));
            }
        };

        let mut frame_base = FrameBaseCache::Empty;
        let resumption = self.function_at(address).and_then(|function| {
            self.resumption(
                function,
                address,
                selected,
                runtime,
                &mut frame_base,
                budget,
            )
        });
        let mut variables = Vec::new();

        for object in objects {
            if budget.consume_variable_value().is_err() {
                break;
            }

            let mut variable = self.inspect_data_object(
                object,
                Some(address),
                context,
                runtime,
                &mut frame_base,
                budget,
            )?;

            if let Some(resumption) = &resumption {
                resumption.check(object, &mut variable);
            }

            variables.push(variable);
        }

        Ok(variables)
    }

    fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<ObjectKey> {
        self.visible_object(address, selected, name)
            .map(|object| ObjectKey(object.id().0))
    }

    fn global_object(&self, id: GlobalVariableId) -> Result<ObjectKey> {
        self.catalog()
            .global(id.index())
            .map(|object| ObjectKey(object.id().0))
            .ok_or_else(|| Error::VariableNotFound(id.to_string()))
    }

    fn object_type(&self, object: ObjectKey) -> std::result::Result<TypeId, Arc<str>> {
        match self.object(object).type_info() {
            TypeResolution::Resolved(id) => Ok(id),
            TypeResolution::Malformed(description) => Err(description),
        }
    }

    fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep> {
        Self::plan_step(self, from, step)
    }

    fn locate(
        &self,
        object: ObjectKey,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed> {
        let variable = self.object(object);
        let mut frame_base = FrameBaseCache::Empty;
        let ty = match variable.type_info() {
            TypeResolution::Resolved(id) => id,
            TypeResolution::Malformed(description) => {
                return Ok(Err(VariableState::Malformed(malformed_reason(
                    VariableMalformedKind::InvalidTypeGraph,
                    description,
                ))));
            }
        };

        // A generic value has its type argument, laid out as its shape.
        let ty = match self.generic_type(ty, variable.instance(), address, runtime, budget)? {
            generic::Generic::Resolved(argument) => argument,
            generic::Generic::Unresolved(shape, _) => shape,
            generic::Generic::Plain => ty,
        };

        match self.located_data_object(variable, address, runtime, &mut frame_base, budget) {
            Ok(storage) => {
                // A running async body's variable that the await it
                // resumed from did not keep holds what another poll left.
                let resumption = address.and_then(|address| {
                    self.resumption(
                        self.function_at(address)?,
                        address,
                        variable.instance(),
                        runtime,
                        &mut frame_base,
                        budget,
                    )
                });
                let memory = match storage {
                    crate::model::ValueStorage::Memory(address) => Some(address),
                    _ => None,
                };

                if let Some(reason) =
                    resumption.and_then(|resumption| resumption.stale(variable, memory))
                {
                    return Ok(Err(VariableState::Unavailable(reason)));
                }

                Ok(Ok(Located { ty, storage }))
            }
            Err(error) => {
                evaluate_error_state(error, VariableMalformedKind::InvalidExpression).map(Err)
            }
        }
    }

    fn apply(
        &self,
        from: &Located,
        step: &PlannedStep,
        indices: &[i128],
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed> {
        let mut frame_base = FrameBaseCache::Empty;
        let storage = from.storage.clone();

        match self.apply_steps(
            storage,
            &step.steps,
            indices,
            address,
            runtime,
            &mut frame_base,
            budget,
        )? {
            Ok(storage) => {
                let Some(ty) = step.result else {
                    return Ok(Err(VariableState::Malformed(malformed_reason(
                        VariableMalformedKind::InvalidExpression,
                        "an untyped step unexpectedly reached storage".into(),
                    ))));
                };

                // A place a composite's piece holds is that piece's.
                let storage = match self.value_shape(ty) {
                    Ok(shape) => storage::narrow(storage, shape.byte_size()),
                    Err(_) => storage,
                };

                Ok(Ok(Located { ty, storage }))
            }
            Err(error) => {
                evaluate_error_state(error, VariableMalformedKind::InvalidExpression).map(Err)
            }
        }
    }

    fn load(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<crate::VariableValue, VariableState>> {
        match self.decode_state(at.ty, &at.storage, context, runtime, budget)? {
            VariableState::Available { value, .. } => Ok(Ok(value)),
            state => Ok(Err(state)),
        }
    }

    fn text_span(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<Option<TextLocation>, VariableState>> {
        let state = self.decode_state(at.ty, &at.storage, context, runtime, budget)?;
        let VariableState::Available { value, .. } = state else {
            return Ok(Err(state));
        };

        let Ok(shape) = self.value_shape(at.ty) else {
            return Ok(Ok(None));
        };

        Ok(self
            .text_location(at.ty, &shape, &value, &at.storage, runtime, budget)
            .map_err(VariableState::Unavailable))
    }

    fn materialize(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue> {
        let type_info = match Self::type_info(self, at.ty) {
            Ok(info) => info.clone(),
            Err(description) => {
                return Ok(inspected_value(
                    None,
                    VariableState::Malformed(malformed_reason(
                        VariableMalformedKind::InvalidTypeGraph,
                        description,
                    )),
                    budget,
                ));
            }
        };

        self.materialize_inspected_value(at.ty, type_info, &at.storage, context, runtime, budget)
    }

    fn object_storage(&self, object: ObjectKey) -> ObjectStorage {
        self.object_storage(self.object(object))
    }

    fn inspect_global(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Variable> {
        let object = self
            .catalog()
            .global(id.index())
            .ok_or_else(|| Error::VariableNotFound(id.to_string()))?;
        if let Err(exhaustion) = budget.consume_variable_value() {
            return Ok(data_object(
                object,
                None,
                VariableState::Unavailable(exhaustion.into()),
            ));
        }

        let mut frame_base = FrameBaseCache::Empty;
        self.inspect_data_object(object, address, context, runtime, &mut frame_base, budget)
    }

    fn returned(
        &self,
        function: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<Vec<crate::debug_info::ReturnedValue>>> {
        self.returned_values(function, runtime, budget)
    }

    fn dereference(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<DereferencedValue> {
        self.dereference_value(reference, runtime, budget)
    }

    fn value_children(
        &self,
        reference: &ValueChildrenReference,
        offset: u64,
        limit: u32,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage> {
        self.value_child_page(reference, offset, limit, runtime, budget)
    }

    fn may_describe_call_site(&self, return_address: ImageAddress) -> bool {
        self.calls().returning_to(return_address).next().is_some()
    }

    fn call_site(
        &self,
        return_address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Option<CallSite>, VariableRuntimeError> {
        self.described_call_site(return_address, runtime, budget)
    }

    fn tail_calls(
        &self,
        from: ImageAddress,
        to: ImageAddress,
    ) -> std::result::Result<crate::debug_info::TailCallChain, VariableRuntimeError> {
        self.tail_call_path(from, to)
    }

    fn call_site_value(
        &self,
        site: CallSiteId,
        parameter: EntryParameter,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError> {
        self.site_parameter_value(site, parameter, runtime, budget)
    }
}

impl DwarfVariableInfo {
    /// The variables of `image`, answered from the type table loading sealed
    /// for it.
    pub(super) fn new(image: &crate::ModuleImage) -> Self {
        let target = image.target();
        Self {
            types: Arc::clone(image.type_table()),
            target,
            endian: match target.byte_order {
                ByteOrder::Little => RunTimeEndian::Little,
                ByteOrder::Big => RunTimeEndian::Big,
            },
        }
    }

    /// Fails when the image's locals and parameters were left out, so
    /// that no frame seems to have none.
    fn locals_read(&self) -> Result<()> {
        crate::image::facts::FactsView::new(self.types.tables())
            .locals_left_out()
            .map_or(Ok(()), |reason| Err(Error::LocalsLeftOut(reason)))
    }

    /// The image's locations.
    fn locations(&self) -> LocationTables<'_> {
        LocationTables::new(self.types.tables())
    }

    /// What reading values of some types takes.
    fn type_facts(&self) -> crate::image::type_facts::TypeFactsView<'_> {
        crate::image::type_facts::TypeFactsView::new(self.types.tables())
    }

    /// The image's resume points and held ranges.
    fn resumes(&self) -> crate::image::resumes::ResumeView<'_> {
        crate::image::resumes::ResumeView::new(self.types.tables())
    }

    fn catalog(&self) -> VariableView<'_> {
        VariableView::new(self.types.tables())
    }

    fn object(&self, key: ObjectKey) -> Object<'_> {
        self.catalog().object(ObjectId(key.0))
    }

    /// Whether `object` is visible at `address`: in its scope's code and,
    /// for a Go local, past its declaration.
    fn visible_at(&self, object: Object<'_>, address: ImageAddress) -> bool {
        object.in_scope(address)
            && object.go_declaration().is_none_or(|declared| {
                visibility::visible_at(self.types.tables(), &declared, address)
            })
    }

    /// The code where `object` is visible.
    fn visible_ranges(&self, object: Object<'_>) -> Arc<[AddressRange<ImageAddress>]> {
        let ranges = object.ranges().collect::<Vec<_>>();
        match object.go_declaration() {
            Some(declared) => {
                visibility::visible_ranges(self.types.tables(), &ranges, &declared).into()
            }
            None => ranges.into(),
        }
    }
}

impl TypeMetadataEntry for TypeNode {
    fn type_info(&self) -> std::result::Result<&TypeInfo, Arc<str>> {
        match self {
            Self::Resolved(info) => Ok(info),
            Self::Malformed { description, .. } => Err(Arc::clone(description)),
        }
    }
}

#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_expression(data: &[u8]) {
    use evaluate::{FrameBase, evaluate};

    use crate::debug_info::{EntryParameter, VariableRuntime, VariableRuntimeError};
    use crate::{ImageAddress, VariableUnavailableReason, VirtualAddress};

    struct FuzzRuntime;

    impl VariableRuntime for FuzzRuntime {
        fn register(
            &mut self,
            register: crate::unwind::DwarfRegister,
        ) -> std::result::Result<crate::debug_info::VariableRegister, VariableRuntimeError>
        {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::RegisterUnavailable(register.to_string().into()),
            ))
        }

        fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::CallFrameUnavailable(
                    crate::CallFrameUnavailableReason::NoInstructionContext,
                ),
            ))
        }

        fn tls_address(
            &mut self,
            _offset: u64,
        ) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
            Err(VariableUnavailableReason::TlsUnavailable(
                crate::TlsUnavailableReason::ProviderUnavailable,
            ))
        }

        fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
            Ok(VirtualAddress::new(address.get()))
        }

        fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress> {
            Some(ImageAddress::new(address.get()))
        }

        fn read_memory(
            &mut self,
            address: VirtualAddress,
            size: usize,
        ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError> {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::MemoryInaccessible {
                    address,
                    requested: u64::try_from(size).unwrap_or(u64::MAX),
                    completed: 0,
                    next_address: address,
                },
            ))
        }

        fn entry_value(
            &mut self,
            _parameter: EntryParameter,
            _budget: &mut InspectionBudget,
        ) -> std::result::Result<u64, VariableRuntimeError> {
            Err(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::EntryValue(crate::EntryValueUnavailableReason::NoCaller),
            ))
        }
    }

    let mut pool = LocationsBuilder::default();
    let encoding = gimli::Encoding {
        format: gimli::Format::Dwarf32,
        version: 5,
        address_size: 8,
    };

    let Ok(id) = pool
        .unit(&location::EvaluationUnit::default())
        .and_then(|unit| {
            pool.expression(
                &data[..data.len().min(MAX_EVALUATION_MEMORY_BYTES)],
                unit,
                encoding,
                &[],
                &[],
            )
        })
    else {
        return;
    };

    let mut budget = InspectionBudget::default();

    if let Ok(pieces) = evaluate(
        pool.tables().expression(id),
        RunTimeEndian::Little,
        None,
        &mut FrameBase::Unsupported,
        &mut FuzzRuntime,
        &mut budget,
    ) {
        fuzz_storage(&pieces, &mut FuzzRuntime, &mut budget);
    }
}

/// Makes fuzzed pieces the storage of a 16-byte object, and reads it whole,
/// by bits, and in part.
#[cfg(feature = "fuzzing")]
fn fuzz_storage(
    pieces: &[gimli::Piece<Reader<'_>>],
    runtime: &mut dyn crate::debug_info::VariableRuntime,
    budget: &mut InspectionBudget,
) {
    let target = TargetDescription {
        architecture: crate::Architecture::X86_64,
        pointer_width: crate::PointerWidth::Bits64,
        byte_order: ByteOrder::Little,
    };

    let Ok(storage) =
        pieces::storage_from_pieces(pieces, 16, None, RunTimeEndian::Little, target, runtime)
    else {
        return;
    };

    let _ = storage::read(&storage, 16, runtime, budget);
    let _ = storage::read_bits(&storage, 3, 61, ByteOrder::Little, runtime, budget);
    let _ = storage::offset(storage, 8).map(|storage| storage::narrow(storage, 8));
}

#[cfg(test)]
mod tests;
