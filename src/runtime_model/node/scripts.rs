//! A process-lifetime table of scripts and functions, like Python's
//! `CodeTable`: each row is found by its object's address and proven again
//! at every use, so a row for an object V8's GC moved, or for an address
//! reused by another object, misses instead of misnaming.
//!
//! A script is proven by its id, which V8 never reuses within an isolate,
//! and its source string. A function's shared info is proven by its
//! script's address, id, source, and its function literal id, which together
//! name one function literal, and so one name. Line ends are decoded once
//! per script, as far as a frame has needed them.

use std::collections::HashMap;
use std::sync::Arc;

use super::objects::{Object, Problem, Reader};
use crate::image::v8::{Constant, Field};

/// The most rows of each kind a table keeps before it begins again.
const MAX_ROWS: usize = 1 << 16;

#[derive(Debug, Default)]
pub(super) struct Table {
    scripts: HashMap<u64, Script>,
    functions: HashMap<u64, Function>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScriptProof {
    id: i64,
    source: u64,
}

#[derive(Debug)]
struct Script {
    proof: ScriptProof,
    path: Option<Arc<str>>,
    lines: Lines,
}

/// The newline offsets read so far: from V8's own line ends, the array at
/// `ends_word`, or else from the source, through `scanned`.
#[derive(Debug, Default)]
struct Lines {
    ends: Vec<u64>,
    ends_word: Option<u64>,
    scanned: u64,
    complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FunctionProof {
    script: u64,
    script_proof: ScriptProof,
    literal: i32,
}

#[derive(Debug)]
struct Function {
    proof: FunctionProof,
    name: Arc<str>,
}

impl Table {
    /// The script row of `script`, proven against the script as it is now.
    fn script(&mut self, reader: &Reader<'_>, script: Object) -> Result<&mut Script, Problem> {
        let proof = ScriptProof {
            id: reader.smi_field(script, Field::ScriptId)?,
            source: reader.field(script, Field::ScriptSource)?,
        };
        let fresh = self
            .scripts
            .get(&script.0)
            .is_none_or(|row| row.proof != proof);
        if fresh {
            if self.scripts.len() >= MAX_ROWS {
                self.scripts.clear();
            }
            let path = super::frames::script_name(reader, script)?
                .map(|name| super::frames::file_url_path(&name).unwrap_or(name).into());
            self.scripts.insert(
                script.0,
                Script {
                    proof,
                    path,
                    lines: Lines::default(),
                },
            );
        }
        self.scripts
            .get_mut(&script.0)
            .ok_or_else(|| "the script's row is missing".into())
    }

    /// The path `script` names, as Node names its file.
    pub(super) fn path(
        &mut self,
        reader: &Reader<'_>,
        script: Object,
    ) -> Result<Option<Arc<str>>, Problem> {
        Ok(self.script(reader, script)?.path.clone())
    }

    /// The name of the function `shared` describes, as
    /// `SharedFunctionInfo::DebugName` gives it.
    pub(super) fn function_name(
        &mut self,
        reader: &Reader<'_>,
        shared: Object,
    ) -> Result<Arc<str>, Problem> {
        // A function of no script, as a native one, has no proof.
        let Some(script) = super::frames::script(reader, shared)? else {
            return Ok(super::names::function_name(reader, shared)?.into());
        };
        let proof = FunctionProof {
            script: script.0,
            script_proof: ScriptProof {
                id: reader.smi_field(script, Field::ScriptId)?,
                source: reader.field(script, Field::ScriptSource)?,
            },
            literal: reader.int32_field(shared, Field::SharedFunctionInfoFunctionLiteralId)?,
        };
        if let Some(row) = self.functions.get(&shared.0)
            && row.proof == proof
        {
            return Ok(Arc::clone(&row.name));
        }
        let name: Arc<str> = super::names::function_name(reader, shared)?.into();
        if self.functions.len() >= MAX_ROWS {
            self.functions.clear();
        }
        self.functions.insert(
            shared.0,
            Function {
                proof,
                name: Arc::clone(&name),
            },
        );
        Ok(name)
    }

