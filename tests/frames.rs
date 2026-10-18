mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use microscope::{
    CodeRole, CoreDumpOptions, DebuggerEvent, DereferenceState, Error, ExceptionDisposition,
    ExitStatus, FloatValue, IntegerValue, PresentedFrame, ResumeScope, ScalarValue, StackFrame,
    StackFrameId, StepKind, StopReason, ThreadId, ValueChildQuery, ValueChildren, Variable,
    VariableState, VariableUnavailableReason, VariableValue, WatchAccess, WatchScope,
    WatchpointInvalidation,
};
use support::Scenario;

/// The frames fixture across compilers, optimization, and PIE.
const VARIANTS: [&str; 5] = ["gcc-o0", "gcc-o2", "gcc-o2-nopie", "clang-o0", "clang-o2"];

fn fixture_line(fixture: &str, needle: &str) -> u64 {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture))
        .expect("read fixture source");
    let index = source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{fixture} has no line containing {needle:?}"));
    u64::try_from(index + 1).expect("line fits u64")
}

fn frames_line(needle: &str) -> u64 {
    fixture_line("tests/fixtures/c/frames.c", needle)
}

fn function_name(frame: &StackFrame) -> Option<&str> {
    frame
        .function
        .as_ref()
        .map(|function| function.name.as_ref())
}

async fn backtrace(scenario: &Scenario) -> Vec<StackFrame> {
    scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await
        .frames
        .to_vec()
}

/// Selects the frame at `level` of the selected thread's backtrace.
async fn select(scenario: &mut Scenario, level: usize) -> StackFrame {
    let frames = backtrace(scenario).await;
    let frame = frames
        .get(level)
        .unwrap_or_else(|| panic!("no frame {level} in {frames:#?}"));
    let selected = scenario
        .operation("select frame", scenario.handle().select_frame(frame.id))
        .await;
    assert_eq!(&selected, frame, "selection returns the backtrace's frame");
    assert_eq!(
        scenario.snapshot().await.selected_frame,
        Some(frame.id),
        "the snapshot publishes the selection"
    );
    selected
}

/// An integer-like value: an integer, enumerator, boolean, or address.
fn integer(value: &VariableValue) -> Option<i128> {
    let integer = |value: &IntegerValue| match *value {
        IntegerValue::Signed(value) => Some(value),
        IntegerValue::Unsigned(value) => i128::try_from(value).ok(),
        _ => None,
    };

    match value {
        VariableValue::Scalar(ScalarValue::Signed(value)) => Some(*value),
        VariableValue::Scalar(ScalarValue::Unsigned(value)) => i128::try_from(*value).ok(),
        VariableValue::Scalar(ScalarValue::Boolean(value)) => Some(i128::from(*value)),
        VariableValue::Enumeration { value, .. } => integer(value),
        VariableValue::Address(address) => Some(i128::from(address.address.get())),
        _ => None,
    }
}

fn available_integer(variable: &Variable) -> Option<i128> {
    match &variable.state {
        VariableState::Available { value, .. } => Some(
            integer(value)
                .unwrap_or_else(|| panic!("{} is not an integer: {value:?}", variable.name)),
        ),
        _ => None,
    }
}

/// Whether an unavailable value says honestly why: the compiler did not
/// keep it here, the debugger does not support its description, or a
/// callee may have overwritten the register holding it.
const fn honestly_unavailable(reason: &VariableUnavailableReason) -> bool {
    matches!(
        reason,
        VariableUnavailableReason::OptimizedOut(_)
            | VariableUnavailableReason::UnavailableAtInstruction
            | VariableUnavailableReason::Unsupported(_)
            | VariableUnavailableReason::RegisterNotSaved(_)
            | VariableUnavailableReason::EntryValue(_)
    )
}

/// The value each named variable of the frames fixture holds while
/// `frames_leaf` runs, when the program determines it.
fn truth(function: &str, name: &str, recursion: Option<i128>, argc: i128) -> Option<i128> {
    match (function, name) {
        ("frames_leaf", "token") | ("frames_relay", "value") | ("frames_inlined", "doubled") => {
            Some(12)
        }
        ("frames_leaf", "leaf_local") => Some(120),
        ("frames_inlined", "base") => Some(6),
        ("frames_keep" | "frames_recurse", "seed") => Some(5),
        ("frames_keep", "kept") => Some(16),
        ("frames_recurse", "depth") => recursion,
        ("frames_recurse", "level") => recursion.map(|depth| depth * 100 + 5),
        ("main", "argc") => Some(argc),
        _ => None,
    }
}

/// The variables a variant must show, beyond never showing a wrong value:
/// every determined one unoptimized, and those optimized builds keep in
/// registers the leaf saves and overwrites.
fn required(variant: &str) -> &'static [(&'static str, &'static str)] {
    match variant {
        "gcc-o0" | "clang-o0" => &[
            ("frames_leaf", "token"),
            ("frames_leaf", "leaf_local"),
            ("frames_relay", "value"),
            ("frames_inlined", "base"),
            ("frames_inlined", "doubled"),
            ("frames_keep", "seed"),
            ("frames_keep", "kept"),
            ("frames_recurse", "depth"),
            ("frames_recurse", "level"),
            ("frames_recurse", "seed"),
            ("main", "argc"),
        ],
        "gcc-o2" | "gcc-o2-nopie" => &[
            ("frames_leaf", "leaf_local"),
            ("frames_inlined", "base"),
            ("frames_inlined", "doubled"),
            ("frames_keep", "seed"),
            ("frames_keep", "kept"),
        ],
        "clang-o2" => &[
            ("frames_leaf", "leaf_local"),
            ("frames_inlined", "base"),
            ("frames_keep", "seed"),
            ("frames_recurse", "depth"),
            ("frames_recurse", "seed"),
        ],
        _ => unreachable!("unknown variant {variant}"),
    }
}

