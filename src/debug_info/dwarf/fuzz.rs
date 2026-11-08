//! The fuzz harness for loading a module's debug information from the
//! bytes of its file, and for every answer the loaded image then gives.
//!
//! An input that begins as an ELF file does is loaded as it is. Any other
//! is a program that writes one: DIEs and their attributes, line rows,
//! call-frame entries, symbols, and the sections language runtimes read,
//! written with `gimli::write` and `object::write`, so that the fuzzer
//! reaches far past the parsers' first checks. What the program leaves
//! after its end byte flips bytes of the file it wrote.

#![expect(
    clippy::indexing_slicing,
    reason = "a fuzz harness reports a bug by panicking"
)]

use gimli::write::{
    Address, AttributeValue, CallFrameInstruction, CommonInformationEntry, DebugFrame,
    DebugInfoRef, Dwarf, EhFrame, EndianVec, Expression, FileId, FrameDescriptionEntry, FrameTable,
    LineProgram, LineString, Location, LocationList, Range, RangeList, Sections, Unit, UnitEntryId,
    UnitId,
};
use gimli::{Encoding, Format, LineEncoding, LittleEndian, Register};
use object::write;

use super::LoadLimits;

/// Where the module is said to be: nowhere, so that no separate debug
/// file or `.dwo` file a relative name gives is found beside it.
const PATH: &str = "/nonexistent/microscope-fuzz/module";

/// Limits small enough that every input loads in milliseconds.
const LIMITS: LoadLimits = LoadLimits {
    per_input_byte: 32,
    floor: 1 << 20,
    walk_ahead: 4,
    most_decompressed: 16 << 20,
};

/// How many bytes of code the written module has, at address zero.
const CODE: usize = 0x200;

/// The deepest DIE the program nests, which keeps the writer's own
/// recursion shallow; the loader's bounds are still reached by flips.
const MOST_DEPTH: usize = 128;

pub(in crate::debug_info) fn fuzz_load(data: &[u8]) {
    let bytes = if data.starts_with(&object::elf::ELFMAG) {
        data.to_vec()
    } else {
        let mut input = Input(data);
        let Some(mut bytes) = write_module(&mut input) else {
            return;
        };

        if !bytes.is_empty() {
            for [low, high, flip] in input.0.as_chunks::<3>().0 {
                let at = usize::from(u16::from_le_bytes([*low, *high])) % bytes.len();
                bytes[at] ^= flip;
            }
        }

        bytes
    };

    let loaded = crate::pool::install_small(|| {
        super::load_debug_info(
            std::path::Path::new(PATH),
            &bytes,
            crate::ModuleImageId::new(0),
            &crate::debug_info::DebugFileSearch::default(),
            LIMITS,
            None,
        )
    });

    if let Ok(Ok((info, _))) = loaded {
        let _ = crate::tools::dump::dump_info(
            &info,
            &crate::tools::dump::Options::default(),
            &mut std::io::sink(),
        );
    }
}

/// The fuzzer's bytes, read from the front; past their end every read is
/// zero.
struct Input<'a>(&'a [u8]);

impl Input<'_> {
    const fn byte(&mut self) -> u8 {
        let Some((&byte, rest)) = self.0.split_first() else {
            return 0;
        };
        self.0 = rest;
        byte
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[usize::from(self.byte()) % from.len()]
    }

    const fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    /// A number that is usually small, and sometimes any.
    fn number(&mut self) -> u64 {
        match self.byte() {
            byte @ 0..0xf0 => u64::from(byte),
            0xf0..0xf8 => u64::from(self.u16()),
            _ => self.u64(),
        }
    }

    /// An address near the code, on either side of its ends.
    fn address(&mut self) -> u64 {
        u64::from(self.u16()) % (CODE as u64 + 0x40)
    }

    fn bytes(&mut self, most: usize) -> Vec<u8> {
        let length = (usize::from(self.byte()) % (most + 1)).min(self.0.len());
        let (bytes, rest) = self.0.split_at(length);
        self.0 = rest;
        bytes.to_vec()
    }

    /// A DWARF expression: a common first operation, then any bytes.
    fn expression(&mut self) -> Expression {
        let mut bytes = vec![self.pick(&OPERATIONS).0];
        bytes.extend(self.bytes(12));
        Expression::raw(bytes)
    }
}

