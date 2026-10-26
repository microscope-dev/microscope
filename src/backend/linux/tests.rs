use crate::debug_info::{VariableContext, VariableRuntime};
use crate::inspection::InspectionBudget;
use crate::protocol::{
    BreakpointHit, BreakpointSpec, ResolvedBreakpointLocation, ResumeScope, SignalPolicy,
};

use crate::unwind::{FrameContext, MemoryReader, RegisterFile};
use crate::{
    AddressRange, CodeInstanceKind, ExceptionDisposition, ExecutionContext, ImageAddress,
    InlineFrameLookup, LaunchOptions, MemoryReadCompletion, MemoryReadUnavailableReason, Path,
    PresentedFrame, VariableQuery, WatchpointHit, WatchpointOptions, WatchpointSpec,
};

use std::cell::RefCell;
use tokio::sync::{broadcast, watch};

use super::activation::StackView;
use super::classify::{WatchStatus, classify_stop_evidence};
use super::frames::{default_inline_visible_count, frame_lookup_address};
use super::memory::{MemoryAccessError, read_logical_memory_with};
use super::modules::{ModuleMapping, parse_maps};
use super::native::{InspectionOps, LinuxTraceOps, interrupt_outcome, queued_in_status};

use super::*;

#[test]
fn maps_parser_preserves_distinct_loads_and_rejects_corruption() {
    let maps = concat!(
        "1000-2000 r--p 00000000 00:01 7 /opt/lib/libsame.so\n",
        "2000-3000 r-xp 00001000 00:01 7 /opt/lib/libsame.so\n",
        "5000-6000 r-xp 00001000 00:01 7 /opt/lib/libsame.so\n",
        "7000-8000 r-xp 00000000 00:01 8 /opt/bin/app (deleted)\n",
        // The kernel pads the pathname column and does not escape spaces
        // within the path itself; both must survive parsing intact.
        "9000-a000 r-xp 00000000 00:01 9     /opt/my libs/libspace.so\n",
        "8000-9000 r-xp 00000000 00:00 0 [vdso]\n",
        "b000-c000 rw-p 00000000 00:00 0\n",
    );
    let mapping = |path: &str, inode, start, file_offset, deleted, executable| ModuleMapping {
        path: PathBuf::from(path),
        inode,
        start,
        file_offset,
        deleted,
        executable,
    };

    assert_eq!(
        parse_maps(maps).expect("valid maps"),
        vec![
            mapping("/opt/bin/app", 8, 0x7000, 0, true, true),
            mapping("/opt/lib/libsame.so", 7, 0x1000, 0, false, false),
            mapping("/opt/lib/libsame.so", 7, 0x2000, 0x1000, false, true),
            mapping("/opt/lib/libsame.so", 7, 0x5000, 0x1000, false, true),
            mapping("/opt/my libs/libspace.so", 9, 0x9000, 0, false, true),
        ]
    );
    assert!(parse_maps("not-a-mapping").is_err());
}

#[test]
fn logical_memory_reads_unaligned_cross_word_ranges_and_hides_traps() {
    let address = VirtualAddress::new(0x1003);
    let mut reads = Vec::new();
    let breakpoints = [
        (0x1005, 0x55, true),
        (0x1008, 0x88, true),
        (0x1009, 0x99, false),
        (0x1010, 0x10, true),
    ]
    .into_iter()
    .map(|(address, original_byte, installed)| {
        let site = BreakpointSite {
            original_byte,
            installed,
            owners: BTreeSet::new(),
        };

        (VirtualAddress::new(address), site)
    })
    .collect::<BTreeMap<_, _>>();
    let bytes = read_logical_memory_with(address, 10, &breakpoints, |current| {
        reads.push(current);
        let mut bytes = [0_u8; 8];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(current + offset as u64 - 0x1000).expect("test byte fits u8");
        }

        for (&site_address, site) in &breakpoints {
            let Some(offset) = site_address.get().checked_sub(current) else {
                continue;
            };

            if site.installed && offset < bytes.len() as u64 {
                bytes[usize::try_from(offset).expect("test offset fits usize")] = BREAKPOINT_OPCODE;
            }
        }

        Ok(u64::from_le_bytes(bytes))
    })
    .expect("logical memory read");
    assert_eq!(bytes.bytes, [3, 4, 0x55, 6, 7, 0x88, 9, 10, 11, 12]);
    assert_eq!(bytes.completion, MemoryReadCompletion::Complete);
    assert_eq!(reads, [0x1000, 0x1008]);
    let empty = read_logical_memory_with(address, 0, &breakpoints, |_| {
        panic!("an empty logical read must not read target memory")
    })
    .expect("empty logical memory read");
    assert!(empty.bytes.is_empty());
    assert_eq!(empty.completion, MemoryReadCompletion::Complete);
}

#[test]
fn logical_memory_reads_return_the_prefix_before_inaccessible_memory() {
    let mut reads = Vec::new();
    let result = read_logical_memory_with(
        VirtualAddress::new(0x1003),
        10,
        &BTreeMap::new(),
        |current| {
            reads.push(current);
            if current == 0x1008 {
                return Err(MemoryAccessError::Inaccessible);
            }

            Ok(u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]))
        },
    )
    .expect("inaccessible memory is a typed partial result");

    assert_eq!(result.bytes, [3, 4, 5, 6, 7]);
    assert_eq!(
        result.completion,
        MemoryReadCompletion::Incomplete {
            next_address: VirtualAddress::new(0x1008),
            reason: MemoryReadUnavailableReason::Inaccessible,
        }
    );
    assert_eq!(reads, [0x1000, 0x1008]);

    // Any other failure is no inaccessible memory.
    let result = read_logical_memory_with(VirtualAddress::new(0x1000), 8, &BTreeMap::new(), |_| {
        Err(MemoryAccessError::Fatal(Error::RequestCancelled))
    });
    assert!(matches!(result, Err(Error::RequestCancelled)));
}

#[test]
fn logical_memory_reads_keep_the_readable_bytes_of_a_partial_word() {
    // Readable memory ends `readable` bytes into the second word.
    let read = |address, size, readable| {
        read_logical_memory_with(
            VirtualAddress::new(address),
            size,
            &BTreeMap::new(),
            |current| {
                let word = u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]);
                if current == 0x1008 {
                    Err(MemoryAccessError::Partial { word, readable })
                } else {
                    Ok(word)
                }
            },
        )
        .expect("partial words are typed partial results")
    };

    let incomplete = |next| MemoryReadCompletion::Incomplete {
        next_address: VirtualAddress::new(next),
        reason: MemoryReadUnavailableReason::Inaccessible,
    };

    let spanning = read(0x1006, 8, 3);
    assert_eq!(spanning.bytes, [6, 7, 0, 1, 2]);
    assert_eq!(spanning.completion, incomplete(0x100b));

    // A read that ends within the readable bytes is complete.
    let within = read(0x1009, 2, 3);
    assert_eq!(within.bytes, [1, 2]);
    assert_eq!(within.completion, MemoryReadCompletion::Complete);

    // A read that begins after them returns nothing.
    let after = read(0x100c, 2, 3);
    assert!(after.bytes.is_empty());
    assert_eq!(after.completion, incomplete(0x100c));

    // A count covering the whole word keeps its final byte, and an
    // oversized count is clamped to the word.
    for readable in [8, 9] {
        let whole = read(0x1006, 10, readable);
        assert_eq!(whole.bytes, [6, 7, 0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(whole.completion, MemoryReadCompletion::Complete);
    }
}

struct UnusedUnwindInfo;

impl UnwindInfo for UnusedUnwindInfo {
    fn cfa(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        panic!("unexpected cfa lookup")
    }

    fn unwind(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        panic!("unexpected unwind")
    }
}

struct UnusedVariableInfo;

impl VariableInfo for UnusedVariableInfo {
    fn inspect(
        &self,
        _address: ImageAddress,
        _selected: Option<crate::CodeInstanceId>,
        _query: &VariableQuery,
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<Vec<crate::Variable>> {
        panic!("unexpected variable lookup")
    }

    fn visible_object(
        &self,
        _address: ImageAddress,
        _selected: Option<crate::CodeInstanceId>,
        _name: &str,
    ) -> Result<crate::debug_info::ObjectKey> {
        panic!("unexpected variable lookup")
    }

    fn global_object(&self, _id: crate::GlobalVariableId) -> Result<crate::debug_info::ObjectKey> {
        panic!("unexpected global variable lookup")
    }

    fn object_type(
        &self,
        _object: crate::debug_info::ObjectKey,
    ) -> std::result::Result<crate::TypeId, Arc<str>> {
        panic!("unexpected object type lookup")
    }

    fn plan_step(
        &self,
        _from: crate::TypeId,
        _step: crate::debug_info::Step<'_>,
    ) -> Result<crate::debug_info::PlannedStep> {
        panic!("unexpected step planning")
    }

    fn locate(
        &self,
        _object: crate::debug_info::ObjectKey,
        _address: Option<ImageAddress>,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::debug_info::Accessed> {
        panic!("unexpected location")
    }

    fn apply(
        &self,
        _from: &crate::debug_info::Located,
        _step: &crate::debug_info::PlannedStep,
        _indices: &[i128],
        _address: Option<ImageAddress>,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::debug_info::Accessed> {
        panic!("unexpected step")
    }

    fn load(
        &self,
        _at: &crate::debug_info::Located,
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<crate::VariableValue, crate::VariableState>> {
        panic!("unexpected load")
    }

    fn materialize(
        &self,
        _at: &crate::debug_info::Located,
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::InspectedValue> {
        panic!("unexpected materialization")
    }

    fn object_storage(
        &self,
        _object: crate::debug_info::ObjectKey,
    ) -> crate::debug_info::ObjectStorage {
        panic!("unexpected storage lookup")
    }

    fn inspect_global(
        &self,
        _id: crate::GlobalVariableId,
        _address: Option<ImageAddress>,
        _context: VariableContext,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::Variable> {
        panic!("unexpected global variable lookup")
    }

    fn dereference(
        &self,
        _reference: &crate::DereferenceReference,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::DereferencedValue> {
        panic!("unexpected dereference")
    }

    fn value_children(
        &self,
        _reference: &crate::ValueChildrenReference,
        _offset: u64,
        _limit: u32,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> Result<crate::ValueChildPage> {
        panic!("unexpected value child lookup")
    }

    fn call_site(
        &self,
        _return_address: ImageAddress,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> std::result::Result<
        Option<crate::debug_info::CallSite>,
        crate::debug_info::VariableRuntimeError,
    > {
        panic!("unexpected call site lookup")
    }

    fn tail_calls(
        &self,
        _from: ImageAddress,
        _to: ImageAddress,
    ) -> std::result::Result<
        crate::debug_info::TailCallChain,
        crate::debug_info::VariableRuntimeError,
    > {
        panic!("unexpected tail call lookup")
    }

    fn call_site_value(
        &self,
        _site: crate::debug_info::CallSiteId,
        _parameter: crate::debug_info::EntryParameter,
        _runtime: &mut dyn VariableRuntime,
        _budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, crate::debug_info::VariableRuntimeError> {
        panic!("unexpected call site value")
    }
}

/// An ELF header without sections, so loader rendezvous discovery finds no
/// dynamic section to read through the fake.
fn sectionless_elf() -> Arc<[u8]> {
    let mut header = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
    header.resize(16, 0);

    // e_type through e_shstrndx: a little-endian x86-64 shared object with
    // no program or section headers.
    for (value, width) in [
        (3_u64, 2),
        (62, 2),
        (1, 4),
        (0, 8),
        (0, 8),
        (0, 8),
        (0, 4),
        (64, 2),
        (56, 2),
        (0, 2),
        (64, 2),
        (0, 2),
        (0, 2),
    ] {
        header.extend_from_slice(&value.to_le_bytes()[..width]);
    }

    header.into()
}

#[test]
fn pause_during_launch_completes_at_the_initial_exec_stop() {
    let mut harness = watch_harness(1);
    harness.controller.inferior = None;
    let (launch_reply, launch_result) = tokio::sync::oneshot::channel();
    harness
        .controller
        .start(Start::Launch(LaunchOptions::default(), launch_reply));
    let pid = harness.controller.inferior.as_ref().expect("launched").tgid;

    assert_eq!(
        harness
            .controller
            .begin_pause(process_id(pid))
            .expect("a launching inferior accepts a pause"),
        ExecutionId::new(1)
    );
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
        .expect("process initial stop");

    assert_eq!(
        launch_result
            .blocking_recv()
            .expect("launch reply")
            .expect("launch success"),
        ExecutionId::new(1)
    );
    let published = harness.published();
    assert!(
        matches!(
            published.as_slice(),
            [
                DebuggerEvent::InferiorLaunched { .. },
                DebuggerEvent::InferiorStopped {
                    execution_id: Some(execution),
                    reason: StopReason::Pause,
                    ..
                },
            ] if *execution == ExecutionId::new(1)
        ),
        "the paused launch must stop without first resuming: {published:?}"
    );
    assert_eq!(
        harness.trace().take_actions(),
        ["spawn".to_owned(), format!("set_options {pid}")],
        "the initial thread must stop without a native stop request or resume"
    );
}

#[test]
fn virtual_inline_step_emits_a_new_stop_without_native_operations() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];

    let (execution, stopped) = harness
        .controller
        .try_virtual_step(process_id(pid), StopId::new(1), pid, StepKind::IntoSource)
        .expect("virtual step")
        .expect("hidden child exists");

    assert_eq!(execution, ExecutionId::new(2));
    assert!(matches!(
        stopped,
        DebuggerEvent::InferiorStopped {
            execution_id: Some(execution_id),
            ..
        } if execution_id == ExecutionId::new(2)
    ));
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorContinued { .. }))
    );

    let stop = harness
        .controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("new public stop");

    // Stop identifiers are process-wide, so only their freshness is known.
    assert!(stop.id.get() > 1, "{:?}", stop.id);
    assert_eq!(
        stop.presentations.get(&pid),
        Some(&FramePresentation {
            instruction: VirtualAddress::new(0x10),
            frame: PresentedFrame::Inline(CodeInstanceId::new(1)),
            hidden_inline_frames: 1,
        })
    );
    assert!(matches!(
        harness.controller.try_virtual_step(
            process_id(pid),
            StopId::new(1),
            pid,
            StepKind::IntoSource
        ),
        Err(Error::StaleStop)
    ));
    assert!(harness.trace().take_actions().is_empty());
}

#[test]
fn outstanding_debugger_stop_is_reused_across_barriers() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let thread = harness.thread(pid);
    thread.state = NativeThreadState::Running;
    thread.debugger_stop_pending = true;

    harness
        .controller
        .request_stops()
        .expect("reuse outstanding debugger stop");

    assert!(
        harness.trace().take_actions().is_empty(),
        "an outstanding SIGSTOP must not be duplicated"
    );
    let thread = harness.thread(pid);
    assert_eq!(thread.state, NativeThreadState::StopRequested);
    assert!(thread.debugger_stop_pending);
}

#[test]
fn an_exec_stop_does_not_turn_an_outstanding_debugger_stop_into_a_signal() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.start_continue();
    harness.controller.request_stops().expect("request a stop");
    *harness.trace().clone.borrow_mut() = Some((pid, pid));

    harness
        .event(pid, libc::PTRACE_EVENT_EXEC)
        .expect("exec overtook the requested SIGSTOP");
    harness.published();
    harness.trace().set_siginfo(
        pid,
        libc::SI_TKILL,
        Some(i32::try_from(std::process::id()).expect("tracer pid fits")),
    );
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGSTOP))
        .expect("consume the outstanding SIGSTOP");

    assert_eq!(harness.stopped(), None, "the debugger's signal is internal");
}

#[test]
fn user_breakpoint_supersedes_a_coincident_exception_barrier() {
    let mut harness = watch_harness(3);
    let [pid, breakpoint_thread, pending_thread] = harness.threads[..] else {
        unreachable!("three threads");
    };

    let exception = exception_info(Signal::SIGURG);
    harness.thread(pid).reason = Some(StopReason::Exception(exception.clone()));
    harness.thread(breakpoint_thread).state = NativeThreadState::Running;
    let pending = harness.thread(pending_thread);
    pending.state = NativeThreadState::StopRequested;
    pending.debugger_stop_pending = true;
    let inferior = harness.inferior();
    inferior.public_stop = None;
    inferior.barrier = Some(StopBarrier::visible(pid, StopReason::Exception(exception)));
    let breakpoint = StopReason::Breakpoint {
        address: Some(VirtualAddress::new(0x20)),
        hits: Arc::from([BreakpointHit {
            breakpoint: BreakpointId::new(1),
            hit_count: 1,
        }]),
    };

    harness
        .controller
        .begin_visible_stop(breakpoint_thread, breakpoint.clone())
        .expect("record coincident breakpoint");

    let barrier = harness
        .inferior()
        .barrier
        .as_ref()
        .expect("pending thread keeps barrier active");
    assert_eq!(barrier.triggering_thread, breakpoint_thread);
    assert_eq!(barrier.reason, Some(breakpoint));
    assert!(harness.trace().take_actions().is_empty());
}

#[test]
fn inline_range_start_presentation_obeys_the_step_kind() {
    let image = virtual_step_image();
    let InlineFrameLookup::Unique(chain) = image.locate(ImageAddress::new(0x10)).inline_frames
    else {
        panic!("test address has one inline chain");
    };

    assert_eq!(
        default_inline_visible_count(
            &image,
            chain.instances.as_ref(),
            ImageAddress::new(0x10),
            true,
        ),
        1,
        "the middle frame starts here while its nested child remains hidden"
    );
    assert_eq!(
        default_inline_visible_count(
            &image,
            chain.instances.as_ref(),
            ImageAddress::new(0x10),
            false,
        ),
        0,
        "next and finish retain the parent presentation"
    );
    assert_eq!(
        default_inline_visible_count(
            &image,
            chain.instances.as_ref(),
            ImageAddress::new(0x11),
            true,
        ),
        2,
        "away from a range boundary the innermost active frame is visible"
    );
}

fn virtual_step_image() -> Arc<ModuleImage> {
    let range = (0x10, 0x20);
    inline_test_image(&[
        (0, None, None, (0, 0x100)),
        (1, Some(0), Some(10), range),
        (2, Some(1), Some(20), range),
    ])
}

/// A code instance of a test image: its index, its parent's, its call
/// site's line if it is inlined, and its address range.
type TestInstance = (u32, Option<u32>, Option<u32>, (u64, u64));

/// An image whose functions `physical`, `middle`, and `leaf` have one code
/// instance each.
fn inline_test_image(instances: &[TestInstance]) -> Arc<ModuleImage> {
    lined_test_image(instances, crate::image::lines::LineTables::default())
}

/// An image as [`inline_test_image`] makes, whose code `lines` describe.
fn lined_test_image(
    instances: &[TestInstance],
    lines: crate::image::lines::LineTables,
) -> Arc<ModuleImage> {
    let source = |line: u32| SourceLocation {
        file: crate::SourceFileId::new(0),
        line: crate::LineNumber::new(u64::from(line)).expect("nonzero line"),
        column: None,
    };

    Arc::new(ModuleImage::new(
        PathBuf::from("/test/inline"),
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        AddressRange {
            start: ImageAddress::new(0),
            end: ImageAddress::new(0x100),
        },
        crate::model::ModuleMetadata {
            functions: ["physical", "middle", "leaf"]
                .into_iter()
                .enumerate()
                .map(|(id, name)| crate::FunctionInfo {
                    id: crate::FunctionId::new(u32::try_from(id).expect("small count")),
                    name: name.into(),
                    linkage_name: None,
                    declaration: None,
                    language: crate::SourceLanguage::C,
                    role: crate::CodeRole::Ordinary,
                    enclosing: None,
                    coroutine: None,
                    generics: std::sync::Arc::from([]),
                    main_subprogram: false,
                })
                .collect(),
            code_instances: instances
                .iter()
                .map(
                    |&(id, parent, call_line, (start, end))| crate::CodeInstanceInfo {
                        id: CodeInstanceId::new(id),
                        function: crate::FunctionId::new(id),
                        parent: parent.map(CodeInstanceId::new),
                        kind: call_line.map_or(CodeInstanceKind::OutOfLine, |line| {
                            CodeInstanceKind::Inline {
                                call_site: Some(source(line)),
                            }
                        }),
                        ranges: Arc::from([AddressRange {
                            start: ImageAddress::new(start),
                            end: ImageAddress::new(end),
                        }]),
                        breakpoint_entry: None,
                    },
                )
                .collect(),
            files: {
                // The file the call sites name.
                let mut files = crate::image::lines::Files::default();
                files.intern(PathBuf::from("/test/inline.c"));
                files
            },
            lines,
            ..crate::model::ModuleMetadata::default()
        },
    ))
}

#[test]
fn frame_symbolization_adjusts_only_ordinary_caller_resume_addresses() {
    let stopped = FrameContext {
        instruction: VirtualAddress::new(0x1000),
        cfa: None,
        signal_frame: false,
    };

    let caller = FrameContext {
        instruction: VirtualAddress::new(0x2000),
        cfa: Some(VirtualAddress::new(0x3000)),
        signal_frame: false,
    };

    let signal = FrameContext {
        instruction: VirtualAddress::new(0x4000),
        cfa: Some(VirtualAddress::new(0x5000)),
        signal_frame: true,
    };

    assert_eq!(frame_lookup_address(0, &stopped), Some(stopped.instruction));
    assert_eq!(
        frame_lookup_address(1, &caller),
        Some(VirtualAddress::new(0x1fff))
    );
    assert_eq!(frame_lookup_address(2, &signal), Some(signal.instruction));
}

#[test]
fn queued_signals_are_read_from_the_threads_unblocked_pending_set() {
    let status = |pending: &str, blocked: &str| {
        format!(
            "Name:\tworker\nShdPnd:\t0000000000020010\nSigPnd:\t{pending}\nSigBlk:\t{blocked}\n"
        )
        .into_bytes()
    };

    let trap = |status: &[u8]| queued_in_status(status, Signal::SIGTRAP).expect("valid masks");
    assert!(trap(&status("0000000000000010", "0000000000000000")));
    assert!(trap(&status("0000000000010110", "0000000000000100")));
    assert!(
        !trap(&status("0000000000000010", "0000000000000010")),
        "a blocked trap cannot be dequeued by resuming"
    );
    assert!(
        !trap(&status("0000000000000000", "0000000000000000")),
        "a process-wide trap is not the thread's own"
    );
    assert_eq!(
        queued_in_status(b"SigPnd:\tnot-hex\n", Signal::SIGTRAP),
        None
    );
    assert_eq!(queued_in_status(b"", Signal::SIGTRAP), None);
}

#[test]
fn pending_signals_with_unreadable_blocking_masks_are_not_classified() {
    let status = b"SigPnd: 0000000000000010\nSigBlk: invalid\n";
    assert_eq!(
        queued_in_status(status, Signal::SIGTRAP),
        None,
        "a malformed blocking mask cannot prove the trap is unblocked"
    );
}

#[test]
fn stop_classifier_preserves_signal_and_trap_provenance() {
    let metadata = |code| {
        Ok(SignalMetadata {
            code,
            sender: (code <= 0).then_some(71),
            fault_address: None,
        })
    };

    let classify_trap = |signal, siginfo, expected: &ExpectedStop, breakpoint, program_trap| {
        classify_stop_evidence(
            signal,
            "raw-status".to_owned(),
            siginfo,
            expected,
            false,
            false,
            breakpoint,
            false,
            program_trap,
            WatchStatus::Absent,
        )
    };

    let classify = |signal, siginfo, expected: &ExpectedStop, breakpoint| {
        classify_trap(signal, siginfo, expected, breakpoint, None)
    };

    let stepping = ExpectedStop::UserStep {
        kind: StepKind::Instruction,
    };

    let none = ExpectedStop::None;
    let traced = |stop| matches!(stop, ClassifiedStop::Trace { ref watch } if watch.is_empty());

    assert!(traced(classify(
        Signal::SIGTRAP,
        metadata(libc::TRAP_TRACE),
        &stepping,
        None
    )));

    // A step across a system call completes from the call's exit path.
    assert!(traced(classify(
        Signal::SIGTRAP,
        metadata(libc::TRAP_BRKPT),
        &stepping,
        None
    )));
    assert!(matches!(
        classify(Signal::SIGTRAP, metadata(libc::SI_TKILL), &none, None),
        ClassifiedStop::SignalDelivery(PendingSignal {
            signal: Signal::SIGTRAP,
            sender: Some(71),
            ..
        })
    ));
    assert!(matches!(
        classify(Signal::SIGSEGV, metadata(libc::SI_KERNEL), &none, None),
        ClassifiedStop::SignalDelivery(PendingSignal {
            signal: Signal::SIGSEGV,
            code: libc::SI_KERNEL,
            ..
        })
    ));
    assert!(matches!(
        classify(Signal::SIGSTOP, Err(Errno::EINVAL), &none, None),
        ClassifiedStop::GroupStop(Signal::SIGSTOP)
    ));
    assert!(matches!(
        classify(Signal::SIGTRAP, Err(Errno::EIO), &none, None),
        ClassifiedStop::Unclassifiable(RawStopRecord {
            siginfo: Err(Errno::EIO),
            ..
        })
    ));
    assert!(matches!(
        classify(
            Signal::SIGTRAP,
            metadata(libc::SI_KERNEL),
            &none,
            Some(VirtualAddress::new(0x1234)),
        ),
        ClassifiedStop::Breakpoint(address) if address == VirtualAddress::new(0x1234)
    ));

    // An int3 of the program's own is reported as a trap, never delivered
    // as a signal; an int3 with no evidence for either cannot be explained.
    assert!(matches!(
        classify_trap(
            Signal::SIGTRAP,
            metadata(libc::SI_KERNEL),
            &none,
            None,
            Some(VirtualAddress::new(0x1233)),
        ),
        ClassifiedStop::ProgramTrap(address) if address == VirtualAddress::new(0x1233)
    ));
    assert!(matches!(
        classify(Signal::SIGTRAP, metadata(libc::SI_KERNEL), &none, None),
        ClassifiedStop::Unclassifiable(_)
    ));

    // SIGKILL wakes a thread from a reported stop: its siginfo vanishes and
    // then describes its exit event.
    let repairing = ExpectedStop::BreakpointRepair {
        address: VirtualAddress::new(0x1234),
    };

    for siginfo in [Err(Errno::ESRCH), metadata(EXIT_EVENT)] {
        assert!(matches!(
            classify(Signal::SIGTRAP, siginfo, &repairing, None),
            ClassifiedStop::Superseded
        ));
    }
}

fn step_execution(thread: Pid, kind: StepKind, start: StepStart) -> ActiveKind {
    ActiveKind::Step {
        owner: StepOwner { thread, task: None },
        kind,
        requested: kind,
        start: Box::new(start),
        progress_owed: false,
    }
}

/// The siginfo code of a thread stopped at its exit event.
const EXIT_EVENT: i32 = libc::SIGTRAP | (libc::PTRACE_EVENT_EXIT << 8);

/// A traced process for the controller: it models per-thread debug
/// registers as Linux exposes them, records every native effect, and
/// injects failures and SIGKILL races.
#[derive(Default)]
pub(super) struct FakeTrace {
    actions: RefCell<Vec<String>>,
    pub(super) memory_write_failure: RefCell<Option<u32>>,
    registers: RefCell<BTreeMap<Pid, [u64; 8]>>,

    /// Fails a write of `(thread, register)` after skipping that many
    /// successful writes.
    failures: RefCell<BTreeMap<(Pid, usize), (u32, Errno)>>,

    /// Debug-register writes that always fail, as for slots another user
    /// holds.
    refusals: RefCell<BTreeMap<(Pid, usize), Errno>>,

    /// Accepts writes without storing them, like gVisor.
    discard_writes: bool,
    siginfo: RefCell<BTreeMap<Pid, SignalMetadata>>,
    program_counters: RefCell<BTreeMap<Pid, u64>>,
    target_ids: BTreeMap<Pid, Option<crate::runtime_model::TargetThreadId>>,

    /// Threads holding a SIGTRAP queued behind an interrupt stop.
    queued_traps: RefCell<BTreeSet<Pid>>,
    queue_error: Option<Pid>,

    /// Threads past their exit, which no interrupt reaches, whose exit
    /// status is due.
    dead: RefCell<BTreeSet<Pid>>,

    /// Threads a sibling's `exit_group` killed out of their ptrace-stop.
    vanished: RefCell<BTreeSet<Pid>>,

    /// A thread SIGKILL takes out of its stop, leaving it at its exit event,
    /// when the controller makes the named request of it for the given
    /// time, counting from zero.
    kill_point: RefCell<Option<(Pid, &'static str, u32)>>,

    /// A thread SIGKILL takes out of its stop as the controller reads the
    /// given address of it, such as a stack slot unwinding needs. That read
    /// fails while the thread runs to its exit event; once there, it answers
    /// requests again, as Linux's threads do at that stop.
    kill_at_read: RefCell<Option<(Pid, u64)>>,

    /// Threads SIGKILL took out of their stop, which refuse every request.
    killed: RefCell<BTreeSet<Pid>>,

    /// The child reported by the next clone event, in the process `tgid`.
    clone: RefCell<Option<(Pid, Pid)>>,

    /// The process's thread list.
    listed_threads: RefCell<Vec<Pid>>,

    /// The thread list after the next listing.
    next_listing: RefCell<Option<Vec<Pid>>>,

    /// Threads that cannot be seized, as an exited leader cannot.
    unseizable: RefCell<BTreeSet<Pid>>,

    /// The fork children the tracer still traces.
    traced_children: RefCell<Vec<Pid>>,

    /// Memory words by address; others read as zero.
    memory: RefCell<BTreeMap<u64, u64>>,

    /// Threads' stack pointers; others' are 0.
    pub(super) stack_pointers: RefCell<BTreeMap<Pid, u64>>,

    /// Words whose reads fail, as unmapped memory's do.
    unreadable: RefCell<BTreeSet<u64>>,

    /// Fails every memory read operationally, unlike unmapped memory.
    read_failure: RefCell<Option<Errno>>,

    /// Processes' start times; others' are unreadable.
    start_times: RefCell<BTreeMap<Pid, u64>>,

    /// Mapped library files, by path, with their load bias and whether
    /// their debug information loads.
    libraries: RefCell<BTreeMap<PathBuf, (crate::LoadBias, bool)>>,

    /// The symbols each library's image names; others name none.
    library_symbols: RefCell<BTreeMap<PathBuf, Vec<crate::SymbolInfo>>>,

    /// Statuses the kernel holds that no waiter has taken.
    unreaped: RefCell<BTreeMap<Pid, WaitEvent>>,
}

impl FakeTrace {
    fn record(&self, action: String) {
        self.actions.borrow_mut().push(action);
    }

    pub(super) fn set_pc(&self, pid: Pid, pc: u64) {
        self.program_counters.borrow_mut().insert(pid, pc);
    }

    /// Takes the actions, and returns whether any begins with `prefix`.
    pub(super) fn took(&self, prefix: &str) -> bool {
        self.take_actions()
            .iter()
            .any(|action| action.starts_with(prefix))
    }

    /// Makes `pid`'s next stop report this siginfo.
    fn set_siginfo(&self, pid: Pid, code: i32, sender: Option<i32>) {
        self.siginfo.borrow_mut().insert(
            pid,
            SignalMetadata {
                code,
                sender,
                fault_address: None,
            },
        );
    }

    /// Takes a thread out of its stop as SIGKILL does, to its exit event.
    fn sigkill(&self, pid: Pid) {
        self.killed.borrow_mut().insert(pid);
        self.vanished.borrow_mut().insert(pid);
        self.siginfo.borrow_mut().insert(
            pid,
            SignalMetadata {
                code: EXIT_EVENT,
                sender: None,
                fault_address: None,
            },
        );
    }

    /// Writes a site's first byte, as installing or removing its trap does.
    fn put_byte(&self, address: VirtualAddress, byte: u8) {
        let mut memory = self.memory.borrow_mut();
        let word = memory.entry(address.get()).or_default();
        *word = (*word & !0xff) | u64::from(byte);
    }

    /// Fails a request of a thread SIGKILL took out of its stop, which this
    /// request may be the one to do.
    fn reach(&self, pid: Pid, request: &'static str) -> Result<()> {
        let mut point = self.kill_point.borrow_mut();
        if let Some((target, name, remaining)) = point.as_mut()
            && *target == pid
            && *name == request
        {
            if *remaining == 0 {
                *point = None;
                self.sigkill(pid);
            } else {
                *remaining -= 1;
            }
        }

        if self.killed.borrow().contains(&pid) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }

        Ok(())
    }

    pub(super) fn take_actions(&self) -> Vec<String> {
        std::mem::take(&mut *self.actions.borrow_mut())
    }

    fn registers_of(&self, pid: Pid) -> [u64; 8] {
        self.registers.borrow().get(&pid).copied().unwrap_or([
            0,
            0,
            0,
            0,
            0,
            0,
            debug_registers::STATUS_IDLE,
            0,
        ])
    }

    /// Fails every write of `register` in `pid` with `error`.
    fn refuse(&self, pid: Pid, register: usize, error: Errno) {
        self.refusals.borrow_mut().insert((pid, register), error);
    }

    fn fail_after(&self, pid: Pid, register: usize, skips: u32, error: Errno) {
        self.failures
            .borrow_mut()
            .insert((pid, register), (skips, error));
    }
}

impl InspectionOps for FakeTrace {
    fn target_thread_id(
        &self,
        _process: Pid,
        thread: Pid,
    ) -> Option<crate::runtime_model::TargetThreadId> {
        // This fixture's default kernel has one PID namespace; tests can
        // supply a nested or unavailable mapping explicitly.
        self.target_ids.get(&thread).copied().unwrap_or_else(|| {
            crate::runtime_model::TargetThreadId::new(u64::try_from(thread.as_raw()).ok()?)
        })
    }

    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        if *self.kill_at_read.borrow() == Some((pid, address)) {
            self.kill_at_read.borrow_mut().take();
            self.siginfo.borrow_mut().insert(
                pid,
                SignalMetadata {
                    code: EXIT_EVENT,
                    sender: None,
                    fault_address: None,
                },
            );
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }

        self.reach(pid, "read_word")?;

        if self.unreadable.borrow().contains(&address) {
            return Err(backend_error(LinuxError::System(Errno::EIO)));
        }

        Ok(self.memory.borrow().get(&address).copied().unwrap_or(0))
    }

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        let failure = if self.reach(pid, "read_memory_word").is_err()
            || self.vanished.borrow().contains(&pid)
        {
            Some(Errno::ESRCH)
        } else {
            *self.read_failure.borrow()
        };

        if let Some(errno) = failure {
            return Err(MemoryAccessError::Fatal(backend_error(LinuxError::System(
                errno,
            ))));
        }

        self.read_word(pid, address)
            .map_err(|_| MemoryAccessError::Inaccessible)
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        self.reach(pid, "registers")?;
        let mut registers =
            super::sim_edge::user_registers(&crate::sim::cpu::Registers::default(), 0);
        registers.rip = self
            .program_counters
            .borrow()
            .get(&pid)
            .copied()
            .unwrap_or(0x5000);
        registers.rsp = self.stack_pointers.borrow().get(&pid).copied().unwrap_or(0);
        Ok(registers)
    }
}

