//! A stack frame's name and place in its program's source, as every front
//! end shows them. A frame whose code is in no image, as an interpreted
//! frame's, has the function and place its runtime recorded instead.

use std::collections::BTreeSet;
use std::path::PathBuf;

use microscope::{
    Backtrace, CodeRole, FrameKind, ModuleImage, SourceLanguage, StackFrame, StackSegment,
};

use crate::cli::format;

/// Where a frame is in its program's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePlace {
    pub path: PathBuf,

    /// The one-based line and column; a column only where it is known.
    pub line: u64,
    pub column: Option<u64>,
}

/// Where `frame` is: the place its runtime recorded, or else its source
/// location in `image`, its module's image.
pub fn place(frame: &StackFrame, image: Option<&ModuleImage>) -> Option<FramePlace> {
    if let Some(recorded) = &frame.recorded {
        return recorded.place.as_ref().map(|place| FramePlace {
            path: PathBuf::from(&*place.path),
            line: u64::from(place.line),
            column: (place.column != 0).then_some(u64::from(place.column)),
        });
    }

    let location = frame.source.as_ref()?;
    let file = image?.source_file(location.file)?;
    Some(FramePlace {
        path: file.path.to_path_buf(),
        line: location.line.get(),
        column: location.column.map(microscope::ColumnNumber::get),
    })
}

/// What `frame` runs: the function its runtime recorded, its function, or
/// its symbol, or `<unknown>`. A function its runtime recorded with no
/// name, such as a module's top level, is `<anonymous>`.
pub fn code(frame: &StackFrame) -> String {
    frame.recorded.as_ref().map_or_else(
        || format::code_name(frame.function.as_ref(), frame.symbol.as_ref()),
        |recorded| {
            if recorded.function.is_empty() {
                "<anonymous>".to_owned()
            } else {
                recorded.function.to_string()
            }
        },
    )
}

/// Whether anything names what `frame` runs.
pub const fn is_named(frame: &StackFrame) -> bool {
    frame.recorded.is_some() || frame.function.is_some() || frame.symbol.is_some()
}

/// A frame's name: the future it awaits, what it runs, or else its address
/// and module, from `image`, its module's image.
pub fn name(frame: &StackFrame, image: Option<&ModuleImage>) -> String {
    if let Some(awaited) = format::awaited_frame(image, frame) {
        return awaited;
    }

    if is_named(frame) {
        return code(frame);
    }

    let module = image
        .and_then(|image| image.path().file_name())
        .map(|name| format!(" in {}", name.to_string_lossy()))
        .unwrap_or_default();

    match (frame.kind, frame.instruction) {
        (_, Some(address)) => format!("{address:#x}{module}"),
        (FrameKind::Interpreted { .. }, None) => format!("an interpreted frame{module}"),
        (_, None) => format!("a suspended frame{module}"),
    }
}

/// The note that frames a frame drives are not shown in full, naming the
/// frame `driver`, as `#3` or `the next frame`.
pub fn unfollowed(note: &microscope::Unfollowed, driver: &str) -> String {
    match note.driven {
        microscope::DrivenFrames::Future => format!(
            "the future {driver} drives is not shown in full: {}",
            note.reason
        ),
        microscope::DrivenFrames::Interpreted => format!(
            "the interpreted frames {driver} runs are not shown in full: {}",
            note.reason
        ),
        microscope::DrivenFrames::Awaiting => format!(
            "the async callers awaiting what {driver} resumed are not shown in full: {}",
            note.reason
        ),
    }
}

/// The language of the code a frame runs, as a short mark and its name,
/// where its function or the interpreter that runs it says.
pub fn language(frame: &StackFrame) -> Option<(&'static str, &'static str)> {
    let language = frame
        .recorded
        .as_ref()
        .map(|recorded| recorded.language)
        .or_else(|| frame.function.as_ref().map(|function| function.language))?;
    Some(match language {
        SourceLanguage::C => ("c", "C"),
        SourceLanguage::Cpp => ("c++", "C++"),
        SourceLanguage::Rust => ("rs", "Rust"),
        SourceLanguage::Go => ("go", "Go"),
        SourceLanguage::Zig => ("zig", "Zig"),
        SourceLanguage::Odin => ("odin", "Odin"),
        SourceLanguage::Fortran => ("f", "Fortran"),
        SourceLanguage::D => ("d", "D"),
        SourceLanguage::Nim => ("nim", "Nim"),
        SourceLanguage::Ada => ("ada", "Ada"),
        SourceLanguage::Python => ("py", "Python"),
        SourceLanguage::JavaScript => ("js", "JavaScript"),
        SourceLanguage::TypeScript => ("ts", "TypeScript"),
        _ => return None,
    })
}