    /// The line and column, each from 1, of the character `offset` of
    /// `script`.
    pub(super) fn line_column(
        &mut self,
        reader: &Reader<'_>,
        script: Object,
        offset: u64,
    ) -> Result<(u32, u32), Problem> {
        let row = self.script(reader, script)?;
        row.lines.cover(reader, script, offset)?;
        let ends = &row.lines.ends;
        // Lines end at their newline; the line of `offset` is the first whose
        // end is at or past it.
        let line = ends.partition_point(|end| *end < offset);
        let start = line.checked_sub(1).map_or(0, |previous| ends[previous] + 1);
        let mut column = offset - start;
        if line == 0 {
            let first = reader.smi_field(script, Field::ScriptColumnOffset)?;
            column = column.saturating_add_signed(first);
        }
        let line_offset = reader.smi_field(script, Field::ScriptLineOffset)?;
        let line = i64::try_from(line).map_err(|_| "a line is out of range")? + line_offset;
        let line = u32::try_from(line + 1).map_err(|_| "a line is out of range")?;
        let column = u32::try_from(column + 1).map_err(|_| "a column is out of range")?;
        Ok((line, column))
    }
}

/// The most source characters scanned for newlines at once.
const SCAN: u64 = 1 << 16;

const MAX_LINE_ENDS: usize = 1 << 22;
const MAX_SOURCE_SCAN: u64 = 1 << 24;

impl Lines {
    /// Reads newline offsets until one is at or past `offset`, or the
    /// script ends: from V8's line ends where it computed them, else from
    /// the source.
    fn cover(&mut self, reader: &Reader<'_>, script: Object, offset: u64) -> Result<(), Problem> {
        let word = reader.field(script, Field::ScriptLineEnds)?;
        let computed = reader
            .object(word)?
            .filter(|ends| reader.is(*ends, Constant::FixedArrayType).unwrap_or(false));
        if computed.map(|ends| ends.0) != self.ends_word {
            *self = Self {
                ends_word: computed.map(|ends| ends.0),
                ..Self::default()
            };
        }
        if self.complete || self.ends.last().is_some_and(|end| *end >= offset) {
            return Ok(());
        }

        if let Some(ends) = computed {
            let count = u64::try_from(reader.smi_field(ends, Field::FixedArrayBaseLength)?)
                .map_err(|_| "a script's line ends have a negative length")?;
            if count > MAX_LINE_ENDS as u64 {
                return Err("a script exceeds the line ends limit".into());
            }
            let first = reader.field_address(ends, Field::FixedArrayObjects)?;
            let mut index = self.ends.len() as u64;
            while index < count {
                let end = reader
                    .smi(reader.word(first + index * 8)?)?
                    .and_then(|end| u64::try_from(end).ok())
                    .ok_or("a script's line end is not an offset")?;
                self.ends.push(end);
                index += 1;
                if end >= offset {
                    return Ok(());
                }
            }
            self.complete = true;
            return Ok(());
        }

        let source = reader
            .object(reader.field(script, Field::ScriptSource)?)?
            .ok_or("the script keeps no source and no line ends")?;
        let length = u64::try_from(reader.int32_field(source, Field::StringLength)?)
            .map_err(|_| "a script's source has a negative length")?;
        let target = offset.saturating_add(1).min(length);
        if target > MAX_SOURCE_SCAN {
            return Err("the script's offset exceeds the source scan limit".into());
        }
        let mut text = Vec::new();
        while self.scanned < target {
            let limit = (target - self.scanned).min(SCAN);
            text.clear();
            // Look ahead one unit so a CRLF straddling reads ends at its LF.
            reader.string_into(source, self.scanned, limit + 1, 0, &mut text)?;
            let scanned = self.scanned;
            let mut ends = Vec::new();
            let consumed = text.len().min(limit as usize);
            if consumed == 0 {
                return Err("the script's source ends before its declared length".into());
            }
            for (at, &unit) in text.iter().take(consumed).enumerate() {
                let end = scanned + at as u64;
                match unit {
                    0x0d => ends.push(end + u64::from(text.get(at + 1) == Some(&0x0a))),
                    0x0a if ends.last().or_else(|| self.ends.last()) != Some(&end) => {
                        ends.push(end);
                    }
                    0x2028 | 0x2029 => ends.push(end),
                    _ => {}
                }
            }
            if ends.len() > MAX_LINE_ENDS - self.ends.len() {
                return Err("a script exceeds the line ends limit".into());
            }
            self.ends.extend(ends);
            self.scanned += consumed as u64;
        }
        self.complete = self.scanned >= length;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::objects::{Object, Reader};
    use super::Table;
    use crate::image::v8::{Constant, Field, V8Layout, V8Version};
    use crate::runtime_model::RuntimeStop;
    use crate::{LoadBias, ThreadId, VirtualAddress};

    /// A V8 heap the tests write: bytes by address.
    #[derive(Default)]
    struct Heap(HashMap<u64, u8>);

    impl Heap {
        fn bytes(&mut self, address: u64, bytes: &[u8]) {
            for (at, byte) in (address..).zip(bytes) {
                self.0.insert(at, *byte);
            }
        }

        fn word(&mut self, address: u64, word: u64) {
            self.bytes(address, &word.to_le_bytes());
        }
    }

    impl RuntimeStop for Heap {
        fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
            for (at, byte) in (address.get()..).zip(bytes.iter_mut()) {
                let Some(read) = self.0.get(&at) else {
                    return false;
                };
                *byte = *read;
            }
            true
        }

        fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
            None
        }

        fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
            None
        }

        fn load_bias(&self) -> LoadBias {
            LoadBias::default()
        }

        fn threads(&self) -> Vec<ThreadId> {
            Vec::new()
        }
    }

    /// 14.6's generated layout, with the tags each build exports.
    fn layout() -> V8Layout {
        let mut layout = crate::debug_info::v8::generated_layout(14, 6).expect("a layout");
        for (constant, value) in [
            (Constant::SmiTag, 0),
            (Constant::SmiTagMask, 1),
            (Constant::SmiShiftSize, 31),
            (Constant::HeapObjectTag, 1),
            (Constant::HeapObjectTagMask, 3),
            (Constant::StringRepresentationMask, 7),
            (Constant::SeqStringTag, 0),
            (Constant::ExternalStringTag, 2),
            (Constant::StringEncodingMask, 8),
            (Constant::OneByteStringTag, 8),
            (Constant::FirstNonstringType, 128),
        ] {
            layout.set_constant(constant, value);
        }
        layout
    }

    fn reader<'a>(heap: &'a Heap, layout: &'a V8Layout) -> Reader<'a> {
        Reader {
            stop: heap,
            layout,
            version: V8Version {
                major: 14,
                minor: 6,
                ..V8Version::default()
            },
            charge: None,
            blocks: None,
        }
    }

    const fn smi(value: u64) -> u64 {
        value << 32
    }

