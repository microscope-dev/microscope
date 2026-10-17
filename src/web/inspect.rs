//! Requests that read the program: stacks at a stop and source files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use microscope::{
    DebuggerHandle, ExecutionContext, FrameKind, LineNumber, StackFrameId, StopContext,
    UnwindTermination,
};

use super::describe::{Images, hex};
use super::protocol::{
    self, Backtrace, ErrorKind, Frame, FrameLanguage, FrameRun, FunctionMatch, FunctionQuery,
    Functions, SourceFiles, SourceLine, SourceText,
};

use super::session::Failure;
use crate::cli::format;
use crate::cli::terminal::Renderer;

/// A thread or task as a message names it.
pub fn executes(execution: ExecutionContext) -> String {
    match execution {
        ExecutionContext::Thread(thread) => format!("thread {thread}"),
        ExecutionContext::Task(task) => format!("task {}", task.number),
    }
}

/// The context of a thread's or task's innermost frame at a stop.
pub const fn innermost(stop: u64, execution: ExecutionContext) -> StopContext {
    StopContext {
        stop: microscope::StopId::new(stop),
        execution,
        frame: StackFrameId::INNERMOST,
    }
}

/// The context of a thread's or task's frame numbered `frame`, counting
/// from the innermost, at a stop.
pub async fn context(
    handle: &DebuggerHandle,
    stop: u64,
    execution: ExecutionContext,
    frame: u32,
) -> Result<StopContext, Failure> {
    let mut context = innermost(stop, execution);
    if frame == 0 {
        return Ok(context);
    }

    let trace = handle.at(context).backtrace().await?;
    context.frame = trace
        .frames
        .iter()
        .find(|candidate| candidate.id.get() == frame)
        .map(|candidate| candidate.id)
        .ok_or_else(|| {
            Failure::new(
                ErrorKind::Invalid,
                format!(
                    "{} has no frame {frame} at stop {stop}",
                    executes(execution)
                ),
            )
        })?;
    Ok(context)
}

pub async fn backtrace(
    handle: &DebuggerHandle,
    images: &Images,
    at: protocol::ThreadAt,
) -> Result<Backtrace, Failure> {
    let trace = handle
        .at(innermost(at.stop, at.execution()))
        .backtrace()
        .await?;
    let mut frames = Vec::with_capacity(trace.frames.len());
    for frame in trace.frames.iter() {
        frames.push(frame_of(images, &trace, frame).await);
    }

    let folds = crate::present::frames::folds(&trace, trace.frames.len(), 0)
        .into_iter()
        .filter_map(|run| {
            Some(FrameRun {
                first: u32::try_from(run.start).ok()?,
                count: u32::try_from(run.len()).ok()?,
            })
        })
        .collect();
    Ok(Backtrace {
        frames,
        folds,
        incomplete: (trace.termination != UnwindTermination::Complete)
            .then(|| trace.termination.to_string()),
    })
}

/// One frame of a stack, as the page shows it.
async fn frame_of(
    images: &Images,
    trace: &microscope::Backtrace,
    frame: &microscope::StackFrame,
) -> Frame {
    let image = match frame.module {
        Some(module) => images.get(module).await,
        None => None,
    };

    let source = crate::present::frames::place(frame, image.as_deref()).map(|place| SourceLine {
        path: place.path.display().to_string(),
        line: place.line,
        column: place.column,
    });
    Frame {
        index: frame.id.get(),
        name: crate::present::frames::name(frame, image.as_deref()),
        kind: match frame.kind {
            FrameKind::Physical => protocol::FrameKind::Physical,
            FrameKind::Inline => protocol::FrameKind::Inline,
            FrameKind::Signal => protocol::FrameKind::Signal,
            FrameKind::TailCall => protocol::FrameKind::TailCall,
            FrameKind::Async { .. } => protocol::FrameKind::Async,
            FrameKind::Awaited { .. } => protocol::FrameKind::Awaited,
            FrameKind::Interpreted { .. } => protocol::FrameKind::Interpreted,
        },
        address: frame.instruction.map(|address| hex(address.get())),
        module: image
            .as_ref()
            .and_then(|image| image.path().file_name())
            .map(|name| name.to_string_lossy().into_owned()),
        source,
        language: crate::present::frames::language(frame).map(|(mark, name)| FrameLanguage {
            mark: mark.to_owned(),
            name: name.to_owned(),
        }),
        unfollowed: trace
            .unfollowed
            .iter()
            .find(|note| note.driver == frame.id)
            .map(|note| crate::present::frames::unfollowed(note, "the next frame")),
    }
}

