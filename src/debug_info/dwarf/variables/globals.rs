//! The catalog of global data objects, deduplicated across units.

use crate::image::lines::Files;
use std::borrow::Cow;
use std::sync::{Arc, PoisonError};

use foldhash::HashMap;
use rayon::prelude::*;

use crate::VariableKind;
use crate::debug_info::dwarf::{
    DieKey, DieWalk, DwarfError, Reader, Units, is_type_unit, str_attribute,
};

use crate::image::variables::Global;

use super::die::{
    debug_info_offset, declaration_with_origins, flag_with_origins, origin_chain,
    string_with_origins, type_with_origins,
};

use super::location::copy_data_object_value_with_origins;
use super::types::TypeArenaBuilder;
use super::{DataObject, Metadata, MetadataAbsence, ValueDescription};

/// One scope a DIE can be in, as a link to the scope enclosing it.
///
/// Every namespace and aggregate of every unit is a scope, and few hold
/// globals, so a scope keeps the one name it adds, borrowed from the
/// debug information, and a path is spelled only for a global's scope.
/// Copying each scope's whole path into one of its own allocated three
/// times for every aggregate a program declares.
#[derive(Clone, Default)]
struct GlobalScope<'data> {
    /// The scope enclosing this one; the root encloses itself.
    parent: u32,

    /// The name this scope adds to its parent's path, if any.
    name: Option<Cow<'data, str>>,

    /// How many names the path has.
    depth: u32,
    routine: bool,
}

/// The scope of every DIE outside type units: the distinct scopes, the
/// first the empty one, and for each unit its DIEs' offsets in order with
/// the scope each has. Most DIEs share their parent's.
struct ScopeTable<'data> {
    scopes: Vec<GlobalScope<'data>>,
    units: Vec<UnitScopes>,
}

#[derive(Default)]
struct UnitScopes {
    offsets: Vec<usize>,
    scopes: Vec<u32>,
}

impl<'data> ScopeTable<'data> {
    /// The scope of the DIE `key` names, by its index.
    fn get(&self, key: DieKey) -> Option<u32> {
        let unit = self.units.get(key.unit)?;
        let index = unit.offsets.binary_search(&key.offset).ok()?;
        Some(unit.scopes[index]).filter(|scope| (*scope as usize) < self.scopes.len())
    }

