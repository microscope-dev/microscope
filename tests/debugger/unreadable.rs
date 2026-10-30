//! Debug information the debugger cannot read, or reads only in part: the
//! module is still debugged by what else describes it, and says why.

use super::libraries::pending_function;
use super::*;
use crate::support::{ScratchDir, corrupt_section};

fn unusable_reason(image: &ModuleImage) -> Arc<str> {
    match image.debug_information() {
        microscope::DebugInformation::Unusable { reason } => reason,
        other => panic!("{other:?}"),
    }
}

/// A program whose DWARF is malformed is debugged as its symbols and
/// call-frame information describe it, and its DWARF's reason says where
/// it went wrong: in the unit headers, a unit's abbreviations, or its line
/// program.
#[tokio::test]
async fn malformed_dwarf_leaves_a_program_described_by_its_symbols() {
    let scratch = ScratchDir::new("malformed-dwarf");
    for (section, expected) in [
        (
            ".debug_info",
            "malformed DWARF in the unit header at .debug_info+0x0: ",
        ),
        (
            ".debug_abbrev",
            "malformed DWARF in the abbreviations at .debug_abbrev+0x0 of the unit at \
             .debug_info+0x0: ",
        ),
        (
            ".debug_line",
            "malformed DWARF in the line program at .debug_line+0x0 of the unit at \
             .debug_info+0x0: ",
        ),
    ] {
        let program = scratch.path().join(section.trim_start_matches('.'));
        corrupt_section(&Scenario::fixture("basic"), &program, section);
        let mut scenario = Scenario::new(section, &program);
        let image = scenario.handle().module_image();
        let reason = unusable_reason(&image);
        assert!(reason.starts_with(expected), "{section}: {reason}");
        assert_eq!(image.functions().len(), 0, "{section}");

        // The symbol table places a breakpoint, and call-frame information
        // unwinds from it, with no source line to show.
        let breakpoint = scenario
            .operation(
                "add breakpoint",
                scenario
                    .handle()
                    .add_breakpoint(BreakpointSpec::Function("breakpoint_target".to_owned())),
            )
            .await;

        let reason = scenario.run_to_stop().await;
        let StopReason::Breakpoint { hits, .. } = &reason else {
            panic!("{section}: stopped for {reason:?}");
        };

        assert_eq!(hits[0].breakpoint, breakpoint.id);
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;

        let names = trace
            .frames
            .iter()
            .take(2)
            .map(|frame| {
                assert!(
                    frame.function.is_none() && frame.source.is_none(),
                    "{frame:?}"
                );
                frame.symbol.as_ref().map(|symbol| &*symbol.name)
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [Some("breakpoint_target"), Some("main")],
            "{section}"
        );

        scenario.shutdown().await;
    }
}

/// A library whose DWARF is malformed still loads, described by its
/// symbols, so a breakpoint waiting for one of its functions stops there.
#[tokio::test]
async fn a_library_with_malformed_dwarf_is_described_by_its_symbols() {
    let scratch = ScratchDir::new("malformed-library");
    let program = scratch.path().join("module-frames");
    fs::copy(Scenario::fixture("module-frames-gcc-o0"), &program).expect("copy the program");

    // The program finds its library beside it.
    corrupt_section(
        &Scenario::fixture("libmodule-frames.so"),
        &scratch.path().join("libmodule-frames.so"),
        ".debug_abbrev",
    );
    let mut scenario = Scenario::new("malformed-library", &program);
    let breakpoint = pending_function(&scenario, "dso_apply").await;
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, breakpoint.id);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let frame = &trace.frames[0];
    assert!(frame.function.is_none(), "{frame:?}");
    assert_eq!(
        frame.symbol.as_ref().map(|symbol| &*symbol.name),
        Some("dso_apply")
    );

    let image = scenario
        .operation(
            "library image",
            scenario
                .handle()
                .loaded_module_image(frame.module.expect("a module")),
        )
        .await;

    assert!(
        unusable_reason(&image).starts_with("malformed DWARF in the abbreviations at "),
        "{:?}",
        image.debug_information()
    );

    // The program's own DWARF is whole.
    assert_eq!(
        scenario.handle().module_image().debug_information(),
        microscope::DebugInformation::Loaded
    );

    scenario.shutdown().await;
}

/// Split DWARF is read from the `.dwo` file each skeleton unit names: the
/// unit's variables and types are there, declared in files of the line
/// table the skeleton keeps.
#[tokio::test]
async fn split_dwarf_is_read_from_its_dwo_file() {
    let mut scenario = Scenario::launch("basic-split-dwarf");
    let image = scenario.handle().module_image();
    assert_eq!(
        image.debug_information(),
        microscope::DebugInformation::Loaded
    );

    scenario
        .operation(
            "add breakpoint",
            scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("breakpoint_target".to_owned())),
        )
        .await;

    scenario.run_to_stop().await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    scenario
        .operation(
            "select main",
            scenario.handle().select_frame(trace.frames[1].id),
        )
        .await;
    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    let first = variables
        .variables
        .iter()
        .find(|variable| &*variable.name == "first")
        .unwrap_or_else(|| panic!("no `first` in {:?}", variables.variables));
    assert_eq!(
        first.type_info.as_ref().map(|info| &*info.name),
        Some("uint64_t")
    );
    let declaration = first.declaration.as_ref().expect("a declaration");
    assert_eq!(
        image
            .source_file(declaration.file)
            .and_then(|file| file.path.file_name())
            .and_then(|name| name.to_str()),
        Some("basic.c")
    );

    scenario.shutdown().await;
}
