//! Code a just-in-time compiler wrote to anonymous memory, as the perf map
//! it keeps describes it: `/tmp/perf-PID.map`, a line for each function,
//! `START SIZE NAME`, the first two in hexadecimal. CPython writes one
//! under `-X perf`, and Node and the JVM can.
//!
//! The map is all a function has: no file holds its code, no build ID
//! proves which code a name belongs to, and no call-frame information
//! describes its frames. Its frames are proved from its instructions
//! instead, read from the process when the map is (see
//! [`super::x86_64::frames`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use super::x86_64::frames::{CallerFramePointer, CodeFrame, CodeFrames};
use super::{DebugInfo, UnwindInfo};
use crate::unwind::{MemoryReader, RBP, RETURN_ADDRESS, RSP, RegisterFile, UnwindStep};
use crate::{
    AddressRange, CodeRole, ImageAddress, ModuleImageId, SymbolBinding, SymbolExtent,
    SymbolExtentProvenance, SymbolId, SymbolInfo, SymbolKind, UnwindTermination, VirtualAddress,
};

/// One function a perf map names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub start: u64,
    pub end: u64,
    pub name: Arc<str>,
}

/// The functions the complete lines of `text` name, and how many bytes
/// those lines take: a map is written as it is read, so its last line may
/// be cut short. A line that names no code is skipped.
pub fn parse(text: &[u8]) -> (Vec<Entry>, usize) {
    let complete = text
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |last| last + 1);
    let entries = text[..complete]
        .split(|byte| *byte == b'\n')
        .filter_map(entry)
        .collect();

    (entries, complete)
}

fn entry(line: &[u8]) -> Option<Entry> {
    let mut fields = line.splitn(3, |byte| *byte == b' ');
    let hex = |field: &[u8]| {
        let text = std::str::from_utf8(field).ok()?;
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .unwrap_or(text);
        u64::from_str_radix(digits, 16).ok()
    };

    let start = hex(fields.next()?)?;
    let size = hex(fields.next()?)?;
    let name = String::from_utf8_lossy(fields.next()?);
    let end = start.checked_add(size).filter(|end| *end > start)?;

    Some(Entry {
        start,
        end,
        name: name.trim_end_matches('\r').into(),
    })
}

/// One function of a map, with its code as far as it could be read.
#[derive(Debug, Clone)]
pub struct Jitted {
    pub entry: Entry,
    pub code: Option<Arc<[u8]>>,
}

/// The debug information of the anonymous executable memory `range`
/// covers, at the addresses the process runs it, which `jitted` describe
/// in the order the map named them. A later function replaces each earlier
/// one it overlaps, as a compiler that frees code reuses its memory.
/// Functions outside the range are left out.
pub fn load(
    path: &Path,
    id: ModuleImageId,
    range: AddressRange<ImageAddress>,
    jitted: &[Jitted],
) -> Result<DebugInfo, crate::model::SealError> {
    let mut functions: BTreeMap<u64, &Jitted> = BTreeMap::new();
    for function in jitted {
        let Entry { start, end, .. } = function.entry;
        if start < range.start.get() || end > range.end.get() {
            continue;
        }

        let overlapped: Vec<u64> = functions
            .range(..end)
            .rev()
            .take_while(|(_, earlier)| earlier.entry.end > start)
            .map(|(start, _)| *start)
            .collect();
        for start in overlapped {
            functions.remove(&start);
        }

        functions.insert(start, function);
    }

    let symbols = functions
        .values()
        .enumerate()
        .map(|(index, function)| {
            let range = AddressRange {
                start: ImageAddress::new(function.entry.start),
                end: ImageAddress::new(function.entry.end),
            };
            SymbolInfo {
                id: SymbolId::new(u32::try_from(index).unwrap_or(u32::MAX)),
                name: Arc::clone(&function.entry.name),
                address: range.start,
                kind: SymbolKind::Function,
                binding: SymbolBinding::Local,
                exported: false,
                extent: Some(SymbolExtent {
                    range,
                    provenance: SymbolExtentProvenance::Declared,
                }),
                storage: None,
                role: CodeRole::Ordinary,
            }
        })
        .collect();

    let tables = crate::model::seal(
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        range,
        crate::model::ModuleMetadata {
            symbols,
            ..crate::model::ModuleMetadata::default()
        },
    )?;

    let binding = crate::model::Binding {
        path: Arc::new(path.to_path_buf()),
        debug_path: None,
        id,
    };
    let image = Arc::new(crate::ModuleImage::bind(&binding, Arc::new(tables))?);

    let unwind = PerfMapUnwind {
        functions: functions
            .values()
            .map(|function| {
                (
                    function.entry.start,
                    Function {
                        end: function.entry.end,
                        code: function.code.clone(),
                        frames: OnceLock::new(),
                    },
                )
            })
            .collect(),
    };

    Ok(DebugInfo {
        unwind: Arc::new(unwind),
        variables: super::dwarf::no_variables(&image),
        image,
    })
}

