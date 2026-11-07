//! A function's samples on its source lines and on its instructions. Code
//! is decoded only from the module's file when it is the very file the
//! recording read, and only forward from a function symbol's start. Each
//! instruction's source lines come from that file's debug information.

use std::collections::BTreeMap;

use object::{Object as _, ObjectSection as _, ObjectSymbol as _, SymbolKind};

use super::record::symbolize;
use super::{Granularity, KeyKind, Profile, Query};
use crate::capture::records::{FrameKind, NONE};
use crate::debug_info::DebugFileSearch;
use crate::disassembly::{
    AssemblySyntax, InstructionReferenceKind, InstructionToken, RawDecode, decoder_for,
};
use crate::{ImageAddress, MAX_FUNCTION_INSTRUCTIONS, ModuleImageId, TargetDescription};

/// What a function's samples weigh on one of its source lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineCost {
    pub file: Option<String>,
    pub line: u32,
    pub own: u64,
    pub total: u64,
}

/// What a function's samples weigh on its source lines, of what every
/// sample selected weighs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineCosts {
    pub weight: u64,
    pub lines: Vec<LineCost>,
}

/// A source line an instruction was compiled from: line 0 when the
/// debug information names none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeLine {
    pub function: String,
    pub file: Option<String>,
    pub line: u32,
}

/// What a function's samples weigh on one of its instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionCost {
    /// Its address in its module's file.
    pub address: u64,

    /// Its text in the decoder's pieces, which say what each one is.
    pub tokens: Vec<InstructionToken>,

    /// The function a direct jump or call goes to, with the offset into it
    /// when that is not its start.
    pub target: Option<String>,
    pub own: u64,
    pub total: u64,

    /// The lines it was compiled from, innermost inlined function first,
    /// ending with the function it belongs to; empty when unknown.
    pub lines: Vec<CodeLine>,
}

impl InstructionCost {
    #[must_use]
    pub fn text(&self) -> String {
        self.tokens.iter().map(|token| &*token.text).collect()
    }
}

/// One symbol's code that a function's samples ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub symbol: String,
    pub instructions: Vec<InstructionCost>,

    /// Why decoding ended before the symbol did, if it did.
    pub stopped: Option<String>,
}

/// A function's code where its samples ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disassembly {
    /// What every sample selected weighs.
    pub weight: u64,
    pub module: String,
    pub blocks: Vec<CodeBlock>,

    /// Sampled addresses no decoded instruction holds: own, then total.
    pub unplaced: Vec<(u64, u64, u64)>,

    /// Why the instructions have no source lines, when they have none.
    pub unlined: Option<String>,

    /// The functions shown, when the function asked for has no code of
    /// its own: every sample ran it inlined into them.
    pub inlined_into: Option<String>,
}

impl Disassembly {
    /// What the instructions weigh on each source line they were compiled
    /// from, by file and line. In each file, an instruction's own weight
    /// is its innermost line's there, and its total weight is every line's
    /// of that file it was inlined through, as a line's total is the cost
    /// of everything it runs.
    #[must_use]
    pub fn line_costs(&self) -> Vec<LineCost> {
        let mut costs: BTreeMap<(&str, u32), (u64, u64)> = BTreeMap::new();
        let instructions = self.blocks.iter().flat_map(|block| &block.instructions);

        for instruction in instructions {
            let mut owned: Vec<&str> = Vec::new();
            let mut counted: Vec<(&str, u32)> = Vec::new();

            for line in &instruction.lines {
                let Some(file) = line.file.as_deref().filter(|_| line.line > 0) else {
                    continue;
                };

                let cost = costs.entry((file, line.line)).or_default();
                if !owned.contains(&file) {
                    owned.push(file);
                    cost.0 += instruction.own;
                }

                if !counted.contains(&(file, line.line)) {
                    counted.push((file, line.line));
                    cost.1 += instruction.total;
                }
            }
        }

        costs
            .into_iter()
            .map(|((file, line), (own, total))| LineCost {
                file: Some(file.to_owned()),
                line,
                own,
                total,
            })
            .collect()
    }
}

/// The query's selection, without its transforms, which name keys of
/// the granularity views show rather than lines or addresses.
fn selection(query: &Query) -> Query {
    Query {
        transforms: Vec::new(),
        ..query.clone()
    }
}