impl LinuxTraceOps for FakeTrace {
    fn spawn(&self, _executable: &Path, _options: LaunchOptions) -> Result<Pid> {
        self.record("spawn".to_owned());
        Ok(Pid::from_raw(7000))
    }

    fn spawn_waiter(
        &self,
        _process: Pid,
        _messages: mpsc::Sender<ControllerMessage>,
    ) -> Result<Waiter> {
        Ok(Waiter::external())
    }

    fn traced_children(&self, _process: Pid, _thread: Pid) -> Vec<Pid> {
        self.traced_children.borrow().clone()
    }

    fn process_threads(&self, _process: Pid) -> Result<Vec<Pid>> {
        let listed = self.listed_threads.borrow().clone();

        if let Some(next) = self.next_listing.take() {
            self.listed_threads.replace(next);
        }

        Ok(listed)
    }

    fn seize(&self, pid: Pid, _exit_kill: bool) -> Result<bool> {
        self.record(format!("seize {pid}"));
        Ok(!self.unseizable.borrow().contains(&pid))
    }

    fn interrupt(&self, pid: Pid) -> Result<bool> {
        self.record(format!("interrupt {pid}"));
        Ok(!self.dead.borrow().contains(&pid))
    }

    fn detach(&self, pid: Pid, signal: Option<Signal>) -> Result<bool> {
        self.record(format!("detach {pid} {signal:?}"));

        // A thread SIGKILL took out of its stop is not released: it stays
        // traced until it exits.
        Ok(self.reach(pid, "detach").is_ok())
    }

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()> {
        self.record(format!("kill {pid} {signal}"));
        Ok(())
    }

    fn reap(&self, _pid: Pid) -> Result<()> {
        panic!("unexpected reap")
    }

    fn wait_status(&self, _pid: Pid) -> std::result::Result<WaitEvent, Errno> {
        panic!("unexpected wait")
    }

    fn poll_status(&self, pid: Pid) -> Option<WaitEvent> {
        self.unreaped.borrow_mut().remove(&pid)
    }

    fn process_start_time(&self, process: Pid) -> Option<u64> {
        self.start_times.borrow().get(&process).copied()
    }

    fn tracer_process(&self) -> i32 {
        i32::try_from(std::process::id()).expect("pid fits")
    }

    fn allocate_stop_id(&self) -> Result<StopId> {
        allocate_stop_id()
    }

    fn identify_module(&self, mapping: &ModuleMapping) -> Option<(PathBuf, crate::LoadBias)> {
        let libraries = self.libraries.borrow();
        let (bias, _) = libraries.get(&mapping.path)?;
        Some((mapping.path.clone(), *bias))
    }

    fn module_mappings(&self, _pid: Pid) -> Result<super::modules::ProcessMappings> {
        Ok(super::modules::ProcessMappings {
            files: self
                .libraries
                .borrow()
                .iter()
                .enumerate()
                .map(|(index, (path, (bias, _)))| ModuleMapping {
                    path: path.clone(),
                    inode: 100 + index as u64,
                    start: bias.get(),
                    file_offset: 0,
                    deleted: false,
                    executable: true,
                })
                .collect(),
            headers: Vec::new(),
            vdso: None,
            anonymous_code: Vec::new(),
        })
    }

    fn load_modules(
        &self,
        modules: &[(PathBuf, crate::ModuleImageId)],
        _search: &crate::debug_info::DebugFileSearch,
    ) -> Vec<Result<DebugInfo>> {
        let names = modules
            .iter()
            .map(|(path, id)| format!("{}#{}", path.display(), id.get()))
            .collect::<Vec<_>>();
        self.record(format!("load_modules {}", names.join(" ")));
        modules
            .iter()
            .map(|(path, id)| {
                let loads = self
                    .libraries
                    .borrow()
                    .get(path)
                    .is_some_and(|(_, loads)| *loads);
                if !loads {
                    return Err(backend_error(LinuxError::System(Errno::ENOEXEC)));
                }

                let tables = crate::model::seal(
                    crate::TargetDescription::X86_64,
                    AddressRange {
                        start: ImageAddress::new(0),
                        end: ImageAddress::new(0x1000),
                    },
                    crate::model::ModuleMetadata {
                        symbols: self
                            .library_symbols
                            .borrow()
                            .get(path)
                            .cloned()
                            .unwrap_or_default(),
                        ..crate::model::ModuleMetadata::default()
                    },
                )
                .unwrap();
                let binding = crate::model::Binding {
                    path: Arc::new(path.clone()),
                    debug_path: None,
                    id: *id,
                };

                Ok(DebugInfo {
                    image: Arc::new(ModuleImage::bind(&binding, Arc::new(tables)).unwrap()),
                    unwind: Arc::new(UnusedUnwindInfo),
                    variables: Arc::new(UnusedVariableInfo),
                })
            })
            .collect()
    }

    /// A pending clone's process, or else the thread's own, a leader's.
    fn thread_group_id(&self, pid: Pid) -> Result<Pid> {
        Ok(self.clone.borrow().map_or(pid, |(_, tgid)| tgid))
    }

    fn load_bias(
        &self,
        _pid: Pid,
        _executable: &Path,
        _image_base: u64,
        _identity: FileIdentity,
    ) -> Result<crate::LoadBias> {
        Ok(crate::LoadBias::default())
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        self.record(format!("write_word {pid} {address:#x} {value:#x}"));

        if let Some(remaining) = self.memory_write_failure.borrow_mut().as_mut() {
            if *remaining == 0 {
                return Err(backend_error(LinuxError::System(Errno::EIO)));
            }
            *remaining -= 1;
        }
        Ok(())
    }

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.record(format!("continue {pid} {signal:?}"));

        if self.vanished.borrow().contains(&pid) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }

        Ok(())
    }

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()> {
        self.record(format!("continue {pid} to shut down"));
        Ok(())
    }

    fn step(&self, pid: Pid, _signal: Option<Signal>) -> Result<()> {
        self.record(format!("step {pid}"));
        Ok(())
    }

    fn continue_to_syscall(&self, pid: Pid, _signal: Option<Signal>) -> Result<()> {
        self.record(format!("continue_to_syscall {pid}"));
        Ok(())
    }

    fn syscall_info(
        &self,
        _pid: Pid,
    ) -> std::result::Result<super::syscalls::info::SyscallInfo, Errno> {
        Err(Errno::EIO)
    }

    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        self.record(format!("set_registers {pid} rip={:#x}", registers.rip));
        Ok(())
    }

    fn set_options(&self, pid: Pid, _exit_kill: bool) -> Result<()> {
        self.record(format!("set_options {pid}"));
        self.reach(pid, "set_options")?;

        if self.vanished.borrow().contains(&pid) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }

        Ok(())
    }

    fn event_message(&self, pid: Pid) -> Result<libc::c_long> {
        // A thread SIGKILL moved to its exit stop reports the kill there.
        if self.reach(pid, "event_message").is_err() {
            return Ok(libc::c_long::from(libc::SIGKILL));
        }

        Ok(libc::c_long::from(
            self.clone.borrow().expect("a clone is pending").0.as_raw(),
        ))
    }

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        self.siginfo
            .borrow()
            .get(&pid)
            .copied()
            .ok_or(Errno::EINVAL)
    }

    fn request_stop(&self, _process: Pid, thread: Pid) -> Result<()> {
        self.record(format!("request_stop {thread}"));

        if self.vanished.borrow().contains(&thread) {
            return Err(backend_error(LinuxError::System(Errno::ESRCH)));
        }

        Ok(())
    }

    fn queued_signal(&self, pid: Pid, signal: Signal) -> Result<bool> {
        if self.queue_error == Some(pid) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed signal masks",
            )
            .into());
        }
        Ok(signal == Signal::SIGTRAP && self.queued_traps.borrow().contains(&pid))
    }

    fn executable(&self, _pid: Pid, _address: VirtualAddress) -> Result<bool> {
        Ok(true)
    }

    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno> {
        self.record(format!("read {pid} dr{index}"));

        if self.reach(pid, "read_debug_register").is_err() {
            return Err(Errno::ESRCH);
        }

        Ok(self.registers_of(pid)[index])
    }

    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        self.record(format!("write {pid} dr{index}={value:#x}"));

        if let Some(&error) = self.refusals.borrow().get(&(pid, index)) {
            return Err(error);
        }

        let mut failures = self.failures.borrow_mut();
        if let Some((skips, error)) = failures.get_mut(&(pid, index)) {
            if *skips == 0 {
                let error = *error;
                failures.remove(&(pid, index));
                return Err(error);
            }
            *skips -= 1;
        }

        drop(failures);
        if !self.discard_writes {
            let mut registers = self.registers_of(pid);
            registers[index] = value;
            self.registers.borrow_mut().insert(pid, registers);
        }

        Ok(())
    }

    fn install_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        self.reach(pid, "install_breakpoint")?;

        if let Some(site) = sites.get_mut(&address) {
            site.owners.insert(owner);
            return Ok(());
        }

        self.record(format!("install_site {address}"));
        self.put_byte(address, BREAKPOINT_OPCODE);
        sites.insert(
            address,
            BreakpointSite {
                original_byte: 0x90,
                installed: true,
                owners: BTreeSet::from([owner]),
            },
        );
        Ok(())
    }

    fn remove_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        self.reach(pid, "remove_breakpoint")?;
        self.record(format!("remove_site {address}"));
        self.put_byte(address, 0x90);
        sites.get_mut(&address).expect("known site").installed = false;
        Ok(())
    }

    fn reinstall_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        self.reach(pid, "reinstall_breakpoint")?;
        self.record(format!("reinstall_site {address}"));
        self.put_byte(address, BREAKPOINT_OPCODE);
        sites.get_mut(&address).expect("known site").installed = true;
        Ok(())
    }
}

pub(super) struct WatchHarness {
    pub(super) controller: Controller<FakeTrace>,
    events: broadcast::Receiver<DebuggerEvent>,
    pub(super) threads: Vec<Pid>,
}

impl WatchHarness {
    pub(super) fn trace(&self) -> &FakeTrace {
        &self.controller.ptrace
    }

    fn inferior(&mut self) -> &mut Inferior {
        self.controller.inferior.as_mut().expect("inferior")
    }

    /// Makes the process one the debugger attached to.
    fn attached(&mut self) {
        self.inferior().origin = InferiorOrigin::Attached;
    }

    pub(super) fn stop_id(&self) -> StopId {
        self.controller
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.public_stop.as_ref())
            .expect("a public stop")
            .id
    }

    /// Reports `pid` stopped at the ptrace `event`.
    fn event(&mut self, pid: Pid, event: i32) -> Result<()> {
        self.controller
            .process_wait(WaitEvent::PtraceEvent(pid, Signal::SIGTRAP, event))
    }

    /// Adds a breakpoint by `spec` while every thread is stopped.
    pub(super) fn add_spec(&mut self, spec: BreakpointSpec) -> Result<Breakpoint> {
        self.edit(|reply| Edit::AddBreakpoint {
            spec,
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
    }

    /// Adds a breakpoint at `address` while every thread is stopped.
    pub(super) fn add_breakpoint(&mut self, address: u64) -> Breakpoint {
        self.edit(|reply| Edit::AddBreakpoint {
            spec: BreakpointSpec::Address(VirtualAddress::new(address)),
            options: Box::default(),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("added")
    }

    /// The thread and reason of the next stop published.
    pub(super) fn stopped(&mut self) -> Option<(DebugThreadId, StopReason)> {
        self.published().into_iter().find_map(|event| match event {
            DebuggerEvent::InferiorStopped {
                thread_id, reason, ..
            } => Some((thread_id, reason)),
            _ => None,
        })
    }

    fn add(&mut self, address: u64, byte_size: u64) -> Result<Watchpoint> {
        self.add_watching(address, byte_size, WatchAccess::Write)
    }

    fn add_watching(
        &mut self,
        address: u64,
        byte_size: u64,
        access: WatchAccess,
    ) -> Result<Watchpoint> {
        self.add_watching_with(address, byte_size, access, WatchpointOptions::default())
    }

    fn add_watching_with(
        &mut self,
        address: u64,
        byte_size: u64,
        access: WatchAccess,
        options: WatchpointOptions,
    ) -> Result<Watchpoint> {
        self.controller.add_watchpoint(
            WatchpointSpec::Location {
                address: VirtualAddress::new(address),
                byte_size,
            },
            access,
            options,
        )
    }

    /// Stores `value` in the word at `address`, as the inferior would.
    pub(super) fn store(&self, address: u64, value: u64) {
        self.trace().memory.borrow_mut().insert(address, value);
    }

    fn thread(&mut self, pid: Pid) -> &mut TraceThread {
        self.controller
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .expect("harness thread exists")
    }

    fn watch_events(&mut self) -> usize {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| matches!(event, DebuggerEvent::WatchpointsChanged { .. }))
            .count()
    }

    fn breakpoint_events(&mut self) -> usize {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| matches!(event, DebuggerEvent::BreakpointsChanged { .. }))
            .count()
    }

    /// Marks every thread running inside a process-wide continue.
    pub(super) fn start_continue(&mut self) {
        let inferior = self.controller.inferior.as_mut().expect("inferior");
        inferior.public_stop = None;
        inferior.active = Some(ActiveExecution {
            id: ExecutionId::new(2),
            kind: ActiveKind::Continue,
            scope: ResumeScope::Process(process_id(inferior.tgid)),
            resume_threads: inferior.threads.keys().copied().collect(),
        });

        for thread in inferior.threads.values_mut() {
            thread.state = NativeThreadState::Running;
            thread.reason = None;
        }
    }

    /// Delivers the SIGSTOP each still-running thread was asked for. A
    /// thread that stopped for another reason keeps its SIGSTOP pending.
    pub(super) fn settle_requested_stops(&mut self) {
        let requested = self
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .threads
            .iter()
            .filter(|(_, thread)| {
                thread.debugger_stop_pending
                    && matches!(thread.state, NativeThreadState::StopRequested)
            })
            .map(|(&pid, _)| pid)
            .collect::<Vec<_>>();

        for pid in requested {
            self.trace().siginfo.borrow_mut().insert(
                pid,
                SignalMetadata {
                    code: libc::SI_TKILL,
                    sender: Some(i32::try_from(std::process::id()).expect("pid fits")),
                    fault_address: None,
                },
            );
            self.controller
                .process_wait(WaitEvent::Stopped(pid, Signal::SIGSTOP))
                .expect("debugger stop");
        }
    }

    /// Reports a SIGTRAP with `code` while DR6 holds `status`.
    pub(super) fn trap(&mut self, pid: Pid, code: i32, status: u64) -> Result<()> {
        let mut registers = self.trace().registers_of(pid);
        registers[debug_registers::STATUS_REGISTER] = status;
        self.trace().registers.borrow_mut().insert(pid, registers);
        self.trace().siginfo.borrow_mut().insert(
            pid,
            SignalMetadata {
                code,
                sender: None,
                fault_address: None,
            },
        );
        self.controller
            .process_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
    }

    /// Reports a hardware breakpoint trap whose DR6 names `slots`.
    fn watch_trap(&mut self, pid: Pid, slots: u64) -> Result<()> {
        self.trap(
            pid,
            TRAP_HARDWARE_BREAKPOINT,
            debug_registers::STATUS_IDLE | slots,
        )
    }

    /// Reports an int3 at `address`, leaving the PC after it.
    pub(super) fn hit_at(&mut self, pid: Pid, address: u64) -> Result<()> {
        self.trace().set_pc(pid, address + 1);
        self.trap(pid, libc::SI_KERNEL, debug_registers::STATUS_IDLE)
    }

    /// Requests an edit and returns the receiver of its reply.
    pub(super) fn edit<T>(
        &mut self,
        make: impl FnOnce(Reply<T>) -> Edit,
    ) -> tokio::sync::oneshot::Receiver<Result<T>> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.controller.edit(make(reply));
        result
    }

    /// Takes the published events other than revision changes.
    pub(super) fn published(&mut self) -> Vec<DebuggerEvent> {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| !matches!(event, DebuggerEvent::StateChanged { .. }))
            .collect()
    }

    fn public_reason(&self) -> Option<StopReason> {
        self.controller
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.public_stop.as_ref())
            .map(|stop| stop.reason.clone())
    }
}

pub(super) fn watch_harness(thread_count: i32) -> WatchHarness {
    watch_harness_of(thread_count, &virtual_step_image())
}

/// A harness of `thread_count` threads stopped for a pause in a process of
/// `image`, the first presenting the stop at 0x10.
fn watch_harness_of(thread_count: i32, image: &Arc<ModuleImage>) -> WatchHarness {
    let threads = (0..thread_count)
        .map(|offset| Pid::from_raw(5000 + offset))
        .collect::<Vec<_>>();
    let (sender, receiver) = mpsc::channel(8);
    let (events, event_receiver) = broadcast::channel(256);
    let mut controller = Controller::new(
        SessionLease::detached(),
        crate::backend::ExecutableSource {
            display_path: Arc::new(PathBuf::from("/test/watch")),
            launch_path: Arc::new(PathBuf::from("/test/watch")),
            data: sectionless_elf(),
            identity: FileIdentity { inode: 0 },
            process_start_time: None,
            debug_files: crate::debug_info::DebugFileSearch::default(),
        }
        .described(),
        DebugInfo {
            image: Arc::clone(image),
            unwind: Arc::new(UnusedUnwindInfo),
            variables: Arc::new(UnusedVariableInfo),
        },
        ControllerChannels {
            sender,
            receiver,
            events: events.into(),
        },
        watch::Sender::new(Arc::clone(image)),
        FakeTrace::default(),
    );
    let pid = threads[0];
    let stopped = || TraceThread {
        state: NativeThreadState::Stopped,
        armed: Some(0),
        ..TraceThread::starting(ExpectedStop::None)
    };

    let mut inferior = Inferior {
        public_stop: Some(PublicStop::new(
            StopId::new(1),
            pid,
            StopReason::Pause,
            BTreeMap::from([(
                pid,
                FramePresentation {
                    instruction: VirtualAddress::new(0x10),
                    frame: PresentedFrame::Physical,
                    hidden_inline_frames: 2,
                },
            )]),
        )),
        next_execution: 1,
        ..Inferior::new(
            InferiorOrigin::Launched,
            pid,
            LoadedModule::main(image.id(), crate::LoadBias::default()),
            threads.iter().map(|&pid| (pid, stopped())).collect(),
            None,
        )
    };

    inferior.thread_mut(pid).expect("first thread").reason = Some(StopReason::Pause);
    controller.inferior = Some(inferior);
    WatchHarness {
        controller,
        events: event_receiver,
        threads,
    }
}

#[test]
fn arming_programs_each_thread_disabled_first_and_verifies_it() {
    let mut harness = watch_harness(3);
    let watchpoint = harness.add(0x6006, 4).expect("arm watchpoint");
    assert_eq!(
        watchpoint.coverage.as_ref(),
        [
            AddressRange {
                start: VirtualAddress::new(0x6006),
                end: VirtualAddress::new(0x6008),
            },
            AddressRange {
                start: VirtualAddress::new(0x6008),
                end: VirtualAddress::new(0x600a),
            },
        ]
    );
    let control = 0x0055_0005;
    let expected = harness
        .threads
        .iter()
        .flat_map(|pid| {
            [
                format!("write {pid} dr7=0x0"),
                format!("write {pid} dr0=0x6006"),
                format!("write {pid} dr1=0x6008"),
                format!("write {pid} dr7={control:#x}"),
                format!("read {pid} dr7"),
                format!("read {pid} dr0"),
                format!("read {pid} dr1"),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(harness.trace().take_actions(), expected);
    assert_eq!(harness.watch_events(), 1);

    for pid in harness.threads.clone() {
        assert_eq!(harness.thread(pid).armed, Some(1));
        assert_eq!(harness.trace().registers_of(pid)[7], control);
    }
}

#[test]
fn a_failure_on_any_thread_restores_every_thread_and_publishes_nothing() {
    for failing_thread in 0..3 {
        let mut harness = watch_harness(3);
        let first = harness.add(0x7000, 8).expect("arm first watchpoint");
        let armed = harness
            .threads
            .iter()
            .map(|&pid| harness.trace().registers_of(pid))
            .collect::<Vec<_>>();
        harness.watch_events();
        let failing = harness.threads[failing_thread];
        harness.trace().fail_after(failing, 1, 0, Errno::ENOSPC);

        let result = harness.add(0x8000, 8);
        assert!(
            matches!(
                result,
                Err(Error::WatchpointHardwareBusy { thread }) if thread == debug_thread_id(failing)
            ),
            "{result:?}"
        );

        for (index, &pid) in harness.threads.clone().iter().enumerate() {
            assert_eq!(
                harness.trace().registers_of(pid)[7],
                armed[index][7],
                "thread {index} keeps only the first watchpoint"
            );
            assert_eq!(harness.thread(pid).armed, Some(1));
        }

        assert_eq!(harness.watch_events(), 0);
        let inferior = harness.controller.inferior.as_ref().expect("inferior");
        assert_eq!(
            inferior
                .watch
                .watchpoints
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [first.id]
        );
        assert_eq!(inferior.watch.generation, 1);
    }
}

#[test]
fn a_failed_rollback_kills_the_inferior_and_reports_both_failures() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    // The second thread refuses its address; restoring the first thread
    // then fails on its third control write, the rollback's disable.
    harness.trace().fail_after(second, 0, 0, Errno::ENOSPC);
    harness.trace().fail_after(first, 7, 2, Errno::EPERM);

    let result = harness.add(0x9000, 8);
    assert!(
        matches!(
            &result,
            Err(Error::Backend(error)) if matches!(
                error.downcast_ref::<LinuxError>(),
                Some(LinuxError::WatchpointArmRecovery { .. })
            )
        ),
        "{result:?}"
    );
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("kill {first} SIGKILL")),
        "an inferior with unknown debug registers is not left running"
    );
    assert_eq!(harness.watch_events(), 0);
}

/// A thread that does not carry the current watchpoints, its slots taken
/// by another user, cannot run until it is armed before a resume, so
/// editing watchpoints leaves it to that: removing one succeeds for every
/// other thread, and nothing is killed.
#[test]
fn editing_watchpoints_leaves_a_thread_that_lost_its_slots_to_the_resume() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let kept = harness.add(0x9000, 8).expect("arm");
    let removed = harness.add(0xa000, 8).expect("arm");
    let inferior = harness.inferior();
    inferior.thread_mut(second).expect("thread").armed = None;
    harness.trace().refuse(second, 0, Errno::ENOSPC);
    harness.trace().take_actions();

    harness
        .controller
        .remove_watchpoint(removed.id)
        .expect("remove");
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("kill") || action.contains(&format!(" {second} "))),
        "{actions:?}"
    );
    assert!(
        actions.contains(&format!("write {first} dr7=0x90001")),
        "{actions:?}"
    );
    let _ = kept;
}

#[test]
fn readback_mismatches_report_unavailable_hardware_and_arm_nothing() {
    let mut harness = watch_harness(2);
    harness.controller.ptrace.discard_writes = true;
    let result = harness.add(0xa000, 8);
    assert!(
        matches!(result, Err(Error::HardwareWatchpointsUnavailable(_))),
        "{result:?}"
    );
    assert_eq!(harness.watch_events(), 0);
    assert!(
        harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .watch
            .watchpoints
            .is_empty()
    );
}

#[test]
fn an_exiting_thread_is_skipped_and_armed_when_execution_resumes() {
    let mut harness = watch_harness(3);
    let exiting = harness.threads[1];
    harness.trace().fail_after(exiting, 7, 0, Errno::ESRCH);
    harness
        .add(0xb000, 8)
        .expect("arm despite an exiting thread");
    assert_eq!(harness.thread(exiting).armed, Some(0));
    assert_eq!(harness.thread(harness.threads[0]).armed, Some(1));
    assert_eq!(harness.thread(harness.threads[2]).armed, Some(1));

    // A thread that is still present is programmed before anything runs.
    harness.trace().take_actions();
    harness
        .controller
        .sync_debug_registers()
        .expect("sync stale thread");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("write {exiting} dr7=0x0"),
            format!("write {exiting} dr0=0xb000"),
            format!("write {exiting} dr7=0x90001"),
            format!("read {exiting} dr7"),
            format!("read {exiting} dr0"),
        ]
    );
    assert_eq!(harness.thread(exiting).armed, Some(1));
}

#[test]
fn new_threads_are_armed_before_they_first_run() {
    let mut harness = watch_harness(1);
    harness.add(0xc000, 8).expect("arm");
    harness.start_continue();
    let child = Pid::from_raw(6000);
    {
        let inferior = harness.inferior();
        inferior
            .threads
            .insert(child, TraceThread::starting(ExpectedStop::None));
        inferior
            .active
            .as_mut()
            .expect("continue")
            .resume_threads
            .insert(child);
    }

    harness.trace().take_actions();
    harness
        .controller
        .process_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
        .expect("thread start");
    let actions = harness.trace().take_actions();
    let armed = actions
        .iter()
        .position(|action| action == &format!("write {child} dr7=0x90001"))
        .expect("the child was armed");
    let resumed = actions
        .iter()
        .position(|action| action == &format!("continue {child} None"))
        .expect("the child was resumed");
    assert!(armed < resumed, "{actions:?}");
    assert_eq!(harness.thread(child).armed, Some(1));
}

#[test]
fn a_new_thread_that_cannot_be_armed_never_runs() {
    let mut harness = watch_harness(1);
    harness.add(0xc000, 8).expect("arm");
    harness.start_continue();
    let child = Pid::from_raw(6000);
    harness
        .inferior()
        .threads
        .insert(child, TraceThread::starting(ExpectedStop::None));
    harness.trace().fail_after(child, 0, 0, Errno::ENOSPC);
    harness.trace().take_actions();
    harness
        .controller
        .process_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
        .expect("thread start");
    harness.settle_requested_stops();

    let reason = harness.public_reason().expect("a public stop");
    assert!(
        matches!(
            &reason,
            StopReason::WatchpointArmFailed { thread_id, .. } if *thread_id == debug_thread_id(child)
        ),
        "{reason:?}"
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with(&format!("continue {child}"))),
        "{actions:?}"
    );

    // Resuming retries the arming and refuses to run while it fails.
    harness.trace().fail_after(child, 0, 0, Errno::ENOSPC);
    let stop = harness.stop_id();
    let tgid = harness.threads[0];
    let result = harness.controller.begin_execution(
        process_id(tgid),
        stop,
        ResumeScope::Process(process_id(tgid)),
        ActiveKind::Continue,
        ExceptionDisposition::Pass,
    );
    assert!(
        matches!(result, Err(Error::WatchpointHardwareBusy { thread }) if thread == debug_thread_id(child)),
        "{result:?}"
    );
    assert!(!harness.trace().took("continue"),);
}

#[test]
fn hits_are_attributed_from_dr6_without_rewinding_the_pc() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let first = harness.add(0xd000, 8).expect("first");
    let second = harness.add(0xd008, 8).expect("second");

    // An installed site just before the reported PC must not turn the
    // hardware trap into an int3.
    harness.trace().set_pc(pid, 0x41);
    harness.inferior().breakpoints.insert(
        VirtualAddress::new(0x40),
        BreakpointSite {
            original_byte: 0x90,
            installed: true,
            owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
        },
    );
    harness.start_continue();
    harness.trace().take_actions();
    harness.watch_trap(pid, 0b11).expect("watch trap");

    let StopReason::Watchpoint { hits } = harness.public_reason().expect("stop") else {
        panic!("expected a watchpoint stop");
    };

    assert_eq!(
        hits.iter().map(|hit| hit.watchpoint).collect::<Vec<_>>(),
        [first.id, second.id]
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("set_registers"))
    );
    let read = actions
        .iter()
        .position(|action| action == &format!("read {pid} dr6"))
        .expect("DR6 was read");
    assert_eq!(
        actions[read + 1],
        format!("write {pid} dr6={:#x}", debug_registers::STATUS_IDLE),
        "DR6 is cleared once consumed"
    );
}