    fn scope(&self, index: u32) -> &GlobalScope<'data> {
        &self.scopes[index as usize]
    }

    /// A scope's index once it is in the table.
    fn add(&mut self, scope: GlobalScope<'data>) -> u32 {
        self.scopes.push(scope);
        super::row(self.scopes.len() - 1)
    }

    /// The names of a scope's path, outermost first, joined by `::`.
    fn path(&self, index: u32) -> String {
        let mut names = Vec::new();
        let mut current = self.scope(index);
        while current.depth > 0 {
            if let Some(name) = &current.name {
                names.push(&**name);
            }

            current = self.scope(current.parent);
        }

        names.reverse();
        names.join("::")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DefinitionResolution {
    New,
    Existing(usize),
    Conflict,
}

#[derive(Default)]
pub(super) struct DefinitionIndex {
    pub(super) by_identity: HashMap<Arc<str>, usize>,
}

impl DefinitionIndex {
    pub(super) fn resolve(
        &mut self,
        identities: &[Arc<str>],
        candidate: usize,
    ) -> DefinitionResolution {
        let mut existing = identities
            .iter()
            .filter_map(|identity| self.by_identity.get(identity).copied())
            .collect::<Vec<_>>();
        existing.sort_unstable();
        existing.dedup();
        match existing.as_slice() {
            [] => {
                for identity in identities {
                    self.by_identity.insert(Arc::clone(identity), candidate);
                }

                DefinitionResolution::New
            }
            [definition] => {
                for identity in identities {
                    self.by_identity.insert(Arc::clone(identity), *definition);
                }

                DefinitionResolution::Existing(*definition)
            }
            _ => {
                // Contradictory producer identities must remain separate and
                // therefore ambiguous; never select one convincing value.
                for identity in identities {
                    self.by_identity
                        .entry(Arc::clone(identity))
                        .or_insert(candidate);
                }

                DefinitionResolution::Conflict
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "global collection deliberately resolves producer variants in one auditable pass"
)]
pub(super) fn load_globals<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &Units<'data>,
    objects: &mut Vec<DataObject>,
    order: &mut u64,
    files: &mut Files,
    types: &mut TypeArenaBuilder<'_, 'data>,
    pool: &std::sync::Mutex<super::location::LocationsBuilder>,
) -> std::result::Result<Vec<Global>, DwarfError> {
    let mut table = ScopeTable {
        scopes: vec![GlobalScope::default()],
        units: Vec::with_capacity(units.len()),
    };

    // Pass one records lexical ownership for every DIE. A later definition
    // may point backward to a declaration nested in a namespace or class.
    // Each unit numbers its own scopes in parallel, and they are numbered
    // in unit order once all are read, as a serial pass numbered them.
    let unit_tables = units
        .par_iter()
        .map(|unit| unit_scopes(dwarf, unit))
        .collect::<Vec<_>>();

    for unit_table in unit_tables {
        let (scopes, mut unit_scopes) = unit_table?;

        // A unit's scope `n` past its root is the table's `first + n - 1`.
        let first = super::row(table.scopes.len());
        let global = |local: u32| if local == 0 { 0 } else { first + local - 1 };

        for scope in scopes.into_iter().skip(1) {
            table.add(GlobalScope {
                parent: global(scope.parent),
                ..scope
            });
        }

        for scope in &mut unit_scopes.scopes {
            *scope = global(*scope);
        }

        table.units.push(unit_scopes);
    }

    let mut globals = Vec::<Global>::new();
    let mut definitions = DefinitionIndex::default();

    // Pass two resolves every non-routine data object independently. Each
    // unit is searched for them in parallel; what they name is resolved in
    // unit order, which is the order types and files are numbered in.
    let found = (0..units.len())
        .into_par_iter()
        .map(|unit_index| unit_globals(units, &table, unit_index))
        .collect::<Vec<_>>();

    for (unit_index, (found, error)) in found.into_iter().enumerate() {
        let unit = &units[unit_index];
        for (key, current_scope, entry) in found {
            let entry = &entry;

            let (chain, chain_error) = match origin_chain(units, unit_index, entry) {
                Ok(chain) => (chain, None),
                Err(error) => (Vec::new(), Some(Arc::from(error.to_string()))),
            };

            let name =
                match string_with_origins(dwarf, units, unit, entry, &chain, gimli::DW_AT_name) {
                    Ok(Some(name)) => name,
                    Ok(None) => format!("<anonymous global at {:#x}>", entry.offset().0).into(),
                    Err(error) => {
                        format!("<malformed global at {:#x}: {error}>", entry.offset().0).into()
                    }
                };

            // A malformed linkage name marks this entry malformed, not the module.
            let linkage =
                |attribute| string_with_origins(dwarf, units, unit, entry, &chain, attribute);
            let linkage_result = match linkage(gimli::DW_AT_linkage_name) {
                Ok(None) => linkage(gimli::DW_AT_MIPS_linkage_name),
                result => result,
            };

            let (linkage_name, linkage_error) = match linkage_result {
                Ok(name) => (name, None),
                Err(error) => (None, Some(Arc::<str>::from(error.to_string()))),
            };

            let scope = chain
                .iter()
                .filter_map(|(origin_unit, origin)| {
                    table.get(DieKey {
                        unit: *origin_unit,
                        offset: origin.offset().0,
                    })
                })
                .chain(std::iter::once(current_scope))
                .max_by_key(|scope| table.scope(*scope).depth)
                .unwrap_or(0);
            let qualified_name = if table.scope(scope).depth == 0 {
                linkage_name
                    .as_ref()
                    .filter(|linkage| !linkage.starts_with('_') && linkage.contains('.'))
                    .cloned()
                    .unwrap_or_else(|| Arc::clone(&name))
            } else {
                Arc::from(format!("{}::{name}", table.path(scope)))
            };

            let declaration = declaration_with_origins(dwarf, units, unit, entry, &chain, files);
            let (type_unit, type_value) = type_with_origins(unit_index, entry, &chain);
            let type_info = types.variable_type(type_unit, type_value);
            let value = copy_data_object_value_with_origins(
                dwarf,
                &mut pool.lock().unwrap_or_else(PoisonError::into_inner),
                units,
                unit_index,
                unit,
                entry,
                &chain,
            );
            let declaration_only = flag_with_origins(entry, &chain, gimli::DW_AT_declaration)
                .unwrap_or(false)
                && matches!(value, Metadata::Absent(_));
            if declaration_only {
                continue;
            }

            let external = flag_with_origins(entry, &chain, gimli::DW_AT_external).unwrap_or(false);
            *order = order.checked_add(1).ok_or_else(|| {
                DwarfError::MalformedVariable("data-object DIE order overflows".into())
            })?;
            let malformed = declaration
                .as_ref()
                .err()
                .map(|error| Arc::from(error.to_string()))
                .or(chain_error)
                .or(linkage_error);
            let object = DataObject {
                debug_info_offset: debug_info_offset(units, unit_index, entry),
                kind: VariableKind::Global,
                name: Arc::clone(&name),
                declaration: declaration.as_ref().ok().cloned().flatten(),
                ranges: Vec::new().into(),
                go_declaration: None,
                instance: None,
                lexical_depth: 0,
                order: *order,
                type_info: type_info.clone(),
                escaped: None,
                hidden: false,
                coroutine: None,
                value,
                frame_base: Metadata::Absent(MetadataAbsence::NotApplicable),
                malformed,
            };

            let info = Global {
                object: 0,
                qualified_name,
                linkage_name: linkage_name.clone(),
                external,
            };

            let canonical_die = chain.first().map_or(key, |(origin_unit, origin)| DieKey {
                unit: *origin_unit,
                offset: origin.offset().0,
            });
            let mut identities = vec![Arc::from(format!(
                "die:{}:{:#x}",
                canonical_die.unit, canonical_die.offset
            ))];
            if let Some(linkage_name) = &linkage_name {
                identities.push(Arc::from(format!("linkage:{linkage_name}")));
            }

            if let DefinitionResolution::Existing(existing_global) =
                definitions.resolve(&identities, globals.len())
            {
                let existing_object = globals[existing_global].object;

                if value_rank(&object.value) > value_rank(&objects[existing_object as usize].value)
                {
                    objects[existing_object as usize] = object;
                    globals[existing_global] = Global {
                        object: existing_object,
                        ..info
                    };
                }

                continue;
            }

            types
                .budget
                .charge("data objects", size_of::<DataObject>())?;
            globals.push(Global {
                object: super::row(objects.len()),
                ..info
            });
            objects.push(object);
        }

        // A unit that could not be read to its end fails the load once the
        // globals before the failure are recorded, as in a serial pass.
        if let Some(error) = error {
            return Err(error);
        }
    }

    Ok(globals)
}

/// One unit's scopes, its root first and each parent by its index there,
/// and the scope of each of its DIEs.
fn unit_scopes<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    unit: &gimli::Unit<Reader<'data>>,
) -> std::result::Result<(Vec<GlobalScope<'data>>, UnitScopes), DwarfError> {
    let mut table = vec![GlobalScope::default()];
    let mut unit_scopes = UnitScopes::default();
    if !is_type_unit(unit) {
        let mut walk = DieWalk::new(unit)?;
        let mut scopes = Vec::<u32>::new();
        while let Some(die) = walk.next()? {
            let depth = usize::try_from(die.depth).map_err(|_| DwarfError::InvalidEntryDepth)?;
            scopes.truncate(depth);
            let parent = scopes.last().copied().unwrap_or(0);
            let parent_scope = &table[parent as usize];

            let scope = match die.tag {
                gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine
                    if !parent_scope.routine =>
                {
                    let depth = parent_scope.depth;
                    table.push(GlobalScope {
                        parent,
                        name: None,
                        depth,
                        routine: true,
                    });
                    super::row(table.len() - 1)
                }

                gimli::DW_TAG_namespace
                | gimli::DW_TAG_module
                | gimli::DW_TAG_class_type
                | gimli::DW_TAG_structure_type
                | gimli::DW_TAG_union_type => {
                    let (depth, routine) = (parent_scope.depth + 1, parent_scope.routine);
                    let name = match str_attribute(dwarf, unit, walk.decode()?, gimli::DW_AT_name) {
                        Ok(Some(name)) => name,
                        Ok(None) if die.tag == gimli::DW_TAG_namespace => {
                            Cow::Borrowed("{anonymous}")
                        }
                        Ok(None) => Cow::Borrowed("{anonymous type}"),
                        Err(error) => Cow::Owned(format!("{{malformed scope: {error}}}")),
                    };

                    table.push(GlobalScope {
                        parent,
                        name: Some(name),
                        depth,
                        routine,
                    });
                    super::row(table.len() - 1)
                }
                _ => parent,
            };

            unit_scopes.offsets.push(die.offset.0);
            unit_scopes.scopes.push(scope);
            scopes.push(scope);
        }
    }

    // Entries follow one another, so their offsets are already in order.
    if !unit_scopes.offsets.is_sorted() {
        let mut pairs = unit_scopes
            .offsets
            .iter()
            .copied()
            .zip(unit_scopes.scopes.iter().copied())
            .collect::<Vec<_>>();
        pairs.sort_by_key(|(offset, _)| *offset);
        (unit_scopes.offsets, unit_scopes.scopes) = pairs.into_iter().unzip();
    }

    Ok((table, unit_scopes))
}

/// A data object DIE outside any routine, its key, and its scope.
type FoundGlobal<'data> = (DieKey, u32, gimli::DebuggingInformationEntry<Reader<'data>>);