impl Profile {
    /// What the samples `query` selects weigh on each source line of the
    /// function `name`, by file and line. The query's transforms are not
    /// applied.
    #[must_use]
    pub fn line_costs(&self, query: &Query, name: &str) -> LineCosts {
        let keys = self.keys(Granularity::Line);
        let capture = &self.capture;
        let costs = keys.functions(&selection(query));
        let mut lines: Vec<LineCost> = costs
            .rows
            .into_iter()
            .filter_map(|cost| match keys.kind(cost.key) {
                KeyKind::Line {
                    function,
                    file,
                    line,
                } if capture.string(capture.functions[function as usize].name.get()) == name => {
                    Some(LineCost {
                        file: (file != NONE).then(|| capture.string(file).to_owned()),
                        line,
                        own: cost.own,
                        total: cost.total,
                    })
                }
                _ => None,
            })
            .collect();

        lines.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));

        LineCosts {
            weight: costs.weight,
            lines,
        }
    }

    /// The code of the function `name` where the samples `query` selects
    /// ran, each instruction with their weight on it, or why it cannot be
    /// shown. The query's transforms are not applied.
    pub fn disassemble(&self, query: &Query, name: &str) -> Result<Disassembly, String> {
        let keys = self.keys(Granularity::Address);
        let capture = &self.capture;

        let costs = keys.functions(&selection(query));
        let function_name =
            |function: u32| capture.string(capture.functions[function as usize].name.get());
        let frames: Vec<(u32, u64, u64)> = costs
            .rows
            .iter()
            .filter_map(|cost| match keys.kind(cost.key) {
                KeyKind::Address { frame } => Some((frame, cost.own, cost.total)),
                _ => None,
            })
            .collect();

        // The functions whose code is shown: `name` itself, or, when every
        // sample ran it inlined, the functions it was inlined into.
        let physical = |frame: u32| {
            capture
                .frame_lines(frame)
                .last()
                .map(|line| function_name(line.function.get()))
        };
        let mut shown: Vec<&str> = Vec::new();
        if !frames
            .iter()
            .any(|&(frame, ..)| physical(frame) == Some(name))
        {
            for &(frame, ..) in &frames {
                let lines = capture.frame_lines(frame);
                let inlined = lines.split_last().is_some_and(|(_, inner)| {
                    inner
                        .iter()
                        .any(|line| function_name(line.function.get()) == name)
                });
                if let Some(outer) =
                    physical(frame).filter(|outer| inlined && !shown.contains(outer))
                {
                    shown.push(outer);
                }
            }
            shown.sort_unstable();
        }
        let inlined_into = (!shown.is_empty()).then(|| shown.join(", "));
        if shown.is_empty() {
            shown.push(name);
        }

        // Each module's sampled addresses in those functions, with their
        // weights.
        let mut sampled: BTreeMap<u32, BTreeMap<u64, (u64, u64)>> = BTreeMap::new();
        for (frame, own, total) in frames {
            if !physical(frame).is_some_and(|outer| shown.contains(&outer)) {
                continue;
            }

            let frame = &capture.frames[frame as usize];
            let weights = sampled
                .entry(frame.module.get())
                .or_default()
                .entry(frame.address.get())
                .or_default();
            weights.0 += own;
            weights.1 += total;
        }

        let (module, addresses) = sampled
            .into_iter()
            .max_by_key(|(_, addresses)| addresses.values().map(|(_, total)| total).sum::<u64>())
            .ok_or_else(|| self.no_code(name))?;
        if module == NONE {
            return Err(format!("{name}'s samples ran code outside every module"));
        }

        let record = &capture.modules[module as usize];
        let path = capture.string(record.path.get()).to_owned();
        if record.digest == [0; 16] {
            return Err(format!(
                "the recording could not read {path}, so its code is unknown"
            ));
        }

        let bytes =
            std::fs::read(&path).map_err(|error| format!("{path} cannot be read: {error}"))?;
        if twox_hash::XxHash3_128::oneshot(&bytes).to_le_bytes() != record.digest {
            return Err(format!("{path} has changed since it was recorded"));
        }

        let file = object::File::parse(bytes.as_slice())
            .map_err(|error| format!("{path} cannot be parsed: {error}"))?;
        let mut code = decode(&file, path.clone(), addresses)?;
        code.weight = costs.weight;
        code.inlined_into = inlined_into;
        code.unlined = place_lines(&path, &bytes, &mut code).err();
        Ok(code)
    }

    /// Why no instruction of the function `name` can be shown: a runtime
    /// interprets it or compiles it as it runs, or it ran in memory no file
    /// holds, or no selected sample ran it.
    fn no_code(&self, name: &str) -> String {
        let capture = &self.capture;
        let mut interpreted = false;
        let mut unmapped = false;

        for (index, frame) in capture.frames.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            let named = capture.frame_lines(index).last().map_or_else(
                || format!("{:#x}", frame.address.get()) == name,
                |line| {
                    capture.string(capture.functions[line.function.get() as usize].name.get())
                        == name
                },
            );
            if !named {
                continue;
            }

            interpreted |= frame.kind == FrameKind::Interpreted as u8;
            unmapped |= frame.kind == FrameKind::Native as u8 && frame.module.get() == NONE;
        }

        if interpreted {
            format!(
                "{name} is Python or JavaScript, which its runtime interprets or compiles as it runs, so no file holds its instructions; its samples by line are under Source"
            )
        } else if unmapped {
            format!(
                "{name} ran in memory no file holds, as code a JIT compiles does, so the recording kept none of its instructions"
            )
        } else {
            format!("no selected sample ran {name}'s code")
        }
    }
}