const TAGS: [gimli::DwTag; 40] = [
    gimli::DW_TAG_subprogram,
    gimli::DW_TAG_variable,
    gimli::DW_TAG_formal_parameter,
    gimli::DW_TAG_lexical_block,
    gimli::DW_TAG_inlined_subroutine,
    gimli::DW_TAG_base_type,
    gimli::DW_TAG_pointer_type,
    gimli::DW_TAG_structure_type,
    gimli::DW_TAG_class_type,
    gimli::DW_TAG_union_type,
    gimli::DW_TAG_member,
    gimli::DW_TAG_inheritance,
    gimli::DW_TAG_enumeration_type,
    gimli::DW_TAG_enumerator,
    gimli::DW_TAG_array_type,
    gimli::DW_TAG_subrange_type,
    gimli::DW_TAG_generic_subrange,
    gimli::DW_TAG_typedef,
    gimli::DW_TAG_const_type,
    gimli::DW_TAG_volatile_type,
    gimli::DW_TAG_atomic_type,
    gimli::DW_TAG_restrict_type,
    gimli::DW_TAG_reference_type,
    gimli::DW_TAG_rvalue_reference_type,
    gimli::DW_TAG_ptr_to_member_type,
    gimli::DW_TAG_subroutine_type,
    gimli::DW_TAG_unspecified_type,
    gimli::DW_TAG_template_type_parameter,
    gimli::DW_TAG_template_value_parameter,
    gimli::DW_TAG_variant_part,
    gimli::DW_TAG_variant,
    gimli::DW_TAG_namespace,
    gimli::DW_TAG_call_site,
    gimli::DW_TAG_call_site_parameter,
    gimli::DW_TAG_GNU_call_site,
    gimli::DW_TAG_label,
    gimli::DW_TAG_imported_unit,
    gimli::DW_TAG_string_type,
    gimli::DW_TAG_dynamic_type,
    gimli::DW_TAG_unspecified_parameters,
];

const ATTRIBUTES: [gimli::DwAt; 52] = [
    gimli::DW_AT_name,
    gimli::DW_AT_linkage_name,
    gimli::DW_AT_MIPS_linkage_name,
    gimli::DW_AT_low_pc,
    gimli::DW_AT_high_pc,
    gimli::DW_AT_ranges,
    gimli::DW_AT_entry_pc,
    gimli::DW_AT_type,
    gimli::DW_AT_byte_size,
    gimli::DW_AT_bit_size,
    gimli::DW_AT_alignment,
    gimli::DW_AT_data_member_location,
    gimli::DW_AT_data_bit_offset,
    gimli::DW_AT_location,
    gimli::DW_AT_frame_base,
    gimli::DW_AT_decl_file,
    gimli::DW_AT_decl_line,
    gimli::DW_AT_call_file,
    gimli::DW_AT_call_line,
    gimli::DW_AT_call_column,
    gimli::DW_AT_abstract_origin,
    gimli::DW_AT_specification,
    gimli::DW_AT_declaration,
    gimli::DW_AT_external,
    gimli::DW_AT_artificial,
    gimli::DW_AT_inline,
    gimli::DW_AT_encoding,
    gimli::DW_AT_lower_bound,
    gimli::DW_AT_upper_bound,
    gimli::DW_AT_count,
    gimli::DW_AT_const_value,
    gimli::DW_AT_discr,
    gimli::DW_AT_discr_value,
    gimli::DW_AT_call_return_pc,
    gimli::DW_AT_call_pc,
    gimli::DW_AT_call_value,
    gimli::DW_AT_call_origin,
    gimli::DW_AT_call_target,
    gimli::DW_AT_low_pc,
    gimli::DW_AT_language,
    gimli::DW_AT_producer,
    gimli::DW_AT_comp_dir,
    gimli::DW_AT_GNU_dwo_name,
    gimli::DW_AT_GNU_dwo_id,
    gimli::DW_AT_containing_type,
    gimli::DW_AT_data_location,
    gimli::DW_AT_object_pointer,
    gimli::DW_AT_accessibility,
    gimli::DW_AT_import,
    gimli::DW_AT_noreturn,
    // Go's kind and runtime type, which gimli does not name.
    gimli::DwAt(0x2900),
    gimli::DwAt(0x2904),
];

