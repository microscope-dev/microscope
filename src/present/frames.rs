//! A stack frame's name and place in its program's source, as every front
//! end shows them. A frame whose code is in no image, as an interpreted
//! frame's, has the function and place its runtime recorded instead.

use std::path::PathBuf;

use microscope::{FrameKind, ModuleImage, StackFrame};

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
/// its symbol, or `<unknown>`.
pub fn code(frame: &StackFrame) -> String {
    frame.recorded.as_ref().map_or_else(
        || format::code_name(frame.function.as_ref(), frame.symbol.as_ref()),
        |recorded| recorded.function.to_string(),
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
    }
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
                place,
            }),
        }
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