#[test]
fn stale_status_is_never_consulted_outside_debug_exceptions() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.add(0xd000, 8).expect("arm");
    harness.trace().set_pc(pid, 0x41);
    harness.inferior().breakpoints.insert(
        VirtualAddress::new(0x40),
        BreakpointSite {
            original_byte: 0x90,
            installed: true,
            owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
        },
    );
    harness.controller.breakpoints.push(Breakpoint {
        pending_locations: Arc::from([]),
        id: BreakpointId::new(1),
        spec: BreakpointSpec::Address(VirtualAddress::new(0x40)),
        locations: Arc::from([]),
        hit_condition: None,
        condition: None,
        log_message: None,
        hit_count: 0,
        enabled: true,
        temporary: false,
    });
    harness.start_continue();
    harness.trace().take_actions();

    // An int3 stop while DR6 still records an earlier hit.
    harness
        .trap(pid, libc::SI_KERNEL, debug_registers::STATUS_IDLE | 0b1)
        .expect("int3");
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Breakpoint {
            address: Some(VirtualAddress::new(0x40)),
            hits: Arc::from([BreakpointHit {
                breakpoint: BreakpointId::new(1),
                hit_count: 1,
            }]),
        })
    );
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.contains("dr6")),
        "DR6 is not read at an int3 stop"
    );
}

#[test]
fn a_hit_in_an_unowned_slot_is_unclassifiable() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.add(0xd000, 8).expect("arm");
    harness.start_continue();
    harness.watch_trap(pid, 0b100).expect("foreign trap");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Unclassifiable { description }) if description.contains("no watchpoint owns")
    ));

    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.start_continue();
    harness
        .trap(pid, TRAP_HARDWARE_BREAKPOINT, debug_registers::STATUS_IDLE)
        .expect("trap without a slot");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Unclassifiable { .. })
    ));
}

#[test]
fn a_hit_while_stepping_over_a_breakpoint_completes_the_repair_and_stops() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let watchpoint = harness.add(0xe000, 8).expect("arm");
    let site = VirtualAddress::new(0x40);
    {
        let inferior = harness.inferior();
        inferior.breakpoints.insert(
            site,
            BreakpointSite {
                original_byte: 0x90,
                installed: false,
                owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
            },
        );
        inferior.repairs = VecDeque::from([RepairGroup {
            address: site,
            remaining: VecDeque::new(),
            current: Some(pid),
            site_removed: true,
        }]);
    }

    harness.start_continue();
    harness.thread(pid).expected = ExpectedStop::BreakpointRepair { address: site };
    harness.trace().take_actions();
    harness
        .trap(
            pid,
            libc::TRAP_TRACE,
            debug_registers::STATUS_IDLE | 1 << 14 | 0b1,
        )
        .expect("watched repair step");

    let StopReason::Watchpoint { hits } = harness.public_reason().expect("stop") else {
        panic!("expected the repair step's hit");
    };

    assert_eq!(hits[0].watchpoint, watchpoint.id);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.repairs.is_empty());
    assert!(
        inferior.breakpoints[&site].installed,
        "the site is restored"
    );
    assert_eq!(inferior.threads[&pid].stopped_at_breakpoint, None);
}

#[test]
fn detaching_disarms_every_thread_first_and_never_redelivers_a_watch_trap() {
    let mut harness = watch_harness(2);
    harness.add(0xf000, 8).expect("arm");
    {
        let inferior = harness.inferior();
        inferior.origin = InferiorOrigin::Attached;
    }

    harness.start_continue();

    // One thread reports a watch trap while the detach interrupts it.
    let (reply, result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let mut registers = harness.trace().registers_of(first);
    registers[debug_registers::STATUS_REGISTER] = debug_registers::STATUS_IDLE | 0b1;
    harness
        .trace()
        .registers
        .borrow_mut()
        .insert(first, registers);
    harness
        .trace()
        .set_siginfo(first, TRAP_HARDWARE_BREAKPOINT, None);
    assert!(
        harness
            .controller
            .handle_detach_wait(WaitEvent::Stopped(first, Signal::SIGTRAP))
    );
    harness.trace().take_actions();
    assert!(
        !harness
            .controller
            .handle_detach_wait(WaitEvent::PtraceEvent(
                second,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_STOP
            ))
    );
    result
        .blocking_recv()
        .expect("shutdown reply")
        .expect("detached");

    let actions = harness.trace().take_actions();
    let first_detach = actions
        .iter()
        .position(|action| action.starts_with("detach"))
        .expect("threads were detached");

    for pid in [first, second] {
        let disarm = actions
            .iter()
            .position(|action| action == &format!("write {pid} dr7=0x0"))
            .expect("every thread is disarmed");
        assert!(disarm < first_detach, "{actions:?}");
        assert!(
            actions.contains(&format!("detach {pid} None")),
            "no signal is delivered on detach: {actions:?}"
        );
    }
}

#[test]
fn a_failed_disarm_still_detaches_and_publishes_the_detach() {
    let mut harness = watch_harness(2);
    harness.add(0xf000, 8).expect("arm");
    harness.attached();
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.trace().fail_after(first, 7, 0, Errno::EPERM);
    harness.trace().take_actions();
    while harness.events.try_recv().is_ok() {}

    let (reply, result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    let result = result.blocking_recv().expect("shutdown reply");
    assert!(
        matches!(
            &result,
            Err(Error::Backend(error)) if matches!(
                error.downcast_ref::<LinuxError>(),
                Some(LinuxError::System(Errno::EPERM))
            )
        ),
        "the disarm failure is reported: {result:?}"
    );

    // Keeping the process traced would not help: the tracer is exiting.
    let actions = harness.trace().take_actions();

    for pid in [first, second] {
        assert!(
            actions.contains(&format!("detach {pid} None")),
            "{actions:?}"
        );
    }

    assert!(harness.controller.inferior.is_none());
    let detached = std::iter::from_fn(|| harness.events.try_recv().ok())
        .filter(|event| matches!(event, DebuggerEvent::InferiorDetached { .. }))
        .count();
    assert_eq!(detached, 1, "clients learn the process is gone");
}

/// A waiter thread that took a status while the controller's queue is full
/// waits to send it there, and only the controller makes room. Ending the
/// session lets that status through rather than joining a waiter that can
/// never finish, and keeps it for the controller to serve.
#[test]
fn ending_a_session_lets_through_a_waiter_blocked_on_a_full_queue() {
    let mut harness = watch_harness(2);
    harness.attached();
    let sender = harness.controller.message_sender.clone();
    let filler = WaitEvent::Exited(Pid::from_raw(9000), 0);
    while sender.try_send(ControllerMessage::Wait(filler)).is_ok() {}
    let held = WaitEvent::Exited(Pid::from_raw(9001), 0);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            // As the waiter does, it sends what it took before looking again.
            let _ = sender.blocking_send(ControllerMessage::Wait(held));
            while !stop.load(Ordering::Acquire) {
                thread::park();
            }
        }
    });
    harness.inferior().waiter = Some(Waiter {
        thread: Some(WaiterThread { stop, handle }),
        hub: None,
    });

    let (ended, end) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let (reply, result) = tokio::sync::oneshot::channel();
        harness.controller.begin_shutdown(Some(reply));
        let result = result.blocking_recv().expect("shutdown reply");
        let _ = ended.send((result, harness));
    });
    let (result, harness) = end
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the session ends while its waiter waits on a full queue");
    assert!(result.is_ok(), "{result:?}");
    harness.controller.drain_messages();
    let pending = harness.controller.pending.borrow();
    assert!(
        pending
            .iter()
            .any(|message| matches!(message, ControllerMessage::Wait(status) if *status == held)),
        "the waiter's status is kept for the controller"
    );
}

/// SIGKILL from outside took every thread of an attached process out of its
/// stop before the session ends: none can be written, but its process is
/// ending, so the traps left in it can never run and the detach succeeds.
#[test]
fn detaching_a_process_killed_from_outside_succeeds() {
    let mut harness = watch_harness(2);
    harness.add_breakpoint(0x40);
    harness.attached();

    for &pid in &harness.threads {
        harness.trace().sigkill(pid);
    }

    harness.trace().take_actions();
    while harness.events.try_recv().is_ok() {}

    let (reply, result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    let result = result.blocking_recv().expect("shutdown reply");
    assert!(result.is_ok(), "{result:?}");
    assert!(harness.controller.inferior.is_none());
    let detached = std::iter::from_fn(|| harness.events.try_recv().ok())
        .filter(|event| matches!(event, DebuggerEvent::InferiorDetached { .. }))
        .count();
    assert_eq!(detached, 1, "clients learn the process is gone");
}

/// A main thread that reaches its exit event as an attached session ends
/// finishes exiting. Linux reports its exit only after every other
/// thread's, so detaching the others never waits for it.
#[test]
fn detaching_waits_for_no_main_thread_that_exits_during_the_detach() {
    let mut harness = watch_harness(2);
    let [leader, sibling] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let inferior = harness.inferior();
    inferior.origin = InferiorOrigin::Attached;

    // The main thread runs alone, as after a client continued it so.
    inferior.thread_mut(leader).expect("leader").state = NativeThreadState::Running;
    harness.trace().take_actions();

    let (reply, mut result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    assert!(
        result.try_recv().is_err(),
        "the detach waits for the main thread"
    );
    harness.controller.handle_wait(WaitEvent::PtraceEvent(
        leader,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_EXIT,
    ));
    assert!(
        matches!(result.try_recv(), Ok(Ok(()))),
        "the detach completes without the main thread's exit"
    );
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("detach {sibling} None")),
        "{actions:?}"
    );
}

/// A failure that ends an attached session detaches the process and ends
/// the controller, as a shutdown does, rather than serve requests after.
#[test]
fn a_failure_that_detaches_an_attached_process_ends_the_controller() {
    let mut harness = watch_harness(2);
    harness.attached();
    let unowned = WaitEvent::PtraceEvent(
        Pid::from_raw(9999),
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_CLONE,
    );
    let keeps_running = harness
        .controller
        .handle_message(ControllerMessage::Wait(unowned));
    assert!(
        harness.controller.inferior.is_none(),
        "the process was detached"
    );
    assert!(!keeps_running, "the controller ends");
}

/// SIGKILL from outside takes an attached thread as the stop its
/// temporary breakpoint made deletes it: lifting its second trap fails,
/// and so would putting back the first. The process is dying, not failed:
/// nothing is released or restored, and the client's shutdown, queued
/// behind the stop, ends when its exit status reports the kill, which is
/// what the client is told.
#[test]
fn a_process_killed_while_its_temporary_breakpoint_is_deleted_reports_its_death() {
    let mut harness = hit_harness(1, ">0");
    harness.attached();
    harness.controller.next_breakpoint_id = 2;
    let pid = harness.threads[0];
    let second = HIT_SITE + 0x10;
    let breakpoint = &mut harness.controller.breakpoints[0];
    breakpoint.temporary = true;
    breakpoint.hit_condition = None;
    breakpoint.locations =
        Arc::from(
            [HIT_SITE, second].map(|address| ResolvedBreakpointLocation {
                location: crate::BreakpointLocation::Virtual(VirtualAddress::new(address)),
                code_instances: Arc::from([]),
                library: None,
            }),
        );
    harness.inferior().breakpoints.insert(
        VirtualAddress::new(second),
        BreakpointSite {
            original_byte: 0x90,
            installed: true,
            owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
        },
    );
    harness.store(second, u64::from(BREAKPOINT_OPCODE));
    *harness.trace().kill_point.borrow_mut() = Some((pid, "remove_breakpoint", 1));

    let (reply, mut answer) = tokio::sync::oneshot::channel();
    harness
        .controller
        .message_sender
        .try_send(ControllerMessage::Request(Request::Shutdown { reply }))
        .expect("the shutdown is queued");
    harness.trace().set_pc(pid, HIT_SITE + 1);
    harness.trace().set_siginfo(pid, libc::SI_KERNEL, None);
    let serve = |harness: &mut WatchHarness, message| harness.controller.handle_message(message);
    let mut running = serve(
        &mut harness,
        ControllerMessage::Wait(WaitEvent::Stopped(pid, Signal::SIGTRAP)),
    );
    while running && let Some(message) = harness.controller.next_message(false) {
        running = serve(&mut harness, message);
    }
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("detach ")),
        "a dying process is not released"
    );
    assert!(answer.try_recv().is_err(), "the shutdown awaits the exit");

    let ended = !serve(
        &mut harness,
        ControllerMessage::Wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false)),
    );
    let published = harness.published();
    assert!(
        published.iter().any(|event| matches!(
            event,
            DebuggerEvent::InferiorExited {
                status: ExitStatus::Terminated(info),
                ..
            } if info.code == 9
        )),
        "the kill is reported: {published:?}"
    );
    assert!(
        !published
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorDetached { .. })),
        "no detach is reported: {published:?}"
    );
    assert!(
        harness.controller.inferior.is_none(),
        "the process is reaped"
    );
    assert!(ended, "the controller ends");
    assert!(
        matches!(answer.try_recv(), Ok(Ok(()))),
        "the shutdown is answered"
    );
}

/// A breakpoint of two locations at [`HIT_SITE`] and past it, which no
/// process has installed.
fn two_location_breakpoint() -> Breakpoint {
    Breakpoint {
        pending_locations: Arc::from([]),
        id: BreakpointId::new(1),
        spec: BreakpointSpec::Address(VirtualAddress::new(HIT_SITE)),
        locations: Arc::from([HIT_SITE, HIT_SITE + 0x10].map(|address| {
            ResolvedBreakpointLocation {
                location: crate::BreakpointLocation::Virtual(VirtualAddress::new(address)),
                code_instances: Arc::from([]),
                library: None,
            }
        })),
        hit_condition: None,
        condition: None,
        log_message: None,
        hit_count: 0,
        enabled: true,
        temporary: false,
    }
}

/// SIGKILL takes a thread as a breakpoint's second trap is written:
/// taking back the first can only fail too. The error is the death's, and
/// each site record still matches memory: the first trap stays, recorded,
/// until the process's exit takes both.
#[test]
fn a_breakpoint_a_dying_process_cannot_take_is_not_taken_back() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let breakpoint = two_location_breakpoint();
    *harness.trace().kill_point.borrow_mut() = Some((pid, "install_breakpoint", 1));
    let controller = &mut harness.controller;
    let inferior = controller.inferior.as_mut().expect("inferior");
    let error = breakpoints::install_logical_breakpoint(&controller.ptrace, inferior, &breakpoint)
        .expect_err("the second trap fails");
    assert!(is_vanished_tracee(&error), "{error:?}");
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("remove_site")),
        "nothing is taken back"
    );
    let inferior = harness.inferior();
    let sites = inferior.breakpoints.keys().copied().collect::<Vec<_>>();
    assert_eq!(sites, [VirtualAddress::new(HIT_SITE)]);
    assert!(inferior.breakpoints[&VirtualAddress::new(HIT_SITE)].installed);
    let memory = harness.trace().memory.borrow();
    assert_eq!(
        memory.get(&HIT_SITE).map(|word| word & 0xff),
        Some(u64::from(BREAKPOINT_OPCODE)),
        "the first trap is in memory, as its record says"
    );
    assert_ne!(
        memory.get(&(HIT_SITE + 0x10)).map(|word| word & 0xff),
        Some(u64::from(BREAKPOINT_OPCODE)),
        "the second trap was never written"
    );
}

/// SIGKILL from outside takes the thread of an attach as its first stop
/// plants a breakpoint of two locations, at the second. The process is
/// dying, not failed: nothing is released, and the client's shutdown,
/// queued behind the stop, ends when its exit status reports the kill,
/// which is what the client is told.
#[test]
fn a_process_killed_while_its_attach_plants_a_breakpoint_reports_its_death() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness
        .controller
        .breakpoints
        .push(two_location_breakpoint());
    harness.controller.next_breakpoint_id = 2;
    let mut attached = harness.begin_attach();
    *harness.trace().kill_point.borrow_mut() = Some((pid, "install_breakpoint", 1));
    let (reply, mut answer) = tokio::sync::oneshot::channel();
    harness
        .controller
        .message_sender
        .try_send(ControllerMessage::Request(Request::Shutdown { reply }))
        .expect("the shutdown is queued");

    let serve = |harness: &mut WatchHarness, message| harness.controller.handle_message(message);
    let mut running = serve(
        &mut harness,
        ControllerMessage::Wait(WaitEvent::PtraceEvent(
            pid,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        )),
    );
    while running && let Some(message) = harness.controller.next_message(false) {
        running = serve(&mut harness, message);
    }
    assert!(
        harness.trace().kill_point.borrow().is_none(),
        "the thread was killed at the second trap"
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions.iter().any(|action| action.starts_with("detach ")),
        "a dying process is not released: {actions:?}"
    );
    assert!(answer.try_recv().is_err(), "the shutdown awaits the exit");

    let ended = !serve(
        &mut harness,
        ControllerMessage::Wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false)),
    );
    let published = harness.published();
    assert!(
        published.iter().any(|event| matches!(
            event,
            DebuggerEvent::InferiorExited {
                status: ExitStatus::Terminated(info),
                ..
            } if info.code == 9
        )),
        "the kill is reported: {published:?}"
    );
    assert!(
        !published
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorDetached { .. })),
        "no detach is reported: {published:?}"
    );
    assert!(
        harness.controller.inferior.is_none(),
        "the process is reaped"
    );
    assert!(ended, "the controller ends");
    assert!(
        matches!(answer.try_recv(), Ok(Ok(()))),
        "the shutdown is answered"
    );
    assert!(
        matches!(attached.try_recv(), Ok(Err(_))),
        "the attach is answered"
    );
}

/// A failure that releases an attached process ends the controller with a
/// client's shutdown still queued. That shutdown is answered, not dropped.
#[test]
fn a_shutdown_queued_behind_a_failure_that_releases_the_process_is_answered() {
    let mut harness = watch_harness(2);
    harness.attached();
    let (reply, mut answer) = tokio::sync::oneshot::channel();
    harness
        .controller
        .message_sender
        .try_send(ControllerMessage::Request(Request::Shutdown { reply }))
        .expect("the shutdown is queued");
    let unowned = WaitEvent::PtraceEvent(
        Pid::from_raw(9999),
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_CLONE,
    );
    let keeps_running = harness
        .controller
        .handle_message(ControllerMessage::Wait(unowned));
    assert!(!keeps_running, "the controller ends");
    assert!(
        matches!(answer.try_recv(), Ok(Ok(()))),
        "the queued shutdown is answered"
    );
}

#[test]
fn an_attached_stop_collects_traps_queued_behind_its_interrupt() {
    let mut harness = watch_harness(2);
    let watchpoint = harness.add(0x1_0000, 8).expect("arm");
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.attached();
    harness.start_continue();
    harness.trace().queued_traps.borrow_mut().insert(second);
    harness.trace().take_actions();

    // The first thread hits; the second is interrupted with its own hit
    // still queued.
    harness.watch_trap(first, 1).expect("first hit");
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("interrupt {second}"))
    );
    harness
        .event(second, libc::PTRACE_EVENT_STOP)
        .expect("interrupt stop");
    assert_eq!(
        harness.public_reason(),
        None,
        "the queued trap is collected first"
    );
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {second} None")]
    );

    harness.trace().queued_traps.borrow_mut().clear();
    harness.watch_trap(second, 1).expect("queued hit");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Watchpoint { ref hits }) if hits[0].watchpoint == watchpoint.id
    ));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");

    for pid in [first, second] {
        assert!(matches!(
            inferior.threads[&pid].reason,
            Some(StopReason::Watchpoint { ref hits }) if hits[0].thread == debug_thread_id(pid)
        ));
    }
}

#[test]
fn detaching_collects_queued_traps_before_releasing_the_process() {
    let mut harness = watch_harness(1);
    harness.add(0x1_0000, 8).expect("arm");
    let pid = harness.threads[0];
    harness.attached();
    harness.trace().queued_traps.borrow_mut().insert(pid);
    harness.trace().take_actions();

    let (reply, result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {pid} None")],
        "detaching waits for the queued trap"
    );

    harness.trace().queued_traps.borrow_mut().clear();
    let mut registers = harness.trace().registers_of(pid);
    registers[debug_registers::STATUS_REGISTER] = debug_registers::STATUS_IDLE | 1;
    harness
        .trace()
        .registers
        .borrow_mut()
        .insert(pid, registers);
    harness
        .trace()
        .set_siginfo(pid, TRAP_HARDWARE_BREAKPOINT, None);
    assert!(
        !harness
            .controller
            .handle_detach_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
    );
    result
        .blocking_recv()
        .expect("shutdown reply")
        .expect("detached");
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("detach {pid} None")),
        "the collected trap is not delivered: {actions:?}"
    );
}

/// A thread that reported its exit while the program ran, and is gone
/// before an attached session's shutdown can interrupt it, is not lost:
/// the shutdown awaits its exit status, and publishes its exit.
#[test]
fn shutting_down_publishes_the_exit_of_a_thread_too_far_gone_to_interrupt() {
    let mut harness = watch_harness(2);
    let [_, sibling] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.attached();
    harness.start_continue();
    harness
        .inferior()
        .thread_mut(sibling)
        .expect("thread")
        .state = NativeThreadState::Exiting;
    harness.trace().dead.borrow_mut().insert(sibling);
    harness.trace().take_actions();
    while harness.events.try_recv().is_ok() {}

    let (reply, _result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    harness
        .controller
        .handle_detach_wait(WaitEvent::Exited(sibling, 3));
    let published = std::iter::from_fn(|| harness.events.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        published.iter().any(|event| matches!(
            event,
            DebuggerEvent::ThreadExited { thread_id, .. } if *thread_id == debug_thread_id(sibling)
        )),
        "{published:?}"
    );
}

/// SIGKILL from outside took a thread out of its stop just as the detach
/// resumed it to collect a queued trap. The thread goes on to its exit, which
/// the detach awaits, and the session still ends without an error.
#[test]
fn detaching_awaits_a_thread_killed_as_it_resumes_to_collect_a_trap() {
    let mut harness = watch_harness(1);
    harness.add(0x1_0000, 8).expect("arm");
    let pid = harness.threads[0];
    harness.attached();
    harness.trace().queued_traps.borrow_mut().insert(pid);
    harness.trace().sigkill(pid);
    harness.trace().take_actions();

    let (reply, mut result) = tokio::sync::oneshot::channel();
    harness.controller.begin_shutdown(Some(reply));
    assert!(
        result.try_recv().is_err(),
        "the detach awaits the killed thread's exit: {:?}",
        harness.trace().take_actions()
    );

    harness
        .controller
        .handle_detach_wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false));
    result
        .blocking_recv()
        .expect("shutdown reply")
        .expect("the session ends with the process");
}

impl WatchHarness {
    /// Makes the harness's threads those of an attach that has seized and
    /// interrupted them, and returns the attach's reply.
    fn begin_attach(&mut self) -> tokio::sync::oneshot::Receiver<Result<StopId>> {
        let (reply, attached) = tokio::sync::oneshot::channel();
        self.controller.attach_reply = Some(reply);
        let inferior = self.controller.inferior.as_mut().expect("inferior");
        inferior.origin = InferiorOrigin::Attached;
        inferior.public_stop = None;
        inferior.barrier = Some(StopBarrier::visible(inferior.tgid, StopReason::Attach));

        for thread in inferior.threads.values_mut() {
            *thread = TraceThread::starting(ExpectedStop::InitialAttach);
        }

        self.trace().listed_threads.replace(self.threads.clone());
        self.trace().take_actions();
        attached
    }

    /// Reports the `PTRACE_EVENT_STOP` an interrupt causes.
    fn interrupted(&mut self, pid: Pid) -> Result<()> {
        self.event(pid, libc::PTRACE_EVENT_STOP)
    }

    /// Reports `parent` stopped at its clone event for `child`.
    fn cloned(&mut self, parent: Pid, child: Pid) -> Result<()> {
        let tgid = self.controller.inferior.as_ref().expect("inferior").tgid;
        self.trace().clone.replace(Some((child, tgid)));
        self.event(parent, libc::PTRACE_EVENT_CLONE)
    }
}

#[test]
fn an_attach_traces_threads_created_while_their_creators_were_seized() {
    let mut harness = watch_harness(1);
    let leader = harness.threads[0];
    let mut attached = harness.begin_attach();

    // The leader was seized inside clone, after the kernel had decided not
    // to trace the thread it was creating, which the thread list now shows.
    let untraced = Pid::from_raw(5100);
    harness.trace().listed_threads.borrow_mut().push(untraced);

    harness.interrupted(leader).expect("leader stops");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("seize {untraced}"), format!("interrupt {untraced}")]
    );
    assert!(
        attached.try_recv().is_err(),
        "{:?}",
        harness.public_reason()
    );

    harness.interrupted(untraced).expect("new thread stops");
    attached
        .try_recv()
        .expect("attach replied")
        .expect("attached");
    assert_eq!(harness.public_reason(), Some(StopReason::Attach));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert_eq!(
        inferior.threads.keys().copied().collect::<Vec<_>>(),
        [leader, untraced]
    );
}

#[test]
fn an_attach_seizes_the_thread_a_leader_left_after_the_listing() {
    let harness = watch_harness(1);
    let leader = harness.threads[0];
    let survivor = Pid::from_raw(5100);

    // The listing saw only the leader, which then created a thread and
    // exited, too soon to be seized. The process lives on in the thread.
    harness.trace().listed_threads.replace(vec![leader]);
    harness
        .trace()
        .next_listing
        .replace(Some(vec![leader, survivor]));
    harness.trace().unseizable.borrow_mut().insert(leader);

    let (tgid, seized, unseized, _) = harness
        .controller
        .seize_process(process_id(leader))
        .expect("the process lives on");
    assert_eq!(
        (tgid, seized, unseized),
        (leader, BTreeSet::from([survivor]), BTreeSet::from([leader]))
    );
}

#[test]
fn an_attach_whose_seized_threads_die_traces_the_threads_left_before_ending() {
    let mut harness = watch_harness(2);
    let (leader, seized) = (harness.threads[0], harness.threads[1]);
    let mut attached = harness.begin_attach();

    // The leader had exited and could not be seized, and before it did it
    // created a thread the listing missed. SIGKILL then reaches the seized
    // thread before its attach stop, while the new thread still runs.
    let inferior = harness.inferior();
    inferior.threads.remove(&leader);
    inferior.unseized_threads.insert(leader);
    inferior.barrier = Some(StopBarrier::visible(seized, StopReason::Attach));
    let untraced = Pid::from_raw(5100);
    harness
        .trace()
        .listed_threads
        .replace(vec![leader, untraced]);

    harness
        .controller
        .process_wait(WaitEvent::Signaled(seized, Signal::SIGKILL, false))
        .expect("seized thread dies");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("seize {untraced}"), format!("interrupt {untraced}")]
    );
    assert!(attached.try_recv().is_err(), "the attach still waits");
    let mut ended = false;
    while let Ok(event) = harness.events.try_recv() {
        ended |= matches!(event, DebuggerEvent::InferiorExited { .. });
    }

    assert!(!ended, "the process lives on in {untraced}");

    harness.trace().listed_threads.replace(vec![leader]);
    harness
        .controller
        .process_wait(WaitEvent::Signaled(untraced, Signal::SIGKILL, false))
        .expect("last thread dies");
    let mut ended = None;
    while let Ok(event) = harness.events.try_recv() {
        if let DebuggerEvent::InferiorExited { status, .. } = event {
            ended = Some(status);
        }
    }

    assert!(
        matches!(ended, Some(ExitStatus::Terminated(_))),
        "{ended:?}"
    );
    assert!(
        attached.try_recv().expect("attach replied").is_err(),
        "the attach fails"
    );
}

#[test]
fn an_interrupt_kept_past_a_clone_event_resumes_the_thread_unseen() {
    let mut harness = watch_harness(1);
    let leader = harness.threads[0];
    let child = Pid::from_raw(5001);
    let mut attached = harness.begin_attach();

    // The leader was already at its clone event when the attach interrupted
    // it. The kernel keeps such an interrupt until the thread resumes.
    harness.trace().listed_threads.borrow_mut().push(child);
    harness.cloned(leader, child).expect("clone event");
    harness.interrupted(child).expect("new thread starts");
    attached
        .try_recv()
        .expect("attach replied")
        .expect("attached");
    harness.resume().expect("continue");
    harness.trace().take_actions();
    while harness.events.try_recv().is_ok() {}

    harness.interrupted(leader).expect("kept interrupt");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {leader} None")]
    );
    assert_eq!(harness.public_reason(), None);
    assert_eq!(harness.published_stops(), 0);
}

#[test]
fn a_seized_thread_whose_interrupt_a_clone_event_took_is_interrupted_again() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let child = Pid::from_raw(5100);
    harness.attached();
    harness.resume().expect("continue");
    let process = process_id(harness.threads[0]);

    // The first thread reaches its clone event after the pause interrupts
    // it. That stop satisfies the interrupt, so no other stop follows.
    harness.controller.begin_pause(process).expect("pause");
    harness.cloned(first, child).expect("clone event");
    harness.interrupted(child).expect("new thread starts");
    harness.interrupted(second).expect("second stops");
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));

    harness.resume().expect("continue");
    harness.trace().take_actions();
    harness.controller.begin_pause(process).expect("pause");
    let actions = harness.trace().take_actions();

    for pid in [first, second, child] {
        assert!(actions.contains(&format!("interrupt {pid}")), "{actions:?}");
    }
}

#[test]
fn a_fork_child_loses_each_trap_it_inherited_even_one_lifted_since() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    harness.trace().clone.replace(Some((child, child)));
    harness
        .event(parent, libc::PTRACE_EVENT_FORK)
        .expect("fork event");
    harness.hit(parent).expect("breakpoint stop");
    assert_eq!(harness.public_reason(), Some(site_hit(1)));

    // The parent steps over the trap it stopped at, lifted from its own
    // memory but not from the child's, when the child first stops.
    harness.resume().expect("continue");
    assert!(harness.trace().take_actions().ends_with(&[
        format!("remove_site {HIT_SITE:#x}"),
        format!("step {parent}")
    ]));

    // The child's copy of the code still holds the trap.
    harness.store(HIT_SITE, u64::from(BREAKPOINT_OPCODE));
    harness
        .controller
        .process_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
        .expect("child's first stop");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("write_word {child} {HIT_SITE:#x} 0x90"),
            format!("detach {child} None"),
        ]
    );
}

#[test]
fn a_fork_whose_parent_is_killed_before_its_event_still_cleans_the_child() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);

    // The breakpoint is removed after the parent forked, before the fork
    // event is handled, and then the parent is killed: the event now
    // describes its exit, so its message is not the child's identifier.
    harness
        .controller
        .remove_breakpoint_owner(
            VirtualAddress::new(HIT_SITE),
            BreakpointOwner::User(BreakpointId::new(1)),
        )
        .expect("removed");
    harness.start_continue();
    harness.trace().take_actions();
    harness.trace().sigkill(parent);

    // The child forked while the trap was installed, so its copy holds it.
    harness.store(HIT_SITE, u64::from(BREAKPOINT_OPCODE));

    for status in [
        WaitEvent::Stopped(child, Signal::SIGSTOP),
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_FORK),
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(parent, Signal::SIGKILL, false),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    assert!(harness.controller.inferior.is_none());
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("write_word {child} {HIT_SITE:#x} 0x90")),
        "the child loses the trap it inherited: {actions:?}"
    );
    assert!(
        actions.contains(&format!("detach {child} None")),
        "{actions:?}"
    );
    assert!(
        !actions.iter().any(|action| action.starts_with("kill")),
        "no process is killed: {actions:?}"
    );
}

