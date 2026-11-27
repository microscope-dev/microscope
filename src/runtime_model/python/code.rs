//! Per-process cache of decoded `PyCodeObject` metadata, local names, and line tables.
//!
//! Cached entries are validated against a `CodeProof` fingerprint on each lookup to catch address
//! reuse after garbage collection, and the whole arena resets at `MAX_ROWS` to keep `CodeId` and
//! `NameId` spans compact.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use super::linetable;
use super::objects::{Problem, Reader, Record, malformed};
use crate::VirtualAddress;
use crate::image::python::{Constant, Field};
use crate::runtime_model::SampledCode;

crate::model::id_type!(CodeId, "A code object's row in one process's code table.");
crate::model::id_type!(NameId, "A string in one process's code table.");

#[derive(Debug)]
pub(super) struct Span<T> {
    first: u32,
    count: u32,
    rows: PhantomData<T>,
}

impl<T> Clone for Span<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Span<T> {}

impl<T> Span<T> {
    const fn range(self) -> std::ops::Range<usize> {
        let first = self.first as usize;
        first..first + self.count as usize
    }

    fn since(first: usize, pool: usize) -> Option<Self> {
        Some(Self {
            first: u32::try_from(first).ok()?,
            count: u32::try_from(pool.checked_sub(first)?).ok()?,
            rows: PhantomData,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Local {
    pub(super) name: NameId,

    pub(super) kind: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodeProof {
    ty: u64,
    qualname: u64,
    filename: u64,
    linetable: u64,
    first_line: i32,
    units: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CodeRecord {
    pub(super) address: VirtualAddress,
    proof: CodeProof,
    pub(super) qualname: NameId,
    pub(super) file: NameId,
    pub(super) first_line: i32,

    pub(super) units: u64,

    pub(super) parameters: u32,

    pub(super) internal: bool,
    pub(super) locals: Span<Local>,
    lines: Span<u8>,

    function: Option<(VirtualAddress, VirtualAddress, NameId)>,
}

#[derive(Debug, Default)]
pub(super) struct CodeTable {
    rows: Vec<CodeRecord>,

    index: HashMap<VirtualAddress, CodeId>,
    names: Vec<Arc<str>>,
    name_index: HashMap<Arc<str>, NameId>,
    locals: Vec<Local>,
    lines: Vec<u8>,
}

const NO_MONITORING_EVENTS: u32 = 0x0200_0000;

const MAX_ROWS: usize = 1 << 16;

const _: () = assert!(MAX_ROWS <= u32::MAX as usize, "every row's id fits its u32");

impl CodeTable {
    pub(super) fn code(
        &mut self,
        reader: &Reader<'_>,
        code_type: VirtualAddress,
        address: VirtualAddress,
    ) -> Result<CodeId, Problem> {
        let header = reader.record(address, Field::CodeSize)?;
        let proof = proof(&header)?;
        if proof.ty != code_type.get() {
            return Err(malformed(address, "code object"));
        }

        if let Some(&id) = self.index.get(&address)
            && self.row(id).proof == proof
        {
            debug_assert_eq!(
                self.row(id).address,
                address,
                "the index finds row {id} at {address:#x}, which is not its"
            );
            return Ok(id);
        }

        if self.rows.len() >= MAX_ROWS {
            *self = Self::default();
        }

        let id = self.decode(reader, &header, proof)?;
        self.index.insert(address, id);
        self.check_row(id.index());
        debug_assert!(
            self.rows
                .get(id.index())
                .is_some_and(|row| row.address == address),
            "the index finds row {id} at {address:#x}, which is not its"
        );

        Ok(id)
    }

    fn decode(
        &mut self,
        reader: &Reader<'_>,
        header: &Record<'_>,
        proof: CodeProof,
    ) -> Result<CodeId, Problem> {
        let pointer = |field| {
            header
                .pointer(field)?
                .ok_or_else(|| malformed(header.address, "code object"))
        };

        let qualname = reader.string(pointer(Field::CodeQualname)?)?;
        let file = source_path(reader.string(pointer(Field::CodeFilename)?)?);
        let lines = reader.bytes_object(pointer(Field::CodeLinetable)?)?;
        let names = reader.tuple(pointer(Field::CodeLocalsplusnames)?)?;
        let kinds = reader.bytes_object(pointer(Field::CodeLocalspluskinds)?)?;
        if names.len() != kinds.len() {
            return Err(malformed(header.address, "code object"));
        }

        let names = names
            .into_iter()
            .map(|name| reader.string(name))
            .collect::<Result<Vec<_>, _>>()?;
        let parameters = parameters(reader, header)?;
        let internal = header.int(Field::CodeFlags)?.cast_unsigned() & NO_MONITORING_EVENTS != 0;

        let id = u32::try_from(self.rows.len())
            .map(CodeId::new)
            .map_err(|_| -> Problem { "the code table is full".into() })?;
        let qualname = self.intern(qualname)?;
        let file = self.intern(file)?;
        let first_local = self.locals.len();
        for (name, kind) in names.into_iter().zip(kinds) {
            let name = self.intern(name)?;
            self.locals.push(Local { name, kind });
        }

        let first_line = self.lines.len();
        self.lines.extend_from_slice(&lines);
        let full = || -> Problem { "the code table is full".into() };
        self.rows.push(CodeRecord {
            address: header.address,
            proof,
            qualname,
            file,
            first_line: proof.first_line,
            units: proof.units,
            parameters,
            internal,
            locals: Span::since(first_local, self.locals.len()).ok_or_else(full)?,
            lines: Span::since(first_line, self.lines.len()).ok_or_else(full)?,
            function: None,
        });
        Ok(id)
    }

    fn intern(&mut self, name: Arc<str>) -> Result<NameId, Problem> {
        if let Some(&id) = self.name_index.get(&name) {
            return Ok(id);
        }

        let id = u32::try_from(self.names.len())
            .map(NameId::new)
            .map_err(|_| -> Problem { "the code table is full".into() })?;
        self.names.push(Arc::clone(&name));
        self.name_index.insert(name, id);
        debug_assert_eq!(
            self.names.len(),
            self.name_index.len(),
            "interning name {id} puts the names out of step with their index"
        );
        Ok(id)
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "a row id is one `code` returned: the table is cleared only inside `code`, \
                  before it pushes the row, and callers use an id before calling it again"
    )]
    pub(super) fn row(&self, id: CodeId) -> &CodeRecord {
        &self.rows[id.index()]
    }

    pub(super) fn proves(&self, id: CodeId, sampled: SampledCode) -> bool {
        let proof = self.row(id).proof;
        proof.qualname == sampled.qualname
            && proof.linetable == sampled.linetable
            && proof.first_line == sampled.first_line
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "a row's span of locals was taken as it was pushed, and `locals` only grows \
                  until it is cleared with the rows"
    )]
    pub(super) fn locals(&self, id: CodeId) -> &[Local] {
        &self.locals[self.row(id).locals.range()]
    }

    pub(super) fn function_name(
        &mut self,
        reader: &Reader<'_>,
        id: CodeId,
        function: VirtualAddress,
        qualname: VirtualAddress,
    ) -> Result<Arc<str>, Problem> {
        if let Some((seen, named, name)) = self.row(id).function
            && (seen, named) == (function, qualname)
        {
            return Ok(Arc::clone(self.name(name)));
        }

        let name = self.intern(reader.string(qualname)?)?;
        #[expect(
            clippy::indexing_slicing,
            reason = "a row id is one `code` returned: the table is cleared only inside `code`, \
                  before it pushes the row, and callers use an id before calling it again"
        )]
        let row = &mut self.rows[id.index()];
        row.function = Some((function, qualname, name));
        self.check_row(id.index());
        Ok(Arc::clone(self.name(name)))
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "a name id is one `intern` pushed, held by a row, and `names` is cleared only \
                  with the rows"
    )]
    pub(super) fn name(&self, id: NameId) -> &Arc<str> {
        &self.names[id.index()]
    }

    pub(super) fn line(
        &self,
        id: CodeId,
        offset: u64,
    ) -> Result<Option<i64>, linetable::Malformed> {
        let row = self.row(id);
        #[expect(
            clippy::indexing_slicing,
            reason = "a row's span of lines was taken as it was pushed, and `lines` only grows \
                      until it is cleared with the rows"
        )]
        let lines = &self.lines[row.lines.range()];
        linetable::line_at(lines, i64::from(row.first_line), offset)
    }

    fn check_row(&self, index: usize) {
        let Some(row) = self.rows.get(index) else {
            debug_assert!(false, "no row {index}");
            return;
        };
        debug_assert!(
            row.locals.range().end <= self.locals.len(),
            "row {index}'s locals lie past the pool"
        );
        debug_assert!(
            row.lines.range().end <= self.lines.len(),
            "row {index}'s line table lies past the pool"
        );
        debug_assert!(
            row.qualname.index() < self.names.len() && row.file.index() < self.names.len(),
            "row {index} names no name"
        );
        debug_assert!(
            row.function
                .is_none_or(|(_, _, name)| name.index() < self.names.len()),
            "row {index}'s function names no name"
        );
        for local in self.locals.get(row.locals.range()).unwrap_or_default() {
            debug_assert!(
                local.name.index() < self.names.len(),
                "a local of row {index} names no name"
            );
        }
    }
}