pub async fn sources(images: &Images) -> SourceFiles {
    let images = images.with_sources().await;
    let mut files = BTreeSet::new();
    for image in &images {
        for file in image.source_files() {
            files.insert(file.path.display().to_string());
        }
    }

    // The executable's image comes first; Go names its main function
    // main.main.
    let entry = images.first().and_then(|image| {
        let main = ["main", "main.main"]
            .into_iter()
            .find_map(|name| image.function_named(name).ok())?;
        let declared = main.declaration()?;
        Some(SourceLine {
            path: image.source_file(declared.file)?.path.display().to_string(),
            line: declared.line.get(),
            column: None,
        })
    });
    SourceFiles {
        files: files.into_iter().collect(),
        entry,
    }
}

/// Functions whose names hold `query`, ASCII case aside: whole names first,
/// then those it begins, holds, or holds the letters of in order, and
/// shorter names before longer ones.
pub async fn functions(images: &Images, query: &FunctionQuery) -> Result<Functions, Failure> {
    let wanted = query.query.trim().to_ascii_lowercase();
    let limit = query.limit.map_or(50, |limit| limit.min(200)) as usize;
    if wanted.is_empty() {
        return Ok(Functions {
            functions: Vec::new(),
            more: false,
        });
    }

    let images = images.with_sources().await;

    // A short query matches most of a large program, so candidates are
    // ranked before any of them is described.
    let mut candidates = Vec::new();
    let mut lower = String::new();
    for (module, image) in images.iter().enumerate() {
        for function in image.functions() {
            lower.clear();
            lower.extend(
                function
                    .name()
                    .chars()
                    .map(|char| char.to_ascii_lowercase()),
            );
            if let Some(rank) = rank(&lower, &wanted) {
                candidates.push((
                    rank,
                    function.name().len(),
                    function.name(),
                    module,
                    function.id(),
                ));
            }
        }
    }

    // Only the best few are sorted: the rest only when repeated declarations
    // of the same functions took all of their places.
    let keep = limit.saturating_mul(8).max(1).min(candidates.len());
    if keep < candidates.len() {
        candidates.select_nth_unstable(keep);
    }

    candidates[..keep].sort_unstable();
    let mut found = Vec::new();
    let mut more = false;
    for position in 0..candidates.len() {
        if position == keep {
            candidates[keep..].sort_unstable();
        }

        let (_, _, _, module, id) = candidates[position];
        let image = &images[module];
        let Some(function) = image.function(id) else {
            return Err(Failure::new(
                ErrorKind::Failed,
                "internal: a ranked candidate names no function",
            ));
        };

        let declared = function.declaration();
        let described = FunctionMatch {
            name: function.name().to_string(),
            path: declared
                .as_ref()
                .and_then(|declared| image.source_file(declared.file))
                .map(|file| file.path.display().to_string()),
            line: declared.as_ref().map(|declared| declared.line.get()),
        };

        // Declarations repeat a function in each unit that names it.
        if found.contains(&described) {
            continue;
        }

        if found.len() == limit {
            more = true;
            break;
        }

        found.push(described);
    }

    Ok(Functions {
        functions: found,
        more,
    })
}