#[test]
fn a_fork_child_keeps_code_mapped_where_a_trap_was_removed() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);

    // The breakpoint's site is removed, and other code is mapped where it
    // was, as when a library is unloaded and another loaded in its place.
    // Only then does the parent fork.
    harness
        .controller
        .remove_breakpoint_owner(
            VirtualAddress::new(HIT_SITE),
            BreakpointOwner::User(BreakpointId::new(1)),
        )
        .expect("removed");
    harness.store(HIT_SITE, 0x55);
    harness.start_continue();
    harness.trace().clone.replace(Some((child, child)));
    harness
        .event(parent, libc::PTRACE_EVENT_FORK)
        .expect("fork event");
    harness.trace().take_actions();
    harness
        .controller
        .process_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
        .expect("child's first stop");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("detach {child} None")],
        "the child's code holds no trap to remove"
    );
}

#[test]
fn a_fork_child_still_starting_when_its_parent_exits_is_released_not_killed() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    harness.start_continue();
    harness.trace().clone.replace(Some((child, child)));

    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_FORK),
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Exited(parent, 0),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    // The program is gone, but the controller still releases its child.
    assert!(harness.controller.inferior.is_none());
    assert!(harness.controller.orphans.is_some());
    harness.trace().take_actions();
    harness.store(HIT_SITE, u64::from(BREAKPOINT_OPCODE));
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Stopped(child, Signal::SIGSTOP))
    );
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("write_word {child} {HIT_SITE:#x} 0x90"),
            format!("detach {child} None"),
        ]
    );
    assert!(harness.controller.orphans.is_none());
}

#[test]
fn a_fork_child_whose_event_was_lost_is_found_as_its_parent_exits() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    harness.start_continue();

    // SIGKILL took the parent out of its fork event's stop before the event
    // was reported; the child it forked still stops.
    harness.trace().traced_children.borrow_mut().push(child);
    harness.trace().sigkill(parent);
    harness.store(HIT_SITE, u64::from(BREAKPOINT_OPCODE));

    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(parent, Signal::SIGKILL, false),
        WaitEvent::Stopped(child, Signal::SIGSTOP),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("write_word {child} {HIT_SITE:#x} 0x90")),
        "{actions:?}"
    );
    assert!(
        actions.contains(&format!("detach {child} None")),
        "{actions:?}"
    );
    assert!(harness.controller.inferior.is_none());
    assert!(harness.controller.orphans.is_none());
}

/// After a panic, an attached process is released as a detach would:
/// the trap a thread executed before any waiter took its stop is given
/// back its instruction and the thread rewound over it, debug registers
/// are cleared, and a signal a thread stopped to take, in a status the
/// controller had not handled, is delivered as it is released.
#[test]
fn a_panic_releases_an_attached_process_as_a_detach_would() {
    let mut harness = watch_harness(2);
    harness.attached();
    let (trapped, signaled) = (harness.threads[0], harness.threads[1]);
    let site = VirtualAddress::new(0x40);
    harness.inferior().breakpoints.insert(
        site,
        BreakpointSite {
            original_byte: 0x55,
            installed: true,
            owners: BTreeSet::from([BreakpointOwner::Runtime]),
        },
    );
    harness.trace().put_byte(site, BREAKPOINT_OPCODE);
    harness.trace().set_pc(trapped, 0x41);
    harness.trace().set_siginfo(trapped, libc::SI_KERNEL, None);
    harness
        .trace()
        .unreaped
        .borrow_mut()
        .insert(trapped, WaitEvent::Stopped(trapped, Signal::SIGTRAP));
    harness
        .controller
        .pending
        .borrow_mut()
        .push_back(ControllerMessage::Wait(WaitEvent::Stopped(
            signaled,
            Signal::SIGUSR1,
        )));
    harness.trace().take_actions();

    harness.controller.release_after_panic();

    let actions = harness.trace().take_actions();

    for expected in [
        format!("write_word {trapped} 0x40 0x55"),
        format!("set_registers {trapped} rip=0x40"),
        format!("write {trapped} dr7=0x0"),
        format!("write {signaled} dr7=0x0"),
        format!("detach {trapped} None"),
        format!("detach {signaled} Some(SIGUSR1)"),
    ] {
        assert!(actions.contains(&expected), "{expected} in {actions:#?}");
    }

    assert!(harness.controller.inferior.is_none());
}

/// The controller's invariant check fires on state that contradicts
/// itself: a site that no owner holds.
#[test]
#[should_panic(expected = "the site at 0x40 has no owners")]
fn the_invariant_check_finds_a_site_no_owner_holds() {
    let mut harness = watch_harness(1);
    harness.controller.check_invariants();
    harness.inferior().breakpoints.insert(
        VirtualAddress::new(0x40),
        BreakpointSite {
            original_byte: 0x90,
            installed: true,
            owners: BTreeSet::new(),
        },
    );
    harness.controller.check_invariants();
}

/// A client's shutdown after one the controller began itself, as a failed
/// attach does, is answered once the first is done, not begun again.
#[test]
fn a_shutdown_requested_during_another_is_answered_when_it_ends() {
    let mut harness = watch_harness(2);
    harness.attached();
    harness.controller.begin_shutdown(None);
    assert!(
        harness.controller.inferior.is_none(),
        "the stopped process is detached at once"
    );
    let (reply, mut result) = tokio::sync::oneshot::channel();
    let keeps_running = harness
        .controller
        .handle_message(ControllerMessage::Request(Request::Shutdown { reply }));
    assert!(!keeps_running, "nothing is left to wait for");
    assert!(
        matches!(result.try_recv(), Ok(Ok(()))),
        "the second shutdown is answered"
    );
}

/// A kill requested while an attached process is being detached is
/// answered when the detach ends the session.
#[test]
fn a_kill_during_a_detach_is_answered_when_it_ends() {
    let mut harness = watch_harness(1);
    harness.attached();
    let (reply, mut result) = tokio::sync::oneshot::channel();
    harness.controller.kill(reply);
    harness.controller.begin_shutdown(None);
    assert!(
        harness.controller.inferior.is_none(),
        "the process is detached"
    );
    assert!(
        matches!(result.try_recv(), Ok(Ok(()))),
        "the kill is answered"
    );
}

/// An attached process whose threads all exit while it is detached hands
/// a child killed for traps it kept to the orphans, whose exit the waiter
/// still collects.
#[test]
fn a_vanished_attached_process_leaves_its_killed_child_to_be_reaped() {
    let mut harness = watch_harness(1);
    harness.attached();
    let child = Pid::from_raw(6000);
    let inferior = harness.inferior();
    inferior.killed_children.insert(child);
    inferior.threads.clear();
    harness.controller.shutting_down = true;
    harness.controller.detach_when_stopped().expect("detach");
    assert!(harness.controller.inferior.is_none());
    assert_eq!(
        harness
            .controller
            .orphans
            .as_ref()
            .map(|orphans| orphans.killed.clone()),
        Some(BTreeSet::from([child]))
    );
}

/// A shutdown that meets a fork event its parent's SIGKILL superseded
/// continues the parent from its exit stop, and the parent is soon reaped.
/// The child it forked is found first, and released once it stops rather
/// than left stopped and traced.
#[test]
fn a_shutdown_meeting_a_superseded_fork_event_still_releases_the_child() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    harness.start_continue();
    harness.trace().traced_children.borrow_mut().push(child);
    harness.controller.begin_shutdown(None);
    harness.trace().sigkill(parent);
    harness.store(HIT_SITE, u64::from(BREAKPOINT_OPCODE));
    assert!(harness.controller.handle_wait(WaitEvent::PtraceEvent(
        parent,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_FORK,
    )));

    // Continued from its exit stop, the parent was reaped, and `/proc`
    // lists its children no more.
    harness.trace().traced_children.borrow_mut().clear();

    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(parent, Signal::SIGKILL, false),
    ] {
        harness.controller.handle_wait(status);
    }

    assert!(
        harness.controller.orphans.is_some(),
        "the controller waits to release the child"
    );
    harness
        .controller
        .handle_wait(WaitEvent::Stopped(child, Signal::SIGSTOP));
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("write_word {child} {HIT_SITE:#x} 0x90")),
        "{actions:?}"
    );
    assert!(
        actions.contains(&format!("detach {child} None")),
        "{actions:?}"
    );
    assert!(harness.controller.orphans.is_none());
}

/// A launch while the controller still releases children the last program
/// forked waits for them, rather than failing as if a program still ran.
#[test]
fn a_launch_while_fork_children_are_released_waits_for_them() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    harness.trace().clone.replace(Some((child, child)));

    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_FORK),
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Exited(parent, 0),
    ] {
        harness.controller.handle_wait(status);
    }

    assert!(harness.controller.orphans.is_some());
    harness.trace().take_actions();

    let (reply, mut launched) = tokio::sync::oneshot::channel();
    harness
        .controller
        .start(Start::Launch(LaunchOptions::default(), reply));
    assert!(
        launched.try_recv().is_err(),
        "the launch waits for the child"
    );
    assert!(harness.trace().take_actions().is_empty());
    harness
        .controller
        .handle_wait(WaitEvent::Stopped(child, Signal::SIGSTOP));
    let actions = harness.trace().take_actions();
    assert_eq!(
        actions.iter().position(|action| action == "spawn"),
        Some(actions.len() - 1),
        "the child is released, and then the program launched: {actions:?}"
    );
    assert!(harness.controller.orphans.is_none());
    assert!(harness.controller.inferior.is_some());
}

#[test]
fn a_fork_child_that_dies_as_it_is_released_is_awaited_until_its_exit() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    harness.start_continue();
    harness.trace().clone.replace(Some((child, child)));

    // The parent forks and starts exiting before its child first stops.
    // SIGKILL then takes the child out of that stop as it is released.
    harness.store(HIT_SITE, u64::from(BREAKPOINT_OPCODE));
    *harness.trace().kill_point.borrow_mut() = Some((child, "detach", 0));

    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_FORK),
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Stopped(child, Signal::SIGSTOP),
        WaitEvent::Exited(parent, 0),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    assert!(
        harness.trace().kill_point.borrow().is_none(),
        "the child was scrubbed and detached"
    );

    // The program is gone, but its child is still traced: it stops at its
    // exit event, which waits for the tracer, and only then exits.
    assert!(harness.controller.inferior.is_none());
    assert!(
        harness.controller.orphans.is_some(),
        "the killed child is still traced"
    );
    harness.trace().take_actions();
    assert!(harness.controller.handle_wait(WaitEvent::PtraceEvent(
        child,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_EXIT,
    )));
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {child} None")]
    );
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Signaled(child, Signal::SIGKILL, false))
    );
    assert!(
        harness.controller.orphans.is_none(),
        "no child is left to wait for"
    );
}

#[test]
fn a_fork_event_whose_thread_is_killed_as_its_message_is_read_is_superseded() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    harness.start_continue();
    harness
        .trace()
        .set_siginfo(parent, libc::SIGTRAP | (libc::PTRACE_EVENT_FORK << 8), None);

    // SIGKILL lands after the stop was seen to be the fork's, so the message
    // read is the exit's, not the child's identifier.
    *harness.trace().kill_point.borrow_mut() = Some((parent, "event_message", 0));
    assert!(harness.controller.handle_wait(WaitEvent::PtraceEvent(
        parent,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_FORK,
    )));
    assert!(harness.trace().kill_point.borrow().is_none());
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(
        inferior.fork_children.is_empty(),
        "no child is expected: {:?}",
        inferior.fork_children.keys().collect::<Vec<_>>()
    );
    assert_eq!(harness.thread(parent).state, NativeThreadState::Running);
}

#[test]
fn unreadable_signal_queue_stops_attach_draining_before_resuming_any_thread() {
    let mut harness = watch_harness(2);
    harness.attached();
    harness
        .trace()
        .queued_traps
        .borrow_mut()
        .insert(harness.threads[0]);
    harness.controller.ptrace.queue_error = Some(harness.threads[1]);
    harness.trace().take_actions();
    let error = harness
        .controller
        .drain_queued_traps()
        .expect_err("queue evidence is required");
    assert!(error.to_string().contains("malformed signal masks"));
    assert!(
        harness.trace().take_actions().is_empty(),
        "no thread may run on unknown queue evidence"
    );
}

#[test]
fn continuing_tolerates_threads_killed_by_a_siblings_exit() {
    let mut harness = watch_harness(3);
    let [first, second, third] = harness.threads[..] else {
        unreachable!("three threads");
    };

    // The first thread runs exit_group as soon as it resumes, killing
    // its siblings out of their stops.
    harness
        .trace()
        .vanished
        .borrow_mut()
        .extend([second, third]);
    let stop = harness.stop_id();
    harness
        .controller
        .begin_execution(
            process_id(first),
            stop,
            ResumeScope::Process(process_id(first)),
            ActiveKind::Continue,
            ExceptionDisposition::Pass,
        )
        .expect("the continue succeeds");
    assert_eq!(harness.thread(first).state, NativeThreadState::Running);

    for pid in [second, third] {
        assert_eq!(harness.thread(pid).state, NativeThreadState::Exiting);
    }

    // Their exits retire them, and the process exit ends the session.
    for pid in [second, third, first] {
        harness
            .controller
            .process_wait(WaitEvent::Exited(pid, 0))
            .expect("exit");
    }

    assert!(harness.controller.inferior.is_none());
}

#[test]
fn a_hit_during_a_pause_outranks_the_pause() {
    let mut harness = watch_harness(2);
    let watchpoint = harness.add(0x1_1000, 8).expect("arm");
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.start_continue();
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause");

    // The first thread hits before its requested stop is delivered.
    harness.watch_trap(first, 1).expect("hit during pause");
    assert_eq!(
        harness.public_reason(),
        None,
        "the second thread is running"
    );
    harness.settle_requested_stops();
    let Some(StopReason::Watchpoint { hits }) = harness.public_reason() else {
        panic!("the hit outranks the pause: {:?}", harness.public_reason());
    };

    assert_eq!(hits[0].watchpoint, watchpoint.id);
    assert_eq!(hits[0].thread, debug_thread_id(first));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert_eq!(inferior.threads[&second].reason, None);
    assert!(
        inferior.threads[&first].debugger_stop_pending,
        "the first thread's requested stop is still outstanding"
    );
}

#[test]
fn a_seized_thread_whose_start_precedes_its_clone_event_is_armed_before_running() {
    let mut harness = watch_harness(1);
    harness.add(0x1_2000, 8).expect("arm");
    let parent = harness.threads[0];
    harness.attached();
    harness.start_continue();
    let child = Pid::from_raw(6100);
    *harness.trace().clone.borrow_mut() = Some((child, parent));
    harness.trace().take_actions();

    // The child's first stop arrives before the debugger knows it exists.
    harness
        .event(child, libc::PTRACE_EVENT_STOP)
        .expect("early child stop is retained");
    assert!(harness.trace().take_actions().is_empty());
    harness
        .event(parent, libc::PTRACE_EVENT_CLONE)
        .expect("clone event");

    let actions = harness.trace().take_actions();
    let armed = actions
        .iter()
        .position(|action| action == &format!("write {child} dr7=0x90001"))
        .expect("the child was armed");
    let resumed = actions
        .iter()
        .position(|action| action == &format!("continue {child} None"))
        .expect("the child was resumed");
    assert!(armed < resumed, "{actions:?}");
    assert_eq!(harness.public_reason(), None, "no unclassifiable stop");
}

#[test]
fn a_new_thread_killed_out_of_its_first_stop_is_retired_by_its_exit() {
    let mut harness = watch_harness(1);
    let parent = harness.threads[0];
    harness.start_continue();
    let child = Pid::from_raw(6100);
    *harness.trace().clone.borrow_mut() = Some((child, parent));
    harness
        .event(parent, libc::PTRACE_EVENT_CLONE)
        .expect("clone event");

    // The parent resumed and called exit_group before the child's first
    // stop was handled, killing the child out of that stop.
    harness.trace().vanished.borrow_mut().insert(child);
    harness
        .event(child, libc::PTRACE_EVENT_STOP)
        .expect("the killed child's start is no failure");
    assert_eq!(harness.thread(child).state, NativeThreadState::Exiting);

    for pid in [child, parent] {
        harness
            .controller
            .process_wait(WaitEvent::Exited(pid, 0))
            .expect("exit");
    }

    assert!(harness.controller.inferior.is_none());
}

#[test]
fn a_requested_stop_that_sigkill_ended_since_is_superseded() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.attached();
    harness.start_continue();
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause");

    // A sibling's exit_group killed the second thread out of the stop it
    // reported, and it now waits at its exit event.
    harness.trace().set_siginfo(second, EXIT_EVENT, None);
    harness
        .event(second, libc::PTRACE_EVENT_STOP)
        .expect("a superseded stop is no failure");
    assert_eq!(harness.thread(second).state, NativeThreadState::Running);

    harness
        .event(first, libc::PTRACE_EVENT_STOP)
        .expect("first stop");
    assert_eq!(harness.public_reason(), None, "the second thread runs");
    harness
        .event(second, libc::PTRACE_EVENT_EXIT)
        .expect("exit event");
    harness
        .controller
        .process_wait(WaitEvent::Exited(second, 0))
        .expect("exit");
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
}

#[test]
fn a_pause_ends_once_a_thread_reaped_before_its_request_is_retired() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.start_continue();

    // The waiter reaped the second thread, whose exit status is still on
    // its way to the controller.
    harness.trace().vanished.borrow_mut().insert(second);
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("a reaped thread does not fail the pause");
    harness.trace().vanished.borrow_mut().clear();
    harness
        .controller
        .process_wait(WaitEvent::Exited(second, 0))
        .expect("exit");
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
}

#[test]
fn a_stop_whose_thread_sigkill_ends_while_it_is_handled_waits_for_the_exit() {
    let mut harness = watch_harness(2);
    let [stepping, sibling] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.start_continue();
    let inferior = harness.inferior();
    inferior.active.as_mut().expect("execution").kind =
        step_execution(stepping, StepKind::OverInstruction, StepStart::default());
    inferior.thread_mut(stepping).expect("thread").expected = ExpectedStop::UserStep {
        kind: StepKind::OverInstruction,
    };

    // The sibling calls exit_group after the step's trap was classified.
    *harness.trace().kill_point.borrow_mut() = Some((stepping, "registers", 0));
    harness
        .trace()
        .set_siginfo(stepping, libc::TRAP_TRACE, None);
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Stopped(stepping, Signal::SIGTRAP))
    );
    assert!(
        harness.trace().kill_point.borrow().is_none(),
        "the handler read the killed thread's registers"
    );
    assert_eq!(harness.thread(stepping).state, NativeThreadState::Running);

    // Both threads exit, and the session ends with the process.
    for status in [
        WaitEvent::PtraceEvent(stepping, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::PtraceEvent(sibling, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Exited(stepping, 0),
        WaitEvent::Exited(sibling, 0),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    assert!(harness.controller.inferior.is_none());
}

#[test]
fn a_step_whose_threads_sigkill_ends_as_it_begins_runs_into_the_exit() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let stop = harness.stop_id();

    // Something outside the debugger kills the program as the step starts.
    *harness.trace().kill_point.borrow_mut() = Some((first, "registers", 0));
    harness.trace().sigkill(second);
    let execution = harness.controller.begin_execution(
        process_id(first),
        stop,
        ResumeScope::Process(process_id(first)),
        step_execution(first, StepKind::IntoSource, StepStart::default()),
        ExceptionDisposition::Pass,
    );
    assert!(
        harness.trace().kill_point.borrow().is_none(),
        "the step read its thread's registers"
    );
    assert!(execution.is_ok(), "{execution:?}");
    assert!(!harness.trace().took("kill"), "nothing kills the program");
}

#[test]
fn a_trap_whose_thread_sigkill_ends_while_it_is_classified_is_superseded() {
    let mut harness = watch_harness(1);
    harness.add(0x1_1000, 8).expect("arm");
    let pid = harness.threads[0];
    harness.start_continue();
    *harness.trace().kill_point.borrow_mut() = Some((pid, "read_debug_register", 0));
    harness
        .watch_trap(pid, 0b1)
        .expect("a superseded trap is no failure");
    assert!(harness.trace().kill_point.borrow().is_none());
    assert_eq!(harness.thread(pid).state, NativeThreadState::Running);
    assert_eq!(harness.public_reason(), None);
}

#[test]
fn plan_traps_a_dying_process_cannot_take_are_not_restored() {
    let plan = BTreeSet::from([VirtualAddress::new(0x50), VirtualAddress::new(0x60)]);

    // As a step begins.
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let stop = harness.stop_id();
    *harness.trace().kill_point.borrow_mut() = Some((pid, "install_breakpoint", 1));
    let error = harness
        .controller
        .begin_execution(
            process_id(pid),
            stop,
            ResumeScope::Process(process_id(pid)),
            step_execution(
                pid,
                StepKind::OverSource,
                StepStart {
                    plan_addresses: plan.clone(),
                    ..StepStart::default()
                },
            ),
            ExceptionDisposition::Pass,
        )
        .expect_err("the second trap fails");
    assert!(is_vanished_tracee(&error), "{error:?}");
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill") || action.starts_with("remove_site")),
        "nothing is restored and nothing killed"
    );

    // And as a step goes on.
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    *harness.trace().kill_point.borrow_mut() = Some((pid, "install_breakpoint", 1));
    let error = harness
        .controller
        .install_additional_plan_breakpoints(ExecutionId::new(9), &plan)
        .expect_err("the second trap fails");
    assert!(is_vanished_tracee(&error), "{error:?}");
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("kill") || action.starts_with("remove_site")),
        "nothing is restored and nothing killed"
    );
}

#[test]
fn a_launch_killed_at_its_first_stop_ends_in_its_exit() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let inferior = harness.inferior();
    inferior.public_stop = None;
    inferior.active = Some(ActiveExecution {
        id: ExecutionId::new(1),
        kind: ActiveKind::Launch,
        scope: ResumeScope::Process(process_id(pid)),
        resume_threads: BTreeSet::from([pid]),
    });
    let thread = inferior.thread_mut(pid).expect("thread");
    thread.state = NativeThreadState::Starting;
    thread.expected = ExpectedStop::InitialExec;
    harness.published();

    // Killed before its options were set.
    harness.trace().sigkill(pid);
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
    );
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false))
    );
    assert!(harness.controller.inferior.is_none());
    assert!(
        !harness.trace().took("kill"),
        "the debugger does not give up on the program"
    );
    assert!(harness.published().iter().any(|event| matches!(
        event,
        DebuggerEvent::InferiorExited {
            status: ExitStatus::Terminated(_),
            ..
        }
    )));
}

#[test]
fn repairs_after_the_debuggers_own_kill_are_left_undone() {
    let mut harness = watch_harness(3);
    harness.add_breakpoint(0x40);
    harness.start_continue();
    let [leader, repairing, running] = harness.threads[..] else {
        unreachable!("three threads");
    };

    let inferior = harness.inferior();
    inferior.repairs = VecDeque::from([RepairGroup {
        address: VirtualAddress::new(0x40),
        remaining: VecDeque::from([running]),
        current: Some(repairing),
        // Lifting the trap already failed as the kill landed.
        site_removed: false,
    }]);
    let thread = inferior.thread_mut(repairing).expect("thread");
    thread.stopped_at_breakpoint = Some(VirtualAddress::new(0x40));
    thread.expected = ExpectedStop::BreakpointRepair {
        address: VirtualAddress::new(0x40),
    };

    inferior
        .thread_mut(running)
        .expect("thread")
        .stopped_at_breakpoint = Some(VirtualAddress::new(0x40));

    // The client kills the program mid-repair. When the repairing thread's
    // death arrives, the next repair has no stopped thread to lift the
    // trap through.
    let (reply, mut killed) = tokio::sync::oneshot::channel();
    harness.controller.kill(reply);

    for pid in [leader, repairing, running] {
        harness.trace().sigkill(pid);
    }

    for status in [
        WaitEvent::PtraceEvent(leader, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(repairing, Signal::SIGKILL, false),
        WaitEvent::PtraceEvent(running, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(running, Signal::SIGKILL, false),
        WaitEvent::Signaled(leader, Signal::SIGKILL, false),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    assert!(harness.controller.inferior.is_none());
    assert!(matches!(killed.try_recv(), Ok(Ok(()))), "the kill succeeds");
    let kills = harness
        .trace()
        .take_actions()
        .into_iter()
        .filter(|action| action.starts_with("kill"))
        .count();
    assert_eq!(kills, 1, "the debugger does not give up on its own kill");
}

#[test]
fn a_repair_whose_process_dies_leaves_its_trap_unrestored() {
    let mut harness = repairing_harness_of(1);
    let (leader, repairing) = (harness.threads[0], harness.threads[1]);

    // Something outside the debugger kills the program mid-repair. The
    // repairing thread's death leaves no thread to restore the trap through.
    harness.trace().sigkill(leader);
    harness.trace().sigkill(repairing);

    for status in [
        WaitEvent::PtraceEvent(leader, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::PtraceEvent(repairing, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Signaled(repairing, Signal::SIGKILL, false),
        WaitEvent::Signaled(leader, Signal::SIGKILL, false),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    assert!(harness.controller.inferior.is_none());
    assert!(
        !harness.trace().took("kill"),
        "the debugger does not give up on the program"
    );
}

#[test]
fn a_pause_that_meets_a_fork_event_instead_completes() {
    let mut harness = watch_harness(1);
    let parent = harness.threads[0];
    harness.start_continue();
    harness
        .controller
        .begin_pause(process_id(parent))
        .expect("pause");

    // The parent forked before its requested stop was delivered.
    let child = Pid::from_raw(6200);
    *harness.trace().clone.borrow_mut() = Some((child, child));
    harness
        .event(parent, libc::PTRACE_EVENT_FORK)
        .expect("fork event");
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
}

/// The trap site of the hit-count harness's one user breakpoint.
const HIT_SITE: u64 = 0x40;

/// A process-wide continue whose threads can reach one user breakpoint at
/// [`HIT_SITE`] with `hit_condition`.
fn hit_harness(thread_count: i32, hit_condition: &str) -> WatchHarness {
    let mut harness = watch_harness(thread_count);
    harness.controller.breakpoints.push(Breakpoint {
        pending_locations: Arc::from([]),
        id: BreakpointId::new(1),
        spec: BreakpointSpec::Address(VirtualAddress::new(HIT_SITE)),
        locations: Arc::from([ResolvedBreakpointLocation {
            location: crate::BreakpointLocation::Virtual(VirtualAddress::new(HIT_SITE)),
            code_instances: Arc::from([]),
            library: None,
        }]),
        hit_condition: Some(hit_condition.parse().expect("test hit condition")),
        condition: None,
        log_message: None,
        hit_count: 0,
        enabled: true,
        temporary: false,
    });
    harness.inferior().breakpoints.insert(
        VirtualAddress::new(HIT_SITE),
        BreakpointSite {
            original_byte: 0x90,
            installed: true,
            owners: BTreeSet::from([BreakpointOwner::User(BreakpointId::new(1))]),
        },
    );
    harness.store(HIT_SITE, u64::from(BREAKPOINT_OPCODE));
    harness.start_continue();
    harness.trace().take_actions();
    harness
}

impl WatchHarness {
    /// Reports `pid` executing the trap at [`HIT_SITE`].
    fn hit(&mut self, pid: Pid) -> Result<()> {
        self.trace().set_pc(pid, HIT_SITE + 1);
        self.trap(pid, libc::SI_KERNEL, debug_registers::STATUS_IDLE)
    }

    /// Completes the single step a breakpoint repair began.
    pub(super) fn finish_step(&mut self, pid: Pid) -> Result<()> {
        self.trap(pid, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
    }

    fn hit_count(&self) -> u64 {
        self.controller.breakpoints[0].hit_count
    }

    fn published_stops(&mut self) -> usize {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| matches!(event, DebuggerEvent::InferiorStopped { .. }))
            .count()
    }

    pub(super) fn resume(&mut self) -> Result<ExecutionId> {
        let inferior = self.controller.inferior.as_ref().expect("inferior");
        let process = process_id(inferior.tgid);
        let stop = inferior.public_stop.as_ref().expect("public stop").id;
        self.controller.begin_execution(
            process,
            stop,
            ResumeScope::Process(process),
            ActiveKind::Continue,
            ExceptionDisposition::Pass,
        )
    }
}

fn site_hit(hit_count: u64) -> StopReason {
    StopReason::Breakpoint {
        address: Some(VirtualAddress::new(HIT_SITE)),
        hits: Arc::from([BreakpointHit {
            breakpoint: BreakpointId::new(1),
            hit_count,
        }]),
    }
}

#[test]
fn declined_hits_lift_the_trap_only_once_every_sibling_stopped() {
    let mut harness = hit_harness(3, "==2");
    let [first, second, third] = harness.threads[..] else {
        panic!("three threads");
    };

    harness.hit(first).expect("declined hit");
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("request_stop {second}")),
        "{actions:?}"
    );
    assert!(
        actions.contains(&format!("request_stop {third}")),
        "{actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("remove_site") || action.starts_with("step")),
        "the trap was lifted while siblings ran: {actions:?}"
    );

    harness.settle_requested_stops();
    let actions = harness.trace().take_actions();
    let repair = [
        format!("remove_site {HIT_SITE:#x}"),
        format!("step {first}"),
    ];
    assert!(actions.ends_with(&repair), "{actions:?}");

    harness.finish_step(first).expect("repair step");
    let actions = harness.trace().take_actions();
    let resumed = [
        format!("reinstall_site {HIT_SITE:#x}"),
        format!("continue {first} None"),
        format!("continue {second} None"),
        format!("continue {third} None"),
    ];
    assert!(actions.ends_with(&resumed), "{actions:?}");
    assert_eq!(harness.published_stops(), 0);
    assert_eq!(harness.public_reason(), None);
    assert_eq!(harness.hit_count(), 1);

    // The next arrival meets the condition and stops every thread.
    harness.hit(second).expect("stopping hit");
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(site_hit(2)));
    assert_eq!(harness.published_stops(), 1);
}

#[test]
fn a_visible_stop_during_an_internal_stop_wins_and_the_declined_hit_counts_once() {
    let mut harness = hit_harness(2, "==5");
    let [first, second] = harness.threads[..] else {
        panic!("two threads");
    };

    harness.hit(first).expect("declined hit");

    // The sibling reports a signal before the stop it was asked for.
    harness.trace().set_siginfo(second, 0, Some(1));
    harness
        .controller
        .process_wait(WaitEvent::Stopped(second, Signal::SIGUSR1))
        .expect("signal stop");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Exception(exception)) if exception.code == Signal::SIGUSR1.code()
    ));
    let declined = harness.thread(first);
    assert_eq!(declined.reason, None);
    assert_eq!(
        declined.stopped_at_breakpoint,
        Some(VirtualAddress::new(HIT_SITE))
    );
    harness.trace().take_actions();

    // Resuming steps the declined thread over the site before anything runs.
    harness.resume().expect("resume");
    let actions = harness.trace().take_actions();
    assert!(
        actions.ends_with(&[
            format!("remove_site {HIT_SITE:#x}"),
            format!("step {first}")
        ]),
        "{actions:?}"
    );
    harness.finish_step(first).expect("repair step");
    let actions = harness.trace().take_actions();
    assert!(
        actions.ends_with(&[
            format!("reinstall_site {HIT_SITE:#x}"),
            format!("continue {first} None"),
            format!("continue {second} Some(SIGUSR1)"),
        ]),
        "{actions:?}"
    );
    assert_eq!(harness.hit_count(), 1);
}

#[test]
fn a_pause_requested_during_an_internal_stop_is_published() {
    let mut harness = hit_harness(2, "==5");
    let first = harness.threads[0];
    harness.hit(first).expect("declined hit");
    let process = process_id(harness.threads[0]);
    harness.controller.begin_pause(process).expect("pause");
    harness.settle_requested_stops();

    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
    assert!(
        !harness.trace().took("remove_site"),
        "a pause must not repair or resume anything"
    );
    assert_eq!(
        harness.thread(first).stopped_at_breakpoint,
        Some(VirtualAddress::new(HIT_SITE))
    );
}