fn parameters(reader: &Reader<'_>, header: &Record<'_>) -> Result<u32, Problem> {
    let count = |field| {
        u32::try_from(header.int(field)?).map_err(|_| malformed(header.address, "code object"))
    };
    let flags = header.int(Field::CodeFlags)?.cast_unsigned();
    let flag = |constant| u32::from(flags & u32::from(reader.layout.constant(constant)) != 0);
    Ok(count(Field::CodeArgcount)?
        .saturating_add(count(Field::CodeKwonlyargcount)?)
        .saturating_add(flag(Constant::VarArgs))
        .saturating_add(flag(Constant::VarKeywords)))
}

fn proof(header: &Record<'_>) -> Result<CodeProof, Problem> {
    Ok(CodeProof {
        ty: header.word(Field::ObjectType)?,
        qualname: header.word(Field::CodeQualname)?,
        filename: header.word(Field::CodeFilename)?,
        linetable: header.word(Field::CodeLinetable)?,
        first_line: header.int(Field::CodeFirstlineno)?,
        units: header.word(Field::VarObjectSize)?,
    })
}

fn source_path(file: Arc<str>) -> Arc<str> {
    let path = std::path::Path::new(&*file);
    if !path.is_absolute() {
        return file;
    }

    let normal = path.components().collect::<std::path::PathBuf>();
    match normal.to_str() {
        Some(normal) if normal != &*file => Arc::from(normal),
        _ => file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_objects_path_drops_its_current_directories() {
        for (file, path) in [
            ("/work/./app.py", "/work/app.py"),
            ("/work//pkg/./mod.py", "/work/pkg/mod.py"),
            ("/work/../other/app.py", "/work/../other/app.py"),
            ("/work/app.py", "/work/app.py"),
            ("<string>", "<string>"),
            ("./relative.py", "./relative.py"),
        ] {
            assert_eq!(&*super::source_path(Arc::from(file)), path, "{file}");
        }
    }
}