    /// Writes into `heap` the objects the tests' rows need.
    struct Writer<'a> {
        heap: &'a mut Heap,
        layout: &'a V8Layout,
    }

    impl Writer<'_> {
        fn offset(&self, field: Field) -> u64 {
            self.layout.field(field).expect("a field")
        }

        /// A map at `at` for objects of instance type `ty`; tagged.
        fn map(&mut self, at: u64, ty: u16) -> u64 {
            let instance_type = self.offset(Field::MapInstanceType);
            self.heap.word(at, at + 1);
            self.heap.bytes(at + instance_type, &ty.to_le_bytes());
            at + 1
        }

        fn field(&mut self, object: u64, field: Field, word: u64) {
            let at = object - 1 + self.offset(field);
            self.heap.word(at, word);
        }

        /// A sequential one-byte string at `at`; tagged.
        fn string(&mut self, at: u64, map: u64, text: &str) -> u64 {
            let length = u32::try_from(text.len()).expect("a short string");
            self.heap.word(at, map);
            let length_at = at + self.offset(Field::StringLength);
            self.heap.bytes(length_at, &length.to_le_bytes());
            let chars = at + self.offset(Field::SeqOneByteStringChars);
            self.heap.bytes(chars, text.as_bytes());
            at + 1
        }

        fn string_units(&mut self, at: u64, map: u64, text: &[u16]) -> u64 {
            self.heap.word(at, map);
            let length_at = at + self.offset(Field::StringLength);
            self.heap
                .bytes(length_at, &u32::try_from(text.len()).unwrap().to_le_bytes());
            let chars = at + self.offset(Field::SeqTwoByteStringChars);
            for (index, unit) in text.iter().enumerate() {
                self.heap
                    .bytes(chars + index as u64 * 2, &unit.to_le_bytes());
            }
            at + 1
        }

        /// A script at `at`, numbered `id`, named `name`, of `source`.
        fn script(&mut self, at: u64, map: u64, (id, name, source): (u64, u64, u64)) -> u64 {
            let script = at + 1;
            self.heap.word(at, map);
            self.field(script, Field::ScriptId, smi(id));
            self.field(script, Field::ScriptName, name);
            self.field(script, Field::ScriptSourceUrl, smi(0));
            self.field(script, Field::ScriptSource, source);
            self.field(script, Field::ScriptLineEnds, smi(0));
            self.field(script, Field::ScriptLineOffset, smi(0));
            self.field(script, Field::ScriptColumnOffset, smi(0));
            script
        }

        /// A shared function info at `at` of the literal `literal` of
        /// `script`, named by the string `name`.
        fn shared(&mut self, at: u64, map: u64, script: u64, literal: u32, name: u64) -> u64 {
            let shared = at + 1;
            self.heap.word(at, map);
            self.field(shared, Field::SharedFunctionInfoScript, script);
            self.field(shared, Field::SharedFunctionInfoNameOrScopeInfo, name);
            let literal_at = at + self.offset(Field::SharedFunctionInfoFunctionLiteralId);
            self.heap.bytes(literal_at, &literal.to_le_bytes());
            shared
        }
    }

    #[test]
    fn source_scanning_counts_every_javascript_line_terminator() {
        let layout = layout();
        for separator in ["\n", "\r", "\r\n", "\u{2028}", "\u{2029}"] {
            let mut heap = Heap::default();
            let mut writer = Writer {
                heap: &mut heap,
                layout: &layout,
            };
            let string_map = writer.map(0x1000, 0);
            let source: Vec<_> = format!("a{separator}b").encode_utf16().collect();
            let source = writer.string_units(0x2000, string_map, &source);
            let name = writer.string_units(0x2100, string_map, &['s' as u16]);
            let script = writer.script(0x3000, 0, (1, name, source));
            let mut table = Table::default();
            let reader = reader(&heap, &layout);
            let at = 1 + separator.encode_utf16().count() as u64;
            assert_eq!(
                table.line_column(&reader, Object(script), at).unwrap(),
                (2, 1),
                "{separator:?}"
            );
            assert_eq!(
                table.line_column(&reader, Object(script), 1).unwrap(),
                (1, 2),
                "{separator:?}"
            );
        }
    }

    #[test]
    fn source_scanning_coalesces_crlf_at_a_read_boundary() {
        let layout = layout();
        let mut heap = Heap::default();
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        let map = writer.map(0x1000, 0);
        let scan = u32::try_from(super::SCAN).unwrap();
        let mut text = vec![u16::from(b'a'); usize::try_from(scan).unwrap() - 1];
        text.extend("\r\nb".encode_utf16());
        let source = writer.string_units(0x10000, map, &text);
        let name = writer.string_units(0x2000, map, &['s' as u16]);
        let script = Object(writer.script(0x3000, 0, (1, name, source)));
        let mut table = Table::default();
        let reader = reader(&heap, &layout);
        assert_eq!(
            table.line_column(&reader, script, super::SCAN - 1).unwrap(),
            (1, scan)
        );
        assert_eq!(
            table.line_column(&reader, script, super::SCAN).unwrap(),
            (1, scan + 1)
        );
        assert_eq!(
            table.line_column(&reader, script, super::SCAN + 1).unwrap(),
            (2, 1)
        );
    }

    #[test]
    fn oversized_line_end_arrays_report_a_limit_instead_of_a_prefix() {
        let layout = layout();
        let mut heap = Heap::default();
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        let ty = u16::try_from(layout.constant(Constant::FixedArrayType).unwrap()).unwrap();
        let map = writer.map(0x1000, ty);
        let script = Object(writer.script(0x2000, 0, (1, 0, 0)));
        let ends = 0x3001;
        writer.heap.word(0x3000, map);
        writer.field(ends, Field::FixedArrayBaseLength, smi((1 << 22) + 1));
        writer.field(ends, Field::FixedArrayObjects, smi(0));
        writer.field(script.0, Field::ScriptLineEnds, ends);
        let mut lines = super::Lines::default();
        let problem = lines
            .cover(&reader(&heap, &layout), script, 0)
            .expect_err("the declared line count exceeds the limit");
        assert!(problem.contains("limit"), "{problem}");
    }

    #[test]
    fn source_scanning_refuses_offsets_beyond_its_total_work_limit() {
        let layout = layout();
        let mut heap = Heap::default();
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        let map = writer.map(0x1000, 8);
        let source = writer.string(0x2000, map, "");
        let length_at = 0x2000 + writer.offset(Field::StringLength);
        writer
            .heap
            .bytes(length_at, &((1_i32 << 24) + 1).to_le_bytes());
        let script = Object(writer.script(0x3000, 0, (1, 0, source)));
        let mut lines = super::Lines::default();
        let problem = lines
            .cover(&reader(&heap, &layout), script, 1 << 24)
            .expect_err("the source scan is bounded");
        assert!(problem.contains("scan limit"), "{problem}");
    }

    #[test]
    fn identity_strings_report_their_limit_instead_of_truncating() {
        let layout = layout();
        let mut heap = Heap::default();
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        let map = writer.map(0x1000, 8);
        let text = "x".repeat(usize::try_from(super::super::objects::MAX_STRING).unwrap());
        let string = Object(writer.string(0x10000, map, &text));
        assert_eq!(reader(&heap, &layout).string(string).unwrap(), text);
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        writer.string(0x10000, map, &(text + "x"));
        let problem = reader(&heap, &layout)
            .string(string)
            .expect_err("the string exceeds the limit");
        assert!(problem.contains("limit"), "{problem}");
    }

    #[test]
    fn uncached_external_strings_do_not_read_the_adjacent_word_as_data() {
        let layout = layout();
        let mut heap = Heap::default();
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        let cached = writer.map(0x1000, 8 | 2);
        let uncached = writer.map(0x1100, 8 | 2 | 16);
        writer.heap.word(0x2000, cached);
        writer.heap.bytes(
            0x2000 + writer.offset(Field::StringLength),
            &3_i32.to_le_bytes(),
        );
        writer.field(0x2001, Field::ExternalStringResourceData, 0x3000);
        writer.heap.bytes(0x3000, b"bad");
        assert_eq!(
            reader(&heap, &layout).string(Object(0x2001)).unwrap(),
            "bad"
        );
        heap.word(0x2000, uncached);
        let problem = reader(&heap, &layout)
            .string(Object(0x2001))
            .expect_err("an uncached external string has no cached data field");
        assert!(problem.contains("uncached"), "{problem}");
    }

    #[test]
    fn function_rows_miss_when_the_script_source_changes_without_its_id() {
        let layout = layout();
        let mut heap = Heap::default();
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        let string_map = writer.map(0x1000, 8);
        let script_map = writer.map(
            0x1100,
            u16::try_from(layout.constant(Constant::ScriptType).unwrap()).unwrap(),
        );
        let first = writer.string(0x2000, string_map, "first");
        let second = writer.string(0x2100, string_map, "second");
        let source_a = writer.string(0x2200, string_map, "a");
        let source_b = writer.string(0x2300, string_map, "b");
        let script = writer.script(0x3000, script_map, (7, first, source_a));
        let shared = Object(writer.shared(0x4000, 0, script, 1, first));
        let mut table = Table::default();
        assert_eq!(
            &*table
                .function_name(&reader(&heap, &layout), shared)
                .unwrap(),
            "first"
        );
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        writer.script(0x3000, script_map, (7, second, source_b));
        writer.shared(0x4000, 0, script, 1, second);
        assert_eq!(
            &*table
                .function_name(&reader(&heap, &layout), shared)
                .unwrap(),
            "second"
        );
    }

    /// A row answers only for the object it was made from: a function or
    /// script that another one replaced at the same address, as after V8's
    /// GC moves objects, is read again.
    #[test]
    fn a_row_misses_when_another_object_takes_its_address() {
        let layout = layout();
        let ty = |constant| u16::try_from(layout.constant(constant).expect("a type")).expect("u16");
        let mut heap = Heap::default();
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        let string_map = writer.map(0x1000, 8);
        let script_map = writer.map(0x1100, ty(Constant::ScriptType));
        let shared_map = writer.map(0x1200, ty(Constant::SharedFunctionInfoType));
        let first = writer.string(0x2000, string_map, "first");
        let second = writer.string(0x2100, string_map, "second");
        let a = writer.string(0x2200, string_map, "/a.js");
        let b = writer.string(0x2300, string_map, "/b.js");
        let a_source = writer.string(0x3000, string_map, "x\nyy\nzzz\n");
        let b_source = writer.string(0x3100, string_map, "one line, then\nanother");
        let script = writer.script(0x4000, script_map, (7, a, a_source));
        let shared = writer.shared(0x5000, shared_map, script, 1, first);

        let version = V8Version {
            major: 14,
            minor: 6,
            ..V8Version::default()
        };
        let mut table = Table::default();
        let read = |heap: &Heap, table: &mut Table| {
            let reader = Reader {
                stop: heap,
                layout: &layout,
                version,
                charge: None,
                blocks: None,
            };
            let (script, shared) = (Object(script), Object(shared));
            (
                table
                    .function_name(&reader, shared)
                    .expect("a name")
                    .to_string(),
                table
                    .path(&reader, script)
                    .expect("a path")
                    .as_deref()
                    .map(str::to_owned),
                // "zzz" starts at 5; its second character is line 3, column 2.
                table.line_column(&reader, script, 6).expect("a line"),
                table.line_column(&reader, script, 1).expect("a line"),
            )
        };
        let expected =
            |name: &str, path: &str, far, near| (name.to_owned(), Some(path.to_owned()), far, near);
        assert_eq!(
            read(&heap, &mut table),
            expected("first", "/a.js", (3, 2), (1, 2))
        );

        // Another literal of the same script takes the function's address.
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        writer.shared(0x5000, shared_map, script, 2, second);
        assert_eq!(
            read(&heap, &mut table),
            expected("second", "/a.js", (3, 2), (1, 2))
        );

        // Another script, with other lines, takes the script's address.
        let mut writer = Writer {
            heap: &mut heap,
            layout: &layout,
        };
        writer.script(0x4000, script_map, (8, b, b_source));
        assert_eq!(
            read(&heap, &mut table),
            expected("second", "/b.js", (1, 7), (1, 2))
        );
    }
}