#[test]
fn a_co_hit_meeting_its_condition_turns_an_internal_stop_visible() {
    let mut harness = hit_harness(2, "==2");
    let [first, second] = harness.threads[..] else {
        panic!("two threads");
    };

    harness.hit(first).expect("declined hit");

    // The sibling trapped at the same site before its requested stop.
    harness.hit(second).expect("stopping co-hit");

    assert_eq!(harness.public_reason(), Some(site_hit(2)));
    let stop = harness
        .controller
        .inferior
        .as_ref()
        .and_then(|inferior| inferior.public_stop.as_ref())
        .expect("published stop");
    assert_eq!(stop.triggering_thread, second);
    assert_eq!(harness.thread(first).reason, None);
    assert_eq!(harness.thread(second).reason, Some(site_hit(2)));
}

/// SIGKILL takes a sibling out of a published stop to its exit, so no
/// site can be edited until it is reaped; a breakpoint removed meanwhile
/// leaves its owner on the trap, which the stopped thread's frames must
/// not take for a breakpoint that still exists.
#[test]
fn frames_ignore_a_breakpoint_removed_while_a_sibling_exits() {
    let mut harness = hit_harness(2, "==1");
    let [first, second] = harness.threads[..] else {
        panic!("two threads");
    };

    harness.hit(first).expect("stopping hit");
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(site_hit(1)));
    harness
        .event(second, libc::PTRACE_EVENT_EXIT)
        .expect("exit event");
    assert!(!harness.controller.sites_live());

    harness
        .controller
        .remove_breakpoint(BreakpointId::new(1))
        .expect("remove");
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    let instances = harness
        .controller
        .breakpoint_code_instances(inferior, VirtualAddress::new(HIT_SITE))
        .expect("the site's breakpoints");
    assert!(instances.is_empty(), "{instances:?}");
}

#[test]
fn a_declined_thread_exiting_during_an_internal_stop_publishes_nothing() {
    let mut harness = hit_harness(3, "==5");
    let [first, second, third] = harness.threads[..] else {
        panic!("three threads");
    };

    // The kernel reports the leader's exit only after every other thread's,
    // so a sibling exits here.
    harness.hit(second).expect("declined hit");
    harness
        .controller
        .process_wait(WaitEvent::Exited(second, 0))
        .expect("thread exit");
    harness.trace().take_actions();
    harness.settle_requested_stops();

    let actions = harness.trace().take_actions();
    assert!(
        actions.ends_with(&[
            format!("continue {first} None"),
            format!("continue {third} None"),
        ]),
        "{actions:?}"
    );
    assert!(!actions.iter().any(|action| action.starts_with("step")));
    assert_eq!(harness.published_stops(), 0);
}

#[test]
fn removing_a_breakpoint_releases_threads_waiting_to_step_over_it() {
    let mut harness = hit_harness(2, "==5");
    let [first, second] = harness.threads[..] else {
        panic!("two threads");
    };

    harness.hit(first).expect("declined hit");
    let process = process_id(harness.threads[0]);
    harness.controller.begin_pause(process).expect("pause");
    harness.settle_requested_stops();
    harness
        .controller
        .remove_breakpoint(BreakpointId::new(1))
        .expect("remove");
    assert_eq!(harness.thread(first).stopped_at_breakpoint, None);
    harness.trace().take_actions();

    harness.resume().expect("resume");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("continue {first} None"),
            format!("continue {second} None")
        ]
    );
}

#[test]
fn amending_a_hit_condition_while_running_applies_to_the_next_hit() {
    let mut harness = hit_harness(1, "==5");
    let pid = harness.threads[0];
    harness.hit(pid).expect("declined hit");
    harness.finish_step(pid).expect("repair step");
    assert_eq!(harness.public_reason(), None);

    let amended = harness
        .controller
        .set_breakpoint_hit_condition(BreakpointId::new(1), Some(">=2".parse().expect("valid")))
        .expect("amend while running");
    assert_eq!(amended.hit_count, 1);
    harness.hit(pid).expect("stopping hit");
    assert_eq!(harness.public_reason(), Some(site_hit(2)));
}

#[test]
fn a_trap_reexecuted_after_a_signal_interrupted_its_repair_is_not_a_new_hit() {
    let mut harness = hit_harness(1, "==2");
    let pid = harness.threads[0];
    harness.hit(pid).expect("declined hit");

    // A signal arrives instead of the repair step's trace trap, before the
    // original instruction ran.
    harness.trace().set_siginfo(pid, 0, Some(1));
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGUSR1))
        .expect("signal during repair");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Exception(_))
    ));
    harness.trace().take_actions();

    harness.resume().expect("resume");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("continue {pid} Some(SIGUSR1)")]
    );

    // After its handler returns the thread executes the same trap again.
    harness.hit(pid).expect("re-executed trap");
    assert_eq!(harness.public_reason(), None, "the hit was counted twice");
    harness.finish_step(pid).expect("repair step");
    assert_eq!(harness.hit_count(), 1);
    assert_eq!(
        harness.trace().take_actions().last(),
        Some(&format!("continue {pid} None"))
    );
}

/// A thread whose step over the breakpoint it hit a pause interrupted
/// still steps over the trap there once the breakpoint is replaced by
/// another at the same place: its arrival there was counted.
#[test]
fn a_breakpoint_replaced_where_a_paused_repair_stands_is_stepped_over() {
    let mut harness = hit_harness(1, "==2");
    let pid = harness.threads[0];
    harness.hit(pid).expect("declined hit");
    harness
        .controller
        .begin_pause(process_id(pid))
        .expect("pause");

    // The pause's SIGSTOP arrives instead of the repair step's trace trap.
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));

    harness
        .edit(|reply| Edit::RemoveBreakpoint {
            id: BreakpointId::new(1),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("removed");
    harness.add_breakpoint(HIT_SITE);
    harness.trace().take_actions();

    harness.resume().expect("resume");
    assert_eq!(
        harness.trace().take_actions(),
        [format!("remove_site {HIT_SITE:#x}"), format!("step {pid}")]
    );
}

/// A thread stepping from a breakpoint it had not hit meets a signal
/// first. Back from the signal it hits the breakpoint, once, at the guard
/// where its step resumes, and steps over the trap: the step executes the
/// instruction the trap covers, not the trap again.
#[test]
fn a_breakpoint_hit_where_a_signal_interrupted_a_step_is_stepped_over() {
    let mut harness = hit_harness(1, "==2");
    let pid = harness.threads[0];
    let inferior = harness.inferior();
    inferior.active.as_mut().expect("execution").kind =
        step_execution(pid, StepKind::Instruction, StepStart::default());
    inferior.thread_mut(pid).expect("thread").expected = ExpectedStop::UserStep {
        kind: StepKind::Instruction,
    };

    harness.trace().set_pc(pid, HIT_SITE);
    let child_exited = Signal::new(libc::SIGCHLD).expect("SIGCHLD");
    deliver(&mut harness, pid, child_exited);
    assert_eq!(
        harness.trace().take_actions().last(),
        Some(&format!("continue {pid} Some({child_exited:?})"))
    );

    harness.hit(pid).expect("the trap at the guard");
    assert_eq!(harness.hit_count(), 1);
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("set_registers {pid} rip={HIT_SITE:#x}"),
            format!("remove_site {HIT_SITE:#x}"),
            format!("step {pid}"),
        ]
    );
    harness.finish_step(pid).expect("repair step");
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Step {
            kind: StepKind::Instruction
        })
    );
    assert_eq!(harness.hit_count(), 1);
}

fn address_breakpoint(address: u64) -> BreakpointSpec {
    BreakpointSpec::Address(VirtualAddress::new(address))
}

#[test]
fn a_hit_on_a_breakpoint_removed_while_running_is_dropped() {
    let mut harness = watch_harness(2);
    let breakpoint = harness.add_breakpoint(0x40);
    harness.start_continue();
    harness.trace().take_actions();
    harness.published();

    let mut removed = harness.edit(|reply| Edit::RemoveBreakpoint {
        id: breakpoint.id,
        reply,
    });

    // The trap was raised before the thread saw its stop request.
    harness
        .hit_at(Pid::from_raw(5000), 0x40)
        .expect("breakpoint trap");
    harness.settle_requested_stops();

    // The hit counted, but the breakpoint is gone before it stops anything.
    let removed = removed.try_recv().expect("reply").expect("removed");
    assert_eq!((removed.id, removed.hit_count), (breakpoint.id, 1));
    let actions = harness.trace().take_actions();
    assert_eq!(
        &actions[actions.len() - 3..],
        [
            "remove_site 0x40",
            "continue 5000 None",
            "continue 5001 None"
        ],
        "the thread resumes the restored instruction without a repair: {actions:?}"
    );
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "a removed breakpoint's hit is never published"
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.breakpoints.is_empty());
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.stopped_at_breakpoint.is_none() && thread.reason.is_none())
    );
}

#[test]
fn a_hit_on_a_breakpoint_that_survives_the_edit_is_published_with_its_owners() {
    let mut harness = watch_harness(2);
    let kept = harness.add_breakpoint(0x40);
    let other = harness.add_breakpoint(0x48);
    harness.start_continue();
    harness.published();

    let mut removed = harness.edit(|reply| Edit::RemoveBreakpoint {
        id: other.id,
        reply,
    });
    harness
        .hit_at(Pid::from_raw(5001), 0x40)
        .expect("breakpoint trap");
    harness.settle_requested_stops();

    removed.try_recv().expect("reply").expect("removed");
    let published = harness.published();
    let position = |predicate: fn(&DebuggerEvent) -> bool| {
        published
            .iter()
            .position(predicate)
            .unwrap_or_else(|| panic!("missing event in {published:?}"))
    };

    assert!(
        position(|event| matches!(event, DebuggerEvent::BreakpointsChanged { .. }))
            < position(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "the edit applies before the stop is published"
    );
    assert!(published.iter().any(|event| matches!(
        event,
        DebuggerEvent::InferiorStopped {
            execution_id: Some(execution),
            thread_id,
            reason: StopReason::Breakpoint { address, hits },
            ..
        } if *execution == ExecutionId::new(2)
            && thread_id.get() == 5001
            && *address == Some(VirtualAddress::new(0x40))
            && hits.iter().map(|hit| hit.breakpoint).eq([kept.id])
    )));
}

#[test]
fn a_signal_during_an_internal_stop_is_published_after_the_edit() {
    let mut harness = watch_harness(2);
    harness.start_continue();
    harness.published();
    let mut added = harness.edit(|reply| Edit::AddBreakpoint {
        spec: address_breakpoint(0x40),
        options: Box::default(),
        reply,
    });
    let pid = Pid::from_raw(5001);
    harness.trace().set_siginfo(pid, libc::SI_USER, Some(1));
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, Signal::SIGUSR1))
        .expect("signal stop");
    harness.settle_requested_stops();

    added.try_recv().expect("reply").expect("added");
    assert!(matches!(
        harness.public_reason(),
        Some(StopReason::Exception(info)) if info.code == Signal::SIGUSR1.code()
    ));
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&"install_site 0x40".to_owned())
    );
}

#[test]
fn another_thread_at_a_stepping_plans_site_is_stepped_over_while_the_others_are_stopped() {
    let mut harness = watch_harness(2);
    let (stepping, other) = (harness.threads[0], harness.threads[1]);
    harness.start_continue();
    {
        let inferior = harness.inferior();
        let active = inferior.active.as_mut().expect("execution");
        active.kind = step_execution(
            stepping,
            StepKind::OverSource,
            StepStart {
                plan_addresses: BTreeSet::from([VirtualAddress::new(0x40)]),
                ..StepStart::default()
            },
        );
        let execution = active.id;
        let owner = BreakpointOwner::Plan(execution);
        inferior
            .plan_sites
            .insert(execution, BTreeSet::from([VirtualAddress::new(0x40)]));
        inferior.breakpoints.insert(
            VirtualAddress::new(0x40),
            BreakpointSite {
                original_byte: 0x90,
                installed: true,
                owners: BTreeSet::from([owner]),
            },
        );
    }

    harness.published();
    harness.trace().take_actions();

    harness.hit_at(other, 0x40).expect("plan site trap");
    assert_eq!(
        harness.trace().take_actions(),
        ["set_registers 5001 rip=0x40", "request_stop 5000"],
        "the stepping thread stops before the site is lifted"
    );
    harness.settle_requested_stops();
    assert_eq!(
        harness.trace().take_actions(),
        ["remove_site 0x40", "step 5001"]
    );
    harness
        .trap(other, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("repair step");

    assert_eq!(
        harness.trace().take_actions(),
        [
            "reinstall_site 0x40",
            "continue 5000 None",
            "continue 5001 None"
        ]
    );
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "the stepping plan's site is never reported for another thread"
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.barrier.is_none() && inferior.repairs.is_empty());
    assert_eq!(
        inferior.active.as_ref().map(|active| active.id),
        Some(ExecutionId::new(2))
    );
}

/// The stepping thread traps at a site its step's plan shares with a user
/// breakpoint, which a client removes or disables while the step runs.
/// The edit's internal stop drops the hit, and the step meets its own site
/// there and ends, rather than stepping over the trap as if only the
/// breakpoint had owned it.
///
/// The simulator found this, with a source step over that ran on past the
/// line it had to stop at. It needs the edit's stop requested before the
/// trap is handled, which on a real kernel is a race between the client's
/// request and the waiter's status that no fixture can force, so this
/// drives the controller directly.
fn a_steps_site_shared_with_a_breakpoint_edited_while_running(
    edit: fn(BreakpointId, Reply<Breakpoint>) -> Edit,
) {
    const SITE: u64 = 0x40;
    let mut harness = watch_harness(2);
    let stepping = harness.threads[0];
    let breakpoint = harness.add_breakpoint(SITE);
    harness.start_continue();
    {
        let inferior = harness.inferior();
        let active = inferior.active.as_mut().expect("execution");
        let execution = active.id;
        active.kind = step_execution(
            stepping,
            StepKind::Advance,
            StepStart {
                plan_addresses: BTreeSet::from([VirtualAddress::new(SITE)]),
                targets: BTreeSet::from([VirtualAddress::new(SITE)]),
                ..StepStart::default()
            },
        );
        inferior
            .plan_sites
            .insert(execution, BTreeSet::from([VirtualAddress::new(SITE)]));
        inferior
            .breakpoints
            .get_mut(&VirtualAddress::new(SITE))
            .expect("the breakpoint's site")
            .owners
            .insert(BreakpointOwner::Plan(execution));
    }

    harness.published();
    harness.trace().take_actions();
    let mut edited = harness.edit(|reply| edit(breakpoint.id, reply));

    // The trap was raised before the thread saw its stop request.
    harness.hit_at(stepping, SITE).expect("shared site trap");
    harness.settle_requested_stops();

    edited.try_recv().expect("reply").expect("edited");
    assert_eq!(
        harness.stopped(),
        Some((
            debug_thread_id(stepping),
            StopReason::Step {
                kind: StepKind::Advance
            }
        )),
        "the step ends at its own site"
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions.contains(&format!("step {stepping}")),
        "the step stops before the instruction its site covers: {actions:?}"
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.breakpoints.is_empty(), "every trap is gone");
}

#[test]
fn a_step_ends_at_its_site_where_a_breakpoint_removed_while_running_was_hit() {
    a_steps_site_shared_with_a_breakpoint_edited_while_running(|id, reply| {
        Edit::RemoveBreakpoint { id, reply }
    });
}

#[test]
fn a_step_ends_at_its_site_where_a_breakpoint_disabled_while_running_was_hit() {
    a_steps_site_shared_with_a_breakpoint_edited_while_running(|id, reply| {
        Edit::SetBreakpointEnabled {
            id,
            enabled: false,
            reply,
        }
    });
}

/// A source step single-steps onto the start of a line, where a
/// breakpoint's `int3` lies, and resumes once to hit it, so the hit judges
/// the arrival. Before the thread runs, an edit's internal stop removes the
/// trap. The step judges its arrival there as it resumes, rather than
/// stepping on past a line it had to stop at.
///
/// The simulator found this. It needs the edit's stop to reach the thread
/// before the one instruction it was resumed to run, which on a real kernel
/// no fixture can force, so this drives the controller directly.
fn a_step_resumed_to_a_trap_edited_away_while_running(
    edit: fn(BreakpointId, Reply<Breakpoint>) -> Edit,
) {
    const SITE: u64 = 0x40;

    // Line 24 at 0x20, and line 25 from the site to 0x60.
    let mut lines = crate::image::lines::LineTables::default();
    lines.begin_sequence().expect("few rows");

    for (index, (address, line)) in [(0x20, 24), (SITE, 25), (0x60, 0)].into_iter().enumerate() {
        let end = line == 0;
        lines
            .push_row(&crate::image::lines::Row {
                address,
                file: (!end).then(|| crate::SourceFileId::new(0)),
                line,
                statement: !end,
                end_sequence: end,
                ..crate::image::lines::Row::default()
            })
            .expect("few rows");

        if !end {
            lines
                .push_range(
                    AddressRange {
                        start: ImageAddress::new(address),
                        end: ImageAddress::new(address + 0x20),
                    },
                    u32::try_from(index).expect("few rows"),
                    true,
                )
                .expect("few ranges");
        }
    }

    let image = lined_test_image(&[(0, None, None, (0, 0x100))], lines);
    let mut harness = watch_harness_of(2, &image);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let stepping = harness.threads[0];
    let breakpoint = harness.add_breakpoint(SITE);
    harness.start_continue();
    {
        let inferior = harness.inferior();
        inferior.active.as_mut().expect("execution").kind =
            step_execution(stepping, StepKind::IntoSource, StepStart::default());
        inferior.thread_mut(stepping).expect("thread").expected = ExpectedStop::UserStep {
            kind: StepKind::IntoSource,
        };
    }

    // The single step lands before the trap, and resumes to hit it.
    harness.published();
    harness.trace().set_pc(stepping, SITE);
    harness
        .trap(stepping, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("trace trap");
    assert_eq!(harness.stopped(), None, "the trap's hit judges the arrival");
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("step {stepping}"))
    );

    // The edit stops the thread before it runs the trap.
    let mut edited = harness.edit(|reply| edit(breakpoint.id, reply));
    harness.settle_requested_stops();
    edited.try_recv().expect("reply").expect("edited");

    assert_eq!(
        harness.stopped(),
        Some((
            debug_thread_id(stepping),
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        )),
        "the step ends at the line it reached"
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions.contains(&format!("step {stepping}")),
        "the step runs no further: {actions:?}"
    );
}

#[test]
fn a_step_resumed_to_hit_a_breakpoint_removed_while_running_ends_where_it_stands() {
    a_step_resumed_to_a_trap_edited_away_while_running(|id, reply| Edit::RemoveBreakpoint {
        id,
        reply,
    });
}

#[test]
fn a_step_resumed_to_hit_a_breakpoint_disabled_while_running_ends_where_it_stands() {
    a_step_resumed_to_a_trap_edited_away_while_running(|id, reply| Edit::SetBreakpointEnabled {
        id,
        enabled: false,
        reply,
    });
}

/// A runtime whose threads run the tasks a test sets, so run control's
/// following of a task is tested without any language's runtime.
#[derive(Debug, Default)]
pub(super) struct ScriptedRuntime {
    pub(super) agent: Option<Arc<dyn crate::runtime_model::RuntimeAgent>>,
    running: std::sync::Mutex<BTreeMap<crate::ThreadId, u64>>,

    /// The runtime's internal hooks.
    internal: Vec<crate::runtime_model::HookPlace>,

    /// The hook each hit was asked of, in turn.
    asked: std::sync::Mutex<Vec<crate::runtime_model::HookPlace>>,

    /// What the runtime was armed with, each time it was.
    armed: std::sync::Mutex<Vec<Vec<crate::runtime_model::ManagedArm>>>,

    /// A scripted refusal while arming, after resolution succeeded.
    arm_failure: std::sync::Mutex<Option<Arc<str>>>,
    mixed_function: bool,

    /// The step the runtime was armed with, each time it was.
    steps: std::sync::Mutex<Vec<Option<crate::runtime_model::RuntimeStep>>>,

    /// What each hit of an internal hook reports, in turn.
    events: std::sync::Mutex<
        std::collections::VecDeque<std::result::Result<crate::runtime_model::HookEvent, Arc<str>>>,
    >,

    /// Where the runtime lists its inspector's targets, which makes the
    /// controller arm it through its inspector.
    inspector_hook: Option<ImageAddress>,
}

impl ScriptedRuntime {
    /// A runtime armed through the inspector `GetTargetIds` at `hook` lists.
    pub(super) fn inspected(hook: ImageAddress) -> Self {
        Self {
            inspector_hook: Some(hook),
            ..Self::default()
        }
    }

    fn runs(&self, thread: Pid, task: u64) {
        self.running
            .lock()
            .expect("the script")
            .insert(debug_thread_id(thread), task);
    }

    /// Scripts what the next hit of an internal hook reports.
    fn reports(&self, event: std::result::Result<crate::runtime_model::HookEvent, Arc<str>>) {
        self.events.lock().expect("the script").push_back(event);
    }
}

impl crate::runtime_model::RuntimeModel for ScriptedRuntime {
    fn agent(&self) -> Option<&dyn crate::runtime_model::RuntimeAgent> {
        self.agent.as_deref()
    }

    fn tasks(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _start: u64,
        _limit: usize,
        _program_only: bool,
    ) -> crate::runtime_model::Partial<crate::runtime_model::TaskPage> {
        crate::runtime_model::Partial {
            value: crate::runtime_model::TaskPage {
                tasks: Vec::new(),
                next: None,
            },
            gaps: Vec::new(),
        }
    }

    fn thread_activity(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        thread: crate::ThreadId,
    ) -> crate::runtime_model::ThreadActivity {
        self.running
            .lock()
            .expect("the script")
            .get(&thread)
            .map_or(crate::runtime_model::ThreadActivity::Idle, |&number| {
                crate::runtime_model::ThreadActivity::Task {
                    number,
                    stack: crate::StackSegment::Task,
                }
            })
    }

    fn task_context(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _task: crate::runtime_model::TaskRef,
    ) -> std::result::Result<Option<crate::runtime_model::TaskContext>, Arc<str>> {
        Ok(None)
    }

    fn thread_stacks(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _thread: crate::ThreadId,
    ) -> std::result::Result<Vec<(std::ops::Range<u64>, crate::StackSegment)>, Arc<str>> {
        Ok(Vec::new())
    }

    fn cross(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _thread: crate::ThreadId,
        _frame: &RegisterFile,
        _after_call: bool,
    ) -> std::result::Result<crate::runtime_model::Crossing, Arc<str>> {
        Ok(crate::runtime_model::Crossing::Stay)
    }

    fn signals(&self) -> crate::runtime_model::RuntimeSignals {
        crate::runtime_model::RuntimeSignals::default()
    }

    fn hooks(&self) -> &[crate::runtime_model::RuntimeHook] {
        &[]
    }

    fn exception(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _hook: ImageAddress,
        _registers: &RegisterFile,
    ) -> std::result::Result<crate::runtime_model::RuntimeException, Arc<str>> {
        Err("the script has no exceptions".into())
    }

    fn dynamic_value(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _representation: &str,
        _value: crate::runtime_model::StoredValue<'_>,
    ) -> Option<std::result::Result<crate::runtime_model::DynamicValue, Arc<str>>> {
        None
    }

    fn stack_mover(&self) -> Option<ImageAddress> {
        None
    }

    fn moving_task(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _registers: &RegisterFile,
    ) -> std::result::Result<u64, Arc<str>> {
        Err("the script moves no stacks".into())
    }

    fn task_stack(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _task: crate::runtime_model::TaskRef,
    ) -> std::result::Result<Option<std::ops::Range<u64>>, Arc<str>> {
        Ok(None)
    }

    fn call_out(
        &self,
        _entry: ImageAddress,
        _registers: &RegisterFile,
    ) -> Option<std::result::Result<VirtualAddress, Arc<str>>> {
        None
    }

    fn task_starters(&self) -> Vec<crate::runtime_model::TaskStarter> {
        Vec::new()
    }

    fn started_task(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _starter: ImageAddress,
        _registers: &RegisterFile,
    ) -> std::result::Result<Option<crate::runtime_model::StartedTask>, Arc<str>> {
        Err("the script starts no tasks".into())
    }

    fn task_noun(&self) -> &'static str {
        "task"
    }

    fn internal_hooks(&self) -> &[crate::runtime_model::HookPlace] {
        &self.internal
    }

    fn inspector_hook(&self) -> Option<ImageAddress> {
        self.inspector_hook
    }

    /// An inspected runtime's agent serves the target the scripted
    /// inspector tests connect to.
    fn inspector_agent(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
    ) -> std::result::Result<Option<(VirtualAddress, Arc<str>)>, Arc<str>> {
        Ok(self
            .inspector_hook
            .map(|_| (VirtualAddress::new(0x7000), Arc::from("0f2b5c1e-uuid"))))
    }

    /// An inspected runtime's I/O thread takes messages at 0x60, every
    /// one the message asked about.
    fn inspector_message_received(&self) -> Option<ImageAddress> {
        self.inspector_hook.map(|_| ImageAddress::new(0x60))
    }

    fn inspector_received_matches(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        _message: VirtualAddress,
        _expected: &[u8],
    ) -> std::result::Result<bool, Arc<str>> {
        Ok(true)
    }

    fn hook_event(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        hook: crate::runtime_model::HookPlace,
        _registers: &RegisterFile,
    ) -> std::result::Result<crate::runtime_model::HookEvent, Arc<str>> {
        self.asked.lock().expect("the script").push(hook);
        self.events
            .lock()
            .expect("the script")
            .pop_front()
            .unwrap_or_else(|| Err("the script reports no more hits".into()))
    }

    /// The script runs files named `*.scripted`, and refuses `refused.scripted`.
    fn resolve_breakpoint(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        spec: &BreakpointSpec,
    ) -> Option<std::result::Result<Vec<crate::ManagedPlace>, Arc<str>>> {
        if self.mixed_function
            && matches!(spec, BreakpointSpec::Function(name) if name == "physical")
        {
            return Some(Ok(vec![crate::ManagedPlace::Function {
                name: "physical".into(),
            }]));
        }
        let BreakpointSpec::Source { path, line } = spec else {
            return None;
        };

        let name = path.to_str()?;

        if name == "refused.scripted" {
            return Some(Err("the script refuses the file".into()));
        }

        name.ends_with(".scripted").then(|| {
            Ok(vec![crate::ManagedPlace::Line {
                path: name.into(),
                line: u32::try_from(line.get()).expect("a small line"),
            }])
        })
    }

    /// Arming writes how many locations the runtime is armed with to the
    /// byte at 0x7000. The script steps by line, never by instruction.
    fn arm(
        &self,
        _stop: &dyn crate::runtime_model::RuntimeStop,
        arming: &crate::runtime_model::Arming<'_>,
    ) -> std::result::Result<Vec<crate::runtime_model::RuntimeWrite>, Arc<str>> {
        if let Some(reason) = self.arm_failure.lock().expect("the script").as_ref() {
            return Err(Arc::clone(reason));
        }

        if arming
            .step
            .is_some_and(|step| step.kind == StepKind::Instruction)
        {
            return Err("the script steps by line".into());
        }

        self.armed
            .lock()
            .expect("the script")
            .push(arming.breakpoints.to_vec());
        self.steps
            .lock()
            .expect("the script")
            .push(arming.step.cloned());
        Ok(vec![crate::runtime_model::RuntimeWrite {
            address: VirtualAddress::new(0x7000),
            expected: vec![0],
            bytes: vec![u8::try_from(arming.breakpoints.len()).expect("few arms")],
        }])
    }
}

/// A stack of one frame, whose CFA is 0x1000 wherever it is.
pub(super) struct FlatUnwindInfo;

impl UnwindInfo for FlatUnwindInfo {
    fn cfa(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        Ok(VirtualAddress::new(0x1000))
    }

    fn unwind(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        Err(UnwindTermination::Complete)
    }
}

/// A watch enabled while its task's stack moves stays set aside, holding
/// no debug registers on the stack being left, until the move returns.
#[test]
fn a_watch_enabled_while_its_stack_moves_stays_set_aside() {
    let mut harness = watch_harness(1);
    let task = crate::TaskId {
        runtime: crate::RuntimeId::new(4),
        number: 1,
    };

    let watchpoint = harness
        .add_watching_with(
            0x6000,
            8,
            WatchAccess::Write,
            WatchpointOptions {
                enabled: false,
                ..WatchpointOptions::default()
            },
        )
        .expect("add a disabled watch");
    let mover = harness.threads[0];
    let inferior = harness.inferior();
    inferior
        .watch
        .watchpoints
        .get_mut(&watchpoint.id)
        .expect("the watch is recorded")
        .task = Some(stack_watches::TaskWatch {
        task,
        below_top: 0x100,
    });
    inferior.stack_moves.begin(mover, task);

    let enabled = harness
        .controller
        .set_watchpoint_enabled(watchpoint.id, true)
        .expect("enable the watch");
    assert!(enabled.enabled);
    let inferior = harness.inferior();
    assert!(
        inferior
            .watch
            .plan
            .slots()
            .iter()
            .flatten()
            .all(|slot| !slot.owners.contains(&watchpoint.id)),
        "the watch holds slots while its stack moves"
    );
}

/// Begins a source step over a line of `task`, which `thread` runs, with
/// its plan's site at 0x40.
fn begin_task_step(harness: &mut WatchHarness, thread: Pid, task: crate::TaskId) {
    harness.start_continue();
    let inferior = harness.inferior();
    let active = inferior.active.as_mut().expect("execution");
    let mut kind = step_execution(
        thread,
        StepKind::OverSource,
        StepStart {
            plan_addresses: BTreeSet::from([VirtualAddress::new(0x40)]),
            ..StepStart::default()
        },
    );

    if let ActiveKind::Step { owner, .. } = &mut kind {
        owner.task = Some(task);
    }

    active.kind = kind;
    let execution = active.id;
    inferior
        .plan_sites
        .insert(execution, BTreeSet::from([VirtualAddress::new(0x40)]));
    inferior.breakpoints.insert(
        VirtualAddress::new(0x40),
        BreakpointSite {
            original_byte: 0x90,
            installed: true,
            owners: BTreeSet::from([BreakpointOwner::Plan(execution)]),
        },
    );
}

/// A step that belongs to a task follows it to whichever thread its
/// runtime runs it on: another task's thread at the step's site is stepped
/// over unseen, and the task's new thread takes the step on.
#[test]
fn a_step_follows_its_task_to_another_thread_and_passes_the_others() {
    let mut harness = watch_harness(3);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let (first, moved, other) = (harness.threads[0], harness.threads[1], harness.threads[2]);
    let runtime = Arc::new(ScriptedRuntime::default());
    let image = Arc::clone(&harness.controller.main.image);
    harness
        .controller
        .runtime_models
        .borrow_mut()
        .insert(image.id(), vec![Ok(Arc::clone(&runtime) as _)]);
    let task = crate::TaskId {
        runtime: crate::RuntimeId::new(harness.inferior().loaded_module.id.get() * 4),
        number: 1,
    };

    runtime.runs(first, 1);
    begin_task_step(&mut harness, first, task);
    harness.published();
    harness.trace().take_actions();

    // The runtime moves the task to another thread, and runs another task
    // where it was.
    runtime.runs(first, 2);
    runtime.runs(moved, 1);
    runtime.runs(other, 3);
    let owner = |harness: &mut WatchHarness| match &harness.inferior().active.as_ref()?.kind {
        ActiveKind::Step { owner, .. } => Some(owner.thread),
        _ => None,
    };

    harness.hit_at(other, 0x40).expect("another task's trap");
    harness.settle_requested_stops();
    assert_eq!(
        harness.trace().take_actions(),
        [
            "set_registers 5002 rip=0x40",
            "request_stop 5000",
            "request_stop 5001",
            "remove_site 0x40",
            "step 5002",
        ],
        "another task's thread is stepped over the site alone"
    );
    harness
        .trap(other, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("the repair step");
    assert_eq!(
        harness.trace().take_actions(),
        [
            "reinstall_site 0x40",
            "continue 5000 None",
            "continue 5001 None",
            "continue 5002 None",
        ]
    );
    assert_eq!(owner(&mut harness), Some(first));

    harness.hit_at(moved, 0x40).expect("the task's trap");
    assert_eq!(
        owner(&mut harness),
        Some(moved),
        "the step follows its task"
    );
    harness.settle_requested_stops();
    assert_eq!(
        harness.trace().take_actions(),
        [
            "set_registers 5001 rip=0x40",
            "request_stop 5000",
            "request_stop 5002",
            "remove_site 0x40",
            "step 5001",
        ],
        "the task's new thread steps on from the site"
    );
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "neither hit is reported"
    );
}