/// Decodes each function symbol that holds sampled addresses, weighing
/// each instruction with the addresses it holds.
fn decode(
    file: &object::File<'_>,
    module: String,
    mut addresses: BTreeMap<u64, (u64, u64)>,
) -> Result<Disassembly, String> {
    let mut symbols: Vec<object::Symbol<'_, '_>> = file
        .symbols()
        .chain(file.dynamic_symbols())
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.size() > 0)
        .collect();
    symbols.sort_by_key(|symbol| (symbol.address(), std::cmp::Reverse(symbol.size())));
    symbols.dedup_by_key(|symbol| symbol.address());
    let named = |address: u64| {
        let index = symbols.partition_point(|symbol| symbol.address() <= address);
        let symbol = symbols[..index].last()?;
        let offset = address - symbol.address();
        (offset < symbol.size()).then_some(())?;
        let name = symbol.name().ok()?;
        let name = crate::demangle::qualified_name(name).unwrap_or_else(|| name.to_owned());
        Some(if offset == 0 {
            name
        } else {
            format!("{name}+{offset:#x}")
        })
    };

    let mut decoder = decoder_for(TargetDescription::X86_64, AssemblySyntax::Intel)
        .map_err(|error| error.to_string())?;
    let mut blocks = Vec::new();

    for symbol in &symbols {
        let (start, end) = (symbol.address(), symbol.address() + symbol.size());
        if addresses.range(start..end).next().is_none() {
            continue;
        }

        let code = symbol
            .section_index()
            .and_then(|index| file.section_by_index(index).ok())
            .and_then(|section| section.data_range(start, symbol.size()).ok().flatten())
            .ok_or_else(|| format!("{module} has no code for {}", symbol.name().unwrap_or("?")))?;
        let name = symbol.name().unwrap_or("?");
        let mut block = CodeBlock {
            symbol: crate::demangle::qualified_name(name).unwrap_or_else(|| name.to_owned()),
            instructions: Vec::new(),
            stopped: None,
        };

        let mut offset = 0;

        while offset < code.len() {
            if block.instructions.len() == MAX_FUNCTION_INSTRUCTIONS {
                block.stopped = Some(format!(
                    "{MAX_FUNCTION_INSTRUCTIONS} instructions are shown"
                ));
                break;
            }

            let address = start + offset as u64;
            let RawDecode::Instruction {
                length,
                tokens,
                references,
                ..
            } = decoder.decode(address, &code[offset..], None)
            else {
                block.stopped = Some(format!("the bytes at {address:#x} are no instruction"));
                break;
            };

            let held: Vec<u64> = addresses
                .range(address..address + length as u64)
                .map(|(at, _)| *at)
                .collect();

            let (own, total) = held.iter().fold((0, 0), |(own, total), at| {
                let (more_own, more_total) = addresses.remove(at).unwrap_or_default();
                (own + more_own, total + more_total)
            });

            block.instructions.push(InstructionCost {
                lines: Vec::new(),
                address,
                tokens,
                target: references
                    .iter()
                    .find(|(kind, _)| *kind == InstructionReferenceKind::BranchTarget)
                    .and_then(|&(_, target)| named(target)),
                own,
                total,
            });
            offset += length;
        }

        blocks.push(block);
    }

    Ok(Disassembly {
        weight: 0,
        module,
        blocks,
        unplaced: addresses
            .into_iter()
            .map(|(address, (own, total))| (address, own, total))
            .collect(),
        unlined: None,
        inlined_into: None,
    })
}

