mod support;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use microscope::{
    Backtrace, CoreDumpInfo, CoreDumpOptions, CoreModuleState, Debugger, Error,
    ExceptionDisposition, InferiorState, MemoryReadCompletion, ModuleIdentity, ProcessId,
    RegisterRole, ResumeScope, ScalarValue, StepKind, StopId, StopReason, ThreadState,
    UnwindTermination, Variable, VariableState, VariableUnavailableReason, VariableValue,
    VariableValueSource, VirtualAddress,
};
use object::read::elf::{FileHeader as _, ProgramHeader as _};
use object::{Endianness, Object as _, elf};

use support::{Scenario, ScratchDir, frame_modules, position_of, register_u64};

const MATRIX: [&str; 3] = ["gcc-o0", "clang-o2", "gcc-o2-nopie"];
const WORKERS: u64 = 3;

fn core(name: &str) -> PathBuf {
    Scenario::fixture(name)
}

fn open_core(name: &str) -> Scenario {
    Scenario::open_core(name, &CoreDumpOptions::new(core(name)))
}

fn options(core_name: &str, executable: Option<&str>, allow: bool) -> CoreDumpOptions {
    CoreDumpOptions {
        executable: executable.map(Scenario::fixture),
        allow_module_mismatch: allow,
        ..CoreDumpOptions::new(core(core_name))
    }
}

fn crash_source_line(needle: &str) -> u64 {
    support::source_line("tests/fixtures/c/crash/main.c", needle)
}

fn available(variable: &Variable) -> &VariableValue {
    match &variable.state {
        VariableState::Available { value, .. } => value,
        state => panic!("{} was not available: {state:?}", variable.name),
    }
}

fn signed(variable: &Variable) -> i128 {
    match available(variable) {
        VariableValue::Scalar(ScalarValue::Signed(value)) => *value,
        value => panic!("{} was not a signed scalar: {value:?}", variable.name),
    }
}

fn binary64(variable: &Variable) -> f64 {
    match available(variable) {
        VariableValue::Scalar(ScalarValue::Floating(microscope::FloatValue::Binary64(bits))) => {
            f64::from_bits(*bits)
        }
        value => panic!("{} was not a binary64 scalar: {value:?}", variable.name),
    }
}

async fn variable(scenario: &Scenario, name: &str) -> Variable {
    scenario
        .operation(name, scenario.handle().variable(name))
        .await
}

async fn named_frames(scenario: &Scenario) -> (Backtrace, Vec<(String, Option<String>)>) {
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let names = frame_modules(&trace, &modules);
    (trace, names)
}

fn core_info(scenario: &Scenario) -> CoreDumpInfo {
    scenario
        .handle()
        .core_dump()
        .expect("a core session describes its dump")
        .as_ref()
        .clone()
}