/// Binds `runtime` to the harness's executable.
pub(super) fn scripted_runtime(
    harness: &WatchHarness,
    runtime: ScriptedRuntime,
) -> Arc<ScriptedRuntime> {
    let runtime = Arc::new(runtime);
    let image = Arc::clone(&harness.controller.main.image);
    harness
        .controller
        .runtime_models
        .borrow_mut()
        .insert(image.id(), vec![Ok(Arc::clone(&runtime) as _)]);
    runtime
}

/// Checks that `hitting` steps over the trap it hit while its siblings
/// wait, and that every thread then continues.
fn steps_over_alone(harness: &mut WatchHarness, hitting: Pid) {
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("step {hitting}")),
        "the thread is stepped over the trap"
    );
    harness
        .trap(hitting, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("the repair step");
    let actions = harness.trace().take_actions();

    for thread in harness.threads.clone() {
        assert!(
            actions.contains(&format!("continue {thread} None")),
            "{actions:?}"
        );
    }
}

/// A runtime's internal hook is armed whatever exceptions stop, and each
/// hit asks the runtime whether its thread stops: a hit that goes on is
/// stepped over unseen, an exception stops when its filter does, and a
/// hit whose report cannot be read stops and says so.
#[test]
fn a_runtime_hook_asks_at_each_hit_whether_its_thread_stops() {
    const HOOK: u64 = 0x40;
    let mut harness = watch_harness(2);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let hitting = harness.threads[0];
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                HOOK,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    let filter = &crate::runtime_model::EXCEPTION_FILTERS[3];
    harness
        .controller
        .set_exception_stops(crate::protocol::ExceptionStops::NONE)
        .expect("no exception stops");
    harness
        .controller
        .sync_runtime_hooks()
        .expect("arm the hook");
    assert!(
        harness
            .inferior()
            .breakpoints
            .get(&VirtualAddress::new(HOOK))
            .is_some_and(|site| site.owners.contains(&BreakpointOwner::Runtime)),
        "the hook is armed with no exception stopping"
    );

    let exception = || crate::runtime_model::HookEvent::Exception {
        filter,
        report: crate::runtime_model::RuntimeException {
            message: "boom".into(),
            value: None,
        },
    };

    let hit = |harness: &mut WatchHarness| {
        harness.start_continue();
        harness.trace().take_actions();
        harness.hit_at(hitting, HOOK).expect("the hook's trap");
        harness.settle_requested_stops();
        harness.stopped()
    };

    runtime.reports(Ok(crate::runtime_model::HookEvent::Continue));
    assert_eq!(hit(&mut harness), None, "a hit that goes on is unseen");
    steps_over_alone(&mut harness, hitting);

    runtime.reports(Ok(exception()));
    assert_eq!(
        hit(&mut harness),
        None,
        "an exception that does not stop is unseen"
    );
    steps_over_alone(&mut harness, hitting);

    harness
        .controller
        .set_exception_stops(
            crate::protocol::ExceptionStops::NONE
                .with(filter.id, true)
                .expect("a filter"),
        )
        .expect("the exception stops");
    runtime.reports(Ok(exception()));
    assert_eq!(
        hit(&mut harness),
        Some((
            debug_thread_id(hitting),
            StopReason::LanguageException(crate::protocol::LanguageException {
                kind: filter.kind,
                message: "boom".into(),
                value: None,
            })
        )),
        "an exception that stops is reported"
    );

    runtime.reports(Err("the mailbox is unreadable".into()));
    let unreadable = hit(&mut harness);
    assert!(
        matches!(
            &unreadable,
            Some((_, StopReason::RuntimeEvent { description }))
                if description.contains("the mailbox is unreadable")
        ),
        "a hit whose report is unreadable stops and says why: {unreadable:?}"
    );
}

/// A runtime's internal hook may be a function a companion image exports,
/// as a library the runtime loads beside its own: it is armed once the
/// image loads, asked of by that name at each hit, and gone with the image.
#[test]
fn a_runtime_hook_in_a_companion_image_is_armed_while_it_is_loaded() {
    const HOOK: crate::runtime_model::HookPlace =
        crate::runtime_model::HookPlace::Companion("scripted_hook");
    let mut harness = watch_harness(1);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let hitting = harness.threads[0];
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![HOOK],
            ..ScriptedRuntime::default()
        },
    );
    let armed = |harness: &mut WatchHarness| {
        harness
            .inferior()
            .breakpoints
            .iter()
            .filter(|(_, site)| site.owners.contains(&BreakpointOwner::Runtime))
            .map(|(&address, _)| address.get())
            .collect::<Vec<_>>()
    };
    harness.controller.sync_runtime_hooks().expect("sync");
    assert_eq!(
        armed(&mut harness),
        [0_u64; 0],
        "no image exports the hook yet"
    );

    let library = PathBuf::from("/lib/libhook.so");
    harness
        .trace()
        .libraries
        .borrow_mut()
        .insert(library.clone(), (crate::LoadBias::new(0x7300_0000), true));
    harness.trace().library_symbols.borrow_mut().insert(
        library.clone(),
        vec![crate::SymbolInfo {
            id: crate::SymbolId::new(0),
            name: "scripted_hook".into(),
            address: ImageAddress::new(0x100),
            kind: crate::SymbolKind::Function,
            binding: crate::SymbolBinding::Global,
            exported: true,
            extent: None,
            storage: None,
            role: crate::CodeRole::Ordinary,
        }],
    );
    harness
        .controller
        .refresh_libraries()
        .expect("the library loads");
    assert_eq!(armed(&mut harness), [0x7300_0100]);

    runtime.reports(Ok(crate::runtime_model::HookEvent::Continue));
    harness.start_continue();
    harness.trace().take_actions();
    harness
        .hit_at(hitting, 0x7300_0100)
        .expect("the hook's trap");
    assert_eq!(harness.stopped(), None);
    assert_eq!(*runtime.asked.lock().expect("the script"), [HOOK]);
    steps_over_alone(&mut harness, hitting);

    harness.trace().libraries.borrow_mut().remove(&library);
    harness
        .controller
        .refresh_libraries()
        .expect("the library unloads");
    assert_eq!(
        armed(&mut harness),
        [0_u64; 0],
        "the hook goes with its image"
    );
    assert!(harness.inferior().runtime_hooks.is_empty());
}

/// A runtime resolves a breakpoint in code it runs itself to a location it
/// manages, which has no address: the runtime is armed with it as it is
/// added and removed, and a hit its hook reports by the location's number
/// stops as a breakpoint's, with no address. A number the runtime was not
/// armed with is no breakpoint's.
#[test]
fn a_runtime_resolves_arms_and_reports_a_managed_breakpoint() {
    const HOOK: u64 = 0x40;
    let mut harness = watch_harness(1);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let hitting = harness.threads[0];
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                HOOK,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    harness
        .controller
        .sync_runtime_hooks()
        .expect("arm the hook");
    let source = |path: &str| BreakpointSpec::Source {
        path: path.into(),
        line: crate::LineNumber::new(3).expect("a line"),
    };
    let add = |harness: &mut WatchHarness, spec| harness.add_spec(spec);

    let refused = add(&mut harness, source("refused.scripted"));
    assert!(
        matches!(&refused, Err(Error::RuntimeBreakpoint(reason)) if reason.contains("refuses")),
        "{refused:?}"
    );

    harness.trace().take_actions();
    let breakpoint = add(&mut harness, source("app.scripted")).expect("added");
    let place = crate::ManagedPlace::Line {
        path: "app.scripted".into(),
        line: 3,
    };
    let runtime_id = crate::RuntimeId::new(harness.inferior().loaded_module.id.get() * 4);
    assert_eq!(
        breakpoint
            .locations
            .iter()
            .map(|resolved| resolved.location.clone())
            .collect::<Vec<_>>(),
        [crate::BreakpointLocation::Managed(crate::ManagedLocation {
            runtime: runtime_id,
            place: place.clone(),
        })]
    );
    assert_eq!(
        runtime.armed.lock().expect("the script").last(),
        Some(&vec![crate::runtime_model::ManagedArm { number: 0, place }])
    );
    let pid = harness.inferior().memory_thread();
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("write_word {pid} 0x7000 0x1")),
        "arming's writes are made"
    );

    let hit = |harness: &mut WatchHarness, numbers: &[u64]| {
        runtime.reports(Ok(crate::runtime_model::HookEvent::Breakpoint {
            numbers: numbers.to_vec(),
        }));
        harness.start_continue();
        harness.trace().take_actions();
        harness.hit_at(hitting, HOOK).expect("the hook's trap");
        harness.settle_requested_stops();
        harness.stopped()
    };
    assert_eq!(hit(&mut harness, &[7]), None, "no breakpoint has number 7");
    steps_over_alone(&mut harness, hitting);
    assert_eq!(
        hit(&mut harness, &[0]),
        Some((
            debug_thread_id(hitting),
            StopReason::Breakpoint {
                address: None,
                hits: Arc::from([BreakpointHit {
                    breakpoint: breakpoint.id,
                    hit_count: 1,
                }]),
            }
        ))
    );

    harness
        .edit(|reply| Edit::RemoveBreakpoint {
            id: breakpoint.id,
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("removed");
    assert_eq!(
        runtime.armed.lock().expect("the script").last(),
        Some(&Vec::new()),
        "the runtime is armed with nothing once the breakpoint is gone"
    );
}

/// One event's hits of several managed locations stop once, for every
/// breakpoint that owns one, as at one native address; a number no location
/// has adds nothing.
#[test]
fn one_report_of_several_managed_hits_stops_once_for_them_all() {
    const HOOK: u64 = 0x40;
    let mut harness = watch_harness(1);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let hitting = harness.threads[0];
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                HOOK,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    harness
        .controller
        .sync_runtime_hooks()
        .expect("arm the hook");
    let source = |path: &str| BreakpointSpec::Source {
        path: path.into(),
        line: crate::LineNumber::new(3).expect("a line"),
    };
    let first = harness.add_spec(source("app.scripted")).expect("added");
    let second = harness.add_spec(source("other.scripted")).expect("added");

    runtime.reports(Ok(crate::runtime_model::HookEvent::Breakpoint {
        numbers: vec![1, 7, 0],
    }));
    harness.start_continue();
    harness.hit_at(hitting, HOOK).expect("the hook's trap");
    harness.settle_requested_stops();
    let mut hits = vec![
        BreakpointHit {
            breakpoint: first.id,
            hit_count: 1,
        },
        BreakpointHit {
            breakpoint: second.id,
            hit_count: 1,
        },
    ];
    hits.sort_by_key(|hit| hit.breakpoint);
    assert_eq!(
        harness.stopped(),
        Some((
            debug_thread_id(hitting),
            StopReason::Breakpoint {
                address: None,
                hits: Arc::from(hits),
            }
        ))
    );
}

/// A runtime whose arming changed has not accepted the change until it is
/// armed, as when an error ends an edit between claiming and arming.
#[test]
fn a_managed_change_not_yet_armed_is_not_shown_ready() {
    let mut harness = watch_harness(1);
    scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                0x40,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    let breakpoint = harness
        .add_spec(BreakpointSpec::Source {
            path: "pending.scripted".into(),
            line: crate::LineNumber::new(3).expect("line"),
        })
        .expect("armed");
    assert!(breakpoint.pending_locations.is_empty());
    let crate::BreakpointLocation::Managed(location) = &breakpoint.locations[0].location else {
        panic!("a managed location: {breakpoint:?}");
    };
    harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .managed
        .mark_stale(location.runtime);
    let snapshot = harness.controller.snapshot();
    assert_eq!(snapshot.breakpoints[0].pending_locations.len(), 1);
    assert_eq!(snapshot.breakpoints[0].ready_locations().count(), 0);
    let _ = harness.controller.sync_runtime_arming();
    let snapshot = harness.controller.snapshot();
    assert!(snapshot.breakpoints[0].pending_locations.is_empty());
}

/// Resolution does not prove the runtime accepted its arming. Public
/// replies and snapshots retain the location and expose the failure.
#[test]
fn managed_arming_failures_remain_visible_until_recovery() {
    let mut harness = watch_harness(1);
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                0x40,
            ))],
            arm_failure: std::sync::Mutex::new(Some("the scripted runtime is initializing".into())),
            ..ScriptedRuntime::default()
        },
    );
    let breakpoint = harness
        .add_spec(BreakpointSpec::Source {
            path: "pending.scripted".into(),
            line: crate::LineNumber::new(3).expect("line"),
        })
        .expect("resolved intent is retained");
    assert_eq!(breakpoint.locations.len(), 1);
    let expected: Arc<[crate::PendingBreakpointLocation]> =
        Arc::from([crate::PendingBreakpointLocation {
            location: breakpoint.locations[0].location.clone(),
            reason: "the scripted runtime is initializing".into(),
        }]);
    assert_eq!(breakpoint.pending_locations, expected);
    assert_eq!(breakpoint.ready_locations().count(), 0);
    let snapshot = harness.controller.snapshot();
    assert_eq!(snapshot.breakpoints[0].pending_locations, expected);
    assert!(runtime.armed.lock().expect("the script").is_empty());
    while harness.events.try_recv().is_ok() {}

    *runtime.arm_failure.lock().expect("the script") = None;
    harness.controller.rearm_reporting_runtimes();
    let snapshot = harness.controller.snapshot();
    assert!(snapshot.breakpoints[0].pending_locations.is_empty());
    assert_eq!(snapshot.breakpoints[0].ready_locations().count(), 1);
    assert_eq!(harness.breakpoint_events(), 1);
}

#[test]
fn pending_managed_locations_survive_edits_and_notify_only_when_the_reason_changes() {
    let mut harness = watch_harness(1);
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                0x40,
            ))],
            arm_failure: std::sync::Mutex::new(Some("waiting".into())),
            ..ScriptedRuntime::default()
        },
    );
    let breakpoint = harness
        .add_spec(BreakpointSpec::Source {
            path: "pending.scripted".into(),
            line: crate::LineNumber::new(3).expect("line"),
        })
        .expect("breakpoint");
    assert_eq!(
        harness
            .add_spec(breakpoint.spec.clone())
            .expect("duplicate"),
        breakpoint
    );
    let disabled = harness
        .controller
        .set_breakpoint_enabled(breakpoint.id, false)
        .expect("disable");
    assert!(disabled.pending_locations.is_empty());
    let enabled = harness
        .controller
        .set_breakpoint_enabled(breakpoint.id, true)
        .expect("enable");
    assert_eq!(enabled.pending_locations, breakpoint.pending_locations);
    let unchanged = harness
        .controller
        .set_breakpoint_enabled(breakpoint.id, true)
        .expect("already enabled");
    assert_eq!(unchanged.pending_locations, breakpoint.pending_locations);
    let edited = harness
        .controller
        .set_breakpoint_condition(breakpoint.id, None)
        .expect("edit");
    assert_eq!(edited.pending_locations, breakpoint.pending_locations);
    harness.breakpoint_events();
    harness.controller.rearm_reporting_runtimes();
    assert_eq!(
        harness.breakpoint_events(),
        0,
        "the same refusal changes nothing"
    );
    *runtime.arm_failure.lock().expect("the script") = Some("tool IDs occupied".into());
    harness.controller.rearm_reporting_runtimes();
    assert_eq!(harness.breakpoint_events(), 1);
    assert_eq!(
        &*harness.controller.snapshot().breakpoints[0].pending_locations[0].reason,
        "tool IDs occupied"
    );
}

#[test]
fn mixed_breakpoint_readiness_is_projected_in_replies_and_snapshots() {
    let mut lines = crate::image::lines::LineTables::default();
    lines.begin_sequence().expect("one sequence");
    lines
        .push_row(&crate::image::lines::Row {
            address: 0x20,
            file: Some(crate::SourceFileId::new(0)),
            line: 3,
            statement: true,
            prologue_end: true,
            ..Default::default()
        })
        .expect("one statement");
    lines
        .push_range(
            AddressRange {
                start: ImageAddress::new(0x20),
                end: ImageAddress::new(0x30),
            },
            0,
            true,
        )
        .expect("one range");
    lines
        .push_row(&crate::image::lines::Row {
            address: 0x30,
            end_sequence: true,
            ..Default::default()
        })
        .expect("end sequence");
    let image = lined_test_image(&[(0, None, None, (0, 0x100))], lines);
    let mut harness = watch_harness_of(1, &image);
    let _runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            mixed_function: true,
            arm_failure: std::sync::Mutex::new(Some("not confirmed".into())),
            ..ScriptedRuntime::default()
        },
    );
    let breakpoint = harness
        .add_spec(BreakpointSpec::Function("physical".into()))
        .expect("mixed resolution");
    assert_eq!(breakpoint.locations.len(), 2);
    assert_eq!(breakpoint.pending_locations.len(), 1);
    let ready = breakpoint.ready_locations().collect::<Vec<_>>();
    assert_eq!(ready.len(), 1);
    assert!(matches!(
        ready[0].location,
        crate::BreakpointLocation::Image(_)
    ));
    assert!(matches!(
        breakpoint.pending_locations[0].location,
        crate::BreakpointLocation::Managed(_)
    ));
    assert_eq!(harness.controller.snapshot().breakpoints[0], breakpoint);
}

#[test]
fn removed_breakpoint_definitions_have_no_pending_arming() {
    for all in [false, true] {
        let mut harness = watch_harness(1);
        let _runtime = scripted_runtime(
            &harness,
            ScriptedRuntime {
                arm_failure: std::sync::Mutex::new(Some("configuration refused".into())),
                ..ScriptedRuntime::default()
            },
        );
        let breakpoint = harness
            .add_spec(BreakpointSpec::Source {
                path: "pending.scripted".into(),
                line: crate::LineNumber::new(3).expect("line"),
            })
            .expect("breakpoint");
        assert_eq!(breakpoint.pending_locations.len(), 1);
        let removed = if all {
            harness
                .controller
                .remove_all_breakpoints()
                .expect("remove all")
                .to_vec()
        } else {
            vec![
                harness
                    .controller
                    .remove_breakpoint(breakpoint.id)
                    .expect("remove"),
            ]
        };
        assert_eq!(removed.len(), 1);
        assert!(
            removed[0].pending_locations.is_empty(),
            "removed definition: {removed:?}"
        );
        assert!(harness.controller.snapshot().breakpoints.is_empty());
    }
}

#[test]
fn a_module_refresh_publishes_new_managed_arming_state_once() {
    let mut harness = watch_harness(1);
    let breakpoint = harness
        .edit(|reply| Edit::AddBreakpoint {
            spec: BreakpointSpec::Source {
                path: "pending.scripted".into(),
                line: crate::LineNumber::new(3).expect("line"),
            },
            options: Box::new(crate::BreakpointOptions {
                pending: true,
                ..Default::default()
            }),
            reply,
        })
        .try_recv()
        .expect("reply")
        .expect("pending intent");
    assert!(breakpoint.locations.is_empty());
    harness.breakpoint_events();
    let _runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            arm_failure: std::sync::Mutex::new(Some("configuration refused".into())),
            ..ScriptedRuntime::default()
        },
    );
    harness.controller.refresh_libraries().expect("refresh");
    let snapshot = harness.controller.snapshot();
    assert_eq!(snapshot.breakpoints[0].locations.len(), 1);
    assert_eq!(snapshot.breakpoints[0].pending_locations.len(), 1);
    assert_eq!(harness.breakpoint_events(), 1);
}

/// A refusal leaves the whole desired configuration unconfirmed. The
/// earlier configuration can remain effective, so its hits still count.
#[test]
fn unconfirmed_runtime_arming_still_judges_hits_from_the_previous_configuration() {
    let mut harness = watch_harness(1);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let hitting = harness.threads[0];
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                0x40,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    harness.controller.sync_runtime_hooks().expect("hook");
    let source = |path: &str| BreakpointSpec::Source {
        path: path.into(),
        line: crate::LineNumber::new(3).expect("line"),
    };
    let first = harness.add_spec(source("first.scripted")).expect("first");
    assert!(first.pending_locations.is_empty());
    *runtime.arm_failure.lock().expect("the script") = Some("new configuration refused".into());
    let second = harness
        .add_spec(source("second.scripted"))
        .expect("second intent");
    assert_eq!(second.pending_locations.len(), 1);
    assert_eq!(
        harness.controller.snapshot().breakpoints[0]
            .pending_locations
            .len(),
        1
    );
    runtime.reports(Ok(crate::runtime_model::HookEvent::Breakpoint {
        numbers: vec![0],
    }));
    harness.start_continue();
    harness
        .hit_at(hitting, 0x40)
        .expect("previously armed hook");
    harness.settle_requested_stops();
    assert!(
        matches!(harness.stopped(), Some((_, StopReason::Breakpoint { hits, .. })) if hits[0].breakpoint == first.id)
    );

    // Site numbers survive changes to the desired list. A deleted site's
    // stale hook names no owner, never the remaining site's breakpoint.
    harness
        .controller
        .remove_breakpoint(first.id)
        .expect("remove first");
    let report = |harness: &mut WatchHarness, number| {
        runtime.reports(Ok(crate::runtime_model::HookEvent::Breakpoint {
            numbers: vec![number],
        }));
        harness
            .controller
            .runtime_hook_stop(hitting, VirtualAddress::new(0x40))
    };
    assert_eq!(report(&mut harness, 0), None);
    assert!(
        matches!(report(&mut harness, 1), Some(StopReason::Breakpoint { hits, .. }) if hits[0].breakpoint == second.id)
    );
    harness
        .controller
        .set_breakpoint_enabled(second.id, false)
        .expect("disable");
    assert_eq!(report(&mut harness, 1), None);
    harness
        .controller
        .set_breakpoint_enabled(second.id, true)
        .expect("enable with a new site number");
    assert_eq!(report(&mut harness, 1), None);
    harness
        .controller
        .remove_all_breakpoints()
        .expect("remove all");
    assert_eq!(report(&mut harness, 2), None);
}

#[test]
fn runtime_step_arming_updates_breakpoint_confirmation() {
    let mut harness = watch_harness(1);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let pid = harness.threads[0];
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                0x40,
            ))],
            arm_failure: std::sync::Mutex::new(Some("initializing".into())),
            ..ScriptedRuntime::default()
        },
    );
    harness.controller.sync_runtime_hooks().expect("hook");
    harness
        .add_spec(BreakpointSpec::Source {
            path: "pending.scripted".into(),
            line: crate::LineNumber::new(3).expect("line"),
        })
        .expect("breakpoint");
    assert_eq!(
        harness.controller.snapshot().breakpoints[0]
            .pending_locations
            .len(),
        1
    );
    let runtime_id = crate::RuntimeId::new(harness.inferior().loaded_module.id.get() * 4);
    *runtime.arm_failure.lock().expect("the script") = None;
    harness.breakpoint_events();
    harness
        .controller
        .begin_runtime_step(
            process_id(pid),
            harness.stop_id(),
            pid,
            (runtime_id, Some(VirtualAddress::new(0x9000))),
            StepKind::OverSource,
            ResumeScope::Process(process_id(pid)),
            ExceptionDisposition::Pass,
        )
        .expect("step confirms the whole arming");
    assert!(
        harness.controller.snapshot().breakpoints[0]
            .pending_locations
            .is_empty()
    );
    assert_eq!(harness.breakpoint_events(), 1);

    // Failure to remove the step also leaves the whole configuration
    // unconfirmed. Old step events are still rejected by execution id.
    *runtime.arm_failure.lock().expect("the script") = Some("cannot disarm".into());
    runtime.reports(Ok(crate::runtime_model::HookEvent::StepEnded { number: 0 }));
    harness.hit_at(pid, 0x40).expect("step ended");
    harness.settle_requested_stops();
    let events = harness.published();
    assert!(events.iter().any(|event| matches!(
        event,
        DebuggerEvent::InferiorStopped {
            reason: StopReason::Step { .. },
            ..
        }
    )));
    assert_eq!(
        &*harness.controller.snapshot().breakpoints[0].pending_locations[0].reason,
        "cannot disarm"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, DebuggerEvent::BreakpointsChanged { .. }))
            .count(),
        1
    );
    *runtime.arm_failure.lock().expect("the script") = None;
    harness.controller.rearm_reporting_runtimes();
    assert!(
        harness.controller.snapshot().breakpoints[0]
            .pending_locations
            .is_empty()
    );
    assert_eq!(harness.breakpoint_events(), 1);
}

/// A step a runtime arms runs every thread, and ends only where the
/// runtime reports its end on the step's own thread: another thread's
/// report, or a report of another step, goes on unseen. The runtime is
/// armed with the step as it begins, and with none as the stop that ends
/// it forms. A step the runtime cannot make is refused, and nothing runs.
#[test]
fn a_runtime_step_ends_where_its_runtime_reports_it_on_its_thread() {
    const HOOK: u64 = 0x40;
    let mut harness = watch_harness(2);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let (stepping, other) = (harness.threads[0], harness.threads[1]);
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                HOOK,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    harness
        .controller
        .sync_runtime_hooks()
        .expect("arm the hook");
    let runtime_id = crate::RuntimeId::new(harness.inferior().loaded_module.id.get() * 4);
    let frame = (runtime_id, VirtualAddress::new(0x9000));
    let step = |harness: &mut WatchHarness, kind| {
        let stop = harness.stop_id();
        harness.controller.begin_runtime_step(
            process_id(stepping),
            stop,
            stepping,
            (frame.0, Some(frame.1)),
            kind,
            ResumeScope::Process(process_id(stepping)),
            ExceptionDisposition::Pass,
        )
    };

    harness.trace().take_actions();
    let refused = step(&mut harness, StepKind::Instruction);
    assert!(
        matches!(&refused, Err(Error::FrameStepUnsupported(reason)) if reason.contains("by line")),
        "{refused:?}"
    );
    assert!(!harness.trace().took("continue"), "nothing runs");
    assert_eq!(
        harness.breakpoint_events(),
        0,
        "a refused step leaves readiness unchanged"
    );

    // The refused step took number 0, which no step takes again.
    step(&mut harness, StepKind::OverSource).expect("the step begins");
    assert_eq!(
        runtime.steps.lock().expect("the script").last(),
        Some(&Some(crate::runtime_model::RuntimeStep {
            number: 1,
            kind: StepKind::OverSource,
            thread: debug_thread_id(stepping),
            frame: Some(frame.1),
            returning: false,
        }))
    );
    let actions = harness.trace().take_actions();

    for thread in [stepping, other] {
        assert!(
            actions.contains(&format!("continue {thread} None")),
            "{actions:?}"
        );
    }

    let ended = |harness: &mut WatchHarness, thread, number| {
        runtime.reports(Ok(crate::runtime_model::HookEvent::StepEnded { number }));
        harness.hit_at(thread, HOOK).expect("the hook's trap");
        harness.settle_requested_stops();
        harness.stopped()
    };
    assert_eq!(ended(&mut harness, other, 1), None, "another thread's");
    steps_over_alone(&mut harness, other);
    assert_eq!(ended(&mut harness, stepping, 0), None, "another step's");
    steps_over_alone(&mut harness, stepping);
    assert_eq!(
        ended(&mut harness, stepping, 1),
        Some((
            debug_thread_id(stepping),
            StopReason::Step {
                kind: StepKind::OverSource
            }
        ))
    );
    assert_eq!(
        runtime.steps.lock().expect("the script").last(),
        Some(&None),
        "the runtime is disarmed"
    );
}

/// A step the runtime reports it could follow no further stops as an
/// incomplete step, with the runtime's reason, on the step's own thread.
#[test]
fn a_runtime_step_it_cannot_follow_stops_incomplete_with_its_reason() {
    const HOOK: u64 = 0x40;
    let mut harness = watch_harness(1);
    harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
    let stepping = harness.threads[0];
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                HOOK,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    harness
        .controller
        .sync_runtime_hooks()
        .expect("arm the hook");
    let runtime_id = crate::RuntimeId::new(harness.inferior().loaded_module.id.get() * 4);
    let stop = harness.stop_id();
    harness
        .controller
        .begin_runtime_step(
            process_id(stepping),
            stop,
            stepping,
            (runtime_id, Some(VirtualAddress::new(0x9000))),
            StepKind::Out,
            ResumeScope::Process(process_id(stepping)),
            ExceptionDisposition::Pass,
        )
        .expect("the step begins");
    runtime.reports(Ok(crate::runtime_model::HookEvent::StepIncomplete {
        number: 0,
        description: "its frame is gone".into(),
    }));
    harness.hit_at(stepping, HOOK).expect("the hook's trap");
    harness.settle_requested_stops();
    assert_eq!(
        harness.stopped(),
        Some((
            debug_thread_id(stepping),
            StopReason::StepIncomplete {
                kind: StepKind::Out,
                description: "its frame is gone".into(),
            }
        ))
    );
    assert_eq!(
        runtime.steps.lock().expect("the script").last(),
        Some(&None),
        "the runtime is disarmed"
    );
}

/// A runtime that reports events through internal hooks is armed again
/// when which exceptions stop changes, since its arming says which do, and
/// not when they stay as they were.
#[test]
fn a_runtime_is_armed_again_when_which_exceptions_stop_changes() {
    let mut harness = watch_harness(1);
    let runtime = scripted_runtime(
        &harness,
        ScriptedRuntime {
            internal: vec![crate::runtime_model::HookPlace::Own(ImageAddress::new(
                0x40,
            ))],
            ..ScriptedRuntime::default()
        },
    );
    let stops = crate::protocol::ExceptionStops::NONE;
    harness
        .controller
        .set_exception_stops(stops)
        .expect("no exception stops");
    let armings = |runtime: &ScriptedRuntime| runtime.armed.lock().expect("the script").len();
    let before = armings(&runtime);

    let filter = &crate::runtime_model::EXCEPTION_FILTERS[3];
    harness
        .controller
        .set_exception_stops(stops.with(filter.id, true).expect("a filter"))
        .expect("the exception stops");
    assert_eq!(armings(&runtime), before + 1, "armed again");

    harness
        .controller
        .set_exception_stops(stops.with(filter.id, true).expect("a filter"))
        .expect("the same stops");
    assert_eq!(armings(&runtime), before + 1, "nothing changed");
}