/// Names that mean something to some loader: languages' own, and some
/// that are not text.
const NAMES: [&[u8]; 32] = [
    b"main",
    b"a",
    b"int",
    b"u8",
    b"i32",
    b"char",
    b"&str",
    b"data_ptr",
    b"length",
    b"pointer",
    b"__0",
    b"__1",
    b"Some",
    b"None",
    b"Option<i32>",
    b"RUST$ENUM$DISR",
    b"{closure_env#0}",
    b"{async_fn_env#0}",
    b"{async_block_env#0}",
    b"__awaitee",
    b"__state",
    b"Unresumed",
    b"Suspend0",
    b"_ZN3foo3barEv",
    b"_ZN4core3fmt5write17h0123456789abcdefE",
    b"main.main",
    b"runtime.goexit",
    b"runtime.g",
    b"a.c",
    b"module.dwo",
    b"\xff\xfe",
    b"",
];

const LANGUAGES: [gimli::DwLang; 7] = [
    gimli::DW_LANG_C11,
    gimli::DW_LANG_C_plus_plus_14,
    gimli::DW_LANG_Rust,
    gimli::DW_LANG_Go,
    gimli::DW_LANG_Zig,
    gimli::DW_LANG_Fortran90,
    gimli::DwLang(0x7fff),
];

const ENCODINGS: [gimli::DwAte; 8] = [
    gimli::DW_ATE_signed,
    gimli::DW_ATE_unsigned,
    gimli::DW_ATE_signed_char,
    gimli::DW_ATE_unsigned_char,
    gimli::DW_ATE_boolean,
    gimli::DW_ATE_float,
    gimli::DW_ATE_UTF,
    gimli::DwAte(0xee),
];

const OPERATIONS: [gimli::DwOp; 20] = [
    gimli::DW_OP_addr,
    gimli::DW_OP_fbreg,
    gimli::DW_OP_breg6,
    gimli::DW_OP_breg7,
    gimli::DW_OP_reg0,
    gimli::DW_OP_reg5,
    gimli::DW_OP_regx,
    gimli::DW_OP_call_frame_cfa,
    gimli::DW_OP_piece,
    gimli::DW_OP_bit_piece,
    gimli::DW_OP_stack_value,
    gimli::DW_OP_entry_value,
    gimli::DW_OP_implicit_value,
    gimli::DW_OP_implicit_pointer,
    gimli::DW_OP_GNU_parameter_ref,
    gimli::DW_OP_form_tls_address,
    gimli::DW_OP_constu,
    gimli::DW_OP_plus_uconst,
    gimli::DW_OP_deref,
    gimli::DW_OP_call4,
];

/// Sections the loaders of language runtimes and others read by name.
const SECTIONS: [&[u8]; 6] = [
    b".PyRuntime",
    b".gopclntab",
    b".go.buildinfo",
    b".note.gnu.build-id",
    b".debug_gdb_scripts",
    crate::view::embedded::SECTION.as_bytes(),
];

/// Symbols the loaders of language runtimes look for.
const SYMBOLS: [&[u8]; 9] = [
    b"main",
    b"Py_Version",
    b"v8dbg_off_fp_function",
    b"_ZN2v88internal7Version6major_E",
    b"_ZN2v88internal7Version6minor_E",
    b"v8_Default_embedded_blob_code_",
    b"Builtins_InterpreterEntryTrampoline",
    b"_ZN2v87Isolate10GetCurrentEv",
    b"runtime.text",
];

/// A unit being written, and the DIEs open in it.
struct Writing {
    id: UnitId,
    open: Vec<UnitEntryId>,
    entries: Vec<UnitEntryId>,
    files: Vec<FileId>,
    sequence: Option<u64>,
}