/// Which of a backtrace's frames its folds hide, by index, with the
/// `selected` frame shown.
pub fn folded_frames(trace: &Backtrace, selected: u32) -> Vec<bool> {
    let mut folded = vec![false; trace.frames.len()];
    for run in folds(trace, trace.frames.len(), selected) {
        folded[run].fill(true);
    }

    folded
}

/// The runs of a runtime's frames a backtrace folds into a line each, by
/// index, of its first `shown` frames: two or more frames in a row on one
/// stack of its machinery, an interpreter's included, its dispatch, and
/// the wrappers between them, at least one the runtime's own; and every
/// frame past the innermost dispatch, which runs the runtime's code for
/// the thread rather than the task's. Where the stack stopped, unless an
/// interpreter's machinery beyond which another frame is selected, and the
/// `selected` frame, are always shown.
pub fn folds(trace: &Backtrace, shown: usize, selected: u32) -> Vec<std::ops::Range<usize>> {
    let frames = &trace.frames[..shown];

    // A frame that drives a future is shown with it, or with why it is not,
    // and so is every async caller suspended in await.
    let mut kept = trace
        .frames
        .windows(2)
        .filter(|pair| {
            pair[0].segment == StackSegment::Future && pair[1].segment != StackSegment::Future
        })
        .map(|pair| pair[1].level)
        .chain(trace.unfollowed.iter().map(|future| future.driver.get()))
        .chain(
            frames
                .iter()
                .filter(|frame| frame.segment == StackSegment::Awaiting)
                .map(|frame| frame.level),
        )
        .collect::<BTreeSet<_>>();
    kept.insert(selected);

    let dispatch = frames
        .iter()
        .position(|frame| frame.role == CodeRole::Dispatch)
        .filter(|at| frames[*at].level >= selected);
    let Some(dispatch) = dispatch else {
        return machinery_runs(frames, &kept);
    };

    let mut runs = machinery_runs(&frames[..=dispatch], &kept);

    // Past it, a kept frame is shown between the runs on either side.
    let mut start = dispatch + 1;
    while start < frames.len() {
        let end = (start..frames.len())
            .find(|at| kept.contains(&frames[*at].level))
            .unwrap_or(frames.len());
        if end - start >= 2 {
            // The dispatch frame joins the run that ends at it.
            match runs.last_mut() {
                Some(run) if run.end == start => run.end = end,
                _ => runs.push(start..end),
            }
        }

        start = end + 1;
    }

    runs
}