/// The source line each frame of the fixture is at, for frames whose
/// position the program determines.
fn call_line(function: &str, recursion: Option<i128>) -> Option<u64> {
    match (function, recursion) {
        ("frames_relay", _) => Some(frames_line("int64_t result = frames_leaf(value);")),
        ("frames_inlined", _) => Some(frames_line("int64_t result = frames_relay(doubled);")),
        ("frames_keep", _) => Some(frames_line("int64_t result = frames_inlined(seed + 1);")),
        ("frames_recurse", Some(0)) => Some(frames_line("return frames_keep(seed) + level;")),
        ("frames_recurse", Some(_)) => Some(frames_line(
            "int64_t below = frames_recurse(depth - 1, seed);",
        )),
        ("main", _) => Some(frames_line(
            "int64_t total = frames_recurse(3, frames_seed);",
        )),
        _ => None,
    }
}

/// Selects every frame of the fixture's stopped thread from main inward and
/// checks that each shows its own values and source position.
async fn check_frame_truth(scenario: &mut Scenario, variant: &str, argc: i128) {
    let frames = backtrace(scenario).await;
    let names = frames
        .iter()
        .map(|frame| function_name(frame).unwrap_or("?").to_owned())
        .collect::<Vec<_>>();
    let main = names
        .iter()
        .position(|name| name == "main")
        .unwrap_or_else(|| panic!("{variant}: no main frame in {names:?}"));
    let recursion_frames = names
        .iter()
        .filter(|name| *name == "frames_recurse")
        .count();
    let mut required = required(variant).to_vec();
    let mut compared = 0;

    // Outer frames first, so each selection moves inward like `down`.
    for level in (0..=main).rev() {
        let frame = select(scenario, level).await;
        let function = names[level].as_str();

        // Unoptimized recursion keeps one frame per call, innermost first.
        let recursion = (function == "frames_recurse" && recursion_frames == 4).then(|| {
            i128::try_from(
                names[..level]
                    .iter()
                    .filter(|name| *name == "frames_recurse")
                    .count(),
            )
            .expect("depth fits i128")
        });
        let context = format!("{variant} frame {level} ({function})");

        let snapshot = scenario
            .operation("variables", scenario.handle().variables())
            .await;
        assert_eq!(snapshot.stack_frame, frame.id, "{context}");
        assert_eq!(
            snapshot.frame,
            if function == "frames_inlined" {
                PresentedFrame::Inline(frame.code_instance.expect("inline frames name an instance"))
            } else {
                PresentedFrame::Physical
            },
            "{context}"
        );
        for variable in snapshot.variables.iter() {
            let expected = truth(function, &variable.name, recursion, argc);
            match &variable.state {
                VariableState::Available { .. } => {
                    if let Some(expected) = expected {
                        assert_eq!(
                            available_integer(variable),
                            Some(expected),
                            "{context}: {} has another frame's value",
                            variable.name
                        );
                        compared += 1;
                        required.retain(|required| *required != (function, &*variable.name));
                    }
                }
                VariableState::Unavailable(reason) => assert!(
                    honestly_unavailable(reason),
                    "{context}: {} is unavailable for an unexpected reason: {reason}",
                    variable.name
                ),
                state => panic!("{context}: {} is {state:?}", variable.name),
            }
        }

        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(Some(location.address), frame.instruction, "{context}");
        assert_eq!(location.image.function, frame.function, "{context}");
        assert_eq!(location.image.source, frame.source, "{context}");
        if let Some(line) = call_line(function, recursion) {
            let source = frame
                .source
                .as_ref()
                .unwrap_or_else(|| panic!("{context} has no source"));
            assert_eq!(source.line.get(), line, "{context}");
            let context_lines = scenario
                .operation("source", scenario.handle().source_context(1))
                .await;
            assert_eq!(context_lines.line.get(), line, "{context}");
            assert_eq!(context_lines.lines.len(), 3, "{context}");
        }
    }

    assert!(
        required.is_empty(),
        "{variant}: values that must be available were not: {required:?}"
    );
    assert!(compared >= 5, "{variant}: only {compared} values compared");
}

/// The leaf's caller keeps `doubled` in rdi up to the return address, which
/// the leaf overwrites without saving. gdb shows the overwritten register as
/// the caller's value; the debugger must call it unknown.
async fn check_unsaved_register(scenario: &mut Scenario) {
    let frames = backtrace(scenario).await;
    let inlined = frames
        .iter()
        .position(|frame| function_name(frame) == Some("frames_inlined"))
        .expect("frames_inlined frame");
    select(scenario, inlined).await;
    let doubled = scenario
        .operation("doubled", scenario.handle().variable("doubled"))
        .await;
    assert_eq!(
        doubled.state,
        VariableState::Unavailable(VariableUnavailableReason::RegisterNotSaved("rdi".into()))
    );

    // The frame's registers say the same, while those the unwinder recovered
    // are the frame's own.
    let registers = scenario
        .operation("caller registers", scenario.handle().registers())
        .await;
    let register = |name: &str| {
        registers
            .registers
            .iter()
            .find(|register| &*register.register.name == name)
            .unwrap_or_else(|| panic!("no register {name}"))
            .bytes
            .clone()
    };

    assert_eq!(register("rdi"), None);
    assert_eq!(register("orig_rax"), None);
    assert_eq!(
        register("rip").as_deref(),
        Some(
            &frames[inlined]
                .instruction
                .expect("a thread's frame has an instruction")
                .get()
                .to_le_bytes()[..]
        )
    );
    assert!(register("rsp").is_some() && register("fs_base").is_some());

    // The innermost frame still reads the live register.
    select(scenario, 0).await;
    let registers = scenario
        .operation("registers", scenario.handle().registers())
        .await;
    let rdi = registers
        .registers
        .iter()
        .find(|register| &*register.register.name == "rdi")
        .expect("rdi");
    assert_eq!(
        rdi.bytes.as_deref(),
        Some(&0x5ca1_ab1e_u64.to_le_bytes()[..])
    );
}

