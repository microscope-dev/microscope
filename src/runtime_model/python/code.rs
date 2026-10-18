//! The code objects of one process the model has read, kept across stops.
//! A code object is decoded once, and its row is proved again each time
//! it is used, by reading the object's header and comparing it with the
//! row's: an object freed and another put in its place reads as a
//! different object, and is decoded again.
//!
//! Rows refer to their names, local names, and line tables by dense ids
//! and spans into pools, so the table is a few vectors.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use super::linetable;
use super::objects::{Problem, Reader, Record, malformed};
use crate::VirtualAddress;
use crate::image::python::{Constant, Field};

crate::model::id_type!(CodeId, "A code object's row in one process's code table.");
crate::model::id_type!(NameId, "A string in one process's code table.");

/// The rows of one pool that a record owns.
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

    /// The span of the rows pushed onto a pool since it held `first`.
    fn since(first: usize, pool: usize) -> Option<Self> {
        Some(Self {
            first: u32::try_from(first).ok()?,
            count: u32::try_from(pool.checked_sub(first)?).ok()?,
            rows: PhantomData,
        })
    }
}

/// A local variable's name and what kind of variable it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Local {
    pub(super) name: NameId,

    /// CPython's `co_localspluskinds` byte.
    pub(super) kind: u8,
}

/// What proves a code object is the one a row was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodeProof {
    ty: u64,
    qualname: u64,
    filename: u64,
    linetable: u64,
    first_line: i32,
    units: u64,
}

/// One code object.
#[derive(Debug, Clone, Copy)]
pub(super) struct CodeRecord {
    pub(super) address: VirtualAddress,
    proof: CodeProof,
    pub(super) qualname: NameId,
    pub(super) file: NameId,
    pub(super) first_line: i32,

    /// How many code units its bytecode holds.
    pub(super) units: u64,

    /// How many of its first locals its caller passes, as parameters.
    pub(super) parameters: u32,
    pub(super) locals: Span<Local>,
    lines: Span<u8>,
}

/// The code objects a process has shown.
#[derive(Debug, Default)]
pub(super) struct CodeTable {
    rows: Vec<CodeRecord>,

    /// The row of each code object, by its address; for lookups only.
    index: HashMap<VirtualAddress, CodeId>,
    names: Vec<Arc<str>>,
    name_index: HashMap<Arc<str>, NameId>,
    locals: Vec<Local>,
    lines: Vec<u8>,
}

/// The most rows a table keeps before it begins again: a long-running
/// program makes and frees code objects, as `exec` and lambdas do.
const MAX_ROWS: usize = 1 << 16;

impl CodeTable {
    /// The row of the code object at `address`, proved against the
    /// object as it is now, and decoded if it is new. `code_type` is
    /// where `PyCode_Type` is, which every code object's type is.
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
            && self.rows[id.index()].proof == proof
        {
            return Ok(id);
        }

        if self.rows.len() >= MAX_ROWS {
            *self = Self::default();
        }

        let id = self.decode(reader, &header, proof)?;
        self.index.insert(address, id);
        self.check_invariants();
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
            locals: Span::since(first_local, self.locals.len()).ok_or_else(full)?,
            lines: Span::since(first_line, self.lines.len()).ok_or_else(full)?,
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
        Ok(id)
    }

    pub(super) fn row(&self, id: CodeId) -> &CodeRecord {
        &self.rows[id.index()]
    }

    /// Whether the row `id` is of the code object whose qualified-name
    /// pointer and first line were `proof` when they were read.
    pub(super) fn proves(&self, id: CodeId, (qualname, first_line): (u64, i32)) -> bool {
        let proof = self.row(id).proof;
        proof.qualname == qualname && proof.first_line == first_line
    }

    /// The locals of the code object in row `id`, in the order its frames
    /// keep them.
    pub(super) fn locals(&self, id: CodeId) -> &[Local] {
        &self.locals[self.row(id).locals.range()]
    }

    pub(super) fn name(&self, id: NameId) -> &Arc<str> {
        &self.names[id.index()]
    }

    /// The line of the code unit at byte `offset` of a code object's
    /// bytecode: `Ok(None)` for code at no line.
    pub(super) fn line(
        &self,
        id: CodeId,
        offset: u64,
    ) -> Result<Option<i64>, linetable::Malformed> {
        let row = self.row(id);
        linetable::line_at(
            &self.lines[row.lines.range()],
            i64::from(row.first_line),
            offset,
        )
    }

    /// Panics unless every span lies within its pool, every name id names
    /// a name, and the index finds each row it names by its address.
    pub(super) fn check_invariants(&self) {
        for (index, row) in self.rows.iter().enumerate() {
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
        }

        for local in &self.locals {
            debug_assert!(
                local.name.index() < self.names.len(),
                "a local names no name"
            );
        }

        #[allow(
            clippy::iter_over_hash_type,
            reason = "every entry is checked, in any order"
        )]
        for (address, id) in &self.index {
            debug_assert!(
                self.rows
                    .get(id.index())
                    .is_some_and(|row| row.address == *address),
                "the index finds row {id} at {address:#x}, which is not its"
            );
        }
    }
}

/// How many parameters a code object's header says it has: its positional
/// and keyword-only ones, then its `*args` and `**kwargs`, which are its
/// first locals in that order.
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

/// What a code object's header says that proves it is the object a row
/// was decoded from.
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

/// A code object's file name as the path it names: CPython makes a script
/// named relatively absolute by joining it to the working directory, so
/// `python ./app.py` compiles `/work/./app.py`. Dropping `.` and repeated
/// separators names the same file; `..` is kept, since through a link it
/// names another. Names that are no absolute path, such as `<string>`,
/// are kept as they are.
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
    #[should_panic(expected = "lies past the pool")]
    fn a_span_past_its_pool_breaks_the_tables_invariants() {
        let mut table = CodeTable::default();
        table.names.push("f".into());
        table.rows.push(CodeRecord {
            address: VirtualAddress::new(0x1000),
            proof: CodeProof {
                ty: 0,
                qualname: 0,
                filename: 0,
                linetable: 0,
                first_line: 1,
                units: 0,
            },
            qualname: NameId::new(0),
            file: NameId::new(0),
            first_line: 1,
            units: 0,
            parameters: 0,
            locals: Span::since(0, 0).expect("a span"),
            lines: Span::since(0, 4).expect("a span"),
        });
        table.check_invariants();
    }

    /// A script named relatively compiles with a `.` in its path, and its
    /// frames name the file without it, as breakpoints and listings do.
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