/// The writes a runtime model asks for are made while every thread is
/// stopped, each only where memory still holds what the model read there,
/// and all of them or none. A trap's hidden byte is what the model read.
#[test]
fn a_runtimes_writes_are_made_only_to_the_memory_it_read() {
    let mut harness = watch_harness(2);
    let pid = harness.inferior().memory_thread();
    let write = |address: u64, expected: &[u8], bytes: &[u8]| crate::runtime_model::RuntimeWrite {
        address: VirtualAddress::new(address),
        expected: expected.to_vec(),
        bytes: bytes.to_vec(),
    };
    harness.store(0x6000, 0x0807_0605_0403_0201);
    harness.store(0x6008, 0x1817_1615_1413_1211);
    harness.add_breakpoint(0x40);
    harness.trace().take_actions();

    // Memory that changed since the model read it takes none of the writes.
    let refused = harness.controller.write_for_runtime(&[
        write(0x6000, &[1, 2], &[0xaa, 0xbb]),
        write(0x6008, &[0x99], &[0xcc]),
    ]);
    assert!(
        refused
            .as_ref()
            .is_err_and(|reason| reason.contains("0x6008") && reason.contains("no longer holds")),
        "{refused:?}"
    );
    assert!(
        !harness
            .trace()
            .take_actions()
            .iter()
            .any(|action| action.starts_with("write_word")),
        "nothing is written"
    );

    harness
        .controller
        .write_for_runtime(&[
            write(0x6000, &[1, 2], &[0xaa, 0xbb]),
            write(0x6009, &[0x12], &[0xcc]),
        ])
        .expect("the writes are made");
    let actions = harness.trace().take_actions();

    for expected in [
        format!("write_word {pid} 0x6000 0x80706050403bbaa"),
        format!("write_word {pid} 0x6008 0x181716151413cc11"),
    ] {
        assert!(actions.contains(&expected), "{expected} in {actions:?}");
    }

    // The trap at 0x40 hides the byte the model reads there, which the
    // write replaces under the trap.
    let original = harness.inferior().breakpoints[&VirtualAddress::new(0x40)].original_byte;
    harness
        .controller
        .write_for_runtime(&[write(0x40, &[original], &[0x55])])
        .expect("the hidden byte is written");
    assert_eq!(
        harness.inferior().breakpoints[&VirtualAddress::new(0x40)].original_byte,
        0x55
    );

    // A running thread could change what was read.
    harness.start_continue();
    assert!(
        harness
            .controller
            .write_for_runtime(&[write(0x6000, &[0xaa], &[1])])
            .is_err()
    );
}

#[test]
fn ending_a_plan_removes_only_the_sites_it_still_owns() {
    let mut harness = watch_harness(1);
    let (ending, other) = (ExecutionId::new(7), ExecutionId::new(8));
    let user = BreakpointOwner::User(BreakpointId::new(1));
    let address = VirtualAddress::new;
    {
        let controller = &mut harness.controller;
        let ptrace = &controller.ptrace;
        let inferior = controller.inferior.as_mut().expect("inferior");

        for site in [0x20, 0x30, 0x40] {
            breakpoints::install_plan_breakpoint(ptrace, inferior, address(site), ending)
                .expect("plan site");
        }

        breakpoints::install_plan_breakpoint(ptrace, inferior, address(0x50), other)
            .expect("other plan's site");
        let pid = inferior.memory_thread();
        ptrace
            .install_breakpoint(pid, &mut inferior.breakpoints, address(0x40), user)
            .expect("user site");

        // A plan may release a site before it ends, as a finished guard does.
        breakpoints::remove_breakpoint_owner_from(
            ptrace,
            inferior,
            address(0x20),
            BreakpointOwner::Plan(ending),
        )
        .expect("released");
    }

    harness.trace().take_actions();

    harness
        .controller
        .cleanup_plan_breakpoints(ending)
        .expect("cleanup");

    assert_eq!(harness.trace().take_actions(), ["remove_site 0x30"]);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    let owners = inferior
        .breakpoints
        .iter()
        .map(|(&site, owners)| (site, owners.owners.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        owners,
        [
            (address(0x40), BTreeSet::from([user])),
            (
                address(0x50),
                BTreeSet::from([BreakpointOwner::Plan(other)])
            ),
        ]
    );
    assert_eq!(
        inferior.plan_sites.keys().copied().collect::<Vec<_>>(),
        [other]
    );
}

#[test]
fn a_hit_on_a_watchpoint_removed_while_running_is_dropped() {
    let mut harness = watch_harness(2);
    let watchpoint = harness.add(0xd000, 8).expect("armed");
    harness.start_continue();
    harness.published();

    let mut removed = harness.edit(|reply| Edit::RemoveWatchpoint {
        id: watchpoint.id,
        reply,
    });
    harness
        .watch_trap(harness.threads[0], 0b1)
        .expect("watch trap");
    harness.settle_requested_stops();

    // The hit counted before the edit applied, but it stops nothing.
    assert_eq!(
        removed.try_recv().expect("reply").expect("removed"),
        Watchpoint {
            hit_count: 1,
            ..watchpoint
        }
    );
    let published = harness.published();
    assert!(
        !published
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "a removed watchpoint's hit is never published: {published:?}"
    );
    assert!(
        published
            .iter()
            .any(|event| matches!(event, DebuggerEvent::WatchpointsChanged { .. }))
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.watch.watchpoints.is_empty() && inferior.barrier.is_none());
    assert!(
        inferior.threads.values().all(
            |thread| thread.state == NativeThreadState::Running && thread.watch_hits.is_empty()
        )
    );
}

/// Two threads hit one breakpoint, and the continue steps the first over it
/// while the second waits. A watch hit by that step on a watchpoint removed
/// meanwhile is dropped, and the second thread's hit belongs to the stop
/// already published, so nothing is published again.
#[test]
fn a_dropped_watch_hit_during_a_repair_publishes_no_earlier_stop_again() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let watchpoint = harness.add(0xd000, 8).expect("armed");
    harness.add_breakpoint(HIT_SITE);
    harness.start_continue();
    harness.hit_at(first, HIT_SITE).expect("first hit");
    harness.hit_at(second, HIT_SITE).expect("second hit");
    assert!(matches!(
        harness.stopped(),
        Some((_, StopReason::Breakpoint { .. }))
    ));
    harness.trace().take_actions();

    harness.resume().expect("resume");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("remove_site {HIT_SITE:#x}"),
            format!("step {first}")
        ]
    );

    let mut removed = harness.edit(|reply| Edit::RemoveWatchpoint {
        id: watchpoint.id,
        reply,
    });
    harness
        .trap(first, libc::TRAP_TRACE, STEPPED_SLOT_ZERO)
        .expect("repair step");
    harness.settle_requested_stops();
    removed.try_recv().expect("reply").expect("removed");

    let published = harness.published();
    assert!(
        !published
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. })),
        "only the dropped hit could stop: {published:?}"
    );
    let actions = harness.trace().take_actions();
    assert!(
        actions.ends_with(&[
            format!("remove_site {HIT_SITE:#x}"),
            format!("step {second}")
        ]),
        "the second thread steps over the trap next: {actions:?}"
    );
}

/// A thread that lost its slots to another user while running can't be
/// armed when an edit's internal stop resumes the program. It stays stopped
/// and the stop is published, as for a new thread that can't be armed;
/// the program is neither killed nor released.
#[test]
fn a_thread_an_internal_stop_cannot_arm_stops_visibly() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let kept = harness.add(0x9000, 8).expect("arm");
    let removed = harness.add(0xa000, 8).expect("arm");
    harness.start_continue();
    harness.published();
    harness.inferior().thread_mut(second).expect("thread").armed = None;
    harness.trace().refuse(second, 0, Errno::ENOSPC);
    harness.trace().take_actions();

    let mut reply = harness.edit(|reply| Edit::RemoveWatchpoint {
        id: removed.id,
        reply,
    });
    harness.settle_requested_stops();
    reply.try_recv().expect("reply").expect("removed");

    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("kill") || action.starts_with("detach")),
        "{actions:?}"
    );
    assert!(
        harness.published().iter().any(|event| matches!(
            event,
            DebuggerEvent::InferiorStopped {
                thread_id,
                reason: StopReason::WatchpointArmFailed { .. },
                ..
            } if *thread_id == debug_thread_id(second)
        )),
        "the thread that can't be armed is stopped visibly"
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.state == NativeThreadState::Stopped)
    );
    let _ = (first, kept);
}

/// The bytes of a watched word holding `value`.
fn word(value: u64) -> Arc<[u8]> {
    Arc::from(value.to_le_bytes())
}

/// DR6 after a single step whose instruction hit slot 0.
const STEPPED_SLOT_ZERO: u64 = debug_registers::STATUS_IDLE | 1 << 14 | 0b1;

#[test]
fn a_store_that_changes_nothing_resumes_only_its_own_thread() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.store(0x1_3000, 7);
    let watchpoint = harness
        .add_watching(0x1_3000, 8, WatchAccess::Change)
        .expect("arm");

    // Its slot reports stores only, as a write watchpoint's does.
    assert_eq!(harness.trace().registers_of(first)[7], 0x90001);
    harness.start_continue();
    harness.published();
    harness.trace().take_actions();

    harness.watch_trap(first, 0b1).expect("unchanged store");
    let actions = harness.trace().take_actions();
    assert_eq!(
        actions.last(),
        Some(&format!("continue {first} None")),
        "{actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("request_stop")),
        "no sibling stops for a store that changed nothing: {actions:?}"
    );
    assert_eq!(harness.published(), []);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(inferior.barrier.is_none());
    assert!(inferior.threads.values().all(|thread| {
        thread.state == NativeThreadState::Running && thread.watch_hits.is_empty()
    }));

    // A store of another value stops every thread and reports the change
    // from the value observed when execution resumed.
    harness.store(0x1_3000, 9);
    harness.watch_trap(second, 0b1).expect("changing store");
    harness.settle_requested_stops();
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Watchpoint {
            hits: Arc::from([WatchpointHit {
                watchpoint: watchpoint.id,
                thread: debug_thread_id(second),
                hit_count: 1,
                previous: Some(word(7)),
                current: Some(word(9)),
                previous_value: None,
                current_value: None,
            }]),
        })
    );
}

#[test]
fn declined_watch_hits_are_counted_and_resume_only_their_thread() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let watchpoint = harness
        .add_watching_with(
            0x1_c000,
            8,
            WatchAccess::Write,
            WatchpointOptions {
                hit_condition: Some("==3".parse().expect("hit condition")),
                ..WatchpointOptions::default()
            },
        )
        .expect("arm");
    harness.start_continue();
    harness.published();
    harness.trace().take_actions();

    for (value, pid) in [(1, first), (2, second)] {
        harness.store(0x1_c000, value);
        harness.watch_trap(pid, 0b1).expect("declined store");
        let actions = harness.trace().take_actions();
        assert_eq!(
            actions.last(),
            Some(&format!("continue {pid} None")),
            "{actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|action| action.starts_with("request_stop")),
            "a declined hit stops no sibling: {actions:?}"
        );
        assert_eq!(harness.published(), []);
    }

    // The third hit stops, reporting from the bytes the second left.
    harness.store(0x1_c000, 3);
    harness.watch_trap(first, 0b1).expect("stopping store");
    harness.settle_requested_stops();
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Watchpoint {
            hits: Arc::from([WatchpointHit {
                watchpoint: watchpoint.id,
                thread: debug_thread_id(first),
                hit_count: 3,
                previous: Some(word(2)),
                current: Some(word(3)),
                previous_value: None,
                current_value: None,
            }]),
        })
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert_eq!(
        inferior.watch.watchpoints[&watchpoint.id]
            .watchpoint
            .hit_count,
        3
    );
}

/// SIGKILL from outside can take a thread out of its stop while its
/// condition is evaluated. The condition did not fail: the thread is gone,
/// and its exit, not a condition failure, is what clients hear of.
#[test]
fn a_thread_killed_while_its_condition_is_evaluated_reports_no_condition_failure() {
    for killed_at in 0..4 {
        let mut harness = watch_harness(1);
        let pid = harness.threads[0];
        let watchpoint = harness
            .add_watching_with(
                0x1_d000,
                8,
                WatchAccess::Write,
                WatchpointOptions {
                    condition: Some(crate::Condition::parse("1 == 0").expect("condition")),
                    ..WatchpointOptions::default()
                },
            )
            .expect("arm");
        harness.start_continue();
        harness.published();
        *harness.trace().kill_point.borrow_mut() = Some((pid, "registers", killed_at));
        let _ = harness.watch_trap(pid, 0b1);
        let killed = harness.trace().kill_point.borrow().is_none();
        let published = harness.published();
        assert!(
            !published
                .iter()
                .any(|event| matches!(event, DebuggerEvent::ConditionFailed { .. })),
            "killed at registers request {killed_at}: {published:?}"
        );
        let counted = harness
            .controller
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.watch.watchpoints.get(&watchpoint.id))
            .is_some_and(|record| record.watchpoint.hit_count == 1);
        assert!(counted || killed, "the hit was judged at {killed_at}");
    }
}

#[test]
fn a_declined_change_keeps_the_bytes_a_siblings_pending_hit_reports_from() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.store(0x1_b000, 7);
    let watchpoint = harness
        .add_watching_with(
            0x1_b000,
            8,
            WatchAccess::Change,
            WatchpointOptions {
                hit_condition: Some("==1".parse().expect("hit condition")),
                ..WatchpointOptions::default()
            },
        )
        .expect("arm");
    harness.start_continue();
    harness.published();

    // The first change stops; while the sibling is being stopped, it makes
    // a second change, which its hit condition declines. Taking that as the
    // last observed value would make the first change look undone.
    harness.store(0x1_b000, 9);
    harness.watch_trap(first, 0b1).expect("stopping change");
    harness.store(0x1_b000, 10);
    harness.watch_trap(second, 0b1).expect("declined change");
    harness.settle_requested_stops();
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Watchpoint {
            hits: Arc::from([WatchpointHit {
                watchpoint: watchpoint.id,
                thread: debug_thread_id(first),
                hit_count: 1,
                previous: Some(word(7)),
                current: Some(word(10)),
                previous_value: None,
                current_value: None,
            }]),
        })
    );
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    let record = &inferior.watch.watchpoints[&watchpoint.id];
    assert_eq!(record.watchpoint.hit_count, 2);
    assert_eq!(record.observed, Some(word(10)));
}

#[test]
fn a_change_undone_before_every_thread_stopped_lets_a_stepi_finish_once() {
    let mut harness = watch_harness(2);
    let [stepping, sibling] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.store(0x1_4000, 7);
    harness
        .add_watching(0x1_4000, 8, WatchAccess::Change)
        .expect("arm");
    harness.start_continue();
    let inferior = harness.inferior();
    inferior.active.as_mut().expect("execution").kind =
        step_execution(stepping, StepKind::Instruction, StepStart::default());
    inferior.thread_mut(stepping).expect("thread").expected = ExpectedStop::UserStep {
        kind: StepKind::Instruction,
    };

    harness.published();

    // The stepped instruction changes the value, so every thread is stopped
    // to report it, but the sibling stores the old value back first.
    harness.store(0x1_4000, 9);
    harness
        .trap(stepping, libc::TRAP_TRACE, STEPPED_SLOT_ZERO)
        .expect("stepped store");
    assert_eq!(harness.public_reason(), None);
    harness.trace().take_actions();
    harness.store(0x1_4000, 7);
    harness.watch_trap(sibling, 0b1).expect("restoring store");

    // Nothing changed once every thread stopped: the step ends after the one
    // instruction it executed, without executing another.
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Step {
            kind: StepKind::Instruction
        })
    );
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.starts_with("step") || action.starts_with("continue")),
        "{actions:?}"
    );
    let stops = harness
        .published()
        .into_iter()
        .filter(|event| matches!(event, DebuggerEvent::InferiorStopped { .. }))
        .count();
    assert_eq!(stops, 1);
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.watch_hits.is_empty())
    );
    assert_eq!(inferior.threads[&sibling].reason, None);
}

#[test]
fn a_pause_survives_the_change_it_coincided_with_being_undone() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.store(0x1_9000, 7);
    harness
        .add_watching(0x1_9000, 8, WatchAccess::Change)
        .expect("arm");
    harness.start_continue();
    harness.store(0x1_9000, 9);
    harness.watch_trap(first, 0b1).expect("changing store");
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause while every thread stops");
    harness.store(0x1_9000, 7);
    harness.watch_trap(second, 0b1).expect("restoring store");

    // The change is gone, but the client still asked for a stop.
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert!(
        inferior
            .threads
            .values()
            .all(|thread| thread.reason.is_none())
    );
}

#[test]
fn a_store_reports_only_the_watchpoints_whose_access_it_matches() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.store(0x1_5000, 7);
    let change = harness
        .add_watching(0x1_5000, 8, WatchAccess::Change)
        .expect("change");
    let write = harness
        .add_watching(0x1_5000, 8, WatchAccess::Write)
        .expect("write");

    for (stored, reported) in [(7, vec![write.id]), (8, vec![change.id, write.id])] {
        harness.start_continue();
        harness.store(0x1_5000, stored);

        // Watchpoints on the same span share its slot.
        harness.watch_trap(pid, 0b1).expect("store");
        let Some(StopReason::Watchpoint { hits }) = harness.public_reason() else {
            panic!("the write watchpoint reports every store");
        };

        assert_eq!(
            hits.iter().map(|hit| hit.watchpoint).collect::<Vec<_>>(),
            reported
        );
        assert!(hits.iter().all(|hit| hit.current == Some(word(stored))));
    }
}

#[test]
fn an_unchanged_store_while_stepping_over_a_breakpoint_finishes_the_repair_and_runs_on() {
    // Linux reports the step's own trap; a hardware trap alone must not make
    // the thread step the instruction again either.
    for (code, status) in [
        (libc::TRAP_TRACE, STEPPED_SLOT_ZERO),
        (TRAP_HARDWARE_BREAKPOINT, debug_registers::STATUS_IDLE | 0b1),
    ] {
        let mut harness = repairing_harness();
        let [repairing, waiting] = harness.threads[..] else {
            unreachable!("two threads");
        };

        harness.store(0x1_6000, 7);
        harness.thread(repairing).state = NativeThreadState::Stopped;
        harness
            .add_watching(0x1_6000, 8, WatchAccess::Change)
            .expect("arm");
        harness.thread(repairing).state = NativeThreadState::Running;
        harness.trace().take_actions();
        harness.published();

        harness.trap(repairing, code, status).expect("repair step");
        let actions = harness.trace().take_actions();
        assert!(
            actions.ends_with(&[
                "reinstall_site 0x40".to_owned(),
                format!("continue {repairing} None"),
                format!("continue {waiting} None"),
            ]),
            "{code}: {actions:?}"
        );
        assert_eq!(harness.published(), []);
        let inferior = harness.controller.inferior.as_ref().expect("inferior");
        assert!(inferior.repairs.is_empty());
        assert_eq!(inferior.threads[&repairing].stopped_at_breakpoint, None);
    }
}

#[test]
fn watched_bytes_becoming_unreadable_or_readable_are_changes() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.store(0x1_8000, 7);
    let watchpoint = harness
        .add_watching(0x1_8000, 8, WatchAccess::Change)
        .expect("arm");
    for (hit_count, (readable, previous, current)) in
        (1..).zip([(false, Some(word(7)), None), (true, None, Some(word(7)))])
    {
        harness.start_continue();

        if readable {
            harness.trace().unreadable.borrow_mut().clear();
        } else {
            harness.trace().unreadable.borrow_mut().insert(0x1_8000);
        }

        harness.watch_trap(pid, 0b1).expect("store");
        assert_eq!(
            harness.public_reason(),
            Some(StopReason::Watchpoint {
                hits: Arc::from([WatchpointHit {
                    watchpoint: watchpoint.id,
                    thread: debug_thread_id(pid),
                    hit_count,
                    previous,
                    current,
                    previous_value: None,
                    current_value: None,
                }]),
            }),
            "readable: {readable}"
        );
    }
}

#[test]
fn a_failed_read_of_watched_bytes_is_an_error_rather_than_a_change() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    let is_eperm = |error: &Error| {
        matches!(
            error,
            Error::Backend(error)
                if matches!(error.downcast_ref::<LinuxError>(), Some(LinuxError::System(Errno::EPERM)))
        )
    };

    harness.store(0x1_9000, 7);

    // A thread killed out of its stop is passed over for one that can read.
    harness.trace().vanished.borrow_mut().insert(first);
    let watchpoint = harness
        .add_watching(0x1_9000, 8, WatchAccess::Change)
        .expect("arm");
    harness.trace().vanished.borrow_mut().clear();
    assert_eq!(
        harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .watch
            .watchpoints[&watchpoint.id]
            .observed,
        Some(word(7))
    );

    // Any other failure is never taken for unreadable bytes: not when a
    // watchpoint is armed, which then arms nothing,
    harness.trace().read_failure.replace(Some(Errno::EPERM));
    let registers = harness.trace().registers_of(first);
    let error = harness
        .add_watching(0x1_a000, 8, WatchAccess::Change)
        .expect_err("unread baseline");
    assert!(is_eperm(&error), "{error:?}");
    assert_eq!(harness.trace().registers_of(first), registers);

    // not when execution resumes,
    let error = harness.resume().expect_err("unread baseline");
    assert!(is_eperm(&error), "{error:?}");

    // and not when a store traps.
    harness.start_continue();
    harness.published();
    let error = harness.watch_trap(second, 0b1).expect_err("unread store");
    assert!(is_eperm(&error), "{error:?}");
    assert_eq!(harness.published(), []);
}

#[test]
fn an_unchanged_store_during_a_pause_gives_its_thread_no_reason() {
    let mut harness = watch_harness(2);
    let [first, second] = harness.threads[..] else {
        unreachable!("two threads");
    };

    harness.store(0x1_7000, 7);
    harness
        .add_watching(0x1_7000, 8, WatchAccess::Change)
        .expect("arm");
    harness.start_continue();
    harness
        .controller
        .begin_pause(process_id(first))
        .expect("pause");
    harness
        .watch_trap(second, 0b1)
        .expect("unchanged store during the pause");
    assert!(
        !harness
            .trace()
            .take_actions()
            .contains(&format!("continue {second} None")),
        "a thread stopped by the pause stays stopped"
    );
    harness.settle_requested_stops();
    assert_eq!(harness.public_reason(), Some(StopReason::Pause));
    let inferior = harness.controller.inferior.as_ref().expect("inferior");
    assert_eq!(inferior.threads[&second].reason, None);
    assert!(inferior.threads[&second].debugger_stop_pending);
}

/// Puts the harness's first thread in the middle of stepping over a user
/// breakpoint at 0x40 while its sibling waits stopped, as a continue does.
fn repairing_harness() -> WatchHarness {
    repairing_harness_of(0)
}

/// Puts one of two threads in the middle of stepping over a user breakpoint
/// at 0x40 while the other waits stopped, as a continue does.
fn repairing_harness_of(repairing: usize) -> WatchHarness {
    let mut harness = watch_harness(2);
    harness.add_breakpoint(0x40);
    harness.start_continue();
    let (repairing, waiting) = (harness.threads[repairing], harness.threads[1 - repairing]);
    let inferior = harness.inferior();
    inferior.repairs = VecDeque::from([RepairGroup {
        address: VirtualAddress::new(0x40),
        remaining: VecDeque::new(),
        current: Some(repairing),
        site_removed: true,
    }]);
    inferior
        .breakpoints
        .get_mut(&VirtualAddress::new(0x40))
        .expect("site")
        .installed = false;
    let thread = inferior.thread_mut(repairing).expect("thread");
    thread.stopped_at_breakpoint = Some(VirtualAddress::new(0x40));
    thread.expected = ExpectedStop::BreakpointRepair {
        address: VirtualAddress::new(0x40),
    };

    inferior.thread_mut(waiting).expect("thread").state = NativeThreadState::Stopped;
    harness.trace().take_actions();
    harness.published();
    harness
}

/// A main thread stepped across its exit exits alone, and Linux reports
/// that only after every other thread's. Its siblings run on in the step's
/// scope, and finishing one's repair resumes them without stepping the
/// thread that is gone.
#[test]
fn a_step_whose_main_thread_exited_alone_runs_its_siblings_on() {
    let mut harness = repairing_harness_of(1);
    let (leader, repairing) = (harness.threads[0], harness.threads[1]);
    let inferior = harness.inferior();
    let active = inferior.active.as_mut().expect("execution");
    active.kind = step_execution(leader, StepKind::IntoSource, StepStart::default());
    inferior.thread_mut(leader).expect("leader").state = NativeThreadState::Exiting;
    harness.trace().killed.borrow_mut().insert(leader);
    harness.trace().take_actions();

    harness
        .trap(repairing, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("the repair finishes");
    let actions = harness.trace().take_actions();
    assert!(
        actions.contains(&format!("continue {repairing} None")),
        "{actions:?}"
    );
    assert!(harness.controller.inferior.is_some());
}

/// A thread waiting to step over its breakpoint that is exiting meanwhile,
/// as when SIGKILL ends its process, can step over nothing: its repair is
/// skipped, and the site restored.
#[test]
fn an_exiting_thread_is_not_stepped_over_its_breakpoint() {
    let mut harness = repairing_harness_of(0);
    let (finished, exiting) = (harness.threads[0], harness.threads[1]);
    let inferior = harness.inferior();
    inferior.repairs.front_mut().expect("group").remaining = VecDeque::from([exiting]);
    let thread = inferior.thread_mut(exiting).expect("thread");
    thread.stopped_at_breakpoint = Some(VirtualAddress::new(0x40));
    thread.state = NativeThreadState::Exiting;
    harness.trace().killed.borrow_mut().insert(exiting);
    harness.trace().take_actions();

    harness
        .trap(finished, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("the first repair finishes");
    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action == &format!("step {exiting}")),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"reinstall_site 0x40".to_owned()),
        "{actions:?}"
    );
}

fn deliver(harness: &mut WatchHarness, pid: Pid, signal: Signal) {
    harness.trace().set_siginfo(pid, libc::SI_USER, Some(1));
    harness
        .controller
        .process_wait(WaitEvent::Stopped(pid, signal))
        .expect("signal stop");
}

#[test]
fn a_quiet_signal_during_a_repair_runs_its_handler_before_the_repair() {
    let mut harness = repairing_harness();
    let (repairing, waiting) = (harness.threads[0], harness.threads[1]);
    let alarm = Signal::new(libc::SIGALRM).expect("SIGALRM");

    deliver(&mut harness, repairing, alarm);

    // The site is restored while the handler runs alone, so the waiting
    // sibling cannot run past it.
    assert_eq!(
        harness.trace().take_actions(),
        [
            "reinstall_site 0x40".to_owned(),
            format!("continue {repairing} Some({alarm:?})"),
        ]
    );
    assert_eq!(
        harness.thread(repairing).awaiting_breakpoint,
        Some(VirtualAddress::new(0x40))
    );
    assert_eq!(harness.thread(waiting).state, NativeThreadState::Stopped);

    // Returning from the handler re-traps at the site, which the thread
    // now steps over before its sibling resumes.
    harness.hit_at(repairing, 0x40).expect("re-trap");
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("set_registers {repairing} rip=0x40"),
            "remove_site 0x40".to_owned(),
            format!("step {repairing}"),
        ]
    );
    harness
        .trap(repairing, libc::TRAP_TRACE, debug_registers::STATUS_IDLE)
        .expect("repair step");
    assert_eq!(
        harness.trace().take_actions(),
        [
            "reinstall_site 0x40".to_owned(),
            format!("continue {repairing} None"),
            format!("continue {waiting} None"),
        ]
    );
    assert!(
        !harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorStopped { .. }))
    );
}

#[test]
fn a_discarded_signal_during_a_repair_repeats_the_repair_step() {
    let mut harness = repairing_harness();
    let repairing = harness.threads[0];
    let alarm = Signal::new(libc::SIGALRM).expect("SIGALRM");
    harness.controller.signals.set(
        alarm,
        SignalPolicy {
            stop: false,
            print: true,
            pass: false,
        },
    );

    deliver(&mut harness, repairing, alarm);
    assert_eq!(
        harness.trace().take_actions(),
        [format!("step {repairing}")]
    );
    let published = harness.published();
    assert!(
        matches!(
            published.as_slice(),
            [DebuggerEvent::SignalReceived { thread_id, exception, .. }]
                if thread_id.get() == u64::try_from(repairing.as_raw()).expect("pid")
                    && exception.code == alarm.code()
        ),
        "{published:?}"
    );
    assert!(harness.thread(repairing).pending_signal.is_none());
}

#[test]
fn threads_ending_before_their_announcement_never_become_live() {
    let mut harness = watch_harness(2);
    let (leader, sibling) = (harness.threads[0], harness.threads[1]);
    let tgid = harness.controller.inferior.as_ref().expect("inferior").tgid;
    harness.start_continue();
    harness.trace().take_actions();
    let unannounced = Pid::from_raw(5009);

    // A sibling's exit_group ends a thread its creator has not announced.
    harness
        .event(unannounced, libc::PTRACE_EVENT_EXIT)
        .expect("an unannounced thread's exit event");
    harness
        .controller
        .process_wait(WaitEvent::Exited(unannounced, 0))
        .expect("an unannounced thread's exit");
    *harness.trace().clone.borrow_mut() = Some((unannounced, tgid));
    harness
        .event(leader, libc::PTRACE_EVENT_CLONE)
        .expect("the late announcement");
    assert!(
        !harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .threads
            .contains_key(&unannounced),
        "an ended thread became live"
    );
    assert_eq!(
        harness.trace().take_actions(),
        [
            format!("continue {unannounced} None"),
            format!("continue {leader} None"),
        ]
    );

    // A thread whose exit event the kernel released early is no failure.
    harness.trace().vanished.borrow_mut().insert(sibling);
    harness
        .event(sibling, libc::PTRACE_EVENT_EXIT)
        .expect("a released exit event");

    // The leader's exit is reported once every thread is gone, even one
    // whose own exit was never seen.
    harness.published();
    harness
        .controller
        .process_wait(WaitEvent::Exited(leader, 0))
        .expect("the leader's exit");
    assert!(harness.controller.inferior.is_none());
    assert!(harness.published().iter().any(|event| matches!(
        event,
        DebuggerEvent::InferiorExited {
            status: ExitStatus::Code(0),
            ..
        }
    )));
}

/// Call-frame information whose rules read a stack slot no read reaches,
/// as a corrupted stack's do.
struct LostUnwindInfo;

impl UnwindInfo for LostUnwindInfo {
    fn cfa(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        Err(UnwindTermination::MemoryReadFailed {
            address: VirtualAddress::new(0x7ff8),
        })
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        Err(self
            .cfa(address, registers, memory)
            .expect_err("no rule resolves"))
    }
}

/// A thread stepping over a source line whose plan site at 0x30 traps where
/// its frame can no longer be unwound.
fn lost_frame_harness() -> WatchHarness {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let site = VirtualAddress::new(0x30);
    let execution = ExecutionId::new(2);
    let controller = &mut harness.controller;
    controller.main.unwind = Arc::new(LostUnwindInfo);
    let inferior = controller.inferior.as_mut().expect("inferior");
    controller
        .ptrace
        .install_breakpoint(
            pid,
            &mut inferior.breakpoints,
            site,
            BreakpointOwner::Plan(execution),
        )
        .expect("plan site");
    inferior
        .plan_sites
        .insert(execution, BTreeSet::from([site]));
    inferior.public_stop = None;
    inferior.active = Some(ActiveExecution {
        id: execution,
        kind: step_execution(
            pid,
            StepKind::OverSource,
            StepStart {
                source: Some(SourceLocation {
                    file: crate::SourceFileId::new(0),
                    line: crate::LineNumber::new(1).expect("nonzero line"),
                    column: None,
                }),
                code_instance: Some(CodeInstanceId::new(0)),
                physical_instance: Some(CodeInstanceId::new(0)),
                activation: Some(StackView::thread(pid).activation(VirtualAddress::new(0x7000))),
                plan_addresses: BTreeSet::from([site]),
                ..StepStart::default()
            },
        ),
        scope: ResumeScope::Thread(debug_thread_id(pid)),
        resume_threads: BTreeSet::from([pid]),
    });
    let thread = harness.thread(pid);
    thread.state = NativeThreadState::Running;
    thread.reason = None;
    harness.trace().take_actions();

    harness
}

