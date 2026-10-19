use super::fake::FakeSource;
use super::*;
use crate::{ByteOrder, PointerWidth};

const BASE: u64 = 0x1000;

fn x86(syntax: AssemblySyntax) -> Box<dyn InstructionDecoder> {
    decoder_for(
        TargetDescription {
            architecture: Architecture::X86_64,
            byte_order: ByteOrder::Little,
            pointer_width: PointerWidth::Bits64,
        },
        syntax,
    )
    .expect("x86-64 decoder")
}

fn function(source: &mut FakeSource, start: u64, end: u64) -> DisassemblyBlock {
    let mut decoder = x86(AssemblySyntax::Intel);
    let mut blocks = Engine::new(source, decoder.as_mut())
        .function(&[AddressRange {
            start: VirtualAddress::new(start),
            end: VirtualAddress::new(end),
        }])
        .expect("fake reads never fail");
    blocks.pop().expect("one block")
}

fn window(
    source: &mut FakeSource,
    address: u64,
    before: u32,
    after: u32,
) -> (TargetBoundary, Option<ContextShortfall>, DisassemblyBlock) {
    let mut decoder = x86(AssemblySyntax::Intel);
    let window = Engine::new(source, decoder.as_mut())
        .window(VirtualAddress::new(address), before, after)
        .expect("fake reads never fail");
    (window.boundary, window.shortfall, window.block)
}

/// Bytes, Intel and AT&T renderings, control flow, and encoded addresses.
type Case = (
    &'static [u8],
    &'static str,
    &'static str,
    ControlFlow,
    &'static [(InstructionReferenceKind, u64)],
);

/// Renders each instruction as `address: text`, with `(bad)` and
/// `(truncated)` for undecodable positions.
fn listing(block: &DisassemblyBlock) -> Vec<String> {
    block
        .instructions
        .iter()
        .map(|instruction| {
            let text = match &instruction.content {
                InstructionContent::Decoded(decoded) => decoded.text(),
                InstructionContent::Invalid => "(bad)".to_owned(),
                InstructionContent::Truncated => "(truncated)".to_owned(),
            };

            format!("{:#x}: {text}", instruction.address)
        })
        .collect()
}

/// `jmp` over two bytes of data that begin a ten-byte `movabs`, followed by
/// the real code at 0x1004. Decoding straight through swallows the real
/// instructions.
fn data_in_code() -> Vec<u8> {
    vec![
        0xeb, 0x02, // jmp 0x1004
        0x48, 0xb8, // data: the start of movabs rax, imm64
        0xb8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
        0xc3, // ret
        0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // padding
    ]
}

#[test]
fn decoding_resumes_at_known_starts_that_an_instruction_overlaps() {
    let mut source =
        FakeSource::new(BASE, data_in_code()).start(0x1004, BoundaryEvidence::CodeSymbol);
    let block = function(&mut source, BASE, 0x100a);

    assert_eq!(
        listing(&block),
        [
            "0x1000: jmp 0x1004",
            "0x1002: mov rax, 0x9090c300000001b8",
            "0x1004: mov eax, 1",
            "0x1009: ret",
        ]
    );
    assert_eq!(
        *block.conflicts,
        [BoundaryConflict {
            boundary: VirtualAddress::new(0x1004),
            evidence: BoundaryEvidence::CodeSymbol,
            instruction: VirtualAddress::new(0x1002),
        }]
    );
    assert_eq!(block.completion, BlockCompletion::Complete);
}

