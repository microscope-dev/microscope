//! Supplements a module's image catalog with functions, lines, and inline trees
//! from `.gopclntab`.
//!
//! Only functions not already covered by DWARF are added, so stripped Go binaries
//! and runtime/assembly frames still get symbols, line numbers, and inline stacks.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use super::{GoFunction, GoPosition, GoTable, PcRun, Result};
use crate::image::lines::{LineTables, Row};
use crate::{
    AddressRange, BreakpointEntry, CodeInstanceId, CodeInstanceInfo, CodeInstanceKind,
    EntryProvenance, FunctionId, FunctionInfo, ImageAddress, LineNumber, SourceFileId,
    SourceLanguage, SourceLocation,
};

type Positions = Vec<(std::ops::Range<u64>, GoPosition)>;

pub struct Catalog<'a> {
    pub functions: &'a mut Vec<FunctionInfo>,
    pub code_instances: &'a mut Vec<CodeInstanceInfo>,
    pub lines: &'a mut LineTables,

    pub source_file: &'a mut dyn FnMut(PathBuf) -> SourceFileId,
}

/// Runs in two passes (`declare` then `define`) so `by_name` is populated before
/// inline trees are expanded and inlined calls reuse their out-of-line `FunctionId`.
pub fn complete<'code>(
    table: &GoTable,
    code: impl Fn(u64, usize) -> Option<&'code [u8]> + Copy,
    catalog: &mut Catalog<'_>,
) -> Result<()> {
    let described = Described::new(catalog.code_instances, catalog.lines);
    let mut builder = Builder {
        table,
        functions: Vec::new(),
        instances: Vec::new(),
        lines: LineTables::default(),
        files: HashMap::new(),
        by_name: HashMap::new(),
        first_function: catalog.functions.len(),
        first_instance: catalog.code_instances.len(),
        source_file: &mut *catalog.source_file,
    };

    let mut declared = Vec::new();
    for function in table.functions() {
        if function.entry < function.end && !described.contains(function.entry) {
            declared.push((function, builder.declare(function)?));
        }
    }

    for (function, (id, positions, inlined)) in declared {
        builder.define(function, id, &positions, &inlined, code)?;
    }

    catalog
        .lines
        .append(&builder.lines, |file| file)
        .map_err(|_| super::malformed("too many line rows"))?;
    catalog.functions.append(&mut builder.functions);
    catalog.code_instances.append(&mut builder.instances);

    Ok(())
}

struct Described(Vec<(u64, u64)>);

impl Described {
    fn new(instances: &[CodeInstanceInfo], lines: &LineTables) -> Self {
        let mut ranges = instances
            .iter()
            .flat_map(|instance| instance.ranges.iter())
            .map(|range| (range.start.get(), range.end.get()))
            .chain(
                lines
                    .ranges
                    .iter()
                    .map(|range| (range.start.get(), range.end.get())),
            )
            .collect::<Vec<_>>();
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());

        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }

        Self(merged)
    }

    fn contains(&self, address: u64) -> bool {
        let after = self.0.partition_point(|(start, _)| *start <= address);
        after
            .checked_sub(1)
            .and_then(|last| self.0.get(last))
            .is_some_and(|(_, end)| address < *end)
    }
}

struct Builder<'a, 'catalog> {
    table: &'a GoTable,
    functions: Vec<FunctionInfo>,
    instances: Vec<CodeInstanceInfo>,
    lines: LineTables,
    files: HashMap<Arc<str>, SourceFileId>,

    by_name: HashMap<Arc<str>, FunctionId>,
    first_function: usize,
    first_instance: usize,
    source_file: &'catalog mut dyn FnMut(PathBuf) -> SourceFileId,
}

impl Builder<'_, '_> {
    fn file(&mut self, path: &Arc<str>) -> SourceFileId {
        if let Some(id) = self.files.get(path) {
            return *id;
        }

        let id = (self.source_file)(PathBuf::from(path.as_ref()));
        self.files.insert(Arc::clone(path), id);
        id
    }

    fn location(&mut self, file: &Arc<str>, line: i32) -> Option<SourceLocation> {
        let line = LineNumber::new(u64::try_from(line).ok()?)?;
        Some(SourceLocation {
            file: self.file(file),
            line,
            column: None,
        })
    }