#[tokio::test]
async fn caller_frames_show_their_own_values_live_across_the_compiler_matrix() {
    let line = frames_line("frames_sink = leaf_local;");
    for variant in VARIANTS {
        let mut scenario = Scenario::launch(&format!("frames-{variant}"));
        scenario.add_source_breakpoint("frames.c", line).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        check_frame_truth(&mut scenario, variant, 1).await;
        if variant == "clang-o2" {
            check_unsaved_register(&mut scenario).await;
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn go_caller_frames_refuse_registers_go_callees_overwrite() {
    let mut scenario = Scenario::launch("frames-go-o2");
    scenario.add_breakpoint("main.leaf").await;
    let mut reason = scenario.run_to_stop().await;

    // The Go runtime preempts goroutines with SIGURG.
    while matches!(&reason, StopReason::Exception(exception) if exception.code == 23) {
        reason = scenario.resume_to_stop().await;
    }

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    assert_eq!(integer_variable(&scenario, "y").await, 2000);

    let frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&frames, "main.held", 0)).await;

    // held's arguments are described in rax and rbx through its call, which
    // Go callees overwrite without saving: rbx holds leaf's 2000, not 12.
    for (name, register) in [("a", "rax"), ("b", "rbx")] {
        let variable = scenario
            .operation(name, scenario.handle().variable(name))
            .await;
        assert_eq!(
            variable.state,
            VariableState::Unavailable(VariableUnavailableReason::RegisterNotSaved(
                register.into()
            )),
            "{name}"
        );
    }

    // A value kept on the stack across the call is still the caller's.
    select(&mut scenario, level_of(&frames, "main.spilled", 0)).await;
    assert_eq!(integer_variable(&scenario, "seed").await, 10);
    scenario.shutdown().await;
}

#[tokio::test]
async fn caller_frames_show_their_own_values_in_core_dumps_across_the_compiler_matrix() {
    for variant in VARIANTS {
        let core = Scenario::fixture(&format!("frames-{variant}.core"));
        let mut scenario = Scenario::open_core(
            format!("frames {variant} core"),
            &CoreDumpOptions::new(core),
        );
        check_frame_truth(&mut scenario, variant, 2).await;
        if variant == "clang-o2" {
            check_unsaved_register(&mut scenario).await;
        }

        scenario.shutdown().await;
    }
}

/// A value as gdb's oracle records it.
#[derive(Debug, Clone, PartialEq)]
enum OracleValue {
    Integer(i128),
    Float(f64),
    OptimizedOut,
    Error,
    Other,

    /// A structure's members by path, depth first, each judged by its own
    /// bits.
    Struct(Vec<(String, Self)>),
}

impl OracleValue {
    /// What gdb could read of the value: a structure is readable when any
    /// member is.
    fn readable(&self) -> bool {
        match self {
            Self::OptimizedOut | Self::Error => false,
            Self::Integer(_) | Self::Float(_) | Self::Other => true,
            Self::Struct(members) => members.iter().any(|(_, member)| member.readable()),
        }
    }
}

#[derive(Debug)]
struct OracleFrame {
    level: usize,
    function: String,
    scope: bool,

    /// Innermost block first.
    variables: Vec<(String, OracleValue)>,
}

/// Parses gdb's frame variables for every thread of a core, by thread.
fn parse_oracle(path: &Path) -> BTreeMap<u64, Vec<OracleFrame>> {
    let text = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "missing oracle {}: {error}; run `just build-test-programs`",
            path.display()
        )
    });
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("microscope-frame-variables-oracle-v2"));
    let mut threads: BTreeMap<u64, Vec<OracleFrame>> = BTreeMap::new();
    let mut thread = None;
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        match fields.as_slice() {
            ["thread", tid] => {
                let tid = tid.parse().expect("thread id");
                threads.insert(tid, Vec::new());
                thread = Some(tid);
            }
            ["frame", level, function, scope] => {
                threads
                    .get_mut(&thread.expect("frame follows a thread"))
                    .expect("thread")
                    .push(OracleFrame {
                        level: level.parse().expect("frame level"),
                        function: (*function).to_owned(),
                        scope: *scope == "scope",
                        variables: Vec::new(),
                    });
            }
            ["var", name, _kind, state, value] => {
                threads
                    .get_mut(&thread.expect("variable follows a thread"))
                    .and_then(|frames| frames.last_mut())
                    .expect("variable follows a frame")
                    .variables
                    .push(((*name).to_owned(), oracle_value(state, value)));
            }
            ["member", path, state, value] => {
                let variable = threads
                    .get_mut(&thread.expect("member follows a thread"))
                    .and_then(|frames| frames.last_mut())
                    .and_then(|frame| frame.variables.last_mut());
                let Some((_, OracleValue::Struct(members))) = variable else {
                    panic!("member {path} does not follow a structure");
                };

                members.push(((*path).to_owned(), oracle_value(state, value)));
            }
            other => panic!("malformed oracle line {other:?} in {}", path.display()),
        }
    }

    threads
}

fn oracle_value(state: &str, value: &str) -> OracleValue {
    match state {
        "int" => OracleValue::Integer(value.parse().expect("integer value")),
        "float" => OracleValue::Float(value.parse().expect("float value")),
        "optimized-out" => OracleValue::OptimizedOut,
        "error" => OracleValue::Error,
        "other" => OracleValue::Other,
        "struct" => OracleValue::Struct(Vec::new()),
        other => panic!("unknown oracle state {other:?}"),
    }
}