/// The runs [`folds`] folds by role alone.
fn machinery_runs(frames: &[StackFrame], kept: &BTreeSet<u32>) -> Vec<std::ops::Range<usize>> {
    let runtime = |frame: &StackFrame| {
        matches!(
            frame.role,
            CodeRole::RuntimeInternal | CodeRole::Dispatch | CodeRole::Interpreter
        )
    };
    // Where the stack stopped shows, unless it is an interpreter's
    // machinery, such as the debugger's hook in a runtime's agent, beyond
    // which the stop selected the frame it is about.
    let folds = |frame: &StackFrame| {
        (frame.level != 0 || frame.role == CodeRole::Interpreter)
            && !kept.contains(&frame.level)
            && (runtime(frame) || frame.role == CodeRole::Wrapper)
    };

    let mut runs = Vec::new();
    let mut start = 0;

    while start < frames.len() {
        let mut end = start;
        while end < frames.len()
            && folds(&frames[end])
            && frames[end].segment == frames[start].segment
        {
            end += 1;
        }

        if end - start >= 2 && frames[start..end].iter().any(runtime) {
            runs.push(start..end);
        }

        start = end.max(start + 1);
    }

    runs
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use microscope::{RecordedFrame, RecordedPlace, StackFrameId, StackSegment, VirtualAddress};

    use super::*;

    fn interpreted(place: Option<RecordedPlace>) -> StackFrame {
        StackFrame {
            id: StackFrameId::INNERMOST,
            level: 0,
            kind: FrameKind::Interpreted {
                frame: VirtualAddress::new(0x1000),
            },
            module: None,
            instruction: None,
            segment: StackSegment::Thread,
            code_instance: None,
            function: None,
            source: None,
            symbol: None,
            role: microscope::CodeRole::Ordinary,
            awaiting: None,
            recorded: Some(RecordedFrame {
                function: "Worker.run".into(),
                language: microscope::SourceLanguage::Python,
                place,
                first_line: Some(1),
                generated: None,
                problem: None,
            }),
        }
    }

    /// A stack stopped in an interpreter's machinery, such as the hook of
    /// the debugger's agent, folds it when the stop selects the program's
    /// frame beyond it, and shows it when it is selected; a stack stopped in
    /// a runtime's own code always shows where.
    #[test]
    fn where_a_stack_stopped_folds_only_as_machinery_beyond_the_selection() {
        let frame = |level, role| StackFrame {
            level,
            role,
            ..interpreted(None)
        };
        let trace = |roles: &[CodeRole]| Backtrace {
            frames: (0..)
                .zip(roles)
                .map(|(level, role)| frame(level, *role))
                .collect::<Vec<_>>()
                .into(),
            context: microscope::ExecutionContext::Thread(microscope::ThreadId::new(1)),
            termination: microscope::UnwindTermination::Complete,
            unfollowed: Arc::from([]),
        };
        let hook = trace(&[
            CodeRole::Interpreter,
            CodeRole::Interpreter,
            CodeRole::Interpreter,
            CodeRole::Ordinary,
            CodeRole::Ordinary,
        ]);
        assert_eq!(folds(&hook, 5, 3), vec![0..3]);
        assert_eq!(folds(&hook, 5, 0), vec![1..3]);
        assert_eq!(folds(&hook, 5, 1), Vec::<std::ops::Range<usize>>::new());

        let runtime = trace(&[
            CodeRole::RuntimeInternal,
            CodeRole::RuntimeInternal,
            CodeRole::RuntimeInternal,
            CodeRole::Ordinary,
        ]);
        assert_eq!(folds(&runtime, 4, 3), vec![1..3]);
    }

    /// The frames past the innermost dispatch fold, but a frame that
    /// drives a future there, as a `block_on` inside a task makes, is
    /// shown between the runs on either side of it.
    #[test]
    fn a_future_driver_below_the_dispatch_is_shown() {
        let frame = |level, role, segment| StackFrame {
            level,
            role,
            segment,
            ..interpreted(None)
        };
        let (thread, future) = (StackSegment::Thread, StackSegment::Future);
        let trace = Backtrace {
            frames: [
                (CodeRole::Ordinary, thread),
                (CodeRole::Dispatch, thread),
                (CodeRole::RuntimeInternal, thread),
                (CodeRole::Ordinary, future),
                (CodeRole::RuntimeInternal, thread),
                (CodeRole::RuntimeInternal, thread),
                (CodeRole::RuntimeInternal, thread),
            ]
            .into_iter()
            .zip(0..)
            .map(|((role, segment), level)| frame(level, role, segment))
            .collect::<Vec<_>>()
            .into(),
            context: microscope::ExecutionContext::Thread(microscope::ThreadId::new(1)),
            termination: microscope::UnwindTermination::Complete,
            unfollowed: Arc::from([]),
        };
        assert_eq!(folds(&trace, 7, 0), vec![2..4, 5..7]);
    }

    #[test]
    fn a_recorded_frame_is_named_and_placed_as_its_runtime_recorded_it() {
        let frame = interpreted(Some(RecordedPlace {
            path: Arc::from("/srv/app/worker.py"),
            line: 12,
            column: 0,
        }));
        assert_eq!(name(&frame, None), "Worker.run");
        assert_eq!(
            place(&frame, None),
            Some(FramePlace {
                path: "/srv/app/worker.py".into(),
                line: 12,
                column: None,
            })
        );

        let unplaced = interpreted(None);
        assert_eq!(place(&unplaced, None), None);
        let unnamed = StackFrame {
            recorded: None,
            ..unplaced
        };
        assert_eq!(name(&unnamed, None), "an interpreted frame");
    }
}