/// Unwinds the frames of a map's functions by what their instructions
/// prove. Image addresses are the process's: a map names the addresses
/// code runs at.
struct PerfMapUnwind {
    functions: BTreeMap<u64, Function>,
}

struct Function {
    end: u64,
    code: Option<Arc<[u8]>>,

    /// What its code proves, worked out on the first unwind through it.
    frames: OnceLock<CodeFrames>,
}

impl PerfMapUnwind {
    fn frame(&self, address: ImageAddress) -> Result<CodeFrame, UnwindTermination> {
        let address = address.get();
        let unknown = UnwindTermination::NoUnwindInfo {
            address: VirtualAddress::new(address),
        };
        let Some((start, function)) = self
            .functions
            .range(..=address)
            .next_back()
            .filter(|(_, function)| address < function.end)
        else {
            return Err(unknown);
        };

        let Some(code) = &function.code else {
            return Err(unknown);
        };

        function
            .frames
            .get_or_init(|| CodeFrames::analyze(code))
            .at(address - start)
            .ok_or(unknown)
    }
}

fn register(
    registers: &RegisterFile,
    number: crate::unwind::DwarfRegister,
    name: &str,
) -> Result<u64, UnwindTermination> {
    registers
        .get(number)
        .ok_or_else(|| UnwindTermination::RegisterUnavailable {
            register: name.into(),
        })
}

fn cfa(frame: CodeFrame, registers: &RegisterFile) -> Result<VirtualAddress, UnwindTermination> {
    let cfa = match frame {
        CodeFrame::Stack { pushed, .. } => register(registers, RSP, "rsp")?
            .checked_add(pushed)
            .and_then(|top| top.checked_add(8)),
        CodeFrame::Based { offset, .. } => register(registers, RBP, "rbp")?.checked_add(offset),
    };

    cfa.map(VirtualAddress::new)
        .ok_or_else(|| UnwindTermination::InvalidCaller {
            description: "a frame ends past the address space".into(),
        })
}

fn read(
    memory: &mut dyn MemoryReader,
    cfa: VirtualAddress,
    below: u64,
) -> Result<u64, UnwindTermination> {
    let address = VirtualAddress::new(cfa.get().wrapping_sub(below));
    memory
        .read_u64(address)
        .ok_or(UnwindTermination::MemoryReadFailed { address })
}

impl UnwindInfo for PerfMapUnwind {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> Result<VirtualAddress, UnwindTermination> {
        cfa(self.frame(address)?, registers)
    }