#[test]
fn context_before_an_address_requires_landing_on_it_from_a_known_start() {
    // Decoding from the function start crosses 0x1004 inside the movabs, so
    // nothing before it is proven and the address itself is suspect.
    let mut source =
        FakeSource::new(BASE, data_in_code()).start(BASE, BoundaryEvidence::FunctionRange);
    let (boundary, shortfall, block) = window(&mut source, 0x1004, 3, 2);
    assert_eq!(
        boundary,
        TargetBoundary::Crossed {
            from: VirtualAddress::new(BASE),
            instruction: VirtualAddress::new(0x1002),
        }
    );
    assert_eq!(
        shortfall,
        Some(ContextShortfall::Desynchronized {
            boundary: VirtualAddress::new(0x1004)
        })
    );
    assert_eq!(listing(&block), ["0x1004: mov eax, 1", "0x1009: ret"]);

    // A program counter proves the address, but still not what precedes it.
    let mut source = source.start(0x1004, BoundaryEvidence::ProgramCounter);
    let (boundary, shortfall, block) = window(&mut source, 0x1004, 3, 1);
    assert_eq!(
        boundary,
        TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
    );
    assert!(matches!(
        shortfall,
        Some(ContextShortfall::Desynchronized { .. })
    ));
    assert_eq!(listing(&block), ["0x1004: mov eax, 1"]);

    // Landing exactly on an address proves it and everything before it.
    let (boundary, shortfall, block) = window(&mut source, 0x1009, 3, 1);
    assert_eq!(
        boundary,
        TargetBoundary::Reached {
            from: VirtualAddress::new(0x1004),
            evidence: BoundaryEvidence::ProgramCounter,
        }
    );

    // Only one instruction lies between the nearest known starts, and the
    // segment before that one crossed it.
    assert_eq!(
        shortfall,
        Some(ContextShortfall::Desynchronized {
            boundary: VirtualAddress::new(0x1004)
        })
    );
    assert_eq!(listing(&block), ["0x1004: mov eax, 1", "0x1009: ret"]);
}

#[test]
fn context_spans_consecutive_known_starts_until_enough_or_none_remain() {
    // Two functions of single-byte and multi-byte instructions.
    let bytes = vec![
        0x55, // push rbp
        0x48, 0x89, 0xe5, // mov rbp, rsp
        0x5d, // pop rbp
        0xc3, // ret
        0x53, // push rbx
        0x31, 0xc0, // xor eax, eax
        0x5b, // pop rbx
        0xc3, // ret
    ];
    let mut source = FakeSource::new(BASE, bytes)
        .start(BASE, BoundaryEvidence::FunctionRange)
        .start(0x1006, BoundaryEvidence::CodeSymbol);

    let (boundary, shortfall, block) = window(&mut source, 0x1009, 4, 1);
    assert_eq!(
        boundary,
        TargetBoundary::Reached {
            from: VirtualAddress::new(0x1006),
            evidence: BoundaryEvidence::CodeSymbol,
        }
    );
    assert_eq!(shortfall, None);
    assert_eq!(
        listing(&block),
        [
            "0x1004: pop rbp",
            "0x1005: ret",
            "0x1006: push rbx",
            "0x1007: xor eax, eax",
            "0x1009: pop rbx",
        ]
    );

    let (_, shortfall, block) = window(&mut source, 0x1009, 10, 0);
    assert_eq!(shortfall, Some(ContextShortfall::NoKnownBoundary));
    assert_eq!(block.instructions.len(), 6);
    assert_eq!(block.range.end, VirtualAddress::new(0x1009));

    // Without any known start, nothing proves the address.
    source.starts.clear();
    let (boundary, shortfall, block) = window(&mut source, 0x1009, 2, 1);
    assert_eq!(
        boundary,
        TargetBoundary::Unverified(ContextShortfall::NoKnownBoundary)
    );
    assert_eq!(shortfall, Some(ContextShortfall::NoKnownBoundary));
    assert_eq!(listing(&block), ["0x1009: pop rbx"]);
}

#[test]
fn unreadable_memory_truncates_instead_of_guessing() {
    // mov eax, 1 straddles the end of readable memory at 0x1003.
    let mut source = FakeSource::new(BASE, vec![0x90, 0xb8, 0x01, 0x00, 0x00, 0x00]);
    source.readable[0].end = 0x1003;
    let block = function(&mut source, BASE, 0x1006);
    assert_eq!(listing(&block), ["0x1000: nop", "0x1001: (truncated)"]);
    assert_eq!(*block.instructions[1].bytes, [0xb8, 0x01]);
    assert_eq!(
        block.completion,
        BlockCompletion::Unreadable {
            address: VirtualAddress::new(0x1003),
            reason: MemoryReadUnavailableReason::Inaccessible,
        }
    );

    // A truncated instruction's length is unknown, so it never claims to
    // cross the end of its range.
    let block = function(&mut source, BASE, 0x1002);
    assert_eq!(listing(&block), ["0x1000: nop", "0x1001: (truncated)"]);
    assert!(block.conflicts.is_empty());
    assert!(matches!(
        block.completion,
        BlockCompletion::Unreadable { .. }
    ));

    // Context cannot be proven across memory that cannot be read.
    let mut source = FakeSource::new(BASE, vec![0x90; 8]).start(BASE, BoundaryEvidence::CodeSymbol);
    source.readable[0].start = 0x1002;
    let (boundary, shortfall, block) = window(&mut source, 0x1004, 2, 1);
    let unreadable = ContextShortfall::Unreadable {
        address: VirtualAddress::new(BASE),
    };

    assert_eq!(boundary, TargetBoundary::Unverified(unreadable));
    assert_eq!(shortfall, Some(unreadable));
    assert_eq!(listing(&block), ["0x1004: nop"]);
}

