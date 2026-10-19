mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use microscope::{
    AddressDescription, AssemblySyntax, BlockCompletion, BoundaryConflict, BoundaryEvidence,
    BreakpointSpec, ContextShortfall, ControlFlow, CoreDumpOptions, CoreModuleState,
    DisassembledInstruction, Disassembly, DisassemblyBlock, DisassemblyQuery, DisassemblyRange,
    DisassemblyView, Error, FunctionOrigin, ImageAddress, IndirectTarget, InstructionContent,
    InstructionReferenceKind, LoadedModuleRecord, MAX_WINDOW_AFTER, MAX_WINDOW_BEFORE,
    MemoryReadUnavailableReason, ModuleImage, StepKind, StopReason, SymbolExtentProvenance,
    SymbolKind, TargetBoundary, VirtualAddress,
};
use object::{Object, ObjectSection, ObjectSymbol, ObjectSymbolTable};
use support::Scenario;

/// GNU objdump's decoding of one ELF file, by image address.
struct Objdump {
    instructions: BTreeMap<u64, ObjdumpInstruction>,
}

#[derive(Debug)]
struct ObjdumpInstruction {
    bytes: Vec<u8>,
    text: String,
}

impl Objdump {
    fn read(file_name: &str) -> Self {
        let path = Scenario::fixture(&format!("disassembly-oracles/{file_name}.objdump"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let mut instructions = BTreeMap::new();
        for line in text.lines() {
            // "  ADDRESS:\tBYTES\tTEXT", where a wide listing pads BYTES.
            let mut fields = line.split('\t');
            let (Some(address), Some(bytes)) = (fields.next(), fields.next()) else {
                continue;
            };

            let Some(address) = address
                .trim()
                .strip_suffix(':')
                .and_then(|address| u64::from_str_radix(address, 16).ok())
            else {
                continue;
            };

            let bytes = bytes
                .split_whitespace()
                .map(|byte| u8::from_str_radix(byte, 16).expect("objdump byte"))
                .collect();
            let text = fields.next().unwrap_or_default().trim().to_owned();
            instructions.insert(address, ObjdumpInstruction { bytes, text });
        }

        assert!(!instructions.is_empty(), "{} is empty", path.display());
        Self { instructions }
    }
}

impl ObjdumpInstruction {
    /// The target of a direct jump or call, which objdump prints as a bare
    /// hexadecimal operand.
    fn branch_target(&self) -> Option<u64> {
        const PREFIXES: [&str; 7] = ["bnd", "notrack", "rep", "repz", "repnz", "data16", "addr32"];
        let mut tokens = self
            .text
            .split_whitespace()
            .skip_while(|token| PREFIXES.contains(token) || token.starts_with("rex"));
        let mnemonic = tokens.next()?;
        let branch = mnemonic.starts_with('j')
            || mnemonic.starts_with("call")
            || mnemonic.starts_with("loop")
            || mnemonic.starts_with("xbegin");
        let operand = tokens.next()?;

        branch
            .then(|| u64::from_str_radix(operand, 16).ok())
            .flatten()
    }

    /// The address objdump computes for a program-counter-relative operand,
    /// which it prints after `#`.
    fn computed_address(&self) -> Option<u64> {
        let (_, comment) = self.text.split_once('#')?;
        let address = comment.split_whitespace().next()?;
        u64::from_str_radix(address.trim_start_matches("0x"), 16).ok()
    }
}

const fn query(range: DisassemblyRange) -> DisassemblyQuery {
    DisassemblyQuery {
        range,
        syntax: AssemblySyntax::Att,
    }
}

const fn window(address: u64, before: u32, after: u32) -> DisassemblyQuery {
    query(DisassemblyRange::Window {
        address: VirtualAddress::new(address),
        before,
        after,
    })
}

async fn disassemble(scenario: &Scenario, query: DisassemblyQuery) -> Disassembly {
    scenario
        .operation("disassemble", scenario.handle().disassemble(query))
        .await
}

fn into_window(
    disassembly: Disassembly,
) -> (TargetBoundary, Option<ContextShortfall>, DisassemblyBlock) {
    match disassembly.view {
        DisassemblyView::Window {
            boundary,
            leading,
            block,
            ..
        } => (boundary, leading, block),
        DisassemblyView::Function { .. } => panic!("a window query returned a function"),
    }
}

fn decoded(instruction: &DisassembledInstruction) -> &microscope::DecodedInstruction {
    match &instruction.content {
        InstructionContent::Decoded(decoded) => decoded,
        other => panic!("{:#x} did not decode: {other:?}", instruction.address),
    }
}

/// The modules of one stopped process with their images.
struct Modules {
    records: Vec<LoadedModuleRecord>,
    images: BTreeMap<microscope::ModuleId, Arc<ModuleImage>>,
}

impl Modules {
    async fn load(scenario: &Scenario) -> Self {
        let snapshot = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        let mut images = BTreeMap::new();
        for record in snapshot.modules.iter() {
            let image = scenario
                .operation(
                    "image",
                    scenario.handle().loaded_module_image(record.module.id),
                )
                .await;
            images.insert(record.module.id, image);
        }

        Self {
            records: snapshot.modules.to_vec(),
            images,
        }
    }

    fn named(&self, file_name: &str) -> (&LoadedModuleRecord, &ModuleImage) {
        let record = self
            .records
            .iter()
            .find(|record| {
                record
                    .path
                    .file_name()
                    .is_some_and(|name| name == file_name)
            })
            .unwrap_or_else(|| panic!("no loaded module named {file_name}"));

        (record, &self.images[&record.module.id])
    }

    /// Returns the runtime address of a uniquely named symbol.
    fn symbol(&self, file_name: &str, name: &str) -> u64 {
        let (record, image) = self.named(file_name);
        let symbol = image
            .symbols()
            .find(|symbol| symbol.name() == name)
            .unwrap_or_else(|| panic!("{file_name} has no symbol {name}"));
        record.module.load_bias.get() + symbol.address().get()
    }
}

/// Pages through every executable section of a module with windows that
/// each begin where the last ended, then compares every instruction with
/// objdump: the same instruction starts, the same bytes, and the same
/// encoded addresses. No known instruction start may conflict.
async fn assert_module_matches_objdump(
    scenario: &Scenario,
    modules: &Modules,
    file_name: &str,
    context: &str,
) -> usize {
    let (record, image) = modules.named(file_name);
    let bias = record.module.load_bias.get();
    let oracle = Objdump::read(file_name);
    let mut compared = 0;

    for section in image.sections().iter().filter(|section| section.executable) {
        let (start, end) = (section.range.start.get(), section.range.end.get());
        let context = format!("{context}: {file_name} {}", section.name);
        let mut blocks = Vec::new();
        let mut address = start;

        while address < end {
            let (boundary, _, block) = into_window(
                disassemble(scenario, window(bias + address, 0, MAX_WINDOW_AFTER)).await,
            );
            assert!(
                !matches!(boundary, TargetBoundary::Crossed { .. }),
                "{context}: {address:#x} is inside an instruction: {boundary:?}"
            );

            // Decoding past the section's end may cross padding into the next
            // section; only conflicts inside the section matter.
            let conflicts = block
                .conflicts
                .iter()
                .filter(|conflict| conflict.boundary.get() - bias < end)
                .map(|conflict| {
                    (
                        conflict.boundary.get() - bias,
                        conflict.evidence,
                        conflict.instruction.get() - bias,
                    )
                })
                .collect::<Vec<_>>();
            assert!(conflicts.is_empty(), "{context}: {conflicts:x?}");

            for instruction in block.instructions.iter() {
                if instruction.address.get() - bias >= end {
                    break;
                }

                address = instruction.end().get() - bias;
            }

            if let BlockCompletion::Unreadable { address: stop, .. } = block.completion {
                assert!(stop.get() - bias >= end, "{context}: unreadable at {stop}");
            }

            blocks.push(block);
        }

        let decoded = blocks
            .iter()
            .flat_map(|block| block.instructions.iter())
            .map(|instruction| (instruction.address.get() - bias, instruction))
            .take_while(|(address, _)| *address < end);
        let pairs = pair_with_objdump(decoded, oracle.instructions.range(start..end), &context);

        for (address, instruction, objdump) in pairs {
            assert_eq!(
                *instruction.bytes, *objdump.bytes,
                "{context}+{address:#x} {objdump:?}"
            );
            assert_eq!(
                matches!(instruction.content, InstructionContent::Invalid),
                objdump.text.starts_with("(bad)"),
                "{context}+{address:#x} {objdump:?}"
            );
            let InstructionContent::Decoded(decoded) = &instruction.content else {
                continue;
            };

            let targets = |kind| {
                decoded
                    .references
                    .iter()
                    .filter(move |reference| reference.kind == kind)
                    .map(|reference| reference.address.get() - bias)
            };

            assert_eq!(
                targets(InstructionReferenceKind::BranchTarget).next(),
                objdump.branch_target(),
                "{context}+{address:#x} {objdump:?}"
            );

            if let Some(computed) = objdump.computed_address() {
                assert!(
                    targets(InstructionReferenceKind::MemoryOperand)
                        .any(|target| target == computed),
                    "{context}+{address:#x} {objdump:?}: {decoded:?}"
                );
            }

            compared += 1;
        }
    }

    assert!(compared > 0, "{context}: {file_name} had no code");
    compared
}

/// Pairs ascending decoded instructions with objdump's at the same image
/// addresses, which must be exactly the same set of instruction starts.
fn pair_with_objdump<'a, 'b>(
    decoded: impl Iterator<Item = (u64, &'a DisassembledInstruction)>,
    expected: impl Iterator<Item = (&'b u64, &'b ObjdumpInstruction)>,
    context: &str,
) -> Vec<(u64, &'a DisassembledInstruction, &'b ObjdumpInstruction)> {
    // Both listings ascend, so one merge pairs every start they share.
    let mut expected = expected.peekable();
    let (mut missing, mut invented, mut pairs) = (Vec::new(), Vec::new(), Vec::new());
    let mut previous = None;

    for (address, instruction) in decoded {
        assert!(
            previous < Some(address),
            "{context}: {address:#x} follows {previous:x?}"
        );
        previous = Some(address);
        while let Some((&objdump, _)) = expected.next_if(|&(&objdump, _)| objdump < address) {
            missing.push(objdump);
        }

        match expected.next_if(|&(&objdump, _)| objdump == address) {
            Some((_, objdump)) => pairs.push((address, instruction, objdump)),
            None => invented.push(address),
        }
    }

    missing.extend(expected.map(|(&address, _)| address));
    missing.truncate(4);
    invented.truncate(4);
    assert!(
        missing.is_empty() && invented.is_empty(),
        "{context}: objdump-only starts {missing:x?}, microscope-only starts {invented:x?}"
    );
    pairs
}

/// Launches a fixture that faults, installs breakpoints at code symbols so
/// that traps are present in the compared code, and returns at the fault,
/// or at the panic a runtime turns it into.
async fn faulted_with_breakpoints(fixture: &str) -> (Scenario, Modules) {
    let mut scenario = Scenario::launch(fixture);
    let stop = scenario.run_to_stop().await;
    assert!(
        matches!(
            stop,
            StopReason::Exception(_) | StopReason::LanguageException(_)
        ),
        "{fixture}: {stop:?}"
    );

    let modules = Modules::load(&scenario).await;
    let (record, image) = modules.named(fixture);
    let starts = image
        .symbols()
        .filter_map(microscope::Symbol::extent)
        .map(|extent| extent.range.start.get())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let bias = record.module.load_bias.get();

    for address in starts.iter().step_by(starts.len().div_ceil(24).max(1)) {
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(bias + address)))
            .await;
    }

    (scenario, modules)
}

#[tokio::test]
async fn c_programs_and_libraries_decode_like_objdump() {
    for fixture in ["crash-gcc-o0", "crash-gcc-o2-nopie", "crash-clang-o2"] {
        let (scenario, modules) = faulted_with_breakpoints(fixture).await;
        assert_module_matches_objdump(&scenario, &modules, fixture, fixture).await;
        assert_module_matches_objdump(&scenario, &modules, "libcrash.so", fixture).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn the_c_library_and_loader_decode_like_objdump() {
    let (scenario, modules) = faulted_with_breakpoints("crash-gcc-o0").await;
    for library in ["libc.so.6", "ld-linux-x86-64.so.2"] {
        let compared = assert_module_matches_objdump(&scenario, &modules, library, "live").await;
        assert!(compared > 10_000, "{library}: {compared}");
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn rust_zig_and_go_programs_decode_like_objdump() {
    for fixture in ["crash-rust-o0", "crash-zig-o0", "crash-go-o0"] {
        let (scenario, modules) = faulted_with_breakpoints(fixture).await;
        assert_module_matches_objdump(&scenario, &modules, fixture, fixture).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn core_dumps_decode_verified_files_like_objdump() {
    for (core, files) in [
        (
            "crash-gcc-o0-segv.core",
            &["crash-gcc-o0", "libcrash.so"][..],
        ),
        (
            "elf-symbols-gcc-o0.core",
            &["elf-symbols-gcc-o0", "libelf-symbols-gcc.so"][..],
        ),
        (
            "elf-symbols-stripped.core",
            &["libelf-symbols-stripped.so"][..],
        ),
    ] {
        let scenario = Scenario::open_core(core, &CoreDumpOptions::new(Scenario::fixture(core)));
        let modules = Modules::load(&scenario).await;
        for file in files {
            assert_module_matches_objdump(&scenario, &modules, file, core).await;
        }

        scenario.shutdown().await;
    }
}

/// Checks one function disassembly against its image and objdump: one block
/// per range, every instruction as objdump decodes it, and no conflicts.
fn assert_function_blocks(
    disassembly: &Disassembly,
    ranges: &[(u64, u64)],
    bias: u64,
    oracle: &Objdump,
    context: &str,
) {
    let DisassemblyView::Function { blocks, .. } = &disassembly.view else {
        panic!("{context}: a function query returned a window");
    };

    let actual = blocks
        .iter()
        .map(|block| (block.range.start.get() - bias, block.range.end.get() - bias))
        .collect::<Vec<_>>();
    assert_eq!(actual, ranges, "{context}");
    for block in blocks.iter() {
        assert_eq!(block.completion, BlockCompletion::Complete, "{context}");
        assert!(
            block.conflicts.is_empty(),
            "{context}: {:?}",
            block.conflicts
        );
        let mut next = block.range.start;
        for instruction in block.instructions.iter() {
            assert_eq!(instruction.address, next, "{context}");
            let image_address = instruction.address.get() - bias;
            let objdump = &oracle.instructions[&image_address];
            assert_eq!(
                *instruction.bytes, *objdump.bytes,
                "{context}+{image_address:#x}"
            );
            next = instruction.end();
        }

        assert_eq!(next, block.range.end, "{context}");
    }
}

#[tokio::test]
async fn functions_cover_every_debug_information_range() {
    for fixture in [
        "crash-gcc-o0",
        "crash-gcc-o2-nopie",
        "crash-clang-o2",
        "crash-rust-o0",
    ] {
        let (scenario, modules) = faulted_with_breakpoints(fixture).await;
        let (record, image) = modules.named(fixture);
        let bias = record.module.load_bias.get();
        let oracle = Objdump::read(fixture);
        let mut split = 0;
        let mut names = Vec::new();

        for instance in image
            .code_instances()
            .filter(|instance| matches!(instance.kind(), microscope::CodeInstanceKind::OutOfLine))
        {
            names.push(
                image
                    .function(instance.function())
                    .expect("instance function")
                    .name(),
            );

            let mut ranges = instance
                .ranges()
                .map(|range| (range.start.get(), range.end.get()))
                .collect::<Vec<_>>();
            ranges.sort_unstable();

            // Every range resolves to the same function.
            for (start, _) in &ranges {
                let disassembly = disassemble(
                    &scenario,
                    query(DisassemblyRange::Function(VirtualAddress::new(
                        bias + start,
                    ))),
                )
                .await;
                let context = format!("{fixture}: instance {}", instance.id());
                assert_function_blocks(&disassembly, &ranges, bias, &oracle, &context);

                let DisassemblyView::Function { function, .. } = &disassembly.view else {
                    unreachable!();
                };

                assert_eq!(function.module, record.module.id);
                assert_eq!(
                    function.origin,
                    FunctionOrigin::DebugInfo {
                        instance: instance.id()
                    },
                    "{context}"
                );
            }

            split += usize::from(ranges.len() > 1);
        }

        assert!(
            names.iter().any(|name| &**name == "main"),
            "{fixture}: {names:?}"
        );

        if fixture == "crash-gcc-o2-nopie" {
            assert!(
                split > 0,
                "the optimized build splits a cold path from main"
            );
            assert_split_main_names_each_range(&scenario, &modules, fixture).await;
        }

        scenario.shutdown().await;
    }
}

/// Each range of a split `main` is named by its own symbol.
async fn assert_split_main_names_each_range(scenario: &Scenario, modules: &Modules, fixture: &str) {
    let main = modules.symbol(fixture, "main");
    let disassembly = disassemble(
        scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(main))),
    )
    .await;
    let DisassemblyView::Function { function, blocks } = &disassembly.view else {
        panic!("not a function");
    };

    assert_eq!(function.name.as_ref(), "main");
    let names = blocks
        .iter()
        .map(|block| {
            block.instructions[0]
                .location
                .module
                .as_ref()
                .and_then(|module| module.image.symbol.as_ref())
                .map(|symbol| (symbol.name.to_string(), symbol.offset))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            Some(("main.cold".to_owned(), 0)),
            Some(("main".to_owned(), 0))
        ]
    );
}

#[tokio::test]
async fn functions_known_only_by_symbols_use_their_extents() {
    let scenario = Scenario::open_core(
        "elf-symbols-gcc-o0.core",
        &CoreDumpOptions::new(Scenario::fixture("elf-symbols-gcc-o0.core")),
    );
    let modules = Modules::load(&scenario).await;
    let library = "libelf-symbols-gcc.so";
    let (record, image) = modules.named(library);
    let bias = record.module.load_bias.get();
    let oracle = Objdump::read(library);
    let mut origins = BTreeSet::new();

    for symbol in image.symbols() {
        let Some(extent) = symbol.extent() else {
            continue;
        };

        let address = VirtualAddress::new(bias + extent.range.start.get());
        let disassembly = disassemble(&scenario, query(DisassemblyRange::Function(address))).await;
        let DisassemblyView::Function { function, .. } = &disassembly.view else {
            panic!("not a function");
        };

        // The preferred symbol at the start may be an alias or an
        // enclosing function.
        let named = image
            .symbolize(extent.range.start)
            .expect("code at a symbol is named");
        let named_extent = image
            .symbol(named.symbol)
            .and_then(microscope::Symbol::extent)
            .expect("extent");

        let context = format!("{library}: {}", symbol.name());
        assert_eq!(
            function.origin,
            FunctionOrigin::Symbol {
                symbol: named.symbol,
                provenance: named.provenance,
            },
            "{context}"
        );

        assert_function_blocks(
            &disassembly,
            &[(named_extent.range.start.get(), named_extent.range.end.get())],
            bias,
            &oracle,
            &context,
        );

        origins.insert((named.name.to_string(), named.provenance));
    }

    for expected in [
        ("asm_nested_outer", SymbolExtentProvenance::Declared),
        ("asm_unsized", SymbolExtentProvenance::Inferred),
        ("lib_fault", SymbolExtentProvenance::Declared),
    ] {
        assert!(
            origins.contains(&(expected.0.to_owned(), expected.1)),
            "{expected:?} not in {origins:?}"
        );
    }

    // Code that no symbol or debug information names has no function.
    let text = image
        .sections()
        .iter()
        .find(|section| section.name.as_ref() == ".text")
        .expect("text")
        .range;
    let unnamed = (text.start.get()..text.end.get())
        .find(|address| image.symbolize(ImageAddress::new(*address)).is_none())
        .expect("unnamed code");

    let missing = VirtualAddress::new(bias + unnamed);
    assert!(matches!(
        scenario
            .handle()
            .disassemble(query(DisassemblyRange::Function(missing)))
            .await,
        Err(Error::NoFunctionContainsAddress(address)) if address == missing
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn context_before_an_address_is_proven_from_known_starts() {
    let (scenario, modules) = faulted_with_breakpoints("crash-gcc-o0").await;
    let oracle = Objdump::read("crash-gcc-o0");
    let (record, _) = modules.named("crash-gcc-o0");
    let bias = record.module.load_bias.get();
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    let pc = location.address.get();

    // The program counter is itself proven, and every leading instruction
    // is reached by decoding forward from a known start.
    let (boundary, leading, block) = into_window(disassemble(&scenario, window(pc, 16, 4)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
    );
    assert_eq!(leading, None);
    assert_eq!(block.instructions.len(), 20);
    assert_eq!(block.instructions[16].address.get(), pc);
    for pair in block.instructions.windows(2) {
        assert_eq!(pair[0].end(), pair[1].address);
    }

    for instruction in block.instructions.iter() {
        let objdump = &oracle.instructions[&(instruction.address.get() - bias)];
        assert_eq!(*instruction.bytes, *objdump.bytes);
    }

    // Any other instruction start inside the function is reached from its
    // entry; an address inside an instruction is crossed.
    let function = location.image.symbol.expect("crash_segv has a symbol");
    let entry = pc - function.offset;
    let inner = oracle
        .instructions
        .range(entry - bias + 1..pc - bias)
        .find(|(_, instruction)| instruction.bytes.len() > 1)
        .map(|(address, _)| bias + address)
        .expect("a multi-byte instruction precedes the fault");

    let (boundary, leading, block) = into_window(disassemble(&scenario, window(inner, 1, 1)).await);
    assert!(
        matches!(boundary, TargetBoundary::Reached { from, .. } if from.get() == entry),
        "{boundary:?}"
    );
    assert_eq!(leading, None);
    assert_eq!(block.instructions.len(), 2);

    let (boundary, leading, block) =
        into_window(disassemble(&scenario, window(inner + 1, 1, 1)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Crossed {
            from: VirtualAddress::new(entry),
            instruction: VirtualAddress::new(inner),
        }
    );
    assert_eq!(
        leading,
        Some(ContextShortfall::Desynchronized {
            boundary: VirtualAddress::new(inner + 1)
        })
    );
    assert_eq!(block.instructions.len(), 1);
    scenario.shutdown().await;
}

/// A code symbol inside the movabs that decoding from the entry produces is
/// a known start, so decoding resumes there.
async fn assert_marked_data(scenario: &Scenario, marked: u64, context: &str) {
    let disassembly = disassemble(
        scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(marked))),
    )
    .await;
    let DisassemblyView::Function { function, blocks } = &disassembly.view else {
        panic!("not a function");
    };

    assert_eq!(function.name.as_ref(), "disasm_marked_data", "{context}");
    let [block] = &blocks[..] else {
        panic!("{context}: {blocks:?}");
    };

    let starts = block
        .instructions
        .iter()
        .map(|instruction| instruction.address.get() - marked)
        .collect::<Vec<_>>();
    assert_eq!(starts, [0, 2, 4, 9], "{context}");

    // The marker is a code symbol and, where the assembler describes
    // assembly functions, a debug-information function too.
    let [conflict] = &block.conflicts[..] else {
        panic!("{context}: {:?}", block.conflicts);
    };

    let evidence = conflict.evidence;
    assert!(
        matches!(
            evidence,
            BoundaryEvidence::FunctionRange | BoundaryEvidence::CodeSymbol
        ),
        "{context}: {conflict:?}"
    );
    assert_eq!(
        *conflict,
        BoundaryConflict {
            boundary: VirtualAddress::new(marked + 4),
            evidence,
            instruction: VirtualAddress::new(marked + 2),
        },
        "{context}"
    );
    assert_eq!(block.completion, BlockCompletion::Complete);

    // The marker proves its own address, but nothing before it.
    let (boundary, leading, _) = into_window(disassemble(scenario, window(marked + 4, 2, 1)).await);
    assert_eq!(boundary, TargetBoundary::Known(evidence));
    assert!(matches!(
        leading,
        Some(ContextShortfall::Desynchronized { boundary }) if boundary.get() == marked + 4
    ));
    let (boundary, leading, block) =
        into_window(disassemble(scenario, window(marked + 9, 1, 1)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Reached {
            from: VirtualAddress::new(marked + 4),
            evidence,
        }
    );
    assert_eq!(leading, None);
    assert_eq!(block.instructions[0].address.get(), marked + 4);
}

/// Without a marker, the movabs runs past the declared end, and asking for
/// the real code after the data says that the address lies inside an
/// instruction and proves nothing before it.
async fn assert_hidden_data(scenario: &Scenario, hidden: u64, context: &str) {
    let disassembly = disassemble(
        scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(hidden))),
    )
    .await;
    let DisassemblyView::Function { blocks, .. } = &disassembly.view else {
        panic!("not a function");
    };

    let block = &blocks[0];
    assert_eq!(block.instructions.len(), 2, "{context}: {block:#?}");
    assert_eq!(block.instructions[1].bytes.len(), 10);
    assert_eq!(
        *block.conflicts,
        [BoundaryConflict {
            boundary: VirtualAddress::new(hidden + 10),
            evidence: BoundaryEvidence::RangeEnd,
            instruction: VirtualAddress::new(hidden + 2),
        }],
        "{context}"
    );

    let (boundary, leading, block) =
        into_window(disassemble(scenario, window(hidden + 4, 4, 2)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Crossed {
            from: VirtualAddress::new(hidden),
            instruction: VirtualAddress::new(hidden + 2),
        },
        "{context}"
    );
    assert_eq!(
        leading,
        Some(ContextShortfall::Desynchronized {
            boundary: VirtualAddress::new(hidden + 4)
        })
    );

    let texts = block
        .instructions
        .iter()
        .map(|instruction| decoded(instruction).text())
        .collect::<Vec<_>>();
    assert_eq!(texts, ["mov $1, %eax", "ret"], "{context}");
}

/// Stops at the hidden code, where the program counter proves it, and the
/// trap installed there is never shown.
async fn assert_hidden_data_at_its_stop(scenario: &mut Scenario, hidden: u64) {
    scenario
        .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(hidden + 4)))
        .await;

    let stop = scenario.resume_to_stop().await;
    assert!(
        matches!(stop, StopReason::Breakpoint { address: Some(address), .. } if address.get() == hidden + 4)
    );

    let (boundary, leading, block) =
        into_window(disassemble(scenario, window(hidden + 4, 4, 2)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
    );

    assert!(matches!(
        leading,
        Some(ContextShortfall::Desynchronized { .. })
    ));
    assert_eq!(*block.instructions[0].bytes, [0xb8, 1, 0, 0, 0]);
}

/// Disassembles a function in both syntaxes and checks that only the text
/// differs, returning the Intel rendering.
async fn assert_syntaxes_agree(
    scenario: &Scenario,
    function: VirtualAddress,
) -> Vec<DisassembledInstruction> {
    let mut renderings = Vec::new();
    for syntax in [AssemblySyntax::Intel, AssemblySyntax::Att] {
        let disassembly = scenario
            .operation(
                "disassemble",
                scenario.handle().disassemble(DisassemblyQuery {
                    range: DisassemblyRange::Function(function),
                    syntax,
                }),
            )
            .await;
        assert_eq!(disassembly.syntax, syntax);
        let DisassemblyView::Function { blocks, .. } = disassembly.view else {
            panic!("not a function");
        };

        renderings.push(blocks[0].instructions.to_vec());
    }

    let att = renderings.pop().expect("AT&T");
    let intel = renderings.pop().expect("Intel");
    assert_eq!(intel.len(), att.len());
    for (intel, att) in intel.iter().zip(att.iter()) {
        assert_eq!((intel.address, &intel.bytes), (att.address, &att.bytes));
        let (intel, att) = (decoded(intel), decoded(att));
        assert_eq!(intel.flow, att.flow);
        assert_eq!(intel.references, att.references);
        assert_eq!(intel.indirect_target, att.indirect_target);
    }

    assert!(
        att.iter()
            .any(|instruction| decoded(instruction).text().contains('%'))
    );

    assert!(
        !intel
            .iter()
            .any(|instruction| decoded(instruction).text().contains('%'))
    );

    intel
}

/// Returns each direct call's target with the section and symbol naming it.
fn call_targets(
    instructions: &[DisassembledInstruction],
) -> Vec<(u64, Option<String>, Option<String>)> {
    instructions
        .iter()
        .map(decoded)
        .filter(|instruction| instruction.flow == ControlFlow::Call)
        .map(|instruction| {
            let reference = &instruction.references[0];
            assert_eq!(reference.kind, InstructionReferenceKind::BranchTarget);
            let module = reference.description.module.as_ref().expect("in a module");
            (
                reference.address.get(),
                module
                    .image
                    .section
                    .as_ref()
                    .map(|section| section.name.to_string()),
                module
                    .image
                    .symbol
                    .as_ref()
                    .map(|symbol| symbol.name.to_string()),
            )
        })
        .collect()
}

/// Calls and operands in `main` and its helper name their targets, and debug
/// information places `main`'s instructions in its source.
async fn assert_named_targets(scenario: &Scenario, modules: &Modules, fixture: &str) {
    let helper = modules.symbol(fixture, "disasm_helper");
    let main = assert_syntaxes_agree(
        scenario,
        VirtualAddress::new(modules.symbol(fixture, "main")),
    )
    .await;

    // A function is named by its symbol, and the C library's entry through
    // the linkage table by the stub binutils names after it.
    let calls = call_targets(&main);
    assert!(
        calls.contains(&(
            helper,
            Some(".text".to_owned()),
            Some("disasm_helper".to_owned())
        )),
        "{fixture}: {calls:?}"
    );
    assert!(
        calls.iter().any(|(_, section, symbol)| {
            section
                .as_deref()
                .is_some_and(|name| name.starts_with(".plt"))
                && symbol.as_deref() == Some("printf@plt")
        }),
        "{fixture}: {calls:?}"
    );

    // The helper's operand names the global it reads.
    let disassembly = disassemble(
        scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(helper))),
    )
    .await;
    let DisassemblyView::Function { blocks, .. } = disassembly.view else {
        panic!("not a function");
    };

    let operands = blocks[0]
        .instructions
        .iter()
        .flat_map(|instruction| decoded(instruction).references.iter())
        .filter(|reference| reference.kind == InstructionReferenceKind::MemoryOperand)
        .collect::<Vec<_>>();
    let [operand] = operands[..] else {
        panic!("{fixture}: {operands:#?}");
    };

    assert_eq!(
        operand.address.get(),
        modules.symbol(fixture, "disasm_counter")
    );

    let symbol = operand
        .description
        .module
        .as_ref()
        .and_then(|module| module.image.symbol.as_ref())
        .expect("named global");
    assert_eq!(
        (symbol.name.as_ref(), symbol.kind, symbol.offset),
        ("disasm_counter", SymbolKind::Data, 0)
    );

    let module = modules.named(fixture).1;
    let sources = main
        .iter()
        .filter_map(|instruction| instruction.source.as_ref())
        .filter_map(|source| module.source_file(source.file))
        .map(|file| {
            file.path
                .file_name()
                .expect("file")
                .to_string_lossy()
                .into_owned()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(sources, BTreeSet::from(["main.c".to_owned()]), "{fixture}");
}

#[tokio::test]
async fn code_that_no_verified_file_or_dump_holds_is_unreadable() {
    // The library's file was deleted after the dump, which saved no code.
    let core = "core-missing-library/crash.core";
    let scenario = Scenario::open_core(core, &CoreDumpOptions::new(Scenario::fixture(core)));
    let info = scenario
        .handle()
        .core_dump()
        .expect("core")
        .as_ref()
        .clone();
    let missing = info
        .modules
        .iter()
        .find(|module| module.recorded_path.ends_with("libcrash.so"))
        .expect("libcrash.so is recorded");
    assert_eq!(missing.state, CoreModuleState::Missing);

    let data = fs::read(Scenario::fixture("libcrash.so")).expect("read libcrash.so");
    let text = object::File::parse(data.as_slice())
        .expect("parse libcrash.so")
        .section_by_name(".text")
        .expect("text")
        .address();
    let address = missing.start.get() + text;

    // Whatever module precedes the library, nothing proven reaches its code.
    let (boundary, leading, block) =
        into_window(disassemble(&scenario, window(address, 2, 4)).await);
    assert!(
        matches!(
            boundary,
            TargetBoundary::Unverified(
                ContextShortfall::NoKnownBoundary | ContextShortfall::Unreadable { .. }
            )
        ),
        "{boundary:?}"
    );
    assert!(leading.is_some());
    assert!(matches!(
        block.completion,
        BlockCompletion::Unreadable { address: stop, .. } if stop.get() == address
    ));
    assert!(block.instructions.is_empty());
    scenario.shutdown().await;

    // A dump that saved no memory cannot verify its executable, which then
    // lends its metadata but never its bytes.
    let options = CoreDumpOptions {
        allow_module_mismatch: true,
        ..CoreDumpOptions::new(Scenario::fixture("crash-gcc-o0-memoryless.core"))
    };

    let unverified = Scenario::open_core("unverified", &options);
    let location = unverified
        .operation("location", unverified.handle().current_location())
        .await;
    let pc = location.address;

    let (boundary, leading, block) =
        into_window(disassemble(&unverified, window(pc.get(), 2, 2)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
    );

    let entry = pc.get() - location.image.symbol.expect("crash_segv").offset;
    assert_eq!(
        leading,
        Some(ContextShortfall::Unreadable {
            address: VirtualAddress::new(entry)
        })
    );
    assert!(block.instructions.is_empty());
    assert!(matches!(
        block.completion,
        BlockCompletion::Unreadable { address, .. } if address == pc
    ));

    // The function's ranges come from the metadata; its bytes do not.
    let function = disassemble(&unverified, query(DisassemblyRange::Function(pc))).await;
    let DisassemblyView::Function { function, blocks } = function.view else {
        panic!("not a function");
    };

    assert_eq!(function.name.as_ref(), "crash_segv");
    assert!(blocks[0].instructions.is_empty());
    assert!(matches!(
        blocks[0].completion,
        BlockCompletion::Unreadable { address, .. } if address.get() == entry
    ));
    unverified.shutdown().await;
}

#[tokio::test]
async fn requests_are_validated() {
    let mut scenario = Scenario::launch("crash-gcc-o0");
    assert!(matches!(
        scenario.handle().disassemble(window(0x1000, 0, 1)).await,
        Err(Error::NotRunning)
    ));

    scenario.run_to_stop().await;
    for (before, after) in [
        (0, 0),
        (MAX_WINDOW_BEFORE + 1, 1),
        (0, MAX_WINDOW_AFTER + 1),
    ] {
        assert!(matches!(
            scenario.handle().disassemble(window(0x1000, before, after)).await,
            Err(Error::InvalidDisassemblyWindow { before: b, after: a }) if (b, a) == (before, after)
        ));
    }

    assert!(matches!(
        scenario
            .handle()
            .disassemble(query(DisassemblyRange::Function(VirtualAddress::new(8))))
            .await,
        Err(Error::NoFunctionContainsAddress(address)) if address.get() == 8
    ));

    // Outside every module, nothing is readable or known.
    let (boundary, _, block) = into_window(disassemble(&scenario, window(8, 1, 1)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Unverified(ContextShortfall::NoKnownBoundary)
    );
    assert!(block.instructions.is_empty());
    scenario.shutdown().await;
}

/// Returns the instruction a window decodes at an address.
async fn instruction_at(scenario: &Scenario, address: u64) -> DisassembledInstruction {
    let (_, _, block) = into_window(disassemble(scenario, window(address, 0, 1)).await);
    block.instructions[0].clone()
}

/// Returns every instruction of the function containing an address.
async fn function_instructions(scenario: &Scenario, address: u64) -> Vec<DisassembledInstruction> {
    let disassembly = disassemble(
        scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(address))),
    )
    .await;
    let DisassemblyView::Function { blocks, .. } = disassembly.view else {
        panic!("a function query returned a window");
    };

    blocks
        .iter()
        .flat_map(|block| block.instructions.iter().cloned())
        .collect()
}

fn indirect_target(instruction: &DisassembledInstruction) -> &IndirectTarget {
    decoded(instruction)
        .indirect_target
        .as_deref()
        .unwrap_or_else(|| panic!("{:#x} is not an indirect branch", instruction.address))
}

/// Returns the slot and target of a target loaded from memory.
fn loaded(target: &IndirectTarget) -> (&AddressDescription, &AddressDescription) {
    match target {
        IndirectTarget::Memory { slot, target } => (slot, target),
        other => panic!("not loaded from memory: {other:?}"),
    }
}

fn section(description: &AddressDescription) -> Option<&str> {
    let module = description.module.as_ref()?;
    module
        .image
        .section
        .as_ref()
        .map(|section| section.name.as_ref())
}

fn symbol(description: &AddressDescription) -> Option<(&str, u64)> {
    let symbol = description.module.as_ref()?.image.symbol.as_ref()?;
    Some((symbol.name.as_ref(), symbol.offset))
}

/// Returns the call in `main` to the procedure linkage table stub for
/// `printf`, the program's only call through one.
async fn printf_call(
    scenario: &Scenario,
    modules: &Modules,
    fixture: &str,
) -> DisassembledInstruction {
    let main = function_instructions(scenario, modules.symbol(fixture, "main")).await;
    let calls = main
        .into_iter()
        .filter(|instruction| {
            decoded(instruction).flow == ControlFlow::Call
                && section(&decoded(instruction).references[0].description)
                    .is_some_and(|name| name.starts_with(".plt"))
        })
        .collect::<Vec<_>>();
    let [call] = &calls[..] else {
        panic!("{fixture}: {calls:#?}");
    };

    call.clone()
}

/// Returns the stub's jump through its global offset table slot.
async fn stub_jump(scenario: &Scenario, call: &DisassembledInstruction) -> DisassembledInstruction {
    let stub = decoded(call).references[0].address.get();
    let (_, _, block) = into_window(disassemble(scenario, window(stub, 0, 4)).await);
    block
        .instructions
        .iter()
        .find(|instruction| decoded(instruction).flow == ControlFlow::IndirectJump)
        .expect("a linkage stub jumps through its slot")
        .clone()
}

fn register(registers: &microscope::RegisterSnapshot, name: &str) -> u64 {
    let value = registers
        .registers
        .iter()
        .find(|value| value.register.name.as_ref() == name)
        .unwrap_or_else(|| panic!("no register {name}"));
    let value = value
        .bytes
        .as_deref()
        .expect("innermost registers are saved");

    let mut bytes = [0; 8];
    bytes[..value.len()].copy_from_slice(value);
    u64::from_le_bytes(bytes)
}

/// Each labeled indirect branch in indirect.S, in execution order.
const INDIRECT_BRANCHES: [&str; 7] = [
    "disasm_call_slot",
    "disasm_call_method",
    "disasm_call_register",
    "disasm_call_thread",
    "disasm_call_got",
    "disasm_jump_table",
    "disasm_return",
];

/// Away from the stop, only a target loaded from memory that the instruction
/// alone addresses is known.
async fn assert_targets_away_from_the_stop(scenario: &Scenario, modules: &Modules, fixture: &str) {
    let symbol_address = |name| modules.symbol(fixture, name);
    let targets = function_instructions(scenario, symbol_address("disasm_indirect"))
        .await
        .into_iter()
        .filter_map(|instruction| {
            let target = decoded(&instruction).indirect_target.clone()?;
            Some((instruction.address.get(), target))
        })
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        targets.len(),
        INDIRECT_BRANCHES.len(),
        "{fixture}: {targets:#?}"
    );

    let (slot, target) = loaded(&targets[&symbol_address("disasm_call_slot")]);
    assert_eq!(
        (slot.address.get(), target.address.get()),
        (
            symbol_address("disasm_slot"),
            symbol_address("disasm_target_one")
        ),
        "{fixture}"
    );
    assert_eq!(symbol(slot), Some(("disasm_slot", 0)));
    assert_eq!(symbol(target), Some(("disasm_target_one", 0)));

    // The dynamic loader filled the slot at startup.
    let (slot, target) = loaded(&targets[&symbol_address("disasm_call_got")]);
    assert_eq!(
        (section(slot), target.address.get()),
        (Some(".got"), modules.symbol("libc.so.6", "labs")),
        "{fixture}"
    );

    for label in [
        "disasm_call_method",
        "disasm_call_register",
        "disasm_call_thread",
        "disasm_jump_table",
        "disasm_return",
    ] {
        assert_eq!(
            *targets[&symbol_address(label)],
            IndirectTarget::NeedsRegisters,
            "{fixture}: {label}"
        );
    }

    let forms = function_instructions(scenario, symbol_address("disasm_indirect_forms"))
        .await
        .iter()
        .map(|instruction| indirect_target(instruction).clone())
        .collect::<Vec<_>>();
    assert_eq!(
        forms,
        [
            IndirectTarget::Unreadable {
                slot: AddressDescription {
                    address: VirtualAddress::new(0),
                    module: None,
                },
                address: VirtualAddress::new(0),
                reason: MemoryReadUnavailableReason::Inaccessible,
            },
            IndirectTarget::Unsupported,
            IndirectTarget::Unsupported,
            IndirectTarget::Unsupported,
        ],
        "{fixture}"
    );
}

/// Stops at each labeled branch, where the registers resolve its target,
/// and steps it to check that execution reaches that target.
async fn assert_targets_at_each_stop(scenario: &mut Scenario, modules: &Modules, fixture: &str) {
    let symbol_address = |name| modules.symbol(fixture, name);
    let return_site = function_instructions(scenario, symbol_address("main"))
        .await
        .into_iter()
        .find(|instruction| {
            let target = decoded(instruction).references.first();
            target.is_some_and(|target| target.address.get() == symbol_address("disasm_indirect"))
        })
        .expect("main calls disasm_indirect")
        .end()
        .get();

    for label in INDIRECT_BRANCHES {
        let address = VirtualAddress::new(symbol_address(label));
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(address))
            .await;
    }

    for label in INDIRECT_BRANCHES {
        let pc = symbol_address(label);
        assert_eq!(
            support::breakpoint_address(&scenario.resume_to_stop().await),
            VirtualAddress::new(pc),
            "{fixture}"
        );

        let registers = scenario.operation("registers", scenario.handle().registers());
        let registers = registers.await;
        let instruction = instruction_at(scenario, pc).await;
        let encoded = decoded(&instruction)
            .references
            .first()
            .map(|reference| reference.address.get());

        // The slot holding each target, and the target.
        let expected = match label {
            "disasm_call_slot" => (encoded, symbol_address("disasm_target_one")),
            "disasm_call_method" => (
                Some(symbol_address("disasm_methods") + 8),
                symbol_address("disasm_target_two"),
            ),
            "disasm_call_register" => (None, symbol_address("disasm_target_four")),
            // The thread's slot lies just below its thread pointer.
            "disasm_call_thread" => (
                Some(register(&registers, "fs_base") - 8),
                symbol_address("disasm_target_two"),
            ),
            "disasm_call_got" => (encoded, modules.symbol("libc.so.6", "labs")),
            "disasm_jump_table" => (
                Some(symbol_address("disasm_cases") + 8),
                symbol_address("disasm_case_one"),
            ),
            "disasm_return" => (Some(register(&registers, "rsp")), return_site),
            _ => unreachable!(),
        };

        let target = indirect_target(&instruction);
        let context = format!("{fixture}: {label}: {target:#?}");
        let (slot, destination) = match target {
            IndirectTarget::Register { target } => (None, target),
            IndirectTarget::Memory { slot, target } => (Some(slot), target),
            _ => panic!("{context}: unresolved at the stop"),
        };

        assert_eq!(
            (
                slot.map(|slot| slot.address.get()),
                destination.address.get()
            ),
            expected,
            "{context}"
        );

        let names = (slot.and_then(symbol), symbol(destination));
        match label {
            "disasm_call_method" => assert_eq!(names.0, Some(("disasm_methods", 8))),
            "disasm_jump_table" => assert_eq!(names.0, Some(("disasm_cases", 8))),
            "disasm_return" => assert_eq!(names.1.map(|(name, _)| name), Some("main")),
            _ => {}
        }

        scenario.step_to_stop(StepKind::Instruction).await;

        let location = scenario.operation("location", scenario.handle().current_location());
        assert_eq!(location.await.address, destination.address, "{context}");
    }
}

/// The fixture's code names its targets in both syntaxes, reports the data
/// hidden inside it, and resolves each indirect branch's target at its stop.
/// `main` calls the data functions before the indirect branches and printf.
#[tokio::test]
async fn code_names_its_targets_and_reports_data_inside_it() {
    for fixture in ["disassembly-gcc-o0", "disassembly-clang-o2-nopie"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main").await;
        scenario.run_to_stop().await;
        let modules = Modules::load(&scenario).await;
        assert_named_targets(&scenario, &modules, fixture).await;
        let hidden = modules.symbol(fixture, "disasm_hidden_data");
        assert_marked_data(
            &scenario,
            modules.symbol(fixture, "disasm_marked_data"),
            fixture,
        )
        .await;
        assert_hidden_data(&scenario, hidden, fixture).await;
        assert_targets_away_from_the_stop(&scenario, &modules, fixture).await;

        // Lazy binding leaves a linkage stub's slot pointing back into the
        // stub until the first call resolves it.
        let call = printf_call(&scenario, &modules, fixture).await;
        let jump = stub_jump(&scenario, &call).await;

        let (unresolved_slot, target) = loaded(indirect_target(&jump));
        let unresolved_slot = unresolved_slot.clone();
        assert_eq!(section(&unresolved_slot), Some(".got.plt"), "{fixture}");
        assert_eq!(target.address, jump.end(), "{fixture}");
        assert_eq!(section(target), Some(".plt"), "{fixture}");

        assert_hidden_data_at_its_stop(&mut scenario, hidden).await;
        assert_targets_at_each_stop(&mut scenario, &modules, fixture).await;

        // After the first call, the slot holds the C library's function.
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(call.end()))
            .await;
        assert_eq!(
            support::breakpoint_address(&scenario.resume_to_stop().await),
            call.end()
        );

        let jump = stub_jump(&scenario, &call).await;
        let (slot, target) = loaded(indirect_target(&jump));
        assert_eq!(*slot, unresolved_slot, "{fixture}");
        assert_eq!(
            target.address.get(),
            modules.symbol("libc.so.6", "printf"),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

/// Returns every indirect jump, call, and return in a module's executable
/// sections, decoded as the objdump comparisons check, outside the extents
/// of the `skipped` symbols.
async fn indirect_branch_sites(
    scenario: &Scenario,
    modules: &Modules,
    file_name: &str,
    skipped: &[&str],
) -> BTreeSet<VirtualAddress> {
    let (record, image) = modules.named(file_name);
    let bias = record.module.load_bias.get();
    let skipped = image
        .symbols()
        .filter(|symbol| skipped.contains(&symbol.name()))
        .filter_map(microscope::Symbol::extent)
        .map(|extent| bias + extent.range.start.get()..bias + extent.range.end.get())
        .collect::<Vec<_>>();
    let kept =
        |address: VirtualAddress| !skipped.iter().any(|range| range.contains(&address.get()));
    let mut sites = BTreeSet::new();

    for section in image.sections().iter().filter(|section| section.executable) {
        let end = bias + section.range.end.get();
        let mut address = bias + section.range.start.get();

        while address < end {
            let (_, _, block) =
                into_window(disassemble(scenario, window(address, 0, MAX_WINDOW_AFTER)).await);
            assert!(
                block
                    .conflicts
                    .iter()
                    .all(|conflict| conflict.boundary.get() >= end || !kept(conflict.instruction)),
                "{file_name}: {:?}",
                block.conflicts
            );
            assert!(!block.instructions.is_empty(), "{file_name}: {address:#x}");

            for instruction in block
                .instructions
                .iter()
                .take_while(|instruction| instruction.address.get() < end)
            {
                address = instruction.end().get();
                if let InstructionContent::Decoded(decoded) = &instruction.content
                    && decoded.indirect_target.is_some()
                    && kept(instruction.address)
                {
                    sites.insert(instruction.address);
                }
            }
        }
    }

    sites
}

/// Runs a fixture from `main` to its exit, stopping at every indirect jump,
/// call, and return in its own code, the C library, and the dynamic loader,
/// and checks that executing each one transfers control where its target
/// said. Returns how many branches of each kind were checked.
///
/// `library_sites` keeps each shared library's sites by image address, so a
/// library that several fixtures load is decoded once.
async fn assert_executed_targets(
    fixture: &str,
    library_sites: &mut BTreeMap<Arc<PathBuf>, BTreeSet<u64>>,
) -> BTreeMap<&'static str, usize> {
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("main").await;
    scenario.run_to_stop().await;
    let modules = Modules::load(&scenario).await;

    // The functions of layout.S hide data in code, which decoding misreads.
    let layout = ["disasm_marked_data", "disasm_hidden_data"];
    let mut sites = indirect_branch_sites(&scenario, &modules, fixture, &layout).await;
    for library in ["libc.so.6", "ld-linux-x86-64.so.2"] {
        let (record, _) = modules.named(library);
        let bias = record.module.load_bias.get();
        if !library_sites.contains_key(&record.path) {
            let found = indirect_branch_sites(&scenario, &modules, library, &[]).await;
            let found = found.into_iter().map(|site| site.get() - bias).collect();
            library_sites.insert(Arc::clone(&record.path), found);
        }

        sites.extend(
            library_sites[&record.path]
                .iter()
                .map(|site| VirtualAddress::new(bias + site)),
        );
    }

    for site in &sites {
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(*site))
            .await;
    }

    let mut verified = BTreeMap::new();
    let mut stop = scenario.resume_to_stop().await;

    loop {
        let pc = match stop {
            StopReason::Exited(status) => {
                assert_eq!(status, microscope::ExitStatus::Code(0), "{fixture}");
                break;
            }
            StopReason::Breakpoint {
                address: Some(address),
                ..
            } => address,
            StopReason::Step { .. } => {
                let location = scenario.operation("location", scenario.handle().current_location());
                location.await.address
            }
            other => panic!("{fixture}: unexpected stop {other:?}"),
        };

        // A step may land on another site, which resuming would skip.
        if !sites.contains(&pc) {
            stop = scenario.resume_to_stop().await;
            continue;
        }

        let instruction = instruction_at(&scenario, pc.get()).await;
        let current = decoded(&instruction);
        let context = format!("{fixture}: {pc}: {current:#?}");
        let (kind, target) = match indirect_target(&instruction) {
            IndirectTarget::Register { target } => ("register", target.address),
            IndirectTarget::Memory { slot, target } => {
                let encoded = current.references.iter().any(|reference| {
                    reference.kind == InstructionReferenceKind::MemoryOperand
                        && reference.address == slot.address
                });
                let kind = match (encoded, current.flow) {
                    (true, _) => "memory the instruction addresses",
                    (false, ControlFlow::Return) => "return address",
                    (false, _) => "memory a register addresses",
                };

                (kind, target.address)
            }
            _ => panic!("{context}: unresolved at the stop"),
        };

        stop = scenario.step_to_stop(StepKind::Instruction).await;
        assert_eq!(
            stop,
            StopReason::Step {
                kind: StepKind::Instruction
            },
            "{context}"
        );

        let location = scenario.operation("location", scenario.handle().current_location());
        assert_eq!(location.await.address, target, "{kind}: {context}");

        *verified.entry(kind).or_default() += 1;
    }

    scenario.shutdown().await;

    verified
}

/// Executing an indirect branch reaches the target read at its stop, for
/// every one a program runs through lazy binding, formatted output, and exit.
#[tokio::test]
async fn indirect_targets_are_where_execution_goes() {
    let mut library_sites = BTreeMap::new();
    for fixture in ["disassembly-gcc-o0", "disassembly-clang-o2-nopie"] {
        let verified = assert_executed_targets(fixture, &mut library_sites).await;
        for kind in [
            "register",
            "memory the instruction addresses",
            "memory a register addresses",
            "return address",
        ] {
            assert!(
                verified.get(kind).is_some_and(|count| *count > 0),
                "{fixture}: {kind}: {verified:?}"
            );
        }
    }
}

/// Maps each slot of an ELF file that the dynamic loader fills to the name of
/// the symbol it holds, by image address.
fn dynamic_slots(file_name: &str) -> BTreeMap<u64, String> {
    let data = fs::read(Scenario::fixture(file_name)).expect("read fixture");
    let file = object::File::parse(data.as_slice()).expect("parse fixture");
    let symbols = file.dynamic_symbol_table().expect("dynamic symbols");

    file.dynamic_relocations()
        .expect("dynamic relocations")
        .filter_map(|(offset, relocation)| {
            let object::RelocationTarget::Symbol(index) = relocation.target() else {
                return None;
            };

            let name = symbols.symbol_by_index(index).ok()?.name().ok()?;
            Some((offset, name.to_owned()))
        })
        .collect()
}

/// Returns each linkage stub's jump through its slot, by the name of the
/// symbol the slot holds.
async fn linkage_jumps(
    scenario: &Scenario,
    modules: &Modules,
    file_name: &str,
) -> BTreeMap<String, DisassembledInstruction> {
    let slots = dynamic_slots(file_name);
    let (record, image) = modules.named(file_name);
    let bias = record.module.load_bias.get();
    let mut jumps = BTreeMap::new();
    for section in image
        .sections()
        .iter()
        .filter(|section| section.executable && section.name.starts_with(".plt"))
    {
        let (start, end) = (section.range.start.get(), section.range.end.get());
        let count = u32::try_from((end - start) / 2).expect("small section");
        let (_, _, block) =
            into_window(disassemble(scenario, window(bias + start, 0, count)).await);

        for instruction in block
            .instructions
            .iter()
            .filter(|instruction| instruction.address.get() < bias + end)
            .filter(|instruction| decoded(instruction).flow == ControlFlow::IndirectJump)
        {
            let slot = match indirect_target(instruction) {
                IndirectTarget::Memory { slot, .. } | IndirectTarget::Unreadable { slot, .. } => {
                    slot.address.get() - bias
                }
                other => panic!("{file_name}: {other:?}"),
            };

            // The first stub jumps to the lazy-binding resolver.
            if let Some(name) = slots.get(&slot) {
                jumps.insert(name.clone(), instruction.clone());
            }
        }
    }

    jumps
}

#[tokio::test]
async fn core_dumps_name_the_targets_their_saved_slots_hold() {
    // The program had called every stub it uses except abort's before it
    // crashed, and lazy binding had resolved those.
    let core = "crash-gcc-o0-segv.core";
    let scenario = Scenario::open_core(core, &CoreDumpOptions::new(Scenario::fixture(core)));
    let modules = Modules::load(&scenario).await;
    let jumps = linkage_jumps(&scenario, &modules, "crash-gcc-o0").await;
    let target = |name: &str| loaded(indirect_target(&jumps[name])).1.clone();
    assert_eq!(
        target("crash_library_touch").address.get(),
        modules.symbol("libcrash.so", "crash_library_touch")
    );
    for name in [
        "pthread_attr_init",
        "pthread_attr_setguardsize",
        "pthread_create",
        "strcmp",
        "__cxa_finalize",
    ] {
        let module = target(name).module.expect(name).module;
        assert_eq!(
            modules
                .records
                .iter()
                .find(|record| record.module.id == module)
                .map(|record| record.path.file_name()),
            Some(Some("libc.so.6".as_ref())),
            "{name}"
        );
    }

    // crash-gcc-o0 links with lazy binding, so abort's slot still points
    // back into its stub.
    assert_eq!(target("abort").address, jumps["abort"].end());
    assert_eq!(jumps.len(), 7, "{jumps:#?}");
    scenario.shutdown().await;

    // A dump that saved only file headers holds no slots, while the stubs
    // themselves come from the verified file.
    let core = "crash-gcc-o0-headers-only.core";
    let scenario = Scenario::open_core(core, &CoreDumpOptions::new(Scenario::fixture(core)));
    let modules = Modules::load(&scenario).await;
    let jumps = linkage_jumps(&scenario, &modules, "crash-gcc-o0").await;
    assert_eq!(jumps.len(), 7, "{jumps:#?}");
    for (name, jump) in &jumps {
        let IndirectTarget::Unreadable { slot, address, .. } = indirect_target(jump) else {
            panic!("{name}: {jump:#?}");
        };

        assert_eq!(slot.address, *address, "{name}");
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn a_register_that_a_restarted_system_call_replaces_is_never_used() {
    let child = support::ExternalProcess::spawn(&Scenario::fixture("attach-restart"));

    // Attaching must interrupt pause(2).
    support::wait_for_system_call(child.process_id(), 34);
    let debugger = child.attach().await;
    let handle = debugger.handle();
    let location = handle.current_location().await.expect("location");
    let modules = handle.loaded_modules().await.expect("modules");
    let executable = &modules.modules[0];
    let image = handle
        .loaded_module_image(executable.module.id)
        .await
        .expect("image");
    let jump = image
        .symbols()
        .find(|symbol| symbol.name() == "attach_restart_jump")
        .expect("the jump is labeled");

    assert_eq!(
        location.address.get(),
        executable.module.load_bias.get() + jump.address().get()
    );

    // The kernel will restart the call, which replaces rax, so the jump
    // through it has no known target even at the stop.
    let registers = handle.registers().await.expect("registers");
    assert_eq!(register(&registers, "orig_rax"), 34);
    assert_eq!(register(&registers, "rax"), (-514_i64).cast_unsigned());
    let disassembly = handle
        .disassemble(window(location.address.get(), 0, 1))
        .await
        .expect("disassemble");
    let (boundary, _, block) = into_window(disassembly);
    assert_eq!(
        boundary,
        TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
    );

    assert_eq!(
        *indirect_target(&block.instructions[0]),
        IndirectTarget::NeedsRegisters
    );

    debugger.shutdown().await.expect("detach");
}