    /// The caller keeps the stack pointer, return address, and frame
    /// pointer the code proves; the code's calling convention is not
    /// known, so no other register is.
    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> Result<UnwindStep, UnwindTermination> {
        let frame = self.frame(address)?;
        let cfa = cfa(frame, registers)?;
        let return_address = read(memory, cfa, 8)?;

        let frame_pointer = match frame {
            CodeFrame::Based { offset, .. }
            | CodeFrame::Stack {
                caller_frame_pointer: CallerFramePointer::Saved(offset),
                ..
            } => Some(read(memory, cfa, offset)?),
            CodeFrame::Stack {
                caller_frame_pointer: CallerFramePointer::Kept,
                ..
            } => registers.get(RBP),
            CodeFrame::Stack {
                caller_frame_pointer: CallerFramePointer::Lost,
                ..
            } => None,
        };

        let mut caller = RegisterFile::new([(RETURN_ADDRESS, return_address), (RSP, cfa.get())]);
        if let Some(frame_pointer) = frame_pointer {
            caller.set(RBP, frame_pointer);
        }

        Ok(UnwindStep {
            registers: caller,
            cfa,
            signal_frame: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unwind::FramePointerRule;

    fn named(start: u64, end: u64, name: &str) -> Entry {
        Entry {
            start,
            end,
            name: name.into(),
        }
    }

    #[test]
    fn a_map_names_functions_by_its_complete_lines() {
        let text = b"7f0000001000 10 py::leaf:/srv/app.py\n\
                     0x7f0000001010 0x8 py::<module>:/srv/a b.py\r\n\
                     not a line\n\
                     7f0000002000 0 empty\n\
                     ffffffffffffffff 2 past the end\n\
                     7f0000003000 10 cut sh";
        let (entries, read) = parse(text);
        assert_eq!(
            entries,
            [
                named(0x7f00_0000_1000, 0x7f00_0000_1010, "py::leaf:/srv/app.py"),
                named(
                    0x7f00_0000_1010,
                    0x7f00_0000_1018,
                    "py::<module>:/srv/a b.py"
                ),
            ]
        );
        assert_eq!(&text[read..], b"7f0000003000 10 cut sh");
        assert_eq!(parse(b"cut"), (Vec::new(), 0));
    }

    /// Memory where each word holds the value `words` gives its address.
    struct Words(BTreeMap<u64, u64>);

    impl MemoryReader for Words {
        fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
            self.0.get(&address.get()).copied()
        }
    }

    const NO_FRAME_POINTER: &[u8] = &[
        0x48, 0x83, 0xec, 0x08, 0xff, 0xd1, 0x48, 0x83, 0xc4, 0x08, 0xc3,
    ];
    const FRAME_POINTER: &[u8] = &[0x55, 0x48, 0x89, 0xe5, 0xff, 0xd1, 0x5d, 0xc3];

    fn loaded(functions: &[(u64, &str, Option<&[u8]>)]) -> DebugInfo {
        let jitted: Vec<Jitted> = functions
            .iter()
            .map(|&(start, name, code)| Jitted {
                entry: named(start, start + 16, name),
                code: code.map(Arc::from),
            })
            .collect();

        load(
            Path::new("/tmp/perf-1.map"),
            ModuleImageId::new(0),
            AddressRange {
                start: ImageAddress::new(0x1000),
                end: ImageAddress::new(0x2000),
            },
            &jitted,
        )
        .expect("the map loads")
    }

    #[test]
    fn functions_are_named_and_unwound_by_what_their_code_proves() {
        let info = loaded(&[
            (0x1000, "py::without", Some(NO_FRAME_POINTER)),
            (0x1010, "py::with", Some(FRAME_POINTER)),
            (0x1020, "unread", None),
        ]);
        let name = |address| {
            info.image
                .symbolize(ImageAddress::new(address))
                .map(|symbol| symbol.name)
        };
        assert_eq!(name(0x1005).as_deref(), Some("py::without"));
        assert_eq!(name(0x101f).as_deref(), Some("py::with"));
        assert_eq!(name(0x1030), None);

        // Inside the call of each: the frame without a frame pointer
        // pushed 8 bytes, and the other saved rbp and points rbp at it.
        let mut memory = Words(BTreeMap::from([
            (0x5010, 0xaaaa),
            (0x6000, 0x7000),
            (0x6008, 0xbbbb),
        ]));
        let registers = RegisterFile::new([(RSP, 0x5008), (RBP, 0x9000)]);
        let step = info
            .unwind
            .unwind(ImageAddress::new(0x1005), &registers, &mut memory)
            .expect("unwinds");
        assert_eq!(step.cfa.get(), 0x5018);
        assert_eq!(
            step.registers,
            RegisterFile::new([(RETURN_ADDRESS, 0xaaaa), (RSP, 0x5018), (RBP, 0x9000)])
        );

        let registers = RegisterFile::new([(RSP, 0x5ff0), (RBP, 0x6000)]);
        let step = info
            .unwind
            .unwind(ImageAddress::new(0x1015), &registers, &mut memory)
            .expect("unwinds");
        assert_eq!(
            step.registers,
            RegisterFile::new([(RETURN_ADDRESS, 0xbbbb), (RSP, 0x6010), (RBP, 0x7000)])
        );

        assert_eq!(
            info.unwind.frame_pointer_rule(ImageAddress::new(0x1015)),
            FramePointerRule::Kept
        );
        assert_eq!(
            info.unwind.frame_pointer_rule(ImageAddress::new(0x1005)),
            FramePointerRule::NotKept
        );

        for unknown in [0x1025, 0x1030, 0x100b] {
            assert!(
                matches!(
                    info.unwind
                        .unwind(ImageAddress::new(unknown), &registers, &mut memory),
                    Err(UnwindTermination::NoUnwindInfo { .. })
                ),
                "{unknown:#x}"
            );
        }
    }

    #[test]
    fn a_later_function_replaces_those_it_overlaps() {
        let jitted = [
            Jitted {
                entry: named(0x1000, 0x1010, "first"),
                code: None,
            },
            Jitted {
                entry: named(0x1010, 0x1020, "second"),
                code: None,
            },
            Jitted {
                entry: named(0x1008, 0x1018, "reused"),
                code: None,
            },
            Jitted {
                entry: named(0x1ff8, 0x2008, "outside"),
                code: None,
            },
        ];

        let info = load(
            Path::new("/tmp/perf-1.map"),
            ModuleImageId::new(0),
            AddressRange {
                start: ImageAddress::new(0x1000),
                end: ImageAddress::new(0x2000),
            },
            &jitted,
        )
        .expect("the map loads");

        let names: Vec<_> = info
            .image
            .symbols()
            .map(|symbol| symbol.name().to_owned())
            .collect();
        assert_eq!(names, ["reused"]);
    }
}