/// Gives each instruction of `code` the source lines the debug
/// information of the module at `path`, holding `bytes`, has for it, or
/// says why it has none.
fn place_lines(path: &str, bytes: &[u8], code: &mut Disassembly) -> Result<(), String> {
    let info = crate::debug_info::load_module_bytes(
        std::path::Path::new(path),
        bytes,
        ModuleImageId::new(0),
        &DebugFileSearch::default(),
    )
    .map_err(|error| format!("{path}'s debug information cannot be read: {error}"))?;

    let mut placed = false;
    for instruction in code
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.instructions)
    {
        instruction.lines = symbolize(&info.image, ImageAddress::new(instruction.address))
            .iter()
            .map(|line| {
                let line = line.input();
                CodeLine {
                    function: line.function.to_owned(),
                    file: line.file.map(str::to_owned),
                    line: line.line,
                }
            })
            .collect();
        placed |= instruction.lines.iter().any(|line| line.line > 0);
    }

    if placed {
        Ok(())
    } else {
        Err(format!(
            "{path}'s debug information has no source lines for this code"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(function: &str, file: Option<&str>, line: u32) -> CodeLine {
        CodeLine {
            function: function.to_owned(),
            file: file.map(str::to_owned),
            line,
        }
    }

    fn instruction(own: u64, total: u64, lines: Vec<CodeLine>) -> InstructionCost {
        InstructionCost {
            address: 0,
            tokens: Vec::new(),
            target: None,
            own,
            total,
            lines,
        }
    }

    fn cost(file: &str, line: u32, own: u64, total: u64) -> LineCost {
        LineCost {
            file: Some(file.to_owned()),
            line,
            own,
            total,
        }
    }

    /// Own weight goes to an instruction's innermost line in each file,
    /// not to its innermost line overall nor to every line it passes
    /// through; total weight goes once to each line of the chain.
    #[test]
    fn instructions_weigh_their_innermost_line_in_each_file_and_every_line_they_pass() {
        let code = Disassembly {
            weight: 100,
            module: "app".to_owned(),
            blocks: vec![CodeBlock {
                symbol: "scan".to_owned(),
                instructions: vec![
                    // step, inlined into scan at line 14.
                    instruction(
                        5,
                        5,
                        vec![line("step", Some("f.c"), 8), line("scan", Some("f.c"), 14)],
                    ),
                    // A helper from a header, inlined at the same line.
                    instruction(
                        2,
                        2,
                        vec![line("get", Some("h.h"), 3), line("scan", Some("f.c"), 14)],
                    ),
                    // A call: its callee's time is total alone.
                    instruction(0, 7, vec![line("scan", Some("f.c"), 15)]),
                    // A function inlined into itself on one line counts once.
                    instruction(
                        1,
                        1,
                        vec![line("walk", Some("f.c"), 20), line("walk", Some("f.c"), 20)],
                    ),
                    // Code no line names, and lines with no file, weigh nothing.
                    instruction(4, 4, vec![line("scan", Some("f.c"), 0)]),
                    instruction(3, 3, vec![line("scan", None, 9)]),
                ],
                stopped: None,
            }],
            unplaced: Vec::new(),
            unlined: None,
            inlined_into: None,
        };

        assert_eq!(
            code.line_costs(),
            vec![
                cost("f.c", 8, 5, 5),
                cost("f.c", 14, 2, 7),
                cost("f.c", 15, 0, 7),
                cost("f.c", 20, 1, 1),
                cost("h.h", 3, 2, 2),
            ]
        );
    }
}