#[test]
fn undecodable_bytes_advance_one_byte_and_range_ends_are_checked() {
    // 0x06 (push es) is invalid in 64-bit mode; the call crosses the end of
    // a range declared one byte too short.
    let mut source = FakeSource::new(BASE, vec![0x06, 0x90, 0xe8, 0, 0, 0, 0, 0xc3]);
    let block = function(&mut source, BASE, 0x1006);
    assert_eq!(
        listing(&block),
        ["0x1000: (bad)", "0x1001: nop", "0x1002: call 0x1007"]
    );
    assert_eq!(
        *block.conflicts,
        [BoundaryConflict {
            boundary: VirtualAddress::new(0x1006),
            evidence: BoundaryEvidence::RangeEnd,
            instruction: VirtualAddress::new(0x1002),
        }]
    );
}

#[test]
fn reads_across_chunk_boundaries_and_unaligned_readable_starts() {
    // A ten-byte instruction straddles a read-chunk boundary.
    let base = READ_CHUNK - 4;
    let mut bytes = vec![0x48, 0xb8, 1, 2, 3, 4, 5, 6, 7, 8];
    bytes.push(0xc3);
    let mut source = FakeSource::new(base, bytes);
    let block = function(&mut source, base, base + 11);
    assert_eq!(
        listing(&block),
        [
            format!("{base:#x}: mov rax, 0x807060504030201"),
            format!("{:#x}: ret", base + 10),
        ]
    );

    // Readable memory that begins after a chunk's unreadable prefix.
    let mut source = FakeSource::new(READ_CHUNK, vec![0x90; 32]);
    source.readable[0].start = READ_CHUNK + 16;
    let block = function(&mut source, READ_CHUNK + 20, READ_CHUNK + 22);
    assert_eq!(block.instructions.len(), 2);
    assert_eq!(block.completion, BlockCompletion::Complete);
}

#[test]
fn function_disassembly_stops_at_its_instruction_limit() {
    let count = MAX_FUNCTION_INSTRUCTIONS + 8;
    let mut source = FakeSource::new(BASE, vec![0x90; count]);
    let mut decoder = x86(AssemblySyntax::Intel);
    let half = BASE + (count / 2) as u64;
    let blocks = Engine::new(&mut source, decoder.as_mut())
        .function(&[
            AddressRange {
                start: VirtualAddress::new(BASE),
                end: VirtualAddress::new(half),
            },
            AddressRange {
                start: VirtualAddress::new(half),
                end: VirtualAddress::new(BASE + count as u64),
            },
        ])
        .expect("fake reads never fail");

    assert_eq!(blocks[0].completion, BlockCompletion::Complete);
    assert_eq!(
        blocks[0].instructions.len() + blocks[1].instructions.len(),
        MAX_FUNCTION_INSTRUCTIONS
    );
    assert_eq!(
        blocks[1].completion,
        BlockCompletion::Limited {
            next: VirtualAddress::new(BASE + MAX_FUNCTION_INSTRUCTIONS as u64)
        }
    );
}