/// Call-frame information whose one rule reads the stack, as a real
/// frame's does, so unwinding fails only when that read does.
struct StackReadingUnwindInfo;

impl StackReadingUnwindInfo {
    const SLOT: VirtualAddress = VirtualAddress::new(0x7ff8);
}

impl UnwindInfo for StackReadingUnwindInfo {
    fn cfa(
        &self,
        _address: ImageAddress,
        _registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        memory
            .read_u64(Self::SLOT)
            .map(|_| VirtualAddress::new(0x8000))
            .ok_or(UnwindTermination::MemoryReadFailed {
                address: Self::SLOT,
            })
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        self.cfa(address, registers, memory)?;
        Err(UnwindTermination::Complete)
    }
}

#[test]
fn a_step_whose_thread_sigkill_ends_as_it_unwinds_publishes_no_lost_frame() {
    let mut harness = lost_frame_harness();
    let pid = harness.threads[0];

    // Something outside the debugger kills the program after the hit is
    // classified, as the step reads its frame from the stack.
    harness.controller.main.unwind = Arc::new(StackReadingUnwindInfo);
    *harness.trace().kill_at_read.borrow_mut() = Some((pid, StackReadingUnwindInfo::SLOT.get()));
    harness.trace().set_pc(pid, 0x31);
    harness.trace().set_siginfo(pid, libc::SI_KERNEL, None);
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
    );
    assert!(
        harness.trace().kill_at_read.borrow().is_none(),
        "the step read its frame"
    );
    assert_eq!(harness.public_reason(), None, "no lost frame is published");
    assert_eq!(harness.thread(pid).state, NativeThreadState::Running);
}

#[test]
fn a_step_that_loses_its_frame_stops_explicitly_and_leaves_the_program_alone() {
    let mut harness = lost_frame_harness();
    let pid = harness.threads[0];

    // The step's plan site traps, but where the step's frame went can no
    // longer be told.
    harness.hit_at(pid, 0x30).expect("the hit is handled");

    match harness.public_reason() {
        Some(StopReason::StepIncomplete { kind, description }) => {
            assert_eq!(kind, StepKind::OverSource);
            assert!(description.contains("caller"), "{description}");
        }
        other => panic!("expected an incomplete step, got {other:?}"),
    }

    let actions = harness.trace().take_actions();
    assert!(
        !actions.iter().any(|action| action.starts_with("kill")),
        "the program must not be killed: {actions:?}"
    );
    assert!(
        harness
            .controller
            .inferior
            .as_ref()
            .expect("inferior")
            .breakpoints
            .values()
            .all(|site| !site.installed),
        "the step's plan sites are removed"
    );
}

#[test]
fn an_edit_waiting_for_an_internal_stop_is_answered_when_the_process_ends_first() {
    let mut harness = watch_harness(2);
    harness.start_continue();
    harness.published();
    let mut added = harness.edit(|reply| Edit::AddBreakpoint {
        spec: address_breakpoint(0x40),
        options: Box::default(),
        reply,
    });
    assert!(added.try_recv().is_err(), "the edit waits for every thread");

    // The program exits before either thread stops for the edit.
    for pid in [harness.threads[1], harness.threads[0]] {
        harness
            .controller
            .process_wait(WaitEvent::Exited(pid, 0))
            .expect("an exit is handled");
    }

    assert!(harness.controller.inferior.is_none());
    let breakpoint = added
        .try_recv()
        .expect("the edit is answered")
        .expect("a breakpoint outlives the process it was set in");
    assert_eq!(
        harness
            .controller
            .breakpoints
            .iter()
            .map(|breakpoint| breakpoint.id)
            .collect::<Vec<_>>(),
        [breakpoint.id]
    );
}

#[test]
fn a_step_whose_thread_exits_while_an_edit_drops_the_other_reason_ends_in_its_exit() {
    let mut harness = watch_harness(2);
    let (other, stepping) = (harness.threads[0], harness.threads[1]);
    let breakpoint = harness.add_breakpoint(0x40);
    harness.start_continue();
    let inferior = harness.inferior();
    inferior.active.as_mut().expect("an execution").kind = step_execution(
        stepping,
        StepKind::OverSource,
        StepStart {
            activation: Some(StackView::thread(stepping).activation(VirtualAddress::new(0x7000))),
            ..StepStart::default()
        },
    );
    harness.published();

    // The edit stops every thread; meanwhile the other thread hits the
    // breakpoint being removed, and the stepping thread exits.
    let mut removed = harness.edit(|reply| Edit::RemoveBreakpoint {
        id: breakpoint.id,
        reply,
    });
    harness.hit_at(other, 0x40).expect("breakpoint trap");
    harness
        .controller
        .process_wait(WaitEvent::Exited(stepping, 0))
        .expect("the stepping thread's exit is handled");
    removed.try_recv().expect("reply").expect("removed");

    // The hit died with its breakpoint, but the step cannot resume without
    // its thread: it ends in that thread's exit.
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::ThreadExited {
            thread_id: debug_thread_id(stepping),
            status: ExitStatus::Code(0),
        })
    );
    assert!(
        !harness.trace().took("kill"),
        "the program must not be killed"
    );
}

/// A stack of two frames: the innermost at 0x30 with its CFA at 0x1000,
/// whose caller at 0x50 is the outermost.
struct OutermostCallerUnwindInfo;

impl UnwindInfo for OutermostCallerUnwindInfo {
    fn cfa(
        &self,
        address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        match address.get() {
            0x30 => Ok(VirtualAddress::new(0x1000)),
            0x4f => Ok(VirtualAddress::new(0x2000)),
            _ => Err(UnwindTermination::NoUnwindInfo {
                address: VirtualAddress::new(address.get()),
            }),
        }
    }

    fn unwind(
        &self,
        address: ImageAddress,
        _registers: &RegisterFile,
        _memory: &mut dyn MemoryReader,
    ) -> std::result::Result<crate::unwind::UnwindStep, UnwindTermination> {
        match address.get() {
            0x30 => Ok(crate::unwind::UnwindStep {
                registers: RegisterFile::new([
                    (crate::unwind::RETURN_ADDRESS, 0x50),
                    (crate::unwind::RSP, 0x1000),
                ]),
                cfa: VirtualAddress::new(0x1000),
                signal_frame: false,
            }),
            _ => Err(UnwindTermination::Complete),
        }
    }
}

#[test]
fn an_activation_missing_from_a_wholly_unwound_stack_has_returned() {
    let mut harness = watch_harness(1);
    harness.controller.main.unwind = Arc::new(OutermostCallerUnwindInfo);
    let mut registers = harness
        .trace()
        .registers(harness.threads[0])
        .expect("registers");
    registers.rip = 0x30;
    registers.rsp = 0x0ff8;

    // Above every frame of this stack: another stack's activation, as a
    // raw-cloned thread computes from its creator's frame pointer.
    let elsewhere = harness
        .controller
        .stack_view(harness.threads[0])
        .activation(VirtualAddress::new(0x9000));
    assert!(
        harness
            .controller
            .location_for_activation(harness.threads[0], &registers, elsewhere)
            .expect("a complete stack is evidence")
            .is_none()
    );
}

#[test]
fn an_interrupt_refused_with_eio_means_gone_only_for_a_thread_that_exited() {
    // An exited thread, as the waiter can reap one between ptrace's check
    // that it is traced and the interrupt taking its signal lock. A zombie
    // keeps its identifier, so nothing else can take it meanwhile.
    let mut exited = std::process::Command::new("true")
        .spawn()
        .expect("spawn a process");
    let pid = Pid::from_raw(i32::try_from(exited.id()).expect("pid fits i32"));
    let stat = format!("/proc/{pid}/stat");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    while !std::fs::read_to_string(&stat).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().next())
            == Some("Z")
    }) {
        assert!(std::time::Instant::now() < deadline, "{pid} did not exit");
        thread::yield_now();
    }

    let gone = interrupt_outcome(pid, Err(Errno::EIO));
    exited.wait().expect("reap the process");
    assert!(!gone.expect("an exited thread is gone"));

    // A live thread's EIO is a failure the debugger must not hide.
    assert!(interrupt_outcome(nix::unistd::gettid(), Err(Errno::EIO)).is_err());
}

/// A pause that arrives once every thread is past its exit event has no
/// thread left to stop. It is accepted, and the exit ends the execution it
/// would have interrupted, as an exit ends a continue.
#[test]
fn a_pause_after_every_thread_began_exiting_ends_with_the_exit() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.start_continue();
    harness.thread(pid).state = NativeThreadState::Exiting;

    let execution = harness
        .controller
        .begin_pause(process_id(pid))
        .expect("the pause is accepted");
    assert_eq!(execution, ExecutionId::new(2));
    harness
        .controller
        .process_wait(WaitEvent::Exited(pid, 0))
        .expect("exit");
    let ended =
        std::iter::from_fn(|| harness.events.try_recv().ok()).find_map(|event| match event {
            DebuggerEvent::InferiorExited { execution_id, .. } => Some(execution_id),
            _ => None,
        });
    assert_eq!(ended, Some(Some(execution)));
}

/// A thread can execute a trap just before another thread's stop removes
/// its site, as when a step's plan ends while siblings run through the same
/// return address. The trap's report then names a site that is gone: the
/// thread re-executes the restored instruction and runs on, and nothing is
/// published.
#[test]
fn a_trap_reported_after_its_site_was_removed_runs_on() {
    let mut harness = watch_harness(2);
    let [first, _] = harness.threads[..] else {
        panic!("two threads");
    };

    harness.start_continue();
    harness
        .inferior()
        .former_sites
        .insert(VirtualAddress::new(HIT_SITE), 0x90);
    harness.trace().take_actions();

    harness.hit(first).expect("late trap");
    let actions = harness.trace().take_actions();
    assert_eq!(
        actions,
        [
            format!("set_registers {first} rip={HIT_SITE:#x}"),
            format!("continue {first} None"),
        ],
        "{actions:?}"
    );
    assert_eq!(harness.published_stops(), 0);
}

/// A main thread that exits alone while a pause stops the others never
/// stops: Linux reports its exit only after every other thread's. Once its
/// exit event is handled, the pause publishes with the threads that remain.
#[test]
fn a_pause_completes_when_the_main_thread_exits_alone() {
    let mut harness = watch_harness(2);
    let [leader, worker] = harness.threads[..] else {
        panic!("two threads");
    };

    harness.start_continue();
    harness
        .controller
        .begin_pause(process_id(leader))
        .expect("the pause is accepted");
    harness.trace().set_siginfo(
        worker,
        libc::SI_TKILL,
        Some(i32::try_from(std::process::id()).expect("pid fits")),
    );
    harness
        .controller
        .process_wait(WaitEvent::Stopped(worker, Signal::SIGSTOP))
        .expect("the worker stops");
    assert_eq!(harness.public_reason(), None);

    // Continued from its exit event, the leader is a zombie that answers no
    // request.
    harness.trace().killed.borrow_mut().insert(leader);
    harness
        .event(leader, libc::PTRACE_EVENT_EXIT)
        .expect("the leader exits");
    let stopped = harness.stopped();
    assert_eq!(stopped, Some((debug_thread_id(worker), StopReason::Pause)));
}

/// A pause stops at once when every thread is stopped or a main thread
/// past its exit event, which Linux reports only after its siblings' exits,
/// and otherwise waits for a starting thread's first stop or an exiting
/// thread's exit, as no request stops either.
#[test]
fn a_pause_waits_only_for_threads_that_will_report() {
    use NativeThreadState::{Exiting, Starting, Stopped};
    for (leader_state, worker_state) in
        [(Exiting, Stopped), (Exiting, Starting), (Stopped, Exiting)]
    {
        let mut harness = watch_harness(2);
        let [leader, worker] = harness.threads[..] else {
            panic!("two threads");
        };

        harness.start_continue();
        harness.thread(leader).state = leader_state;
        harness.thread(worker).state = worker_state;

        if leader_state == Exiting {
            harness.trace().killed.borrow_mut().insert(leader);
        }

        harness
            .controller
            .begin_pause(process_id(leader))
            .expect("the pause is accepted");
        let presenting = match worker_state {
            Stopped => worker,
            Starting => {
                assert_eq!(harness.public_reason(), None);
                harness.trace().set_siginfo(worker, libc::SI_USER, Some(0));
                harness
                    .controller
                    .process_wait(WaitEvent::Stopped(worker, Signal::SIGSTOP))
                    .expect("the worker's first stop");
                worker
            }
            _ => {
                assert_eq!(harness.public_reason(), None);
                harness
                    .controller
                    .process_wait(WaitEvent::Exited(worker, 0))
                    .expect("the worker exits");
                leader
            }
        };

        assert_eq!(
            harness.stopped(),
            Some((debug_thread_id(presenting), StopReason::Pause)),
            "{leader_state:?} {worker_state:?}"
        );
    }
}

/// Where the debug information describes two inlined calls that overlap
/// without nesting, no frame can be presented, so source steps are refused
/// rather than guessed. An instruction step needs no frame: it executes one
/// instruction whatever is presented.
#[test]
fn instruction_steps_work_where_the_inline_frame_is_ambiguous() {
    let mut harness = watch_harness_of(
        1,
        &inline_test_image(&[
            (0, None, None, (0, 0x100)),
            (1, Some(0), Some(10), (0x10, 0x20)),
            (2, Some(0), Some(11), (0x18, 0x28)),
        ]),
    );
    let pid = harness.threads[0];
    harness.trace().set_pc(pid, 0x1a);
    let inferior = harness.inferior();
    let stop = inferior.public_stop.as_mut().expect("public stop");
    stop.presentations.insert(
        pid,
        FramePresentation {
            instruction: VirtualAddress::new(0x1a),
            frame: PresentedFrame::Ambiguous(Arc::from([
                CodeInstanceId::new(1),
                CodeInstanceId::new(2),
            ])),
            hidden_inline_frames: 0,
        },
    );
    let stop = stop.id;
    let process = process_id(inferior.tgid);
    let request_step = |harness: &mut WatchHarness, kind| {
        let (reply, result) = tokio::sync::oneshot::channel();
        harness
            .controller
            .handle_message(ControllerMessage::Request(Request::Step {
                process_id: process,
                stop_id: stop,
                context: ExecutionContext::Thread(debug_thread_id(pid)),
                frame: StackFrameId::INNERMOST,
                kind,
                call: None,
                scope: ResumeScope::Process(process),
                exception: ExceptionDisposition::Pass,
                reply,
            }));
        result.blocking_recv().expect("a reply")
    };

    assert!(matches!(
        request_step(&mut harness, StepKind::OverSource),
        Err(Error::AmbiguousInlineFrame)
    ));
    harness.trace().take_actions();
    request_step(&mut harness, StepKind::Instruction).expect("an instruction step");
    assert!(
        harness
            .trace()
            .take_actions()
            .contains(&format!("step {pid}")),
        "the thread single-steps"
    );
    harness.finish_step(pid).expect("the step completes");
    assert_eq!(
        harness.public_reason(),
        Some(StopReason::Step {
            kind: StepKind::Instruction
        })
    );
}

/// Inspection of a stop waits behind run control queued after it, which it
/// would only delay; every other message keeps its order, and inspection
/// that run control interrupted is served again after it.
#[test]
#[expect(clippy::too_many_lines, reason = "one queue, served in two rounds")]
fn inspection_of_a_stop_waits_behind_run_control_queued_after_it() {
    let harness = watch_harness(1);
    let (controller, pid) = (&harness.controller, harness.threads[0]);
    let sender = controller.message_sender.clone();
    let stop_id = StopId::new(7);
    let context = ExecutionContext::Thread(debug_thread_id(pid));
    let frame = StackFrameId::INNERMOST;
    let mut replies = Vec::new();
    let mut evaluate = |text: &str, mode| {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        replies.push(receiver);
        Request::Evaluate {
            expression: crate::Expression::parse(text).expect("an expression"),
            mode,
            limits: crate::InspectionLimits::default(),
            stop_id,
            context,
            frame,
            reply,
        }
    };

    let read = evaluate("a", crate::EvaluationMode::Read);
    let assign = evaluate("b = 1", crate::EvaluationMode::Assign);
    let (variables_reply, _variables) = tokio::sync::oneshot::channel();
    let (breakpoint_reply, _breakpoint) = tokio::sync::oneshot::channel();
    let (continue_reply, _continued) = tokio::sync::oneshot::channel();
    let (memory_reply, _memory) = tokio::sync::oneshot::channel();

    for request in [
        read,
        Request::AddBreakpoint {
            spec: BreakpointSpec::Function("f".into()),
            options: Box::default(),
            reply: breakpoint_reply,
        },
        Request::Variables {
            query: VariableQuery::All,
            limits: crate::InspectionLimits::default(),
            stop_id,
            context,
            frame,
            reply: variables_reply,
        },
        assign,
        Request::Continue {
            process_id: process_id(pid),
            stop_id,
            scope: ResumeScope::Process(process_id(pid)),
            exception: ExceptionDisposition::Pass,
            reply: continue_reply,
        },
        Request::ReadMemory {
            process_id: process_id(pid),
            stop_id,
            address: VirtualAddress::new(0x1000),
            byte_count: 8,
            reply: memory_reply,
        },
    ] {
        sender
            .try_send(ControllerMessage::Request(request))
            .unwrap_or_else(|_| panic!("the queue has room"));
    }

    let served = std::iter::from_fn(|| controller.next_message(false))
        .map(|message| match message {
            ControllerMessage::Request(request) => request.describe(),
            ControllerMessage::Wait(_) => "wait".to_owned(),
            ControllerMessage::AgentTimeout => "agent timeout".to_owned(),
            ControllerMessage::Inspector(_) => "inspector".to_owned(),
        })
        .collect::<Vec<_>>();
    let served = served.iter().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(
        served,
        [
            "add breakpoint Function(\"f\")",
            "evaluate `b = 1`",
            "continue StopId(7) Process(ProcessId(5000)) Pass",
            "evaluate `a`",
            "variables",
            "read memory",
        ]
    );

    // Run control waiting interrupts inspection, which is served again
    // after it.
    assert!(!controller.run_control_waiting());
    let (pause_reply, _paused) = tokio::sync::oneshot::channel();
    sender
        .try_send(ControllerMessage::Request(Request::Pause {
            process_id: process_id(pid),
            reply: pause_reply,
        }))
        .unwrap_or_else(|_| panic!("the queue has room"));
    assert!(controller.run_control_waiting());
    let mut evaluate = |text: &str| {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        replies.push(receiver);
        Request::Evaluate {
            expression: crate::Expression::parse(text).expect("an expression"),
            mode: crate::EvaluationMode::Read,
            limits: crate::InspectionLimits::default(),
            stop_id,
            context,
            frame,
            reply,
        }
    };

    controller.serve_later(evaluate("interrupted"));
    let served = std::iter::from_fn(|| controller.next_message(false))
        .map(|message| match message {
            ControllerMessage::Request(request) => request.describe(),
            ControllerMessage::Wait(_) => "wait".to_owned(),
            ControllerMessage::AgentTimeout => "agent timeout".to_owned(),
            ControllerMessage::Inspector(_) => "inspector".to_owned(),
        })
        .collect::<Vec<_>>();
    assert_eq!(served, ["pause 5000", "evaluate `interrupted`"]);

    // Serving decides only the order; nothing was answered.
    assert!(replies.iter_mut().all(|reply| reply.try_recv().is_err()));
}

/// A presentation looks for waiting run control after every
/// [`INTERRUPT_INTERVAL`](super::evaluation::INTERRUPT_INTERVAL) units of
/// its frame's work, however that work is split among the views that
/// present values one inside another, each with a machine of its own.
#[test]
fn nested_presentations_share_one_interval_between_looks_for_run_control() {
    use super::evaluation::{INTERRUPT_INTERVAL, StopMachine};
    use super::frames::{FrameRegisters, FrameScope, ResolvedFrame};
    use crate::eval::target::Machine as _;

    let harness = watch_harness(1);
    let controller = &harness.controller;
    let pid = harness.threads[0];
    let inferior = controller.inferior.as_ref().expect("inferior");
    let resolved = ResolvedFrame {
        id: StackFrameId::INNERMOST,
        presented: PresentedFrame::Physical,
        frame: None,
        code: None,
        scope: FrameScope::Unavailable,
        registers: FrameRegisters::Thread(controller.ptrace.registers(pid).expect("registers")),
        cfa: Err(crate::VariableUnavailableReason::EvaluationLimit.into()),
        activation: 0,
        below_stack_pointer: None,
    };

    let root = super::frames::StackRoot::of_thread(pid);
    let frame = controller.frame_for(inferior, StopId::new(1), &root, &resolved);
    let (pause_reply, _paused) = tokio::sync::oneshot::channel();
    controller
        .message_sender
        .try_send(ControllerMessage::Request(Request::Pause {
            process_id: process_id(inferior.tgid),
            reply: pause_reply,
        }))
        .unwrap_or_else(|_| panic!("the queue has room"));
    let mut budget = InspectionBudget::new(crate::InspectionLimits::default());

    // Two views, each doing just over half an interval's work, as a page of
    // small presented values does.
    let mut charges = 0;
    let interrupted = (0..2).any(|_| {
        let mut machine = StopMachine::new(&frame, &mut budget, true);
        (0..=INTERRUPT_INTERVAL / 2).any(|_| {
            charges += 1;
            matches!(
                machine.charge(),
                Err(crate::eval::target::Stop::Failed(Error::Interrupted))
            )
        })
    });
    assert!(interrupted, "the work never looked for run control");
    assert_eq!(charges, INTERRUPT_INTERVAL);
}

/// glibc describes its signal trampoline, `__restore_rt`, with call-frame
/// expressions that find every register in the `ucontext` the kernel
/// saved. Evaluating them must agree with the kernel's `sigcontext`
/// layout, which unwinding through a trampoline reads directly.
#[test]
fn glibc_signal_trampoline_expressions_find_the_kernels_saved_registers() {
    use super::frames::{SIGCONTEXT_REGISTERS, UCONTEXT_MCONTEXT};

    struct Saved(BTreeMap<u64, u64>);

    impl MemoryReader for Saved {
        fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
            self.0.get(&address.get()).copied()
        }
    }

    let libraries = std::env::var_os("GLIBC_LIBRARIES").expect("the flake names glibc");
    let debug = crate::debug_info::load_module(
        &std::path::Path::new(&libraries).join("libc.so.6"),
        crate::ModuleImageId::new(1),
        &crate::debug_info::DebugFileSearch::default(),
    )
    .expect("load libc");
    let trampoline = debug
        .image
        .symbols_named("__restore_rt")
        .next()
        .expect("glibc's signal trampoline")
        .address();

    // A handler's frame returned into the trampoline, whose stack holds the
    // ucontext; each saved register gets a value of its own.
    let stack = 0x7fff_0000_u64;
    let expected: Vec<_> = SIGCONTEXT_REGISTERS
        .iter()
        .enumerate()
        .map(|(slot, &register)| (register, 0x1000 + slot as u64 * 0x11))
        .collect();
    let mut memory = Saved(
        expected
            .iter()
            .enumerate()
            .map(|(slot, &(_, value))| (stack + UCONTEXT_MCONTEXT + 8 * slot as u64, value))
            .collect(),
    );
    let registers = RegisterFile::new([
        (crate::unwind::RSP, stack),
        (crate::unwind::RETURN_ADDRESS, trampoline.get()),
    ]);
    let step = debug
        .unwind
        .unwind(trampoline, &registers, &mut memory)
        .expect("glibc's CFI unwinds its trampoline");
    assert!(
        step.signal_frame,
        "the trampoline's CIE marks a signal frame"
    );

    for (register, value) in expected {
        assert_eq!(
            step.registers.get(register),
            Some(value),
            "DWARF register {register}"
        );
    }
}

/// The modules a stop observes for the first time load as one batch and
/// are numbered and registered in the order they were observed, whatever
/// order their loads finish in; one that does not load keeps its number
/// and leaves its frames unnamed, and a later stop loads only what is new,
/// trying again what did not load.
#[test]
fn new_modules_load_together_and_register_in_observation_order() {
    let mut harness = watch_harness(1);

    for (path, bias, loads) in [
        ("/lib/libc.so", 0x7000_0000, true),
        ("/lib/liba.so", 0x7100_0000, true),
        ("/lib/libb.so", 0x7200_0000, false),
    ] {
        harness
            .trace()
            .libraries
            .borrow_mut()
            .insert(PathBuf::from(path), (crate::LoadBias::new(bias), loads));
    }

    harness.trace().take_actions();
    let libraries = |harness: &WatchHarness| {
        harness
            .controller
            .modules
            .values()
            .filter(|module| module.loaded.id != crate::ModuleId::new(0))
            .map(|module| {
                (
                    module.loaded.id.get(),
                    module.image.path().display().to_string(),
                    module.image.id().get(),
                )
            })
            .collect::<Vec<_>>()
    };

    let loaded = |harness: &mut WatchHarness| {
        harness
            .published()
            .into_iter()
            .filter_map(|event| match event {
                DebuggerEvent::ModuleLoaded { module, .. } => {
                    Some((module.module.id.get(), module.path.display().to_string()))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    harness.controller.refresh_modules().unwrap();
    let loads = harness
        .trace()
        .take_actions()
        .into_iter()
        .filter(|action| action.starts_with("load_modules"))
        .collect::<Vec<_>>();
    assert_eq!(
        loads,
        ["load_modules /lib/liba.so#1 /lib/libb.so#2 /lib/libc.so#3"]
    );
    assert_eq!(
        libraries(&harness),
        [(1, "/lib/liba.so".into(), 1), (3, "/lib/libc.so".into(), 3)]
    );
    assert_eq!(
        loaded(&mut harness),
        [(1, "/lib/liba.so".into()), (3, "/lib/libc.so".into())]
    );

    harness.trace().libraries.borrow_mut().insert(
        PathBuf::from("/lib/libd.so"),
        (crate::LoadBias::new(0x7300_0000), true),
    );
    harness.controller.refresh_modules().unwrap();
    let loads = harness
        .trace()
        .take_actions()
        .into_iter()
        .filter(|action| action.starts_with("load_modules"))
        .collect::<Vec<_>>();
    assert_eq!(loads, ["load_modules /lib/libb.so#4 /lib/libd.so#5"]);
    assert_eq!(loaded(&mut harness), [(5, "/lib/libd.so".into())]);
}

/// Stepping knows the role of code in any loaded module, not only in the
/// program's own: a source step leaves a library's runtime machinery for
/// its caller as it leaves the program's.
#[test]
fn a_step_leaves_runtime_machinery_in_a_library() {
    let mut harness = watch_harness(1);
    let library = PathBuf::from("/lib/libruntime.so");
    harness
        .trace()
        .libraries
        .borrow_mut()
        .insert(library.clone(), (crate::LoadBias::new(0x7300_0000), true));
    let symbol = |id, name: &str, start, role| crate::SymbolInfo {
        id: crate::SymbolId::new(id),
        name: name.into(),
        address: ImageAddress::new(start),
        kind: crate::SymbolKind::Function,
        binding: crate::SymbolBinding::Global,
        exported: true,
        extent: Some(crate::SymbolExtent {
            range: AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(start + 0x100),
            },
            provenance: crate::SymbolExtentProvenance::Declared,
        }),
        storage: None,
        role,
    };
    harness.trace().library_symbols.borrow_mut().insert(
        library,
        vec![
            symbol(0, "machinery", 0x100, crate::CodeRole::RuntimeInternal),
            symbol(1, "ordinary", 0x200, crate::CodeRole::Ordinary),
        ],
    );
    harness.controller.refresh_modules().unwrap();

    let pid = harness.threads[0];
    let at = |harness: &WatchHarness, address: u64| {
        harness.trace().set_pc(pid, address);
        harness
            .controller
            .stopped_where_step_leaves(pid)
            .expect("the thread's registers")
    };
    assert!(at(&harness, 0x7300_0150), "in the library's machinery");
    assert!(!at(&harness, 0x7300_0250), "in the library's ordinary code");
    assert_eq!(
        harness
            .controller
            .code_role(VirtualAddress::new(0x7300_0180)),
        Some(crate::CodeRole::RuntimeInternal)
    );
    assert_eq!(
        harness
            .controller
            .code_role(VirtualAddress::new(0x7400_0000)),
        None,
        "no module holds the address"
    );
}

#[test]
#[cfg(debug_assertions)]
fn recorded_identity_batches_preserve_nested_and_unknown_thread_mappings() {
    let host = Pid::from_raw(5000);
    let collision = Pid::from_raw(7);
    let target = crate::runtime_model::TargetThreadId::new(7).unwrap();
    let edge = super::recorded::Recorded(FakeTrace {
        target_ids: BTreeMap::from([(host, Some(target)), (collision, None)]),
        ..FakeTrace::default()
    });
    assert_eq!(
        edge.target_thread_ids(host, &[collision, host]),
        [(host, target)]
    );
    assert_eq!(edge.target_thread_id(host, collision), None);
}

impl WatchHarness {
    /// Makes the session a member of a group that follows the children its
    /// process forks, the next of which is `child`.
    fn follow_forks(&mut self, child: Pid) -> Arc<super::follow::Following> {
        let following = Arc::new(super::follow::Following::new(super::follow::Policy {
            policy: crate::FollowPolicy::default(),
            launched: None,
        }));
        self.controller.join_group(super::follow::Membership {
            following: Arc::clone(&following),
            root: true,
            parent: None,
            settings_taken: 0,
        });
        self.trace().clone.replace(Some((child, child)));
        following
    }
}

#[test]
fn a_followed_fork_child_is_scrubbed_and_handed_over_still_traced_whichever_stop_comes_first() {
    for child_first in [false, true] {
        let mut harness = hit_harness(1, ">=1");
        let parent = harness.threads[0];
        let child = Pid::from_raw(6000);
        let following = harness.follow_forks(child);
        let fork = WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_FORK);
        let first_stop = WaitEvent::Stopped(child, Signal::SIGSTOP);
        let order = if child_first {
            [first_stop, fork]
        } else {
            [fork, first_stop]
        };

        for status in order {
            assert!(harness.controller.handle_wait(status));
        }

        // The child keeps its tracer: nothing stops, detaches, or resumes
        // it, so it runs no instruction before its own controller does.
        let actions = harness.trace().take_actions();
        let touched = actions
            .iter()
            .filter(|action| action.contains(&child.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            touched,
            [&format!("write_word {child} {HIT_SITE:#x} 0x90")],
            "child first: {child_first}: {actions:?}"
        );
        assert_eq!(following.take_forks(), [(child, parent, false)]);
        assert_eq!(harness.thread(parent).state, NativeThreadState::Running);
    }
}

#[test]
fn a_handed_over_child_is_not_taken_for_a_lost_one_as_its_parent_exits() {
    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    let child = Pid::from_raw(6000);
    let following = harness.follow_forks(child);
    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_FORK),
        WaitEvent::Stopped(child, Signal::SIGSTOP),
    ] {
        assert!(harness.controller.handle_wait(status));
    }
    assert_eq!(following.take_forks().len(), 1);
    harness.trace().take_actions();

    // The tracer thread still traces the child, as its new owner's, when
    // the parent exits: the parent's session must not wait for it.
    harness.trace().traced_children.borrow_mut().push(child);
    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Exited(parent, 0),
    ] {
        harness.controller.handle_wait(status);
    }

    let actions = harness.trace().take_actions();
    assert!(
        !actions
            .iter()
            .any(|action| action.contains(&child.to_string())),
        "{actions:?}"
    );
    assert!(harness.controller.inferior.is_none());
    assert!(harness.controller.orphans.is_none());
}
