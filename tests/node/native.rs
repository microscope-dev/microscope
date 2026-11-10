//! Native backtraces of Node threads: through V8's builtins, which no
//! call-frame information describes, and through the code it compiles for
//! JavaScript, to where the thread began. `stacks.rs`, `frames.rs`, and
//! `debian.rs` hold each thread they stop to it.

use microscope::{Backtrace, LoadedModuleSnapshot, UnwindTermination};

use crate::support::frame_modules;

/// Holds one thread's native backtrace to a complete walk in which every
/// frame lies in a module, and returns how many frames ran V8's compiled
/// code and how many its interpreter.
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
    // Builtins have symbols, not debug information, where the build keeps
    // them; elsewhere an interpreted frame is one outside V8's compiled
    // code that runs a fixture's function.
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