/// How well `name` matches `query`, both lowercase, best first, or none.
fn rank(name: &str, query: &str) -> Option<u8> {
    match name.find(query) {
        Some(0) if name.len() == query.len() => Some(0),
        Some(0) => Some(1),
        Some(_) => Some(2),
        None => {
            let mut letters = name.chars();
            query
                .chars()
                .all(|wanted| letters.any(|letter| letter == wanted))
                .then_some(3)
        }
    }
}

/// Reads a file the debug information names, with the lines any module
/// can break at.
pub async fn source(
    handle: &DebuggerHandle,
    images: &Images,
    path: &str,
) -> Result<SourceText, Failure> {
    let recorded = Path::new(path);
    let mut found = None;
    let mut breakable = BTreeSet::new();
    let every = FIRST_LINE..=LAST_LINE;
    for image in images.with_sources().await {
        let Some(file) = image
            .source_files()
            .iter()
            .find(|file| file.path.as_path() == recorded)
        else {
            continue;
        };

        breakable.extend(image.breakpoint_lines(file.id, every.clone()));
        found.get_or_insert_with(|| file.clone());
    }

    let file = found.ok_or_else(|| {
        Failure::new(
            ErrorKind::Invalid,
            format!("the debug information names no source file {path}"),
        )
    })?;
    let (read, text) = handle.read_source_file(&file).await?;
    Ok(SourceText {
        path: path.to_owned(),
        read: read.display().to_string(),
        text,
        breakable: breakable.into_iter().map(LineNumber::get).collect(),
    })
}

/// The most tasks a list holds, each of which costs a backtrace.
const TASK_LIMIT: usize = 512;

const FIRST_LINE: LineNumber = LineNumber::new(1).expect("line one is nonzero");
const LAST_LINE: LineNumber = LineNumber::new(u64::MAX).expect("u64::MAX is nonzero");

/// The program's tasks at a stop, each where the code the program wrote
/// has it, as the CLI's `tasks` says.
pub async fn tasks(
    handle: &DebuggerHandle,
    images: &Images,
    at: protocol::StopAt,
) -> Result<protocol::TaskList, Failure> {
    let stop = microscope::StopId::new(at.stop);
    let page = handle.program_tasks_at(stop, None, TASK_LIMIT).await?;
    let mut tasks = Vec::with_capacity(page.tasks.len());
    for task in page.tasks.iter() {
        let trace = handle
            .at(innermost(at.stop, ExecutionContext::Task(task.id)))
            .backtrace()
            .await;
        let mut known = BTreeMap::new();
        if let Ok(trace) = &trace {
            for module in trace.frames.iter().filter_map(|frame| frame.module) {
                if let Some(image) = images.get(module).await {
                    known.insert(module, image);
                }
            }
        }

        let plain = Renderer::new(false);
        let frame = match &trace {
            Ok(trace) => match trace.user_frame() {
                Some(frame) => Some(frame_of(images, trace, frame).await),
                None => None,
            },
            Err(_) => None,
        };

        tasks.push(protocol::Task {
            frame,
            key: protocol::TaskKey {
                runtime: task.id.runtime.get(),
                number: task.id.number,
            },
            state: match task.state {
                microscope::TaskState::Running => protocol::TaskState::Running,
                microscope::TaskState::Runnable => protocol::TaskState::Runnable,
                microscope::TaskState::Blocked => protocol::TaskState::Blocked,
                microscope::TaskState::Exited => protocol::TaskState::Exited,
                microscope::TaskState::Unknown(_) => protocol::TaskState::Unknown,
            },
            place: format::task_place(task, &trace, &known, plain),
            detail: match &task.state {
                microscope::TaskState::Unknown(reason) => Some(reason.to_string()),
                _ => task.detail.as_deref().map(str::to_owned),
            },
            labels: format::task_labels(task),
            thread: task.thread.map(microscope::ThreadId::get),
        });
    }

    Ok(protocol::TaskList {
        noun: page.tasks.first().map(|task| task.noun.to_owned()),
        tasks,
        more: page.next.is_some(),
        gaps: page.gaps.iter().map(ToString::to_string).collect(),
    })
}