    fn new_function(
        &mut self,
        name: Arc<str>,
        declaration: Option<SourceLocation>,
        language: SourceLanguage,
        role: crate::CodeRole,
    ) -> Result<FunctionId> {
        let id = FunctionId::new(
            u32::try_from(self.first_function + self.functions.len())
                .map_err(|_| super::malformed("too many functions"))?,
        );
        self.functions.push(FunctionInfo {
            id,
            name,
            linkage_name: None,
            declaration,
            language,
            role,
            enclosing: None,
            coroutine: None,
            generics: std::sync::Arc::from([]),
            main_subprogram: false,
        });

        Ok(id)
    }

    fn new_instance(
        &mut self,
        function: FunctionId,
        parent: Option<CodeInstanceId>,
        kind: CodeInstanceKind,
        ranges: Vec<AddressRange<ImageAddress>>,
        breakpoint_entry: Option<BreakpointEntry>,
    ) -> Result<CodeInstanceId> {
        let id = CodeInstanceId::new(
            u32::try_from(self.first_instance + self.instances.len())
                .map_err(|_| super::malformed("too many code instances"))?,
        );
        self.instances.push(CodeInstanceInfo {
            id,
            function,
            parent,
            kind,
            ranges: ranges.into(),
            breakpoint_entry,
        });

        Ok(id)
    }

    fn declare(&mut self, function: &GoFunction) -> Result<(FunctionId, Positions, Vec<PcRun>)> {
        let table = self.table;
        let name = table.name(function)?;
        let positions = table.positions(function)?;
        let inlined = table.inline_runs(function)?;
        let declared = positions
            .iter()
            .find(|(pcs, _)| value_at(&inlined, pcs.start) < 0)
            .map(|(_, position)| Arc::clone(&position.file));
        let declaration = declared
            .as_ref()
            .and_then(|file| self.location(file, function.start_line));
        let go = function.is_go();
        let cgo = declared
            .as_deref()
            .is_some_and(|file| crate::debug_info::roles::cgo_wrote(std::path::Path::new(file)));
        let role = if go {
            crate::debug_info::roles::go_role(&name, Some(function.facts), cgo)
        } else {
            crate::debug_info::roles::symbol_role(&name)
        };

        let language = if go {
            SourceLanguage::Go
        } else {
            SourceLanguage::Unknown
        };

        let id = self.new_function(Arc::clone(&name), declaration, language, role)?;

        // Autogenerated method wrappers should not claim the canonical name for inline callees.
        if function.facts.special != Some(super::SpecialFunction::Wrapper) {
            self.by_name.entry(name).or_insert(id);
        }

        Ok((id, positions, inlined))
    }

