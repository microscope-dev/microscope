use microscope::{Backtrace, LoadedModuleSnapshot, UnwindTermination};

use crate::support::frame_modules;

pub fn check_thread(
    context: &str,
    main: bool,
    trace: &Backtrace,
    modules: &LoadedModuleSnapshot,
) -> (usize, usize) {
    let frames = frame_modules(trace, modules);
    assert_eq!(
        trace.termination,
        UnwindTermination::Complete,
        "{context}: {frames:#?}"
    );
    if main {
        let start = trace.frames.last().expect("frames");
        assert_eq!(
            start.symbol.as_ref().map(|symbol| &*symbol.name),
            Some("_start"),
            "{context}: {frames:#?}"
        );
    }
    let compiled = frames
        .iter()
        .filter(|(module, _)| module.starts_with("[V8 code "))
        .count();
    let interpreted = trace
        .frames
        .iter()
        .zip(&frames)
        .filter(|(frame, (module, _))| {
            frame.symbol.as_ref().map(|symbol| &*symbol.name)
                == Some("Builtins_InterpreterEntryTrampoline")
                || (!module.starts_with("[V8 code ")
                    && frame.recorded.as_ref().is_some_and(|recorded| {
                        recorded
                            .place
                            .as_ref()
                            .is_some_and(|place| place.path.contains("/tests/fixtures/node/"))
                    }))
        })
        .count();
    (compiled, interpreted)
}
