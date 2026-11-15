use std::collections::BTreeSet;
use std::path::Path;

use iced_x86::{ConditionCode, Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};

use super::RecordError;
use crate::debug_info::gopclntab::GoTable;
use crate::{ImageAddress, LoadBias, VirtualAddress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calls {
    pub address: ImageAddress,

    pub bias: LoadBias,

    pub repeats_when: Option<Branch>,
}

impl Calls {
    #[must_use]
    pub const fn loaded(self) -> VirtualAddress {
        self.bias.wrapping_load(self.address)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    Below,

    BelowOrEqual,
}

impl Branch {
    pub const fn taken(self, flags: u64) -> bool {
        const CARRY: u64 = 1;
        const ZERO: u64 = 1 << 6;
        match self {
            Self::Below => flags & CARRY != 0,
            Self::BelowOrEqual => flags & (CARRY | ZERO) != 0,
        }
    }
}

const UNRANDOMIZED_PIE_BASE: u64 = 0x5555_5555_4aaa;

fn inlined_everywhere(program: &Path, data: &[u8], symbol: &str) -> Option<RecordError> {
    const SHOWN: usize = 8;
    let search = crate::debug_info::DebugFileSearch::default();
    let info = crate::debug_info::load_program(program, data, &search).ok()?;
    let image = &info.image;
    let mut copies = 0;
    let mut enclosing_functions = BTreeSet::new();

    for function in image.functions_named(symbol) {
        for instance in function.instances() {
            if instance.is_out_of_line() {
                return None;
            }

            copies += 1;
            let enclosing = instance
                .ranges()
                .next()
                .and_then(|range| image.locate(range.start).physical_instance)
                .and_then(|id| image.code_instance(id))
                .and_then(|outer| image.function(outer.function()))
                .map(|outer| {
                    let outer = outer.info();
                    outer
                        .linkage_name
                        .as_deref()
                        .and_then(crate::demangle::qualified_name)
                        .unwrap_or_else(|| outer.name.to_string())
                });
            enclosing_functions.extend(enclosing);
        }
    }

    if copies == 0 {
        return None;
    }

    let mut shown: Vec<String> = enclosing_functions.into_iter().collect();
    if shown.len() > SHOWN {
        let more = shown.len() - SHOWN;
        shown.truncate(SHOWN);
        shown.push(format!("{more} more"));
    }

    Some(RecordError::Inlined {
        name: symbol.to_owned(),
        copies,
        into: shown,
    })
}

pub fn plan(program: &Path, symbol: &str, bias: Option<LoadBias>) -> Result<Calls, RecordError> {
    let unreadable = |error: &dyn std::fmt::Display| RecordError::Program(error.to_string());
    let program =
        super::session::which(program).ok_or_else(|| RecordError::Program("not found".into()))?;
    let data = std::fs::read(&program).map_err(|error| unreadable(&error))?;
    let file = object::File::parse(data.as_slice()).map_err(|error| unreadable(&error))?;
    let bias = bias.unwrap_or_else(|| {
        LoadBias::new(match file.kind() {
            object::ObjectKind::Dynamic => {
                let first = file.segments().map(|segment| segment.address()).min();
                UNRANDOMIZED_PIE_BASE.wrapping_sub(first.unwrap_or(0)) & !0xfff
            }
            _ => 0,
        })
    });

    let last = symbol
        .rsplit("::")
        .next()
        .unwrap_or(symbol)
        .split(['(', '<', ' '])
        .next()
        .unwrap_or(symbol);
    let addresses: BTreeSet<u64> = file
        .symbols()
        .filter(|candidate| {
            candidate.kind() == object::SymbolKind::Text
                && candidate.name().is_ok_and(|name| {
                    name == symbol
                        || (name.contains(last)
                            && crate::demangle::qualified_name(name)
                                .is_some_and(|name| name == symbol))
                })
        })
        .map(|found| found.address())
        .collect();
    let mut addresses = addresses.into_iter();
    let entry = match (addresses.next(), addresses.len()) {
        (None, _) => {
            return Err(inlined_everywhere(&program, &data, symbol)
                .unwrap_or_else(|| RecordError::NoSuchFunction(symbol.to_owned())));
        }
        (Some(entry), 0) => entry,
        (Some(_), others) => {
            return Err(RecordError::AmbiguousFunction {
                name: symbol.to_owned(),
                count: others + 1,
            });
        }
    };

    let plain = Calls {
        address: ImageAddress::new(entry),
        bias,
        repeats_when: None,
    };

    let Some(table) = crate::debug_info::gopclntab::load(&file).ok().flatten() else {
        return Ok(plain);
    };

    let Some(check) = stack_check(&file, &table, entry) else {
        return Ok(plain);
    };

    let uncountable = |why: &str| RecordError::Uncountable {
        name: symbol.to_owned(),
        why: why.to_owned(),
    };

    let repeats_when = match check.condition_code() {
        ConditionCode::b => Branch::Below,
        ConditionCode::be => Branch::BelowOrEqual,
        _ => {
            return Err(uncountable(
                "its stack check branches on a condition microscope does not read",
            ));
        }
    };

    let function = table
        .function_containing(entry)
        .ok_or_else(|| uncountable("its stack check is not in a Go function table"))?;
    if table.preemptible(function, check.ip()).unwrap_or(true) {
        return Err(uncountable(
            "the Go runtime may preempt it at its stack check",
        ));
    }

    if table.restarts_at_entry(function).unwrap_or(true) {
        return Err(uncountable("the Go runtime may restart it at its entry"));
    }

    Ok(Calls {
        address: ImageAddress::new(check.ip()),
        bias,
        repeats_when: Some(repeats_when),
    })
}

fn stack_check(file: &object::File<'_>, table: &GoTable, entry: u64) -> Option<Instruction> {
    let (start, data) = file.sections().find_map(|section| {
        let start = section.address();
        let offset = usize::try_from(entry.checked_sub(start)?).ok()?;
        let data = section.data().ok()?;
        (offset < data.len()).then_some((start, data))
    })?;
    let decode = |at: u64| {
        let offset = usize::try_from(at.wrapping_sub(start)).unwrap_or(usize::MAX);
        let code = data.get(offset..).unwrap_or_default();
        let mut decoder = Decoder::with_ip(64, code, at, DecoderOptions::NONE);
        std::iter::from_fn(move || decoder.can_decode().then(|| decoder.decode()))
    };

    let grows_the_stack = |at: u64| {
        let block: Vec<Instruction> = decode(at).take(8).collect();
        let calls_morestack = block.iter().any(|instruction| {
            instruction.mnemonic() == Mnemonic::Call
                && table.grows_stacks(instruction.near_branch_target())
        });

        let returns_to_entry = block.iter().any(|instruction| {
            instruction.flow_control() == FlowControl::UnconditionalBranch
                && instruction.near_branch_target() == entry
        });
        calls_morestack && returns_to_entry
    };

    decode(entry)
        .take(5)
        .filter(|instruction| {
            instruction.flow_control() == FlowControl::ConditionalBranch
                && grows_the_stack(instruction.near_branch_target())
        })
        .last()
}