fn loaded_identity<'a>(info: &'a CoreDumpInfo, file_name: &str) -> &'a ModuleIdentity {
    info.modules
        .iter()
        .find_map(|module| match &module.state {
            CoreModuleState::Loaded { module, identity }
                if module
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(file_name)) =>
            {
                Some(identity)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{file_name} was not loaded: {info:#?}"))
}

struct StoppedCore {
    process: ProcessId,
    stop: StopId,
    thread: microscope::ThreadId,
    reason: StopReason,
}

async fn stopped(scenario: &mut Scenario) -> (StoppedCore, microscope::StateSnapshot) {
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped {
        process_id,
        stop_id,
        thread_id,
        reason,
    } = snapshot.inferior.clone()
    else {
        panic!("a core is always stopped: {snapshot:?}");
    };

    assert_eq!(snapshot.stop_id, Some(stop_id));
    (
        StoppedCore {
            process: process_id,
            stop: stop_id,
            thread: thread_id,
            reason,
        },
        snapshot,
    )
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one opened core is checked through every inspection boundary in order"
)]
async fn segv_cores_present_the_faulting_frame_across_the_compiler_matrix() {
    let fault_line = crash_source_line("*record->poison = depth;");
    for variant in MATRIX {
        let fixture = format!("crash-{variant}");
        let mut scenario = open_core(&format!("{fixture}-segv.core"));

        let (stop, snapshot) = stopped(&mut scenario).await;
        let StopReason::CoreDump {
            exception: Some(exception),
        } = &stop.reason
        else {
            panic!("{variant}: unexpected stop reason {:?}", stop.reason);
        };

        assert_eq!(exception.code, 11, "{variant}");
        assert!(
            exception
                .description
                .contains("SIGSEGV (SEGV_MAPERR) at 0x0"),
            "{variant}: {exception:?}"
        );
        assert_eq!(
            snapshot
                .selected
                .and_then(microscope::ExecutionContext::as_thread),
            Some(stop.thread)
        );
        assert_eq!(
            snapshot.threads.len(),
            1 + usize::try_from(WORKERS).unwrap()
        );
        assert!(snapshot.threads.iter().all(|thread| {
            matches!(
                &thread.state,
                ThreadState::Stopped { reason } if (thread.id == stop.thread) == reason.is_some()
            )
        }));

        let info = core_info(&scenario);
        assert_eq!(info.process_id, stop.process);
        assert_eq!(info.exception.as_ref(), Some(exception));

        // pr_fname keeps a bounded prefix of the executable name; gdb fills
        // all 16 bytes while the kernel reserves one for a terminator.
        assert!(
            fixture.starts_with(info.process_name.as_ref())
                && info.process_name.len() >= fixture.len().min(15),
            "{variant}: {:?}",
            info.process_name
        );

        // pr_psargs keeps a bounded prefix of the command line.
        let command_line = format!("{} segv", Scenario::fixture(&fixture).display());
        assert!(
            !info.arguments.is_empty() && command_line.starts_with(info.arguments.as_ref()),
            "{info:?}"
        );

        let CoreModuleState::Loaded { module, .. } = &info.modules[0].state else {
            panic!("{variant}: the executable was not loaded: {info:?}");
        };

        assert_eq!(
            module.path.as_ref(),
            &Scenario::fixture(&fixture).canonicalize().unwrap()
        );

        // Build-ids are only linked into the gcc -O0 variant; the others are
        // proven by their saved read-only bytes. Every library has a build-id.
        let executable_identity = loaded_identity(&info, &fixture);
        if variant == "gcc-o0" {
            assert_eq!(executable_identity, &ModuleIdentity::BuildId);
        } else {
            assert!(
                matches!(executable_identity, ModuleIdentity::SavedContent { compared_bytes } if *compared_bytes > 0),
                "{variant}: {executable_identity:?}"
            );
        }

        for library in ["libc.so", "libcrash.so", "ld-linux"] {
            assert_eq!(
                loaded_identity(&info, library),
                &ModuleIdentity::BuildId,
                "{variant}: {library}"
            );
        }

        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("crash_segv"),
            "{variant}"
        );
        assert_eq!(
            location
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(fault_line),
            "{variant}"
        );
        let registers = scenario
            .operation("registers", scenario.handle().registers())
            .await;
        assert_eq!(
            register_u64(&registers, RegisterRole::ProgramCounter),
            location.address.get()
        );

        let (trace, names) = named_frames(&scenario).await;
        assert_eq!(names[0], (fixture.clone(), Some("crash_segv".to_owned())));
        assert_eq!(names[1], (fixture.clone(), Some("main".to_owned())));
        assert!(names[2].0.starts_with("libc.so"), "{variant}: {names:#?}");
        assert_eq!(trace.termination, UnwindTermination::Complete, "{variant}");

        assert_eq!(signed(&variable(&scenario, "depth").await), 3, "{variant}");

        // At -O2 `scale` lives in xmm0, read from the dump's NT_FPREGSET.
        assert!(
            (binary64(&variable(&scenario, "scale").await) - 2.5).abs() < f64::EPSILON,
            "{variant}"
        );
        assert_eq!(signed(&variable(&scenario, "crash_counter").await), 8);
        assert_eq!(
            signed(&variable(&scenario, "crash_library_value").await),
            321
        );
        assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
        assert_eq!(signed(&variable(&scenario, "crash_library_tls").await), 654);

        let record = scenario
            .operation(
                "record id",
                scenario
                    .handle()
                    .inspect(&microscope::Expression::parse("(*record).id").unwrap()),
            )
            .await;
        if variant == "gcc-o2-nopie" {
            // GCC describes the dead pointer only by the value its caller
            // passed, and the call describes none.
            assert!(
                matches!(
                    &record.state,
                    VariableState::Unavailable(VariableUnavailableReason::EntryValue(
                        microscope::EntryValueUnavailableReason::NoParameter
                    ))
                ),
                "{variant}: {record:?}"
            );
        } else {
            assert!(
                matches!(
                    &record.state,
                    VariableState::Available {
                        value: VariableValue::Scalar(ScalarValue::Signed(42)),
                        ..
                    }
                ),
                "{variant}: {record:?}"
            );
        }

        // .rodata is never saved by the dump; a verified file supplies it.
        let message = scenario
            .operation(
                "crash_message address",
                scenario.handle().runtime_address("crash_message"),
            )
            .await;
        let read = scenario
            .operation("read message", scenario.handle().read_memory(message, 29))
            .await;
        assert_eq!(read.completion, MemoryReadCompletion::Complete);
        assert_eq!(read.bytes.as_ref(), b"post-mortem read-only message");
        assert_eq!(read.stop_id, stop.stop);
        let unmapped = scenario
            .operation(
                "read null",
                scenario.handle().read_memory(VirtualAddress::new(0), 8),
            )
            .await;
        assert!(unmapped.bytes.is_empty());
        assert!(matches!(
            unmapped.completion,
            MemoryReadCompletion::Incomplete { next_address, .. } if next_address == VirtualAddress::new(0)
        ));

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn abort_cores_unwind_from_libc_into_the_aborting_caller() {
    for variant in MATRIX {
        let fixture = format!("crash-{variant}");
        let mut scenario = open_core(&format!("{fixture}-abort.core"));
        let (stop, _) = stopped(&mut scenario).await;
        let StopReason::CoreDump {
            exception: Some(exception),
        } = &stop.reason
        else {
            panic!("{variant}: unexpected stop reason {:?}", stop.reason);
        };

        assert_eq!(exception.code, 6);

        // abort() signals its own thread, so the sender is the process itself.
        assert!(
            exception.description.contains(&format!(
                "SIGABRT (SI_TKILL) sent by process {}",
                stop.process
            )),
            "{variant}: {exception:?}"
        );

        let (trace, names) = named_frames(&scenario).await;
        let caller = position_of(&names, &fixture, "crash_abort");
        assert!(caller > 0, "{variant}: {names:#?}");
        assert!(
            names[..caller]
                .iter()
                .all(|(module, _)| module.starts_with("libc.so")),
            "{variant}: abort's frames must belong to libc: {names:#?}"
        );
        assert_eq!(
            names[caller + 1],
            (fixture.clone(), Some("main".to_owned()))
        );
        assert_eq!(trace.termination, UnwindTermination::Complete);

        // The innermost frame is libc's: no main-image locals are in scope.
        assert!(matches!(
            scenario.handle().variables().await,
            Err(Error::VariableContextUnsupported)
        ));
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn every_dumped_thread_keeps_its_own_registers_stack_and_tls() {
    let mut scenario = open_core("crash-gcc-o0-segv.core");
    let (stop, snapshot) = stopped(&mut scenario).await;
    let spin_line = crash_source_line("while (atomic_load(&release) == 0) {");
    let mut stack_pointers = vec![register_u64(
        &scenario
            .operation("registers", scenario.handle().registers())
            .await,
        RegisterRole::StackPointer,
    )];
    let mut workers = Vec::new();

    for thread in snapshot
        .threads
        .iter()
        .filter(|thread| thread.id != stop.thread)
    {
        scenario
            .operation("select", scenario.handle().select_context(thread.id))
            .await;

        let selected = scenario.snapshot().await;
        assert_eq!(
            selected
                .selected
                .and_then(microscope::ExecutionContext::as_thread),
            Some(thread.id)
        );
        assert_eq!(
            selected.stop_id,
            Some(stop.stop),
            "selection never changes the stop"
        );

        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(
            location
                .image
                .function
                .as_ref()
                .map(|function| function.name.as_ref()),
            Some("worker_spin")
        );
        assert_eq!(
            location
                .image
                .source
                .as_ref()
                .map(|source| source.line.get()),
            Some(spin_line)
        );

        let worker = signed(&variable(&scenario, "local_id").await);
        assert_eq!(
            signed(&variable(&scenario, "squared").await),
            worker * worker
        );
        assert_eq!(
            signed(&variable(&scenario, "crash_tls").await),
            100 + worker
        );
        assert_eq!(
            signed(&variable(&scenario, "crash_library_tls").await),
            654 + worker
        );
        workers.push(worker);
        stack_pointers.push(register_u64(
            &scenario
                .operation("registers", scenario.handle().registers())
                .await,
            RegisterRole::StackPointer,
        ));

        let (trace, names) = named_frames(&scenario).await;
        assert_eq!(names[0].1.as_deref(), Some("worker_spin"));
        assert_eq!(names[1].1.as_deref(), Some("worker_main"));
        assert_eq!(trace.termination, UnwindTermination::Complete, "{names:#?}");
    }

    workers.sort_unstable();
    assert_eq!(workers, [1, 2, 3]);
    stack_pointers.sort_unstable();
    stack_pointers.dedup();
    assert_eq!(stack_pointers.len(), 4, "each thread has its own stack");

    // Returning to the faulting thread restores its own frame and values.
    scenario
        .operation("select", scenario.handle().select_context(stop.thread))
        .await;
    assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
    assert!(matches!(
        scenario
            .handle()
            .select_context(microscope::ThreadId::new(1))
            .await,
        Err(Error::UnknownThread(thread)) if thread.get() == 1
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn post_mortem_targets_reject_execution_modification_and_breakpoints() {
    let mut scenario = open_core("crash-gcc-o0-segv.core");
    let (stop, before) = stopped(&mut scenario).await;
    let handle = scenario.handle().clone();
    let pc = handle.current_location().await.unwrap().address;
    let original = handle.read_word(pc).await.unwrap();
    assert_ne!(original, 0, "the rejected write stores zero");

    let rejected = [
        ("launch", handle.launch().await.map(drop)),
        ("run", handle.run().await.map(drop)),
        ("continue", handle.resume().await.map(drop)),
        (
            "continue thread",
            handle
                .continue_execution(
                    stop.stop,
                    ResumeScope::Thread(stop.thread),
                    ExceptionDisposition::Suppress,
                )
                .await
                .map(drop),
        ),
        ("pause", handle.pause().await.map(drop)),
        ("step", handle.step(StepKind::Instruction).await.map(drop)),
        ("next", handle.step(StepKind::OverSource).await.map(drop)),
        ("finish", handle.step(StepKind::Out).await.map(drop)),
        (
            "attach",
            handle.attach_process(stop.process).await.map(drop),
        ),
        ("write", handle.write_word(pc, 0).await),
        (
            "break",
            handle
                .add_breakpoint(microscope::BreakpointSpec::Function("main".to_owned()))
                .await
                .map(drop),
        ),
        (
            "delete",
            handle
                .remove_breakpoint(microscope::BreakpointId::new(1))
                .await
                .map(drop),
        ),
        (
            "delete all",
            handle.remove_all_breakpoints().await.map(drop),
        ),
        (
            "watch condition",
            handle
                .set_watchpoint_condition(microscope::WatchpointId::new(1), None)
                .await
                .map(drop),
        ),
        (
            "watch hit condition",
            handle
                .set_watchpoint_hit_condition(microscope::WatchpointId::new(1), None)
                .await
                .map(drop),
        ),
    ];

    for (operation, result) in rejected {
        assert!(
            matches!(result, Err(Error::PostMortemTarget)),
            "{operation} must be rejected: {result:?}"
        );
    }

    // Nothing changed: the same stop, revision, and memory remain inspectable.
    let (after_stop, after) = stopped(&mut scenario).await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after_stop.stop, stop.stop);
    assert!(after.breakpoints.is_empty());
    assert_eq!(handle.read_word(pc).await.unwrap(), original);

    scenario.drain_pending_events();
    assert_eq!(
        scenario.last_revision(),
        0,
        "a core session publishes no events"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn module_mismatches_are_fatal_unless_explicitly_allowed() {
    // A rebuild with the same layout differs only in content and build-id.
    let rebuilt = Debugger::open_core(&options(
        "crash-gcc-o0-segv.core",
        Some("crash-gcc-o0-rebuilt"),
        false,
    ));
    assert!(
        matches!(&rebuilt, Err(Error::CoreModuleMismatch { path, detail })
            if path.ends_with("crash-gcc-o0-rebuilt") && detail.contains("build-id")),
        "{:?}",
        rebuilt.err()
    );

    // Without a build-id, saved read-only bytes expose a different program.
    for (core_name, executable) in [
        ("crash-clang-o2-segv.core", "crash-gcc-o2-nopie"),
        ("crash-gcc-o0-segv.core", "crash-clang-o2"),
        ("crash-clang-o2-segv.core", "fatal-signal"),
    ] {
        let result = Debugger::open_core(&options(core_name, Some(executable), false));
        assert!(
            matches!(&result, Err(Error::CoreModuleMismatch { path, .. }) if path.ends_with(executable)),
            "{core_name} with {executable}: {:?}",
            result.err()
        );
    }

    // Allowing the mismatch uses the file's metadata but never its contents:
    // the dump did not save .rodata, which only a verified file may supply.
    let verified = open_core("crash-gcc-o0-segv.core");
    let message = verified
        .operation(
            "message",
            verified.handle().runtime_address("crash_message"),
        )
        .await;
    let verified_message = verified
        .operation("read message", verified.handle().read_memory(message, 16))
        .await;
    assert_eq!(verified_message.completion, MemoryReadCompletion::Complete);
    verified.shutdown().await;

    let allowed = Scenario::open_core(
        "allowed mismatch",
        &options("crash-gcc-o0-segv.core", Some("crash-gcc-o0-rebuilt"), true),
    );
    let info = core_info(&allowed);
    assert!(matches!(
        &info.modules[0].state,
        CoreModuleState::Loaded { identity: ModuleIdentity::Mismatched { detail }, .. }
            if detail.contains("build-id")
    ));
    assert_eq!(
        allowed
            .operation("message", allowed.handle().runtime_address("crash_message"))
            .await,
        message,
        "the rebuilt file has the same layout"
    );

    let unbacked = allowed
        .operation("read message", allowed.handle().read_memory(message, 16))
        .await;
    assert!(unbacked.bytes.is_empty(), "{unbacked:?}");
    assert!(matches!(
        unbacked.completion,
        MemoryReadCompletion::Incomplete { .. }
    ));

    // Libraries that do match remain verified and keep their file contents.
    assert_eq!(loaded_identity(&info, "libc.so"), &ModuleIdentity::BuildId);
    allowed.shutdown().await;
}

#[tokio::test]
async fn unverifiable_modules_require_permission_and_expose_only_registers() {
    let refused = Debugger::open_core(&options("crash-gcc-o0-memoryless.core", None, false));
    assert!(
        matches!(&refused, Err(Error::CoreModuleUnverified { path }) if path.ends_with("crash-gcc-o0")),
        "{:?}",
        refused.err()
    );

    let mut scenario = Scenario::open_core(
        "memoryless",
        &options("crash-gcc-o0-memoryless.core", None, true),
    );
    let info = core_info(&scenario);
    assert!(file_images(&info).count() >= 4, "{info:#?}");
    assert!(file_images(&info).all(|module| matches!(
        &module.state,
        CoreModuleState::Loaded {
            identity: ModuleIdentity::Unverified,
            ..
        }
    )));

    // Dumps save the vDSO whatever their filter, and it needs no file.
    assert_eq!(
        loaded_identity(&info, "[vdso]"),
        &ModuleIdentity::DumpedMemory
    );

    let (_, snapshot) = stopped(&mut scenario).await;
    assert_eq!(snapshot.threads.len(), 4);

    // Registers survive in notes; every byte of memory is unavailable.
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(
        location
            .image
            .function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("crash_segv")
    );

    let (trace, _) = named_frames(&scenario).await;
    assert_eq!(trace.frames.len(), 1);
    assert!(
        matches!(
            trace.termination,
            UnwindTermination::MemoryReadFailed { .. }
        ),
        "{trace:?}"
    );

    let depth = variable(&scenario, "depth").await;
    assert!(
        matches!(
            depth.state,
            VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible { .. })
        ),
        "{depth:?}"
    );

    let text = scenario
        .operation(
            "read text",
            scenario.handle().read_memory(location.address, 1),
        )
        .await;
    assert!(matches!(
        text.completion,
        MemoryReadCompletion::Incomplete { .. }
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn header_pages_omitted_from_a_dump_are_recovered_from_verified_files() {
    let scenario = open_core("crash-gcc-o0-headerless.core");
    let info = core_info(&scenario);

    for module in ["crash-gcc-o0", "libc.so", "libcrash.so"] {
        assert!(
            matches!(loaded_identity(&info, module), ModuleIdentity::SavedContent { compared_bytes } if *compared_bytes > 0),
            "{module}: {info:#?}"
        );
    }

    let (_, names) = named_frames(&scenario).await;
    assert_eq!(names[1].1.as_deref(), Some("main"));
    assert!(names[2].0.starts_with("libc.so"), "{names:#?}");
    assert_eq!(
        signed(&variable(&scenario, "crash_library_value").await),
        321
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn missing_files_are_reported_and_leave_their_images_unavailable() {
    let scenario = open_core("core-missing-library/crash.core");
    let info = core_info(&scenario);
    let missing = info
        .modules
        .iter()
        .find(|module| {
            module
                .recorded_path
                .ends_with("core-missing-library/libcrash.so")
        })
        .unwrap_or_else(|| panic!("the deleted library is not reported: {info:#?}"));
    assert_eq!(missing.state, CoreModuleState::Missing);
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert!(
        modules
            .modules
            .iter()
            .all(|module| !module.path.ends_with("libcrash.so"))
    );
    assert!(matches!(
        scenario.handle().variable("crash_library_value").await,
        Err(Error::VariableNotFound(_))
    ));

    // The executable's own state is unaffected.
    assert_eq!(signed(&variable(&scenario, "depth").await), 3);
    assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
    scenario.shutdown().await;

    let unavailable = Debugger::open_core(&CoreDumpOptions::new(core(
        "core-missing-executable/crash.core",
    )));
    assert!(
        matches!(&unavailable, Err(Error::CoreExecutableUnavailable(message)) if message.contains("supply the executable")),
        "{:?}",
        unavailable.err()
    );
    let explicit = Scenario::open_core(
        "explicit executable",
        &options(
            "core-missing-executable/crash.core",
            Some("crash-gcc-o0"),
            false,
        ),
    );
    assert_eq!(
        loaded_identity(&core_info(&explicit), "crash-gcc-o0"),
        &ModuleIdentity::BuildId
    );
    assert_eq!(signed(&variable(&explicit, "depth").await), 3);
    explicit.shutdown().await;
}

/// Writes an edited copy of a fixture, named `name`, which lives until the
/// returned directory is dropped.
fn edited(
    test: &str,
    fixture: &str,
    name: &str,
    edit: impl FnOnce(&mut Vec<u8>),
) -> (ScratchDir, PathBuf) {
    let mut bytes = fs::read(Scenario::fixture(fixture)).expect("read fixture");
    edit(&mut bytes);
    let directory = ScratchDir::new(&format!("post-mortem-{test}"));
    let path = directory.path().join(name);
    fs::write(&path, bytes).expect("write edited fixture");
    (directory, path)
}

/// Offsets of fields in an ELF64 program header.
const PHDR_OFFSET: usize = 8;
const PHDR_VADDR: usize = 16;
const PHDR_FILESZ: usize = 32;
const PHDR_MEMSZ: usize = 40;

/// Returns the file offset of each `PT_LOAD` header and the address range it maps.
fn load_headers(bytes: &[u8]) -> Vec<(usize, u64, u64)> {
    let header = elf::FileHeader64::<Endianness>::parse(bytes).expect("core header");
    let endian = header.endian().expect("core endianness");
    let table = usize::try_from(header.e_phoff.get(endian)).unwrap();
    let size = usize::from(header.e_phentsize.get(endian));
    header
        .program_headers(endian, bytes)
        .expect("program headers")
        .iter()
        .enumerate()
        .filter(|(_, program)| program.p_type(endian) == elf::PT_LOAD)
        .map(|(index, program)| {
            (
                table + index * size,
                program.p_vaddr(endian),
                program.p_vaddr(endian) + program.p_memsz(endian),
            )
        })
        .collect()
}

/// Returns the file range of the core's note segment.
fn note_segment(bytes: &[u8]) -> (usize, usize) {
    let header = elf::FileHeader64::<Endianness>::parse(bytes).expect("core header");
    let endian = header.endian().expect("core endianness");
    header
        .program_headers(endian, bytes)
        .expect("program headers")
        .iter()
        .find(|program| program.p_type(endian) == elf::PT_NOTE)
        .map(|program| {
            (
                usize::try_from(program.p_offset(endian)).unwrap(),
                usize::try_from(program.p_filesz(endian)).unwrap(),
            )
        })
        .expect("a note segment")
}

/// The offset of the general registers in `NT_PRSTATUS`.
const PRSTATUS_REGISTERS: usize = 112;

/// The file offset of the faulting thread's `NT_PRSTATUS` descriptor, the
/// first.
fn first_prstatus(bytes: &[u8]) -> usize {
    let (start, size) = note_segment(bytes);
    let word = |offset: usize| {
        usize::try_from(u32::from_le_bytes(
            bytes[offset..offset + 4].try_into().unwrap(),
        ))
        .unwrap()
    };

    let mut note = start;

    while note < start + size {
        let (name, descriptor, kind) = (word(note), word(note + 4), word(note + 8));
        let descriptor_start = note + 12 + name.next_multiple_of(4);
        if kind == 1 && &bytes[note + 12..note + 16] == b"CORE" {
            return descriptor_start;
        }

        note = descriptor_start + descriptor.next_multiple_of(4);
    }

    panic!("the core has no NT_PRSTATUS note");
}

/// Rewrites one program header, given the file offset of that header.
type SegmentEdit = fn(&mut Vec<u8>, usize);

#[tokio::test]
async fn unsaved_and_truncated_memory_stays_unavailable_rather_than_guessed() {
    // Locate the faulting thread's stack segment in an intact dump.
    let reference = open_core("crash-gcc-o0-segv.core");
    let stack = register_u64(
        &reference
            .operation("registers", reference.handle().registers())
            .await,
        RegisterRole::StackPointer,
    );
    reference.shutdown().await;

    let edits: [(&str, SegmentEdit); 2] = [
        // The kernel's form for pages it chose not to save.
        ("unsaved", |bytes, header| {
            bytes[header + PHDR_FILESZ..header + PHDR_FILESZ + 8]
                .copy_from_slice(&0_u64.to_le_bytes());
        }),
        // A dump truncated before the segment's data.
        ("truncated", |bytes, header| {
            let past_end = u64::try_from(bytes.len()).unwrap() + 4096;
            bytes[header + PHDR_OFFSET..header + PHDR_OFFSET + 8]
                .copy_from_slice(&past_end.to_le_bytes());
        }),
    ];
    for (name, edit) in edits {
        let (_scratch, path) = edited(name, "crash-gcc-o0-segv.core", "edited.core", |bytes| {
            let (header, _, _) = load_headers(bytes)
                .into_iter()
                .find(|&(_, start, end)| (start..end).contains(&stack))
                .expect("a load segment holds the stack");
            edit(bytes, header);
        });
        let scenario = Scenario::open_core(name, &CoreDumpOptions::new(path));
        let registers = scenario
            .operation("registers", scenario.handle().registers())
            .await;
        assert_eq!(register_u64(&registers, RegisterRole::StackPointer), stack);

        let (trace, _) = named_frames(&scenario).await;
        assert_eq!(trace.frames.len(), 1, "{name}: {trace:?}");
        assert!(
            matches!(
                trace.termination,
                UnwindTermination::MemoryReadFailed { .. }
            ),
            "{name}"
        );

        let depth = variable(&scenario, "depth").await;
        assert!(
            matches!(
                depth.state,
                VariableState::Unavailable(VariableUnavailableReason::MemoryInaccessible { .. })
            ),
            "{name}: {depth:?}"
        );

        // Memory outside the edited segment is unaffected.
        assert_eq!(
            signed(&variable(&scenario, "crash_counter").await),
            8,
            "{name}"
        );
        scenario.shutdown().await;
    }
}

/// A stack the dump's producer saved but never wrote is missing, not
/// empty. gcore does this to a thread whose stack holds one of glibc's
/// guard regions, which it cannot read: it writes nothing more of the
/// mapping and leaves a hole in the core file, which reads as zeros. Those
/// zeros would end the walk at the first frame as if it had no caller.
#[tokio::test]
async fn a_stack_the_dump_never_wrote_is_missing_rather_than_empty() {
    use std::io::{Seek as _, SeekFrom, Write as _};
    use std::os::fd::AsFd as _;

    let reference = open_core("crash-gcc-o0-segv.core");
    let stack = register_u64(
        &reference
            .operation("registers", reference.handle().registers())
            .await,
        RegisterRole::StackPointer,
    );
    let (intact, _) = named_frames(&reference).await;
    assert!(intact.frames.len() > 1, "{intact:?}");
    reference.shutdown().await;

    // The stack's segment, written with a hole over every whole block of
    // it, as gcore leaves one.
    let bytes = fs::read(core("crash-gcc-o0-segv.core")).expect("read fixture");
    let header = elf::FileHeader64::<Endianness>::parse(bytes.as_slice()).expect("core header");
    let endian = header.endian().expect("core endianness");
    let segment = header
        .program_headers(endian, bytes.as_slice())
        .expect("program headers")
        .iter()
        .find(|program| {
            program.p_type(endian) == elf::PT_LOAD
                && (program.p_vaddr(endian)..program.p_vaddr(endian) + program.p_memsz(endian))
                    .contains(&stack)
        })
        .expect("a load segment holds the stack");
    let start = segment.p_offset(endian).next_multiple_of(4096);
    let end = (segment.p_offset(endian) + segment.p_filesz(endian)) / 4096 * 4096;
    let at_stack = segment.p_offset(endian) + (stack - segment.p_vaddr(endian));
    assert!((start..end).contains(&at_stack), "{start:#x}..{end:#x}");

    let directory = ScratchDir::new("post-mortem-unwritten-stack");
    let path = directory.path().join("unwritten.core");
    let mut file = fs::File::create(&path).expect("create the core");
    let (start, end) = (
        usize::try_from(start).unwrap(),
        usize::try_from(end).unwrap(),
    );
    file.write_all(&bytes[..start]).expect("write the core");
    file.seek(SeekFrom::Start(end as u64))
        .expect("seek past the hole");
    file.write_all(&bytes[end..]).expect("write the core");
    drop(file);
    let file = fs::File::open(&path).expect("open the core");
    let hole = nix::unistd::lseek(
        file.as_fd(),
        i64::try_from(at_stack).unwrap(),
        nix::unistd::Whence::SeekHole,
    )
    .expect("the file system says where holes are");
    assert_eq!(
        u64::try_from(hole).unwrap(),
        at_stack,
        "the scratch file system keeps no hole at the stack"
    );

    let scenario = Scenario::open_core("unwritten stack", &CoreDumpOptions::new(path));
    let (trace, _) = named_frames(&scenario).await;
    assert_eq!(trace.frames.len(), 1, "{trace:?}");
    assert_eq!(
        trace.termination,
        UnwindTermination::StackNotSaved {
            address: VirtualAddress::new(stack)
        }
    );

    // Memory outside the hole is as the program held it.
    assert_eq!(signed(&variable(&scenario, "crash_counter").await), 8);
    scenario.shutdown().await;
}

/// Reads `length` bytes and returns them only when the read completed.
async fn complete_read(
    scenario: &Scenario,
    address: VirtualAddress,
    length: u64,
) -> Option<Vec<u8>> {
    let read = scenario
        .operation("read", scenario.handle().read_memory(address, length))
        .await;
    if read.completion == MemoryReadCompletion::Complete {
        Some(read.bytes.to_vec())
    } else {
        assert!(read.bytes.is_empty(), "{read:?}");
        None
    }
}

#[tokio::test]
async fn modified_file_pages_missing_from_a_dump_are_never_read_from_the_file() {
    let message_bytes = b"post-mortem read-only message\0";

    // Each core is a separate randomized run with its own load addresses.
    let addresses = async |scenario: &Scenario| {
        let handle = scenario.handle();
        (
            scenario
                .operation("counter", handle.runtime_address("crash_counter"))
                .await,
            scenario
                .operation("message", handle.runtime_address("crash_message"))
                .await,
        )
    };

    let reference = open_core("crash-gcc-o0-segv.core");
    let (counter, message) = addresses(&reference).await;
    assert_eq!(
        complete_read(&reference, counter, 4).await,
        Some(8_i32.to_le_bytes().to_vec()),
        "the dump saved the incremented counter"
    );
    reference.shutdown().await;

    // A dump that saved only header pages omits the modified .data page too.
    // The file still holds the counter's initial value, which is not the
    // process's memory.
    let headers_only = open_core("crash-gcc-o0-headers-only.core");
    assert_eq!(
        loaded_identity(&core_info(&headers_only), "crash-gcc-o0"),
        &ModuleIdentity::BuildId
    );

    let (headers_counter, headers_message) = addresses(&headers_only).await;
    assert_eq!(
        complete_read(&headers_only, headers_message, message_bytes.len() as u64).await,
        Some(message_bytes.to_vec()),
        "read-only pages are recovered from the verified file"
    );
    assert_eq!(complete_read(&headers_only, headers_counter, 4).await, None);
    headers_only.shutdown().await;

    // A dump cut short inside the saved .data segment loses the counter. Its
    // header records that the producer saved those bytes, so the file cannot
    // stand in for them. gcore writes notes last, so the segment's data is
    // moved to end at the counter instead of truncating the file itself.
    let (_scratch, path) = edited(
        "truncated-data",
        "crash-gcc-o0-segv.core",
        "edited.core",
        |bytes| {
            let (header, start, _) = load_headers(bytes)
                .into_iter()
                .find(|&(_, start, end)| (start..end).contains(&counter.get()))
                .expect("a load segment holds the counter");
            let cut = u64::try_from(bytes.len()).unwrap() - (counter.get() - start);
            bytes[header + PHDR_OFFSET..header + PHDR_OFFSET + 8]
                .copy_from_slice(&cut.to_le_bytes());
        },
    );

    let truncated = Scenario::open_core("truncated data", &CoreDumpOptions::new(path));
    assert_eq!(
        loaded_identity(&core_info(&truncated), "crash-gcc-o0"),
        &ModuleIdentity::BuildId
    );
    assert_eq!(
        complete_read(&truncated, message, message_bytes.len() as u64).await,
        Some(message_bytes.to_vec())
    );
    assert_eq!(complete_read(&truncated, counter, 4).await, None);
    truncated.shutdown().await;
}

fn with_executable(executable: &Path, allow: bool) -> CoreDumpOptions {
    CoreDumpOptions {
        executable: Some(executable.to_owned()),
        allow_module_mismatch: allow,
        ..CoreDumpOptions::new(core("crash-gcc-o0-segv.core"))
    }
}

#[tokio::test]
async fn allowed_mismatches_keep_the_recorded_placement_or_refuse_to_relocate() {
    let reference = open_core("crash-gcc-o0-segv.core");
    let message = reference
        .operation(
            "message",
            reference.handle().runtime_address("crash_message"),
        )
        .await;
    reference.shutdown().await;

    // A segment extending past the end of the file proves a different file,
    // but the recorded mapping still places it.
    let (_oversized_scratch, oversized) =
        edited("oversized", "crash-gcc-o0", "crash-gcc-o0", |bytes| {
            let (header, _, _) = *load_headers(bytes).last().expect("a load segment");
            let past_end = u64::try_from(bytes.len()).unwrap() + 1;
            bytes[header + PHDR_FILESZ..header + PHDR_FILESZ + 8]
                .copy_from_slice(&past_end.to_le_bytes());
        });

    assert!(matches!(
        Debugger::open_core(&with_executable(&oversized, false)),
        Err(Error::CoreModuleMismatch { detail, .. }) if detail.contains("outside")
    ));

    let allowed = Scenario::open_core("oversized", &with_executable(&oversized, true));
    assert!(matches!(
        loaded_identity(&core_info(&allowed), "crash-gcc-o0"),
        ModuleIdentity::Mismatched { detail } if detail.contains("outside")
    ));
    assert_eq!(
        allowed
            .operation("message", allowed.handle().runtime_address("crash_message"))
            .await,
        message,
        "the module is relocated to its recorded mapping"
    );
    allowed.shutdown().await;

    // Linked above the recorded image, the file has no load bias that places
    // it there. Relocating it anywhere would be a guess.
    let (_unplaceable_scratch, unplaceable) =
        edited("unplaceable", "crash-gcc-o0", "crash-gcc-o0", |bytes| {
            for (header, _, _) in load_headers(bytes) {
                let address = u64::from_le_bytes(
                    bytes[header + PHDR_VADDR..header + PHDR_VADDR + 8]
                        .try_into()
                        .unwrap(),
                );
                bytes[header + PHDR_VADDR..header + PHDR_VADDR + 8]
                    .copy_from_slice(&(address + 0x7f00_0000_0000).to_le_bytes());
            }
        });
    assert!(matches!(
        Debugger::open_core(&with_executable(&unplaceable, false)),
        Err(Error::CoreModuleMismatch { .. })
    ));

    let refused = Debugger::open_core(&with_executable(&unplaceable, true));
    assert!(
        matches!(&refused, Err(Error::CoreModuleUnplaceable { path, detail })
            if path.ends_with("crash-gcc-o0") && detail.contains("no loadable segment")),
        "{:?}",
        refused.err()
    );
}

#[tokio::test]
async fn invalid_core_files_fail_with_typed_errors() {
    let scratch = ScratchDir::new("post-mortem-invalid");
    let directory = scratch.path();
    let open = |path: PathBuf| Debugger::open_core(&CoreDumpOptions::new(path));
    let invalid = |result: microscope::Result<Debugger>, expected: &str| match result {
        Err(Error::InvalidCoreDump(message)) => {
            assert!(message.contains(expected), "{message:?} lacks {expected:?}");
        }
        other => panic!("expected an invalid core ({expected}): {:?}", other.err()),
    };

    assert!(matches!(
        open(directory.join("absent.core")),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));

    let empty = directory.join("empty.core");
    fs::write(&empty, []).unwrap();
    invalid(open(empty), "not a 64-bit ELF file");
    let garbage = directory.join("garbage.core");
    fs::write(
        &garbage,
        (0..4096_u32)
            .map(|value| (value * 31 % 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    invalid(open(garbage), "not a 64-bit ELF file");
    invalid(open(Scenario::fixture("crash-gcc-o0")), "not a core dump");

    let source = fs::read(core("crash-gcc-o0-segv.core")).unwrap();
    let truncated = directory.join("truncated.core");
    fs::write(&truncated, &source[..64]).unwrap();
    invalid(open(truncated), "program headers are malformed");

    // Truncation inside the note segment loses thread state, which must
    // never be reconstructed.
    let (notes_offset, notes_size) = note_segment(&source);
    let headless = directory.join("notes-truncated.core");
    fs::write(&headless, &source[..notes_offset + notes_size / 2]).unwrap();
    invalid(open(headless), "note segment is malformed");

    invalid(
        open(
            edited(
                "machine",
                "crash-gcc-o0-segv.core",
                "edited.core",
                |bytes| {
                    bytes[18..20].copy_from_slice(&elf::EM_AARCH64.to_le_bytes());
                },
            )
            .1,
        ),
        "only x86-64",
    );
    invalid(
        open(
            edited(
                "overlap",
                "crash-gcc-o0-segv.core",
                "edited.core",
                |bytes| {
                    let headers = load_headers(bytes);
                    let (_, second_start, _) = headers[1];
                    let (first, _, _) = headers[0];

                    // Grow the first segment's memory size over the second.
                    let start = u64::from_le_bytes(
                        bytes[first + PHDR_VADDR..first + PHDR_VADDR + 8]
                            .try_into()
                            .unwrap(),
                    );
                    let size = second_start - start + 4096;
                    bytes[first + PHDR_MEMSZ..first + PHDR_MEMSZ + 8]
                        .copy_from_slice(&size.to_le_bytes());
                },
            )
            .1,
        ),
        "overlap",
    );

    // An explicit executable that does not exist is an error naming it, not
    // a missing module.
    assert!(matches!(
        Debugger::open_core(&options("crash-gcc-o0-segv.core", Some("absent-executable"), false)),
        Err(Error::CoreModuleRead { path, error })
            if path.ends_with("absent-executable") && error.kind() == std::io::ErrorKind::NotFound
    ));
}

/// A core file, its innermost expected functions, and selected locals.
type LanguageCore = (
    &'static str,
    &'static [&'static str],
    &'static [(&'static str, i128)],
);

#[tokio::test]
async fn go_rust_and_zig_cores_present_frames_and_locals() {
    let cases: [LanguageCore; 3] = [
        (
            "crash-go-o0.core",
            &["main.crashNow", "main.main", "runtime.main"],
            &[("depth", 3), ("doubled", 6)],
        ),
        // Rust -O0 keeps ptr::write_volatile as a physical frame.
        (
            "crash-rust-o0.core",
            &["write_volatile<i32>", "crash_now", "main"],
            &[("src", 6)],
        ),
        (
            "crash-zig-o0.core",
            &["crashNow", "main"],
            &[("depth", 3), ("doubled", 6)],
        ),
    ];

    for (core_name, expected_frames, locals) in cases {
        let mut scenario = open_core(core_name);
        let (stop, _) = stopped(&mut scenario).await;
        assert!(
            matches!(&stop.reason, StopReason::CoreDump { exception: Some(exception) } if exception.code == 11),
            "{core_name}: {:?}",
            stop.reason
        );

        let (trace, names) = named_frames(&scenario).await;
        let functions = names
            .iter()
            .map(|(_, function)| function.as_deref())
            .collect::<Vec<_>>();

        for (index, expected) in expected_frames.iter().enumerate() {
            assert_eq!(functions[index], Some(*expected), "{core_name}: {names:#?}");
        }

        assert_eq!(
            trace.termination,
            UnwindTermination::Complete,
            "{core_name}"
        );

        for (name, value) in locals {
            assert_eq!(
                signed(&variable(&scenario, name).await),
                *value,
                "{core_name}: {name}"
            );
        }

        scenario.shutdown().await;
    }
}

/// lld packs a small program's segments so that its code shares the
/// header's first file page, and each is mapped from file offset zero. The
/// mappings are still one image, loaded once at the address of the first.
#[tokio::test]
async fn an_image_mapped_from_offset_zero_more_than_once_is_one_module() {
    let core_name = "crash-rust-o0.core";
    let scenario = open_core(core_name);
    let executable = Scenario::fixture("crash-rust-o0");
    let info = core_info(&scenario);

    let recorded = file_images(&info)
        .filter(|module| *module.recorded_path == executable)
        .collect::<Vec<_>>();
    let [module] = recorded[..] else {
        panic!("{core_name}: {:#?}", info.modules);
    };

    assert!(
        matches!(
            &module.state,
            CoreModuleState::Loaded { module: loaded, identity }
                if identity.is_verified() && loaded.module.load_bias.get() == module.start.get()
        ),
        "{core_name}: {module:#?}"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn core_sessions_coexist_with_each_other_and_a_live_session() {
    let first = open_core("crash-gcc-o0-segv.core");
    let second = open_core("crash-clang-o2-abort.core");
    let mut live = Scenario::new("live", Scenario::fixture("fatal-signal"));
    assert!(
        matches!(live.run_to_stop().await, StopReason::Exception(exception) if exception.code == 11)
    );

    let (first_names, second_names) = tokio::join!(named_frames(&first), named_frames(&second));
    assert_eq!(first_names.1[0].1.as_deref(), Some("crash_segv"));
    assert!(
        second_names
            .1
            .iter()
            .any(|(_, function)| function.as_deref() == Some("crash_abort"))
    );

    let first_stop = first.handle().snapshot().await.unwrap().stop_id;
    let second_stop = second.handle().snapshot().await.unwrap().stop_id;
    assert_ne!(
        first_stop, second_stop,
        "stop identifiers are unique across sessions"
    );

    // Dropping a session without an explicit shutdown releases it cleanly.
    drop(second);
    first.shutdown().await;
    live.shutdown().await;
}

/// File-backed memory ends where a segment's file contents end, which need
/// not be a word boundary: reads keep every backed byte up to there.
#[tokio::test]
async fn core_reads_keep_backed_bytes_that_end_inside_a_word() {
    let scenario = open_core("elf-symbols-gcc-o0.core");
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let library = modules
        .modules
        .iter()
        .find(|record| record.path.ends_with("libelf-symbols-gcc.so"))
        .expect("library loaded");
    let image = scenario
        .operation(
            "image",
            scenario.handle().loaded_module_image(library.module.id),
        )
        .await;

    // .fini ends the executable segment, whose file contents end with it.
    let fini = image
        .sections()
        .iter()
        .find(|section| section.name.as_ref() == ".fini")
        .expect(".fini")
        .range;
    let end = library.module.load_bias.get() + fini.end.get();
    assert_ne!(end % 8, 0, "the segment must end inside a word");

    let whole = scenario
        .operation(
            "read to the end",
            scenario
                .handle()
                .read_memory(VirtualAddress::new(end - 5), 5),
        )
        .await;
    assert_eq!(whole.completion, MemoryReadCompletion::Complete);
    let data = fs::read(Scenario::fixture("libelf-symbols-gcc.so")).expect("read library");
    let offset = usize::try_from(fini.end.get()).expect("offset") - 5;
    assert_eq!(*whole.bytes, data[offset..offset + 5]);

    let past = scenario
        .operation(
            "read past the end",
            scenario
                .handle()
                .read_memory(VirtualAddress::new(end - 3), 8),
        )
        .await;
    assert_eq!(*past.bytes, data[offset + 2..offset + 5]);
    assert_eq!(
        past.completion,
        MemoryReadCompletion::Incomplete {
            next_address: VirtualAddress::new(end),
            reason: microscope::MemoryReadUnavailableReason::Inaccessible,
        }
    );
    scenario.shutdown().await;
}

/// A core written on "another machine": at its recorded paths this machine
/// holds different builds of the executable and library, and no C library.
const FOREIGN_CORE: &str = "core-foreign/crash.core";

fn foreign(update: impl FnOnce(&mut CoreDumpOptions)) -> CoreDumpOptions {
    let mut options = CoreDumpOptions::new(core(FOREIGN_CORE));
    update(&mut options);
    options
}

/// Each module the foreign core records, with the file that was there when
/// the dump was written.
async fn foreign_modules() -> Vec<(PathBuf, PathBuf)> {
    let scenario = Scenario::open_core(
        "foreign modules",
        &foreign(|options| options.allow_module_mismatch = true),
    );
    let info = core_info(&scenario);
    scenario.shutdown().await;

    file_images(&info)
        .map(|module| {
            let recorded = PathBuf::clone(&module.recorded_path);
            let original = if recorded
                .parent()
                .is_some_and(|directory| directory.ends_with("core-foreign"))
            {
                match recorded.file_name().and_then(|name| name.to_str()) {
                    Some("libc.so.6") => core("libc-foreign.so.6"),
                    Some(name) => core(name),
                    None => panic!("unnamed module {}", recorded.display()),
                }
            } else {
                recorded.clone()
            };

            (recorded, original)
        })
        .collect()
}

fn named(modules: &[(PathBuf, PathBuf)], prefix: &str) -> (PathBuf, PathBuf) {
    modules
        .iter()
        .find(|(recorded, _)| {
            recorded
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("no {prefix} module in {modules:#?}"))
        .clone()
}

/// The recorded images files back: all but the vDSO.
fn file_images(info: &CoreDumpInfo) -> impl Iterator<Item = &microscope::CoreModule> {
    info.modules
        .iter()
        .filter(|module| module.recorded_path.as_os_str() != support::VDSO)
}

/// Copies `source` to where `recorded` lies under `root`.
fn place(root: &Path, recorded: &Path, source: &Path) -> PathBuf {
    let target = root.join(recorded.strip_prefix("/").expect("absolute recorded path"));
    fs::create_dir_all(target.parent().expect("recorded parent")).expect("create directories");
    fs::copy(source, &target).expect("copy module");
    target
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    })
}

fn build_id(path: &Path) -> Vec<u8> {
    let data = fs::read(path).expect("read module");
    object::File::parse(data.as_slice())
        .expect("parse module")
        .build_id()
        .expect("read build-id")
        .expect("module has a build-id")
        .to_vec()
}

/// The canonical path of the file loaded for the recorded image whose file
/// name is `name`, and how it was proven.
fn loaded_from(info: &CoreDumpInfo, name: &str) -> (PathBuf, ModuleIdentity) {
    info.modules
        .iter()
        .find_map(|module| match &module.state {
            CoreModuleState::Loaded {
                module: loaded,
                identity,
            } if module.recorded_path.file_name() == Some(name.as_ref()) => {
                Some((PathBuf::clone(&loaded.path), identity.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} is not loaded: {info:#?}"))
}

#[tokio::test]
async fn cores_from_other_machines_load_every_module_from_a_sysroot() {
    // This machine has a different build at the executable's recorded path.
    let host = Debugger::open_core(&foreign(|_| {}));
    assert!(
        matches!(&host, Err(Error::CoreModuleMismatch { path, detail })
            if path.ends_with("core-foreign/crash-gcc-o0") && detail.contains("build-id")),
        "{:?}",
        host.err()
    );

    let modules = foreign_modules().await;
    let (interpreter, interpreter_file) = named(&modules, "ld-linux");
    let root = ScratchDir::new("foreign-sysroot");

    for (recorded, original) in modules
        .iter()
        .filter(|(recorded, _)| *recorded != interpreter)
    {
        place(root.path(), recorded, original);
    }

    let options = foreign(|options| options.sysroot = Some(root.path().to_owned()));

    // A module the sysroot lacks is missing even though this machine has a
    // file at its recorded path; its recorded build-id says what to supply.
    let partial = Scenario::open_core("partial sysroot", &options);
    let info = core_info(&partial);
    let missing = info
        .modules
        .iter()
        .find(|module| *module.recorded_path == interpreter)
        .expect("the interpreter is recorded");
    assert_eq!(missing.state, CoreModuleState::Missing);
    assert_eq!(
        missing.build_id.as_deref(),
        Some(build_id(&interpreter_file).as_slice())
    );
    partial.shutdown().await;

    place(root.path(), &interpreter, &interpreter_file);
    let scenario = Scenario::open_core("sysroot", &options);
    let info = core_info(&scenario);
    let canonical_root = root.path().canonicalize().expect("canonical sysroot");
    assert_eq!(file_images(&info).count(), modules.len());
    for module in file_images(&info) {
        let CoreModuleState::Loaded {
            module: loaded,
            identity,
        } = &module.state
        else {
            panic!(
                "{} is not loaded: {info:#?}",
                module.recorded_path.display()
            );
        };

        assert_eq!(identity, &ModuleIdentity::BuildId);
        assert!(loaded.path.starts_with(&canonical_root), "{loaded:?}");

        // The build-id read from the dump's saved header is the file's own,
        // as an independent ELF reader sees it.
        assert_eq!(
            module.build_id.as_deref(),
            Some(build_id(&loaded.path).as_slice())
        );
    }

    let (_, names) = named_frames(&scenario).await;
    assert_eq!(names[0].1.as_deref(), Some("crash_segv"));
    assert_eq!(names[1].1.as_deref(), Some("main"));
    assert_eq!(names[2].0, "libc.so.6", "{names:#?}");
    assert_eq!(signed(&variable(&scenario, "depth").await), 3);

    // Saved data is the dump's own; the rebuilt library on this machine
    // would say 123.
    assert_eq!(
        signed(&variable(&scenario, "crash_library_value").await),
        321
    );

    // Unsaved read-only data comes from the verified sysroot file.
    let message = scenario
        .operation(
            "message",
            scenario.handle().runtime_address("crash_message"),
        )
        .await;
    let read = scenario
        .operation("read message", scenario.handle().read_memory(message, 29))
        .await;
    assert_eq!(read.completion, MemoryReadCompletion::Complete);
    assert_eq!(&*read.bytes, b"post-mortem read-only message");

    // The other machine's C library describes its own structures, which
    // locate its TLS.
    assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_sysroot_copy_of_this_machine_serves_tls_through_its_c_library() {
    let name = "crash-gcc-o0-segv.core";
    let recorded = open_core(name);
    let info = core_info(&recorded);
    recorded.shutdown().await;
    let root = ScratchDir::new("host-sysroot");

    for module in file_images(&info) {
        place(root.path(), &module.recorded_path, &module.recorded_path);
    }

    let scenario = Scenario::open_core(
        "host sysroot",
        &CoreDumpOptions {
            sysroot: Some(root.path().to_owned()),
            ..CoreDumpOptions::new(core(name))
        },
    );

    let canonical_root = root.path().canonicalize().expect("canonical sysroot");
    let (libc, identity) = loaded_from(&core_info(&scenario), "libc.so.6");
    assert!(libc.starts_with(&canonical_root), "{}", libc.display());
    assert_eq!(identity, ModuleIdentity::BuildId);

    // The C library's descriptors are found through the sysroot file, and
    // the TLS blocks in the dump.
    assert_eq!(signed(&variable(&scenario, "crash_tls").await), 100);
    assert_eq!(signed(&variable(&scenario, "crash_library_tls").await), 654);
    scenario.shutdown().await;
}

#[tokio::test]
async fn sysroot_paths_resolve_inside_the_sysroot_and_open_only_regular_files() {
    let modules = foreign_modules().await;
    let (executable, executable_file) = named(&modules, "crash-gcc-o0");
    let (library, library_file) = named(&modules, "libcrash.so");
    let (libc, _) = named(&modules, "libc.so.6");
    let root = ScratchDir::new("escaping-sysroot");
    for (recorded, original) in &modules {
        place(root.path(), recorded, original);
    }

    // Each link would reach this machine's different build if it escaped the
    // sysroot; inside it, the link reaches the original.
    let decoy = |name: &str| core(name).strip_prefix("/").expect("absolute").to_owned();
    let absolute = place(root.path(), &core("libcrash-rebuilt.so"), &library_file);
    let relative = place(root.path(), &core("crash-gcc-o0-rebuilt"), &executable_file);

    let library_link = root.path().join(library.strip_prefix("/").unwrap());
    fs::remove_file(&library_link).unwrap();
    std::os::unix::fs::symlink(
        Path::new("/").join(decoy("libcrash-rebuilt.so")),
        &library_link,
    )
    .unwrap();

    let executable_link = root.path().join(executable.strip_prefix("/").unwrap());
    fs::remove_file(&executable_link).unwrap();
    let climb = "../".repeat(executable.components().count() + 4);
    std::os::unix::fs::symlink(
        Path::new(&climb).join(decoy("crash-gcc-o0-rebuilt")),
        &executable_link,
    )
    .unwrap();

    let options = foreign(|options| options.sysroot = Some(root.path().to_owned()));
    let scenario = Scenario::open_core("links in sysroot", &options);
    let info = core_info(&scenario);
    assert_eq!(
        loaded_from(&info, "crash-gcc-o0"),
        (relative.canonicalize().unwrap(), ModuleIdentity::BuildId)
    );
    assert_eq!(
        loaded_from(&info, "libcrash.so"),
        (absolute.canonicalize().unwrap(), ModuleIdentity::BuildId)
    );
    scenario.shutdown().await;

    // Recorded paths can name devices and FIFOs, which are never opened for
    // reading, so this fails at once instead of blocking.
    let libc_path = root.path().join(libc.strip_prefix("/").unwrap());
    fs::remove_file(&libc_path).unwrap();
    nix::unistd::mkfifo(&libc_path, nix::sys::stat::Mode::S_IRWXU).expect("create FIFO");

    let fifo = Debugger::open_core(&options);
    assert!(
        matches!(&fifo, Err(Error::CoreModuleRead { path, error })
            if path.ends_with("core-foreign/libc.so.6")
                && error.kind() == std::io::ErrorKind::InvalidInput),
        "{:?}",
        fifo.err()
    );

    // Search locations that are not directories fail before any lookup.
    let not_directory = foreign(|options| options.sysroot = Some(core("crash-gcc-o0")));
    assert!(matches!(
        Debugger::open_core(&not_directory),
        Err(Error::CoreModuleSearch { path, error })
            if path.ends_with("crash-gcc-o0") && error.kind() == std::io::ErrorKind::NotADirectory
    ));
    let absent = foreign(|options| options.module_paths = vec![root.path().join("absent")]);
    assert!(matches!(
        Debugger::open_core(&absent),
        Err(Error::CoreModuleSearch { path, error })
            if path.ends_with("absent") && error.kind() == std::io::ErrorKind::NotFound
    ));
    let not_directory = foreign(|options| options.module_paths = vec![core("crash-gcc-o0")]);
    assert!(matches!(
        Debugger::open_core(&not_directory),
        Err(Error::CoreModuleSearch { error, .. })
            if error.kind() == std::io::ErrorKind::NotADirectory
    ));
}

#[tokio::test]
async fn module_paths_supply_files_by_name_then_by_build_id() {
    let modules = foreign_modules().await;
    let (_, executable_file) = named(&modules, "crash-gcc-o0");
    let (_, library_file) = named(&modules, "libcrash.so");
    let (_, libc_file) = named(&modules, "libc.so.6");
    let canonical = |path: &Path| path.canonicalize().expect("canonical path");

    // Files named as recorded are found after this machine's mismatched ones.
    let by_name = ScratchDir::new("module-path-names");

    for (name, source) in [
        ("crash-gcc-o0", &executable_file),
        ("libcrash.so", &library_file),
        ("libc.so.6", &libc_file),
    ] {
        fs::copy(source, by_name.path().join(name)).unwrap();
    }

    let scenario = Scenario::open_core(
        "module path names",
        &foreign(|options| options.module_paths = vec![by_name.path().to_owned()]),
    );
    let info = core_info(&scenario);

    for name in ["crash-gcc-o0", "libcrash.so", "libc.so.6"] {
        assert_eq!(
            loaded_from(&info, name),
            (
                canonical(&by_name.path().join(name)),
                ModuleIdentity::BuildId
            )
        );
    }

    // Modules this machine does hold stay at their recorded paths.
    let (pthread, identity) = loaded_from(&info, "libpthread.so.0");
    assert!(!pthread.starts_with(canonical(by_name.path())));
    assert_eq!(identity, ModuleIdentity::BuildId);
    assert_eq!(
        signed(&variable(&scenario, "crash_library_value").await),
        321
    );

    let (_, names) = named_frames(&scenario).await;
    assert_eq!(names[2].0, "libc.so.6", "{names:#?}");
    scenario.shutdown().await;

    // Renamed files are found by build-id, past a same-named different build
    // and a same-named directory.
    let renamed = ScratchDir::new("module-path-build-ids");

    for (name, source) in [
        ("app", &executable_file),
        ("libcrash.so.1", &library_file),
        ("libc-other.so", &libc_file),
        ("libcrash.so", &core("libcrash-rebuilt.so")),
    ] {
        fs::copy(source, renamed.path().join(name)).unwrap();
    }

    fs::create_dir(renamed.path().join("crash-gcc-o0")).unwrap();

    let scenario = Scenario::open_core(
        "module path build-ids",
        &foreign(|options| {
            options.module_paths = vec![renamed.path().to_owned()];
        }),
    );
    let info = core_info(&scenario);

    for (recorded, file) in [
        ("crash-gcc-o0", "app"),
        ("libcrash.so", "libcrash.so.1"),
        ("libc.so.6", "libc-other.so"),
    ] {
        assert_eq!(
            loaded_from(&info, recorded),
            (
                canonical(&renamed.path().join(file)),
                ModuleIdentity::BuildId
            )
        );
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn unproven_module_files_are_errors_only_at_their_recorded_paths() {
    let modules = foreign_modules().await;
    let (_, executable_file) = named(&modules, "crash-gcc-o0");
    let (_, libc_file) = named(&modules, "libc.so.6");

    // With no proven file, the earliest equally usable candidate explains
    // the failure, and is the one used once mismatches are allowed.
    let decoys = ScratchDir::new("module-path-decoys");
    fs::copy(&executable_file, decoys.path().join("crash-gcc-o0")).unwrap();
    fs::copy(&libc_file, decoys.path().join("libc.so.6")).unwrap();
    fs::copy(
        core("libcrash-rebuilt.so"),
        decoys.path().join("libcrash.so"),
    )
    .unwrap();
    let strict = foreign(|options| options.module_paths = vec![decoys.path().to_owned()]);
    let refused = Debugger::open_core(&strict);
    assert!(
        matches!(&refused, Err(Error::CoreModuleMismatch { path, .. })
            if path.ends_with("core-foreign/libcrash.so")),
        "{:?}",
        refused.err()
    );
    let allowed = Scenario::open_core(
        "module path mismatch",
        &CoreDumpOptions {
            allow_module_mismatch: true,
            ..strict
        },
    );

    let (path, identity) = loaded_from(&core_info(&allowed), "libcrash.so");
    assert!(
        path.ends_with("core-foreign/libcrash.so"),
        "{}",
        path.display()
    );
    assert!(matches!(identity, ModuleIdentity::Mismatched { .. }));
    assert_eq!(
        signed(&variable(&allowed, "crash_library_value").await),
        321
    );
    allowed.shutdown().await;

    // A file that searching found only shares the name, so where nothing is
    // at the recorded path, a mismatched one leaves the module missing
    // unless mismatches are allowed.
    let root = ScratchDir::new("sysroot-without-library");

    for (recorded, original) in &modules {
        if !recorded.ends_with("libcrash.so") {
            place(root.path(), recorded, original);
        }
    }

    let searched = foreign(|options| {
        options.sysroot = Some(root.path().to_owned());
        options.module_paths = vec![decoys.path().to_owned()];
    });
    let scenario = Scenario::open_core("searched mismatch", &searched);
    let library = core_info(&scenario)
        .modules
        .iter()
        .find(|module| module.recorded_path.ends_with("libcrash.so"))
        .expect("library is recorded")
        .state
        .clone();
    assert_eq!(library, CoreModuleState::Missing);
    assert!(matches!(
        scenario.handle().variable("crash_library_tls").await,
        Err(Error::VariableNotFound(_))
    ));
    scenario.shutdown().await;

    let allowed = Scenario::open_core(
        "allowed searched mismatch",
        &CoreDumpOptions {
            allow_module_mismatch: true,
            ..searched
        },
    );

    let (path, identity) = loaded_from(&core_info(&allowed), "libcrash.so");
    assert_eq!(
        path,
        decoys.path().join("libcrash.so").canonicalize().unwrap()
    );
    assert!(matches!(identity, ModuleIdentity::Mismatched { .. }));
    allowed.shutdown().await;
}

#[tokio::test]
async fn executables_found_nowhere_name_the_search_and_the_build_id_to_supply() {
    let modules = foreign_modules().await;
    let (_, executable_file) = named(&modules, "crash-gcc-o0");

    // An executable found nowhere names what was searched and what to supply.
    let empty = ScratchDir::new("empty-sysroot");
    let unavailable = Debugger::open_core(&foreign(|options| {
        options.sysroot = Some(empty.path().to_owned());
        options.module_paths = vec![empty.path().to_owned()];
    }));

    let expected_build_id = hex(&build_id(&executable_file));
    assert!(
        matches!(&unavailable, Err(Error::CoreExecutableUnavailable(message))
            if message.contains(&format!("(build-id {expected_build_id}) does not exist under the sysroot"))
                && message.contains("no module path holds a file matching it")),
        "{:?}",
        unavailable.err()
    );

    // An explicit executable needs no search.
    let explicit = Scenario::open_core(
        "explicit executable with sysroot",
        &foreign(|options| {
            options.sysroot = Some(empty.path().to_owned());
            options.executable = Some(executable_file.clone());
        }),
    );

    assert_eq!(
        loaded_from(&core_info(&explicit), "crash-gcc-o0"),
        (
            executable_file.canonicalize().unwrap(),
            ModuleIdentity::BuildId
        )
    );
    explicit.shutdown().await;
}

/// Each thread's TLS variables, with their addresses, ordered by value.
async fn thread_tls(scenario: &Scenario) -> Vec<[(i128, u64); 2]> {
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;
    let mut threads = Vec::new();

    for thread in snapshot.threads.iter() {
        scenario
            .operation("select", scenario.handle().select_context(thread.id))
            .await;
        let mut located = [(0, 0); 2];

        for (slot, name) in located.iter_mut().zip(["crash_tls", "crash_library_tls"]) {
            let variable = variable(scenario, name).await;
            let VariableState::Available {
                source: VariableValueSource::Memory(address),
                ..
            } = variable.state
            else {
                panic!("{name} is not in memory: {variable:?}");
            };
            *slot = (signed(&variable), address.get());
        }

        threads.push(located);
    }

    threads.sort_unstable();
    threads
}

fn tls_values(threads: &[[(i128, u64); 2]]) -> Vec<[i128; 2]> {
    threads
        .iter()
        .map(|[(main, _), (library, _)]| [*main, *library])
        .collect()
}

#[tokio::test]
async fn core_tls_is_located_through_the_c_librarys_own_descriptors() {
    let expected = [[100, 654], [101, 655], [102, 656], [103, 657]];
    let scenario = open_core("crash-gcc-o0-segv.core");
    assert_eq!(tls_values(&thread_tls(&scenario).await), expected);
    scenario.shutdown().await;

    // A thread whose thread pointer is unset has no TLS.
    let (_directory, path) = edited(
        "unset-thread-pointer",
        "crash-gcc-o0-segv.core",
        "edited.core",
        |bytes| {
            let fs_base = first_prstatus(bytes) + PRSTATUS_REGISTERS + 21 * 8;
            bytes[fs_base..fs_base + 8].fill(0);
        },
    );
    let unset = Scenario::open_core("unset thread pointer", &CoreDumpOptions::new(path));
    let reason = match variable(&unset, "crash_tls").await.state {
        VariableState::Unavailable(reason) => reason.to_string(),
        state => panic!("TLS without a thread pointer was available: {state:?}"),
    };

    assert!(
        reason.ends_with("the thread has not allocated the module's TLS block"),
        "{reason}"
    );
    unset.shutdown().await;

    // Another machine's build of the C library describes its own structures,
    // which locate every thread's blocks.
    let modules = foreign_modules().await;
    let originals = ScratchDir::new("foreign-tls");

    for name in ["crash-gcc-o0", "libcrash.so", "libc.so.6"] {
        fs::copy(named(&modules, name).1, originals.path().join(name)).unwrap();
    }

    let foreign_scenario = Scenario::open_core(
        "foreign TLS",
        &foreign(|options| options.module_paths = vec![originals.path().to_owned()]),
    );
    assert_eq!(tls_values(&thread_tls(&foreign_scenario).await), expected);
    foreign_scenario.shutdown().await;
}

/// The C library's thread structures in the dump locate each thread's TLS,
/// with or without a dynamic loader: musl's, statically linked glibc's,
/// and those of Ubuntu's glibc 2.27, 2.31, and 2.35.
#[tokio::test]
async fn cores_locate_every_threads_tls_in_every_module_on_each_c_library() {
    for (name, plugin, threads) in [
        ("tls-modules-musl-gcc-o0.core", true, 3),
        ("tls-modules-musl-clang-static-pie.core", false, 3),
        ("tls-modules-gcc-static.core", false, 3),
        ("tls-modules-single-thread-clang-static.core", false, 1),
        ("tls-modules-glibc2.27.core", true, 3),
        ("tls-modules-glibc2.31.core", true, 3),
        ("tls-modules-glibc2.35.core", true, 3),
    ] {
        let mut scenario = open_core(name);
        support::assert_tls_modules(&mut scenario, name, plugin, threads).await;
        scenario.shutdown().await;
    }
}

/// The dump holds the only copy of the vDSO, which no file backs. A dump
/// that lost its memory, or holds something other than its image there,
/// lists the vDSO as missing and leaves its frames unnamed rather than
/// guessed.
#[tokio::test]
async fn vdso_images_the_dump_lost_are_missing_rather_than_guessed() {
    const FIXTURE: &str = "vdso-gcc-o0-time.core";
    let vdso = |info: &CoreDumpInfo| {
        info.modules
            .iter()
            .find(|module| module.recorded_path.as_os_str() == support::VDSO)
            .unwrap_or_else(|| panic!("no vDSO in {info:#?}"))
            .clone()
    };

    let reference = open_core(FIXTURE);
    let start = vdso(&core_info(&reference)).start.get();
    let (trace, _) = named_frames(&reference).await;
    let fault = trace.frames[0]
        .instruction
        .expect("a thread's frame has an instruction");
    reference.shutdown().await;

    let edits: [(&str, SegmentEdit); 3] = [
        ("unsaved", |bytes, header| {
            bytes[header + PHDR_FILESZ..header + PHDR_FILESZ + 8]
                .copy_from_slice(&0_u64.to_le_bytes());
        }),
        ("truncated", |bytes, header| {
            let past_end = u64::try_from(bytes.len()).unwrap() + 4096;
            bytes[header + PHDR_OFFSET..header + PHDR_OFFSET + 8]
                .copy_from_slice(&past_end.to_le_bytes());
        }),
        // Saved bytes that are not an ELF image.
        ("overwritten", |bytes, header| {
            let offset = u64::from_le_bytes(
                bytes[header + PHDR_OFFSET..header + PHDR_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            );
            bytes[usize::try_from(offset).unwrap()] = 0;
        }),
    ];

    for (name, edit) in edits {
        let (_scratch, path) = edited(name, FIXTURE, "edited.core", |bytes| {
            let (header, _, _) = load_headers(bytes)
                .into_iter()
                .find(|&(_, segment, _)| segment == start)
                .expect("a load segment holds the vDSO");
            edit(bytes, header);
        });

        let scenario = Scenario::open_core(name, &CoreDumpOptions::new(path));
        let info = core_info(&scenario);
        let missing = vdso(&info);
        assert_eq!(missing.state, CoreModuleState::Missing, "{name}");
        assert_eq!(missing.start.get(), start, "{name}");

        // The modules files back are unaffected.
        assert!(
            loaded_identity(&info, "vdso-gcc-o0").is_verified(),
            "{name}: {info:#?}"
        );

        let (trace, frames) = named_frames(&scenario).await;
        assert!(
            trace.frames.len() == 1 && trace.frames[0].instruction == Some(fault),
            "{name}: {trace:#?}"
        );
        assert_eq!(frames[0].0, "?", "{name}");
        assert!(trace.frames[0].symbol.is_none(), "{name}");
        assert_eq!(
            trace.termination,
            UnwindTermination::ModuleNotFound { address: fault },
            "{name}"
        );
        scenario.shutdown().await;
    }
}