#[test]
fn instructions_render_in_either_syntax_and_report_encoded_addresses() {
    let cases: [Case; 8] = [
        (
            &[0xe8, 0x0b, 0x00, 0x00, 0x00],
            "call 0x1010",
            "call 0x1010",
            ControlFlow::Call,
            &[(InstructionReferenceKind::BranchTarget, 0x1010)],
        ),
        (
            &[0x74, 0xfe],
            "je 0x1000",
            "je 0x1000",
            ControlFlow::ConditionalJump,
            &[(InstructionReferenceKind::BranchTarget, 0x1000)],
        ),
        (
            &[0x48, 0x8b, 0x05, 0xf9, 0x0f, 0x00, 0x00],
            "mov rax, [rip+0xff9]",
            "mov 0xff9(%rip), %rax",
            ControlFlow::Sequential,
            &[(InstructionReferenceKind::MemoryOperand, 0x2000)],
        ),
        (
            &[0x8b, 0x04, 0x25, 0x10, 0x40, 0x40, 0x00],
            "mov eax, [0x404010]",
            "mov 0x404010, %eax",
            ControlFlow::Sequential,
            &[(InstructionReferenceKind::MemoryOperand, 0x0040_4010)],
        ),
        // Thread-local storage is not a flat address.
        (
            &[0x64, 0x48, 0x8b, 0x04, 0x25, 0x28, 0x00, 0x00, 0x00],
            "mov rax, fs:[0x28]",
            "mov %fs:0x28, %rax",
            ControlFlow::Sequential,
            &[],
        ),
        (
            &[0xff, 0x25, 0xfa, 0x0f, 0x00, 0x00],
            "jmp qword ptr [rip+0xffa]",
            "jmpq *0xffa(%rip)",
            ControlFlow::IndirectJump,
            &[(InstructionReferenceKind::MemoryOperand, 0x2000)],
        ),
        (
            &[0xf0, 0x0f, 0xb1, 0x0b],
            "lock cmpxchg [rbx], ecx",
            "lock cmpxchg %ecx, (%rbx)",
            ControlFlow::Sequential,
            &[],
        ),
        (&[0x0f, 0x0b], "ud2", "ud2", ControlFlow::Exception, &[]),
    ];

    for (bytes, intel, att, flow, references) in cases {
        for (syntax, text) in [(AssemblySyntax::Intel, intel), (AssemblySyntax::Att, att)] {
            let RawDecode::Instruction {
                length,
                tokens,
                flow: decoded_flow,
                references: decoded_references,
                ..
            } = x86(syntax).decode(BASE, bytes, None)
            else {
                panic!("{bytes:x?} did not decode");
            };

            let rendered = tokens
                .iter()
                .map(|token| token.text.as_ref())
                .collect::<String>();
            assert_eq!((length, rendered.as_str()), (bytes.len(), text));
            assert_eq!(decoded_flow, flow, "{text}");
            assert_eq!(decoded_references, references, "{text}");
        }
    }

    // Tokens classify the text for clients that color it.
    let RawDecode::Instruction { tokens, .. } =
        x86(AssemblySyntax::Intel).decode(BASE, &[0xe8, 0x0b, 0x00, 0x00, 0x00], None)
    else {
        panic!("call did not decode");
    };

    let kinds = tokens
        .iter()
        .filter(|token| token.kind != InstructionTokenKind::Text)
        .map(|token| (token.kind, token.text.as_ref()))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            (InstructionTokenKind::Mnemonic, "call"),
            (InstructionTokenKind::Address, "0x1010"),
        ]
    );
}

/// Decodes one instruction at `BASE`, with a thread stopped there when
/// `registers` are given. Pointer slots follow at 0x2000: an address at
/// 0x2000, 0x1010 at 0x2008, and four readable bytes at 0x2010.
fn indirect_target(code: &[u8], registers: Option<&RegisterFile>) -> Option<IndirectTarget> {
    let mut bytes = vec![0; 0x1018];
    bytes[..code.len()].copy_from_slice(code);
    bytes[0x1000..0x1008].copy_from_slice(&0x1234_5678_9abc_def0_u64.to_le_bytes());
    bytes[0x1008..0x1010].copy_from_slice(&0x1010_u64.to_le_bytes());

    let mut source = FakeSource::new(BASE, bytes);
    source.readable[0].end = 0x2014;
    source.stopped = registers.map(|registers| (VirtualAddress::new(BASE), registers.clone()));
    let (_, _, block) = window(&mut source, BASE, 0, 1);

    match &block.instructions[0].content {
        InstructionContent::Decoded(decoded) => decoded.indirect_target.as_deref().cloned(),
        other => panic!("{code:x?} did not decode: {other:?}"),
    }
}