/// The data objects of unit `unit_index` outside any routine, in order,
/// and the error that ended the search early, if one did.
fn unit_globals<'data>(
    units: &Units<'data>,
    table: &ScopeTable<'data>,
    unit_index: usize,
) -> (Vec<FoundGlobal<'data>>, Option<DwarfError>) {
    let unit = &units[unit_index];
    let mut found = Vec::new();
    if is_type_unit(unit) {
        return (found, None);
    }

    let mut search = || {
        let mut walk = DieWalk::new(unit)?;

        while let Some(die) = walk.next()? {
            if die.tag != gimli::DW_TAG_variable {
                continue;
            }

            let key = DieKey {
                unit: unit_index,
                offset: die.offset.0,
            };

            let current_scope = table.get(key).unwrap_or(0);
            if table.scope(current_scope).routine {
                continue;
            }

            found.push((key, current_scope, walk.decode()?.clone()));
        }

        Ok::<_, DwarfError>(())
    };

    let error = search().err();
    (found, error)
}

const fn value_rank(value: &Metadata<ValueDescription>) -> u8 {
    match value {
        Metadata::Value(ValueDescription::Location(_)) => 3,
        Metadata::Value(ValueDescription::Constant(_)) => 2,
        Metadata::Absent(_) => 1,
        Metadata::Malformed(_) => 0,
    }
}