    fn define<'code>(
        &mut self,
        function: &GoFunction,
        id: FunctionId,
        positions: &Positions,
        inlined: &[PcRun],
        code: impl Fn(u64, usize) -> Option<&'code [u8]>,
    ) -> Result<()> {
        let entry = self.table.prologue(function, code)?.end.map_or_else(
            || BreakpointEntry {
                address: ImageAddress::new(function.entry),
                provenance: EntryProvenance::RangeStart,
            },
            |end| BreakpointEntry {
                address: ImageAddress::new(end),
                provenance: EntryProvenance::AnalyzedPrologue,
            },
        );
        let physical = self.new_instance(
            id,
            None,
            CodeInstanceKind::OutOfLine,
            vec![range(function.entry, function.end)],
            Some(entry),
        )?;
        self.add_lines(positions)?;

        self.add_inlined_calls(function, inlined, physical)
    }

    fn add_lines(&mut self, positions: &Positions) -> Result<()> {
        let Some((last, _)) = positions.last() else {
            return Ok(());
        };

        let too_many = |_| super::malformed("too many line rows");
        self.lines.begin_sequence().map_err(too_many)?;

        for (pcs, position) in positions {
            let location = self.location(&position.file, position.line);
            let row = self
                .lines
                .push_row(&Row {
                    address: pcs.start,
                    file: location.as_ref().map(|location| location.file),
                    line: location.as_ref().map_or(0, |location| location.line.get()),
                    statement: true,
                    ..Row::default()
                })
                .map_err(too_many)?;
            if location.is_some() {
                self.lines
                    .push_range(range(pcs.start, pcs.end), row, true)
                    .map_err(too_many)?;
            }
        }

        self.lines
            .push_row(&Row {
                address: last.end,
                end_sequence: true,
                ..Row::default()
            })
            .map_err(too_many)?;

        Ok(())
    }

    fn add_inlined_calls(
        &mut self,
        function: &GoFunction,
        runs: &[PcRun],
        physical: CodeInstanceId,
    ) -> Result<()> {
        let table = self.table;

        let mut entries = BTreeMap::<i32, (Vec<AddressRange<ImageAddress>>, i32)>::new();
        for run in runs.iter().filter(|run| run.value >= 0) {
            let mut index = run.value;
            for depth in 0.. {
                if index < 0 {
                    break;
                }

                if depth == MAX_INLINE_DEPTH {
                    return Err(super::malformed("the inline tree is too deep"));
                }

                let parent = if let Some((_, parent)) = entries.get(&index) {
                    *parent
                } else {
                    value_at(runs, table.inlined_call(function, index)?.parent_pc)
                };

                if parent == index {
                    return Err(super::malformed("an inlined call is its own caller"));
                }

                let (ranges, _) = entries.entry(index).or_insert_with(|| (Vec::new(), parent));
                match ranges.last_mut() {
                    Some(last) if last.end.get() == run.start => {
                        last.end = ImageAddress::new(run.end);
                    }
                    _ => ranges.push(range(run.start, run.end)),
                }

                index = parent;
            }
        }

        let mut instances = BTreeMap::<i32, CodeInstanceId>::new();

        // Inline tree indices are not guaranteed to be topologically ordered, so resolve
        // parents iteratively and reject the tree if a full pass makes no progress.
        let mut pending = entries.iter().collect::<Vec<_>>();

        while !pending.is_empty() {
            let before = pending.len();
            let mut deferred = Vec::new();

            for item in pending {
                let (&index, (ranges, parent)) = item;
                let parent_instance = if *parent < 0 {
                    physical
                } else if let Some(instance) = instances.get(parent) {
                    *instance
                } else {
                    deferred.push(item);
                    continue;
                };

                let call = table.inlined_call(function, index)?;
                let name = table.inlined_name(&call)?;
                let call_site = table
                    .position(function, call.parent_pc)?
                    .and_then(|position| self.location(&position.file, position.line));
                let callee = match self.by_name.get(&name) {
                    Some(id) => *id,
                    None => self.inlined_function(function, &call, name, ranges)?,
                };

                let instance = self.new_instance(
                    callee,
                    Some(parent_instance),
                    CodeInstanceKind::Inline { call_site },
                    ranges.clone(),
                    ranges.first().map(|first| BreakpointEntry {
                        address: first.start,
                        provenance: EntryProvenance::RangeStart,
                    }),
                )?;
                instances.insert(index, instance);
            }

            if deferred.len() == before {
                return Err(super::malformed("the inline tree's calls form a cycle"));
            }

            pending = deferred;
        }

        Ok(())
    }
}

impl Builder<'_, '_> {
    fn inlined_function(
        &mut self,
        function: &GoFunction,
        call: &super::InlinedCall,
        name: Arc<str>,
        ranges: &[AddressRange<ImageAddress>],
    ) -> Result<FunctionId> {
        let declaration = match ranges.first() {
            Some(first) => self
                .table
                .position(function, first.start.get())?
                .and_then(|position| self.location(&position.file, call.start_line)),
            None => None,
        };

        let facts = super::GoFunctionFacts {
            special: call.special,
            ..super::GoFunctionFacts::default()
        };

        let role = crate::debug_info::roles::go_role(&name, Some(facts), false);
        let id = self.new_function(Arc::clone(&name), declaration, SourceLanguage::Go, role)?;
        self.by_name.insert(name, id);

        Ok(id)
    }
}

const MAX_INLINE_DEPTH: usize = 256;

fn value_at(runs: &[PcRun], address: u64) -> i32 {
    let after = runs.partition_point(|run| run.start <= address);
    after
        .checked_sub(1)
        .and_then(|last| runs.get(last))
        .filter(|run| address < run.end)
        .map_or(-1, |run| run.value)
}

const fn range(start: u64, end: u64) -> AddressRange<ImageAddress> {
    AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    }
}