/// Writes the module the program at the front of `input` describes,
/// leaving `input` at what follows its end byte; `None` when the writer
/// refuses what it describes.
#[allow(
    clippy::too_many_lines,
    reason = "one interpreter, an arm for each operation"
)]
fn write_module(input: &mut Input<'_>) -> Option<Vec<u8>> {
    let mut dwarf = Dwarf::new();
    let mut units: Vec<Writing> = Vec::new();
    let mut every: Vec<(UnitId, UnitEntryId)> = Vec::new();
    let mut frames = FrameTable::default();
    let cie = frames.add_cie({
        let mut cie = CommonInformationEntry::new(
            Encoding {
                format: Format::Dwarf32,
                version: 1,
                address_size: 8,
            },
            1,
            -8,
            Register(16),
        );
        cie.add_instruction(CallFrameInstruction::Cfa(Register(7), 8));
        cie.add_instruction(CallFrameInstruction::Offset(Register(16), -8));
        cie
    });
    let mut debug_frame = false;

    // A function that saves the frame pointer and returns, over and over.
    let mut code = [0x55, 0x48, 0x89, 0xe5, 0x5d, 0xc3]
        .into_iter()
        .cycle()
        .take(CODE)
        .collect::<Vec<u8>>();
    let mut symbols = Vec::new();
    let mut sections = Vec::new();
    let architecture = match input.byte() % 16 {
        0 => object::Architecture::Aarch64,
        _ => object::Architecture::X86_64,
    };

    for _ in 0..4096 {
        if input.0.is_empty() {
            break;
        }

        let operation = input.byte();
        if operation == 0xff {
            break;
        }

        if units.is_empty() || operation.is_multiple_of(16) && units.len() < 4 {
            let encoding = Encoding {
                format: if input.byte().is_multiple_of(8) {
                    Format::Dwarf64
                } else {
                    Format::Dwarf32
                },
                version: 2 + u16::from(input.byte() % 4),
                address_size: 8,
            };
            let program = LineProgram::new(
                encoding,
                LineEncoding::default(),
                LineString::String(b"/nonexistent".to_vec()),
                None,
                LineString::String(b"a.c".to_vec()),
                None,
            );
            let mut unit = Unit::new(encoding, program);
            let root = unit.root();
            let language = input.pick(&LANGUAGES);
            unit.get_mut(root)
                .set(gimli::DW_AT_language, AttributeValue::Language(language));
            unit.get_mut(root)
                .set(gimli::DW_AT_name, AttributeValue::String(b"a.c".to_vec()));
            let directory = unit.line_program.default_directory();
            let files = [b"a.c".as_slice(), b"b.rs", b"c.go"]
                .into_iter()
                .map(|name| {
                    unit.line_program
                        .add_file(LineString::String(name.to_vec()), directory, None)
                })
                .collect();
            let id = dwarf.units.add(unit);
            units.push(Writing {
                id,
                open: vec![root],
                entries: vec![root],
                files,
                sequence: None,
            });
            continue;
        }

        let current = units.len() - 1;
        let writing = &mut units[current];
        let unit = dwarf.units.get_mut(writing.id);
        let parent = *writing.open.last()?;

        match operation % 16 {
            // A DIE, open until it is closed.
            1..=3 => {
                let tag = input.pick(&TAGS);
                let entry = unit.add(parent, tag);
                writing.entries.push(entry);
                every.push((writing.id, entry));
                if writing.open.len() < MOST_DEPTH {
                    writing.open.push(entry);
                }
            }

            4 => {
                if writing.open.len() > 1 {
                    writing.open.pop();
                }
            }

            // An attribute of the DIE open last.
            5..=8 => {
                let name = input.pick(&ATTRIBUTES);
                let value = match input.byte() % 22 {
                    0 => AttributeValue::Udata(input.number()),
                    1 => AttributeValue::Sdata(input.number().cast_signed()),
                    2 => AttributeValue::Data1(input.byte()),
                    3 => AttributeValue::Data2(input.u16()),
                    4 => AttributeValue::Data4(u32::from(input.u16())),
                    5 => AttributeValue::Data8(input.number()),
                    6 => AttributeValue::Address(Address::Constant(input.address())),
                    7 => AttributeValue::String(input.pick(&NAMES).to_vec()),
                    8 => AttributeValue::StringRef(dwarf.strings.add(input.pick(&NAMES))),
                    9 => AttributeValue::Flag(input.byte() & 1 == 1),
                    10 => AttributeValue::FlagPresent,
                    11 | 12 => AttributeValue::UnitRef(input.pick(&writing.entries)),
                    13 => AttributeValue::Exprloc(input.expression()),
                    14 => AttributeValue::Block(input.bytes(16)),
                    15 => AttributeValue::Language(input.pick(&LANGUAGES)),
                    16 => AttributeValue::Encoding(input.pick(&ENCODINGS)),
                    17 => {
                        let mut list = Vec::new();
                        for _ in 0..input.byte() % 4 {
                            let begin = input.address();
                            // gimli's writer adds the length to the start
                            // unchecked before DWARF 5.
                            list.push(Location::StartLength {
                                begin: Address::Constant(begin),
                                length: input.number().min(u64::MAX - begin),
                                data: input.expression(),
                            });
                        }
                        if input.byte().is_multiple_of(4) {
                            list.push(Location::DefaultLocation {
                                data: input.expression(),
                            });
                        }
                        AttributeValue::LocationListRef(unit.locations.add(LocationList(list)))
                    }
                    18 => {
                        let mut list = Vec::new();
                        for _ in 0..input.byte() % 4 {
                            let begin = input.address();
                            list.push(Range::StartLength {
                                begin: Address::Constant(begin),
                                length: input.number().min(u64::MAX - begin),
                            });
                        }
                        AttributeValue::RangeListRef(unit.ranges.add(RangeList(list)))
                    }
                    19 => AttributeValue::FileIndex(Some(input.pick(&writing.files))),
                    20 if !every.is_empty() => {
                        let (unit, entry) = input.pick(&every);
                        AttributeValue::DebugInfoRef(DebugInfoRef::Entry(unit, entry))
                    }
                    _ => AttributeValue::Inline(gimli::DwInl(input.byte() % 4)),
                };
                unit.get_mut(parent).set(name, value);
            }

            // A row of the unit's line program, at an address no lower than
            // the row before it.
            9 | 10 => {
                let program = &mut unit.line_program;
                if writing.sequence.is_none() {
                    let start = input.address();
                    program.begin_sequence(Some(Address::Constant(start)));
                    writing.sequence = Some(start);
                }

                let row = program.row();
                row.address_offset += u64::from(input.byte() % 16);
                row.line = u64::from(input.u16() % 64);
                row.column = u64::from(input.byte() % 8);
                row.file = writing.files[usize::from(input.byte()) % writing.files.len()];
                let flags = input.byte();
                row.is_statement = flags & 1 == 0;
                row.prologue_end = flags & 2 != 0;
                row.epilogue_begin = flags & 4 != 0;
                row.discriminator = u64::from(flags >> 6);
                program.generate_row();
            }

            11 => {
                if writing.sequence.take().is_some() {
                    let end = unit.line_program.row().address_offset + u64::from(input.byte());
                    unit.line_program.end_sequence(end);
                }
            }

            // A call-frame entry, its instructions in the order of their
            // offsets.
            12 => {
                debug_frame |= input.byte() & 1 == 1;
                let start = input.address();
                let length = u32::from(input.byte());
                let mut fde = FrameDescriptionEntry::new(Address::Constant(start), length);
                let mut offset = 0;
                for _ in 0..input.byte() % 6 {
                    offset += u32::from(input.byte() % 8);
                    let register = Register(u16::from(input.byte() % 18));
                    let instruction = match input.byte() % 14 {
                        0 => CallFrameInstruction::CfaOffset(i32::from(input.byte()) * 8),
                        1 => CallFrameInstruction::CfaRegister(register),
                        2 => CallFrameInstruction::Cfa(register, i32::from(input.byte())),
                        3 => CallFrameInstruction::Offset(register, -i32::from(input.byte())),
                        4 => CallFrameInstruction::ValOffset(register, i32::from(input.byte())),
                        5 => CallFrameInstruction::Restore(register),
                        6 => CallFrameInstruction::Undefined(register),
                        7 => CallFrameInstruction::SameValue(register),
                        8 => CallFrameInstruction::Register(register, Register(6)),
                        9 => CallFrameInstruction::RememberState,
                        10 => CallFrameInstruction::RestoreState,
                        11 => CallFrameInstruction::CfaExpression(input.expression()),
                        12 => CallFrameInstruction::Expression(register, input.expression()),
                        _ => CallFrameInstruction::ValExpression(register, input.expression()),
                    };
                    fde.add_instruction(offset, instruction);
                }
                frames.add_fde(cie, fde);
            }

            // A symbol.
            13 => {
                let name = input.pick(&SYMBOLS).to_vec();
                let kind = input.byte();
                let value = input.address();
                let size = input.number();
                symbols.push((name, kind, value, size));
            }

            // A section a runtime's loader reads, or bytes of code.
            14 => {
                let name = input.pick(&SECTIONS);
                let mut data = Vec::new();
                if name == b".PyRuntime" && input.byte() & 1 == 0 {
                    data.extend_from_slice(b"xdebugpy");
                }
                data.extend(input.bytes(64));
                sections.push((name, data));
            }

            _ => {
                let at = usize::from(input.u16()) % CODE;
                for (to, byte) in code[at..].iter_mut().zip(input.bytes(16)) {
                    *to = byte;
                }
            }
        }
    }

    for writing in &mut units {
        if writing.sequence.is_some() {
            let unit = dwarf.units.get_mut(writing.id);
            let end = unit.line_program.row().address_offset + 1;
            unit.line_program.end_sequence(end);
        }
    }

    let mut written = Sections::new(EndianVec::new(LittleEndian));
    dwarf.write(&mut written).ok()?;

    let mut object = write::Object::new(
        object::BinaryFormat::Elf,
        architecture,
        object::Endianness::Little,
    );
    let text = object.add_section(Vec::new(), b".text".to_vec(), object::SectionKind::Text);
    object.append_section_data(text, &code, 16);
    let data = object.add_section(Vec::new(), b".data".to_vec(), object::SectionKind::Data);
    object.append_section_data(data, &[0; 64], 8);

    let mut add_debug = |name: &[u8], bytes: &[u8]| {
        if !bytes.is_empty() {
            let section = object.add_section(Vec::new(), name.to_vec(), object::SectionKind::Debug);
            object.append_section_data(section, bytes, 1);
        }
    };

    written
        .for_each(|id, section| -> Result<(), ()> {
            add_debug(id.name().as_bytes(), section.slice());
            Ok(())
        })
        .ok()?;

    if debug_frame {
        let mut section = DebugFrame(EndianVec::new(LittleEndian));
        frames.write_debug_frame(&mut section).ok()?;
        add_debug(b".debug_frame", section.slice());
    } else {
        let mut section = EhFrame(EndianVec::new(LittleEndian));
        frames.write_eh_frame(&mut section).ok()?;
        let eh_frame = object.add_section(
            Vec::new(),
            b".eh_frame".to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        object.append_section_data(eh_frame, section.slice(), 8);
    }

    for (name, bytes) in sections {
        let section = object.add_section(Vec::new(), name.to_vec(), object::SectionKind::Data);
        object.append_section_data(section, &bytes, 1);
    }

    for (name, kind, value, size) in symbols {
        let (section, st_type) = match kind % 4 {
            0 | 1 => (text, object::elf::STT_FUNC),
            2 => (data, object::elf::STT_OBJECT),
            _ => (text, object::elf::STT_NOTYPE),
        };
        object.add_symbol(write::Symbol {
            name,
            value,
            size,
            kind: object::SymbolKind::Unknown,
            scope: object::SymbolScope::Dynamic,
            weak: false,
            section: write::SymbolSection::Section(section),
            flags: object::SymbolFlags::Elf {
                st_info: (object::elf::STB_GLOBAL << 4) | st_type,
                st_other: 0,
            },
        });
    }

    object.write().ok()
}