#[test]
fn indirect_targets_are_read_from_the_stopped_state() {
    let at = |address| AddressDescription {
        address: VirtualAddress::new(address),
        module: None,
    };

    let memory = |slot, target| {
        Some(IndirectTarget::Memory {
            slot: at(slot),
            target: at(target),
        })
    };

    let unreadable = |slot, address| {
        Some(IndirectTarget::Unreadable {
            slot: at(slot),
            address: VirtualAddress::new(address),
            reason: MemoryReadUnavailableReason::Inaccessible,
        })
    };

    let registers = RegisterFile::new([
        (crate::unwind::RAX, 0x1_0000_2008),
        (crate::unwind::RSP, 0x2008),
        (crate::unwind::FS_BASE, 0x1ff0),
    ]);
    let needs = Some(IndirectTarget::NeedsRegisters);

    // Bytes, the target away from the stop, and the target at it.
    let cases: [(&[u8], _, _); 12] = [
        // Program-counter-relative and absolute slots hold everywhere.
        (
            &[0xff, 0x25, 0xfa, 0x0f, 0, 0], // jmp [rip+0xffa]
            memory(0x2000, 0x1234_5678_9abc_def0),
            memory(0x2000, 0x1234_5678_9abc_def0),
        ),
        (
            &[0xff, 0x14, 0x25, 0x08, 0x20, 0, 0], // call [0x2008]
            memory(0x2008, 0x1010),
            memory(0x2008, 0x1010),
        ),
        (
            &[0xff, 0x25, 0x0a, 0x10, 0, 0], // jmp [rip+0x100a]
            unreadable(0x2010, 0x2014),
            unreadable(0x2010, 0x2014),
        ),
        // Registers are known only at the stop, including the FS base and
        // a 32-bit address computation.
        (
            &[0xff, 0xd0], // call rax
            needs.clone(),
            Some(IndirectTarget::Register {
                target: at(0x1_0000_2008),
            }),
        ),
        (
            &[0xff, 0x50, 0x08], // call [rax+8]
            needs.clone(),
            unreadable(0x1_0000_2010, 0x1_0000_2010),
        ),
        (&[0x67, 0xff, 0x20], needs.clone(), memory(0x2008, 0x1010)), // jmp [eax]
        (
            &[0x64, 0xff, 0x14, 0x25, 0x10, 0, 0, 0], // call fs:[0x10]
            needs.clone(),
            memory(0x2000, 0x1234_5678_9abc_def0),
        ),
        (
            &[0xff, 0x24, 0xc5, 0x08, 0x20, 0, 0], // jmp [rax*8+0x2008]
            needs.clone(),
            unreadable(0x8_0001_2048, 0x8_0001_2048),
        ),
        (&[0xc3], needs.clone(), memory(0x2008, 0x1010)), // ret
        (&[0xc2, 0x08, 0], needs.clone(), memory(0x2008, 0x1010)), // ret 8
        // Other instructions have no indirect target.
        (&[0xe8, 0, 0, 0, 0], None, None), // call 0x1005
        (&[0x90], None, None),
    ];

    for (code, away, stopped) in cases {
        assert_eq!(indirect_target(code, None), away, "{code:x?}");
        assert_eq!(
            indirect_target(code, Some(&registers)),
            stopped,
            "{code:x?}"
        );
    }

    // Far transfers, returns from interrupts, and branches whose operand
    // size differs between Intel and AMD processors.
    for code in [
        &[0x66, 0xff, 0xe0][..], // jmp rax, or jmp ax on AMD
        &[0x66, 0xc3],           // ret, or retw on AMD
        &[0xff, 0x28],           // jmp far [rax]
        &[0x48, 0xcf],           // iretq
        &[0xcb],                 // retf
    ] {
        for registers in [None, Some(&registers)] {
            assert_eq!(
                indirect_target(code, registers),
                Some(IndirectTarget::Unsupported),
                "{code:x?}"
            );
        }
    }

    // A register the stop does not determine is never guessed.
    let mut unknown = registers;
    unknown.remove(crate::unwind::RAX);
    assert_eq!(indirect_target(&[0xff, 0xd0], Some(&unknown)), needs);
    assert_eq!(
        indirect_target(&[0xff, 0x24, 0xc5, 0x08, 0x20, 0, 0], Some(&unknown)),
        needs
    );
}