/// Decodes a value the way the oracle records it, or `None` for one the
/// oracle does not compare.
fn oracle_view(value: &VariableValue) -> Option<OracleValue> {
    if let Some(integer) = integer(value) {
        return Some(OracleValue::Integer(integer));
    }

    match value {
        VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary32(bits))) => {
            Some(OracleValue::Float(f64::from(f32::from_bits(*bits))))
        }
        VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary64(bits))) => {
            Some(OracleValue::Float(f64::from_bits(*bits)))
        }
        _ => None,
    }
}

/// Whether a frame names the function gdb names, which gdb may qualify with
/// the function's namespace or package.
fn same_function(ours: &StackFrame, gdb: &str) -> bool {
    function_name(ours).is_some_and(|name| {
        name == gdb || gdb.ends_with(&format!("::{name}")) || gdb.ends_with(&format!(".{name}"))
    })
}

#[derive(Default, Debug)]
struct Agreement {
    /// Values both debuggers read and found equal.
    equal: usize,

    /// Values only gdb read, which this debugger reports as unknowable or
    /// unsupported instead.
    declined: usize,
}

async fn compare_frame(
    scenario: &Scenario,
    frame: &StackFrame,
    oracle: &OracleFrame,
    go: bool,
    context: &str,
    agreement: &mut Agreement,
) {
    let selected = scenario
        .operation("select frame", scenario.handle().select_frame(frame.id))
        .await;
    assert_eq!(&selected, frame, "{context}");
    if !oracle.scope {
        assert!(
            matches!(
                scenario.handle().variables().await,
                Err(Error::VariableContextUnsupported)
            ),
            "{context}: gdb has no scope for this frame"
        );
        return;
    }

    assert!(
        same_function(frame, &oracle.function),
        "{context}: gdb's frame is {:?}, ours is {:?}",
        oracle.function,
        function_name(frame)
    );
    let ours = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    let mut our_names = ours
        .variables
        .iter()
        .map(|variable| variable.name.to_string())
        .collect::<Vec<_>>();
    let mut gdb_names = oracle
        .variables
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    our_names.sort();
    gdb_names.sort();

    // gdb shows a Go local before the line declaring it has run; microscope,
    // like Delve, only past it.
    if go {
        gdb_names.retain(|name| our_names.contains(name));
    }

    assert_eq!(our_names, gdb_names, "{context}: visible variables differ");

    let mut seen = Vec::new();
    for (name, expected) in &oracle.variables {
        // The innermost of several equal names is the one a lookup finds.
        if seen.contains(name) || !gdb_names.contains(name) {
            continue;
        }

        seen.push(name.clone());
        let variable = scenario
            .operation(name, scenario.handle().variable(name.clone()))
            .await;
        let context = format!("{context}: {name}");
        let (VariableState::Available { .. }, OracleValue::Struct(members)) =
            (&variable.state, expected)
        else {
            compare_value(&variable.state, expected, &context, agreement);
            continue;
        };

        for (path, expected) in members {
            // A nested structure's own members follow it.
            if matches!(expected, OracleValue::Struct(_)) {
                continue;
            }

            let member = scenario
                .operation(
                    path,
                    scenario
                        .handle()
                        .inspect(&microscope::Expression::parse(path).expect("member path")),
                )
                .await;
            compare_value(
                &member.state,
                expected,
                &format!("{context}: {path}"),
                agreement,
            );
        }
    }
}

/// Compares one value, judging a structure ours cannot read by whether gdb
/// can read any of it.
fn compare_value(
    state: &VariableState,
    expected: &OracleValue,
    context: &str,
    agreement: &mut Agreement,
) {
    match (state, expected) {
        (VariableState::Available { value, .. }, expected) => {
            match (oracle_view(value), expected) {
                (Some(ours), OracleValue::Integer(_) | OracleValue::Float(_)) => {
                    assert_eq!(&ours, expected, "{context}");
                    agreement.equal += 1;
                }
                (_, OracleValue::OptimizedOut | OracleValue::Error) => {
                    panic!("{context}: gdb cannot read {value:?}")
                }

                // Pointers and aggregates have no numeric view to compare.
                _ => {}
            }
        }
        (VariableState::Unavailable(reason), expected) if !expected.readable() => {
            assert!(honestly_unavailable(reason), "{context}: {reason}");
        }
        (VariableState::Unavailable(reason), _) => {
            assert!(
                matches!(
                    reason,
                    VariableUnavailableReason::RegisterNotSaved(_)
                        | VariableUnavailableReason::Unsupported(_)
                ),
                "{context}: gdb reads {expected:?}, but ours is unavailable: {reason}"
            );
            agreement.declined += 1;
        }
        (state, expected) => panic!("{context}: ours is {state:?}, gdb's is {expected:?}"),
    }
}

/// Cores whose every thread and frame gdb described, with their executables.
const ORACLE_CORES: [&str; 23] = [
    "frames-gcc-o0.core",
    "frames-gcc-o2.core",
    "frames-gcc-o2-nopie.core",
    "frames-clang-o0.core",
    "frames-clang-o2.core",
    "crash-gcc-o0-segv.core",
    "crash-gcc-o0-abort.core",
    "crash-clang-o2-segv.core",
    "crash-clang-o2-abort.core",
    "crash-gcc-o2-nopie-segv.core",
    "crash-gcc-o2-nopie-abort.core",
    "crash-rust-o0.core",
    "crash-go-o0.core",
    "crash-zig-o0.core",
    "vdso-gcc-o0-clock.core",
    "vdso-gcc-o0-time.core",
    "vdso-gcc-o2-clock.core",
    "vdso-gcc-o2-time.core",
    "vdso-clang-o2-nopie-clock.core",
    "vdso-clang-o2-nopie-time.core",
    "locations-gcc-o2.core",
    "locations-gcc-o2-nopie.core",
    "locations-clang-o2.core",
];

