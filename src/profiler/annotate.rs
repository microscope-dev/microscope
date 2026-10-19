//! A function's samples on its source lines and on its instructions. Code
//! is decoded only from the module's file when it is the very file the
//! recording read, and only forward from a function symbol's start.

use std::collections::BTreeMap;

use object::{Object as _, ObjectSection as _, ObjectSymbol as _, SymbolKind};

use super::{Granularity, KeyKind, Profile, Query};
use crate::capture::records::NONE;
use crate::disassembly::{AssemblySyntax, InstructionReferenceKind, RawDecode, decoder_for};
use crate::{MAX_FUNCTION_INSTRUCTIONS, TargetDescription};

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

/// What a function's samples weigh on one of its instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionCost {
    /// Its address in its module's file.
    pub address: u64,
    pub text: String,

    /// The function a direct jump or call goes to, with the offset into it
    /// when that is not its start.
    pub target: Option<String>,
    pub own: u64,
    pub total: u64,
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

        // Each module's sampled addresses in the function, with their weights.
        let mut sampled: BTreeMap<u32, BTreeMap<u64, (u64, u64)>> = BTreeMap::new();
        let costs = keys.functions(&selection(query));

        for cost in costs.rows {
            let KeyKind::Address { frame } = keys.kind(cost.key) else {
                continue;
            };

            let physical = capture.frame_lines(frame).last();
            if physical.is_none_or(|line| {
                capture.string(capture.functions[line.function.get() as usize].name.get()) != name
            }) {
                continue;
            }

            let frame = &capture.frames[frame as usize];
            let weights = sampled
                .entry(frame.module.get())
                .or_default()
                .entry(frame.address.get())
                .or_default();
            weights.0 += cost.own;
            weights.1 += cost.total;
        }

        let (module, addresses) = sampled
            .into_iter()
            .max_by_key(|(_, addresses)| addresses.values().map(|(_, total)| total).sum::<u64>())
            .ok_or_else(|| format!("no selected sample ran {name}'s code"))?;
        if module == NONE {
            return Err(format!("{name}'s samples ran code no module covers"));
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
        let mut code = decode(&file, path, addresses)?;
        code.weight = costs.weight;
        Ok(code)
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
            .ok_or_else(|| {
                format!(
                    "{module} holds no code for {}",
                    symbol.name().unwrap_or("?")
                )
            })?;
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
                address,
                text: tokens.iter().map(|token| &*token.text).collect(),
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
    })
}