#[tokio::test]
async fn every_frame_of_every_dumped_thread_agrees_with_gdb() {
    let mut total = Agreement::default();
    for core in ORACLE_CORES {
        let path = Scenario::fixture(core);
        let oracle = parse_oracle(&Scenario::fixture(&format!("{core}.gdb-frame-variables")));
        let scenario = Scenario::open_core(core, &CoreDumpOptions::new(path));
        let mut agreement = Agreement::default();
        for (tid, frames) in &oracle {
            scenario
                .operation(
                    "select thread",
                    scenario.handle().select_context(ThreadId::new(*tid)),
                )
                .await;
            let ours = backtrace(&scenario).await;
            for frame in frames {
                let context = format!("{core} thread {tid} frame {}", frame.level);

                // gdb unwinds past code no module describes, where ours
                // stops, and past the frame a runtime declares its stack's
                // first, into the code that started the thread.
                let Some(our_frame) = ours.get(frame.level) else {
                    let outermost = ours
                        .last()
                        .and_then(|frame| frame.function.as_ref())
                        .is_some_and(|function| function.role == CodeRole::Outermost);
                    assert!(
                        !frame.scope || outermost,
                        "{context}: our backtrace is shorter: {ours:#?}"
                    );
                    continue;
                };

                let go = core.contains("-go-");
                compare_frame(&scenario, our_frame, frame, go, &context, &mut agreement).await;
            }
        }

        assert!(agreement.equal > 0, "{core}: nothing compared");
        total.equal += agreement.equal;
        total.declined += agreement.declined;
        scenario.shutdown().await;
    }

    assert!(total.equal >= 200, "too few values compared: {total:?}");
}

/// Launches a frames fixture and stops in its leaf.
async fn stop_in_leaf(variant: &str) -> Scenario {
    let mut scenario = Scenario::launch(&format!("frames-{variant}"));
    scenario
        .add_source_breakpoint("frames.c", frames_line("frames_sink = leaf_local;"))
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario
}

/// The level of the `nth` frame, counting outward from the innermost, that
/// runs `function`.
fn level_of(frames: &[StackFrame], function: &str, nth: usize) -> usize {
    frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| function_name(frame) == Some(function))
        .nth(nth)
        .map_or_else(
            || panic!("no frame {nth} of {function} in {frames:#?}"),
            |(level, _)| level,
        )
}

async fn integer_variable(scenario: &Scenario, name: &str) -> i128 {
    let variable = scenario
        .operation(name, scenario.handle().variable(name))
        .await;
    available_integer(&variable).unwrap_or_else(|| panic!("{name} is {:?}", variable.state))
}

async fn innermost_function(scenario: &Scenario) -> String {
    function_name(&backtrace(scenario).await[0])
        .expect("innermost frame has a function")
        .to_owned()
}

#[tokio::test]
async fn frame_selection_lasts_until_the_next_stop() {
    let mut scenario = stop_in_leaf("gcc-o0").await;
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.selected_frame, Some(StackFrameId::INNERMOST));

    let frames = backtrace(&scenario).await;
    let recursion = level_of(&frames, "frames_recurse", 0);
    select(&mut scenario, recursion).await;
    let selected = scenario.snapshot().await;
    assert!(
        selected.revision > snapshot.revision,
        "selection is a state change"
    );
    assert_eq!(
        selected.stop_id, snapshot.stop_id,
        "selection keeps the stop"
    );
    assert_eq!(integer_variable(&scenario, "depth").await, 0);

    // The backtrace describes the thread, whatever is selected, while the
    // registers are the selected frame's own.
    assert_eq!(backtrace(&scenario).await, frames);
    assert_eq!(
        scenario
            .operation("registers", scenario.handle().registers())
            .await
            .registers
            .iter()
            .find(|register| &*register.register.name == "rip")
            .and_then(|register| register.bytes.clone()),
        Some(
            frames[recursion]
                .instruction
                .expect("a thread's frame has an instruction")
                .get()
                .to_le_bytes()
                .into()
        )
    );

    let outermost = frames.last().expect("frames").id;
    scenario
        .add_source_breakpoint("frames.c", frames_line("frames_sink = total;"))
        .await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.snapshot().await.selected_frame,
        Some(StackFrameId::INNERMOST),
        "a new stop selects the innermost frame"
    );
    assert_eq!(innermost_function(&scenario).await, "main");
    assert_eq!(integer_variable(&scenario, "total").await, 769);

    // A frame of the previous stop's deeper stack does not exist now.
    let shallow = backtrace(&scenario).await;
    assert!(matches!(
        scenario.handle().select_frame(outermost).await,
        Err(Error::FrameNotFound { frame, frames })
            if frame == outermost && frames as usize == shallow.len()
    ));
    assert_eq!(
        scenario.snapshot().await.selected_frame,
        Some(StackFrameId::INNERMOST),
        "a failed selection keeps the selected frame"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn explicit_contexts_inspect_any_frame_without_selecting_it() {
    use microscope::StopContext;

    let mut scenario = stop_in_leaf("gcc-o0").await;
    let snapshot = scenario.snapshot().await;
    let stop = snapshot.stop_id.expect("stopped");
    let thread = snapshot
        .selected
        .and_then(microscope::ExecutionContext::as_thread)
        .expect("selected thread");
    let frames = backtrace(&scenario).await;
    let recursion = level_of(&frames, "frames_recurse", 0);
    let handle = scenario.handle().clone();
    let view = handle.at(StopContext {
        stop,
        execution: thread.into(),
        frame: frames[recursion].id,
    });

    let location = scenario.operation("location", view.location()).await;
    assert_eq!(
        location
            .image
            .function
            .map(|function| function.name.to_string()),
        Some("frames_recurse".to_owned())
    );
    let depth = scenario
        .operation("variables", view.variables())
        .await
        .variables
        .iter()
        .find(|variable| &*variable.name == "depth")
        .and_then(available_integer);
    assert_eq!(depth, Some(0));
    let registers = scenario.operation("registers", view.registers()).await;
    assert!(registers.registers.iter().any(|register| {
        &*register.register.name == "rip"
            && register.bytes.as_deref()
                == Some(
                    &frames[recursion]
                        .instruction
                        .expect("a thread's frame has an instruction")
                        .get()
                        .to_le_bytes()[..],
                )
    }));
    assert_eq!(
        scenario
            .operation("backtrace", view.backtrace())
            .await
            .frames[..],
        frames[..]
    );
    let source = scenario.operation("source", view.source_context(0)).await;
    assert_eq!(
        source.line.get(),
        frames[recursion]
            .source
            .as_ref()
            .expect("source")
            .line
            .get()
    );

    // The view leaves the selection alone.
    assert_eq!(
        scenario.snapshot().await.selected_frame,
        Some(StackFrameId::INNERMOST)
    );
    assert_eq!(integer_variable(&scenario, "leaf_local").await, 120);

    assert!(matches!(
        scenario
            .handle()
            .at(StopContext {
                stop,
                execution: ThreadId::new(u64::from(u32::MAX)).into(),
                frame: StackFrameId::INNERMOST,
            })
            .backtrace()
            .await,
        Err(Error::UnknownThread(_))
    ));

    // Every request through a view of an earlier stop fails.
    scenario
        .add_source_breakpoint("frames.c", frames_line("frames_sink = total;"))
        .await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert!(matches!(view.variables().await, Err(Error::StaleStop)));
    assert!(matches!(view.registers().await, Err(Error::StaleStop)));
    assert!(matches!(view.location().await, Err(Error::StaleStop)));
    scenario.shutdown().await;
}

#[tokio::test]
async fn each_thread_keeps_its_own_selected_frame() {
    let core = Scenario::fixture("crash-gcc-o0-segv.core");
    let mut scenario = Scenario::open_core("crash threads", &CoreDumpOptions::new(core));
    let snapshot = scenario.snapshot().await;
    let crashing = snapshot
        .selected
        .and_then(microscope::ExecutionContext::as_thread)
        .expect("selected thread");
    let worker = snapshot
        .threads
        .iter()
        .map(|thread| thread.id)
        .find(|thread| *thread != crashing)
        .expect("worker thread");

    select(&mut scenario, 1).await;
    assert_eq!(innermost_function(&scenario).await, "crash_segv");
    assert_eq!(
        scenario
            .operation("location", scenario.handle().current_location())
            .await
            .image
            .function
            .map(|function| function.name.to_string()),
        Some("main".to_owned())
    );

    scenario
        .operation("select worker", scenario.handle().select_context(worker))
        .await;
    assert_eq!(
        scenario.snapshot().await.selected_frame,
        Some(StackFrameId::INNERMOST),
        "another thread starts at its innermost frame"
    );
    let worker_frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&worker_frames, "worker_main", 0)).await;
    assert!(matches!(
        scenario.handle().variable("local_id").await,
        Err(Error::VariableNotFound(_))
    ));

    scenario
        .operation(
            "select crashing",
            scenario.handle().select_context(crashing),
        )
        .await;
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.selected_frame.map(StackFrameId::get), Some(1));

    // Names resolve in main's frame, not the crashing callee's.
    assert!(matches!(
        scenario.handle().variable("depth").await,
        Err(Error::VariableNotFound(_))
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn value_capabilities_evaluate_in_the_frame_that_produced_them() {
    // gcc -O2 describes `kept_pointer` by its target, `kept`, which lives in
    // a register the leaf saved and overwrote: dereferencing it reads the
    // register in the producing frame, not the stopped thread's.
    let core = Scenario::fixture("frames-gcc-o2.core");
    let mut scenario = Scenario::open_core("gcc-o2", &CoreDumpOptions::new(core));
    let frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&frames, "frames_keep", 0)).await;
    let pointer = scenario
        .operation("kept_pointer", scenario.handle().variable("kept_pointer"))
        .await;
    let VariableState::Available {
        value: VariableValue::ImplicitPointer,
        dereference: DereferenceState::Available(reference),
        ..
    } = pointer.state
    else {
        panic!("kept_pointer is {:?}", pointer.state);
    };

    assert_eq!(integer_variable(&scenario, "kept").await, 16);

    select(&mut scenario, 0).await;
    let target = scenario
        .operation("dereference", scenario.handle().dereference(reference))
        .await;
    let VariableState::Available { value, .. } = &target.state else {
        panic!("*kept_pointer is {:?}", target.state);
    };

    assert_eq!(integer(value), Some(16));
    scenario.shutdown().await;

    // Unoptimized aggregates expand their members from the producing frame.
    let core = Scenario::fixture("frames-gcc-o0.core");
    let mut scenario = Scenario::open_core("gcc-o0", &CoreDumpOptions::new(core));
    let frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&frames, "frames_keep", 0)).await;
    let pair = scenario
        .operation("pair", scenario.handle().variable("pair"))
        .await;
    let VariableState::Available {
        children: ValueChildren::Available(reference),
        ..
    } = pair.state
    else {
        panic!("pair is {:?}", pair.state);
    };

    select(&mut scenario, 0).await;
    let members = scenario
        .operation(
            "pair members",
            scenario
                .handle()
                .value_children(reference, ValueChildQuery::default()),
        )
        .await;
    let values = members
        .children
        .iter()
        .map(|child| match &child.state {
            VariableState::Available { value, .. } => integer(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(values, [Some(5), Some(-5)]);
    scenario.shutdown().await;
}

#[tokio::test]
async fn finish_runs_until_the_selected_frame_returns() {
    // The selected recursion frame's return address is reached first by its
    // callee's return into it; only its own return completes the step.
    for variant in ["gcc-o0", "clang-o2"] {
        let mut scenario = stop_in_leaf(variant).await;
        let frames = backtrace(&scenario).await;
        let depth_one = level_of(&frames, "frames_recurse", 1);
        select(&mut scenario, depth_one).await;
        assert_eq!(integer_variable(&scenario, "depth").await, 1, "{variant}");

        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{variant}"
        );
        let after = backtrace(&scenario).await;
        assert_eq!(
            function_name(&after[0]),
            Some("frames_recurse"),
            "{variant}"
        );
        assert_eq!(
            after.len(),
            frames.len() - depth_one - 1,
            "{variant}: the selected frame and every frame it called returned"
        );
        assert_eq!(integer_variable(&scenario, "depth").await, 2, "{variant}");

        // Optimized code may attribute the return address to another line.
        if variant == "gcc-o0" {
            assert_eq!(
                after[0].source.as_ref().map(|source| source.line.get()),
                Some(frames_line(
                    "int64_t below = frames_recurse(depth - 1, seed);"
                )),
            );
        }

        assert_eq!(
            scenario.snapshot().await.selected_frame,
            Some(StackFrameId::INNERMOST)
        );
        scenario.shutdown().await;
    }

    // A physical frame whose inline callee is selectable separately.
    let mut scenario = stop_in_leaf("gcc-o0").await;
    let frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&frames, "frames_keep", 0)).await;
    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    assert_eq!(innermost_function(&scenario).await, "frames_recurse");
    assert_eq!(integer_variable(&scenario, "depth").await, 0);
    assert_eq!(
        backtrace(&scenario).await[0]
            .source
            .as_ref()
            .map(|source| source.line.get()),
        Some(frames_line("return frames_keep(seed) + level;"))
    );

    // Leaving main returns into the C library, which has no source, so the
    // step runs on to the exit.
    let frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&frames, "main", 0)).await;
    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn finish_leaves_an_inline_frame_of_the_innermost_activation() {
    for fixture in ["variables-inline-gcc-o1", "variables-inline-clang-o1"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("inline_target").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let frames = backtrace(&scenario).await;
        assert_eq!(
            function_name(&frames[0]),
            Some("inline_target"),
            "{fixture}"
        );
        let caller = level_of(&frames, "inline_caller", 0);
        assert_eq!(
            caller, 1,
            "{fixture}: the caller is the inline frame's activation"
        );
        select(&mut scenario, caller).await;
        assert_eq!(
            integer_variable(&scenario, "caller_local").await,
            8,
            "{fixture}"
        );
        assert!(
            matches!(
                scenario.handle().variable("inline_local").await,
                Err(Error::VariableNotFound(_))
            ),
            "{fixture}: the inline callee's locals are out of the caller's scope"
        );

        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        assert_eq!(innermost_function(&scenario).await, "main", "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn stepping_from_a_selected_outer_frame_is_explicit() {
    let mut scenario = stop_in_leaf("gcc-o0").await;
    let frames = backtrace(&scenario).await;

    // Only stepping out names a frame other than the innermost one.
    let snapshot = scenario.snapshot().await;
    let (stop, thread) = (
        snapshot.stop_id.expect("stopped"),
        snapshot
            .selected
            .and_then(microscope::ExecutionContext::as_thread)
            .expect("selected thread"),
    );
    for kind in [
        StepKind::Instruction,
        StepKind::IntoSource,
        StepKind::OverSource,
    ] {
        assert!(matches!(
            scenario
                .handle()
                .start_step(
                    stop,
                    thread,
                    frames[2].id,
                    kind,
                    ResumeScope::Thread(thread),
                    ExceptionDisposition::Pass
                )
                .await,
            Err(Error::FrameStepUnsupported(_))
        ));
    }

    assert_eq!(scenario.snapshot().await.stop_id, Some(stop), "nothing ran");

    // An inline frame of an outer activation has no return of its own to
    // run to.
    select(&mut scenario, level_of(&frames, "frames_inlined", 0)).await;
    assert!(matches!(
        scenario.handle().step(StepKind::Out).await,
        Err(Error::FrameStepUnsupported(_))
    ));

    // Every other step begins at the innermost frame.
    select(&mut scenario, level_of(&frames, "frames_recurse", 2)).await;
    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    let after = backtrace(&scenario).await;
    assert_eq!(function_name(&after[0]), Some("frames_leaf"));
    assert_eq!(
        after[0].source.as_ref().map(|source| source.line.get()),
        Some(frames_line("if (frames_crash) {"))
    );
    assert_eq!(after.len(), frames.len());
    scenario.shutdown().await;

    // Stepping out applies only to frames of the main executable.
    let mut scenario = Scenario::launch("module-frames-gcc-o0");
    scenario.add_breakpoint("module_callback").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&frames, "dso_apply", 0)).await;
    assert!(matches!(
        scenario.handle().step(StepKind::Out).await,
        Err(Error::FrameStepUnsupported(_))
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_caller_frames_local_can_be_watched_until_its_activation_ends() {
    let mut scenario = stop_in_leaf("gcc-o0").await;
    let frames = backtrace(&scenario).await;

    // Not visible in the stopped frame, only in the selected caller.
    assert!(matches!(
        scenario.handle().variable("below").await,
        Err(Error::VariableNotFound(_))
    ));
    let depth_one = level_of(&frames, "frames_recurse", 1);
    select(&mut scenario, depth_one).await;
    let below = microscope::Expression::parse("below").expect("expression");
    let target = scenario
        .operation(
            "resolve below",
            scenario.handle().resolve_watch_target(&below),
        )
        .await;
    let WatchScope::Frame { activation, .. } = *target.scope() else {
        panic!("a local is frame-scoped: {:?}", target.scope());
    };

    assert!(target.address() < activation);

    // The same name in a deeper activation is other storage, bounded by
    // that activation, on x86-64's downward-growing stack.
    select(&mut scenario, level_of(&frames, "frames_recurse", 0)).await;
    let inner = scenario
        .operation(
            "resolve inner below",
            scenario.handle().resolve_watch_target(&below),
        )
        .await;
    let WatchScope::Frame {
        activation: inner_activation,
        ..
    } = *inner.scope()
    else {
        panic!("a local is frame-scoped: {:?}", inner.scope());
    };

    assert!(inner.address() < target.address());
    assert!(inner_activation < activation);
    assert!(target.address() > inner_activation);
    select(&mut scenario, depth_one).await;
    let watchpoint = scenario
        .operation(
            "watch below",
            scenario.handle().watch(&below, WatchAccess::Write),
        )
        .await;
    assert_eq!(watchpoint.address, target.address());

    // depth 1 stores what depth 0 returned: frames_keep's 149 plus 5.
    let reason = scenario.resume_to_stop().await;
    let StopReason::Watchpoint { hits } = &reason else {
        panic!("expected the store to below, got {reason:?}");
    };

    assert_eq!(hits.len(), 1, "{reason:?}");
    assert_eq!(
        hits[0].current.as_deref(),
        Some(&154_i64.to_le_bytes()[..]),
        "{reason:?}"
    );
    assert_eq!(innermost_function(&scenario).await, "frames_recurse");
    assert_eq!(integer_variable(&scenario, "depth").await, 1);

    // Once depth 1 returns, its storage belongs to nothing being watched:
    // the next stop, whatever stops first, ends the watchpoint.
    scenario
        .add_source_breakpoint("frames.c", frames_line("frames_sink = total;"))
        .await;
    let mut events = scenario.handle().subscribe();
    let reason = match scenario.resume_to_stop().await {
        StopReason::WatchpointInvalidated { invalidated } => {
            assert_eq!(invalidated.len(), 1, "{invalidated:?}");
            invalidated[0].reason
        }
        StopReason::Breakpoint { .. } => std::iter::from_fn(|| events.try_recv().ok())
            .find_map(|event| match event {
                DebuggerEvent::WatchpointsInvalidated { invalidated, .. } => {
                    invalidated.first().map(|entry| entry.reason)
                }
                _ => None,
            })
            .expect("the stop after depth 1 returned invalidated the watchpoint"),
        other => panic!("expected the watchpoint to end, got {other:?}"),
    };

    assert_eq!(reason, WatchpointInvalidation::ScopeExited);
    assert!(scenario.snapshot().await.watchpoints.is_empty());
    scenario.shutdown().await;
}

#[tokio::test]
async fn shared_library_and_c_library_caller_frames_are_selectable() {
    let mut scenario = Scenario::launch("module-frames-gcc-o0");
    scenario.add_breakpoint("compare_values").await;
    scenario.add_breakpoint("module_callback").await;

    // qsort calls the comparison from the C library, which has no debug
    // information; its frames still lead to the main image's caller.
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let frames = backtrace(&scenario).await;
    let sort = level_of(&frames, "sort_values", 0);
    assert!(sort > 1, "{frames:#?}");
    select(&mut scenario, 1).await;
    assert!(matches!(
        scenario.handle().variables().await,
        Err(Error::VariableContextUnsupported)
    ));
    assert!(matches!(
        scenario.handle().current_location().await,
        Ok(location) if location.image.source.is_none()
    ));

    // Globals stay visible from any frame.
    assert_eq!(integer_variable(&scenario, "frames_sink").await, 0);
    select(&mut scenario, sort).await;
    let values = scenario
        .operation("values", scenario.handle().variable("values"))
        .await;
    let VariableState::Available {
        children: ValueChildren::Available(reference),
        ..
    } = values.state
    else {
        panic!("values is {:?}", values.state);
    };

    let page = scenario
        .operation(
            "values elements",
            scenario
                .handle()
                .value_children(reference, ValueChildQuery::default()),
        )
        .await;
    let mut elements = page
        .children
        .iter()
        .map(|child| match &child.state {
            VariableState::Available { value, .. } => integer(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    elements.sort();
    assert_eq!(
        elements,
        [Some(1), Some(2), Some(3)],
        "qsort permutes in place"
    );

    // The shared library's frame reads its own locals and source.
    scenario.remove_all_breakpoints().await;
    scenario.add_breakpoint("module_callback").await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(integer_variable(&scenario, "value").await, 42);
    let frames = backtrace(&scenario).await;
    let library = level_of(&frames, "dso_apply", 0);
    select(&mut scenario, library).await;
    assert_eq!(integer_variable(&scenario, "value").await, 41);
    assert_eq!(integer_variable(&scenario, "adjusted").await, 42);
    let callback = integer_variable(&scenario, "callback").await;
    let module_callback = scenario
        .operation(
            "module_callback",
            scenario.handle().runtime_address("module_callback"),
        )
        .await;
    assert_eq!(callback, i128::from(module_callback.get()));
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(Some(location.module), frames[library].module);
    assert_ne!(location.module, frames[0].module.expect("main module"));
    let source = scenario
        .operation("source", scenario.handle().source_context(0))
        .await;
    assert!(
        source.path.ends_with("module-frames/library.c"),
        "{source:?}"
    );
    assert_eq!(
        source.line.get(),
        fixture_line(
            "tests/fixtures/c/module-frames/library.c",
            "return callback(adjusted) + 1;"
        )
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn revealing_an_inline_frame_without_running_selects_the_innermost_frame() {
    let mut scenario = Scenario::launch("inline-gcc-o2");
    scenario.add_breakpoint("caller").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let stop = scenario.snapshot().await.stop_id;
    let frames = backtrace(&scenario).await;
    select(&mut scenario, level_of(&frames, "main", 0)).await;

    // Stepping into the inline call hidden at the stop publishes a new stop
    // without running the inferior.
    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let snapshot = scenario.snapshot().await;
    assert_ne!(snapshot.stop_id, stop);
    assert_eq!(snapshot.selected_frame, Some(StackFrameId::INNERMOST));
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(
        location
            .image
            .function
            .map(|function| function.name.to_string()),
        Some("middle".to_owned())
    );
    scenario.shutdown().await;
}
