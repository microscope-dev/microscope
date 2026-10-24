//! The methods of V8's inspector protocol that the debugger sends, and the
//! events it reads, as Node's inspector names them.

pub use super::exceptions::{filter as exception_filter, pause_state};

/// Lets a node the launch preload holds in `waitForDebugger` run on.
pub const RELEASE: &str = "Runtime.runIfWaitingForDebugger";

/// Asks Node to report when it waits for the debugger, which it does at
/// once if it already waits.
pub const ENABLE_RUNTIME: &str = "NodeRuntime.enable";

/// Node waits for `RELEASE`, which it forgets if it came before the wait.
pub const WAITING: &str = "NodeRuntime.waitingForDebugger";

/// Asks Node to report the program's end, rather than wait silently for
/// the debugger to leave.
pub const NOTIFY_END: &str = "NodeRuntime.notifyWhenWaitingForDisconnect";

/// The program has ended, and Node waits for the debugger to leave.
pub const END: &str = "NodeRuntime.waitingForDisconnect";

pub const ENABLE_DEBUGGER: &str = "Debugger.enable";

/// Reports each worker as it starts, as a session inside the one that
/// asked, holding it until its debugger releases it.
pub const ENABLE_WORKERS: &str = "NodeWorker.enable";
pub const WORKER_ATTACHED: &str = "NodeWorker.attachedToWorker";
pub const WORKER_DETACHED: &str = "NodeWorker.detachedFromWorker";
pub const SET_BREAKPOINT: &str = "Debugger.setBreakpointByUrl";
pub const REMOVE_BREAKPOINT: &str = "Debugger.removeBreakpoint";

/// Sets a breakpoint at one place in one script.
pub const SET_BREAKPOINT_AT: &str = "Debugger.setBreakpoint";

/// Lists the places a breakpoint can be set at.
pub const POSSIBLE_BREAKPOINTS: &str = "Debugger.getPossibleBreakpoints";

/// A script's text.
pub const SCRIPT_SOURCE: &str = "Debugger.getScriptSource";

pub const ENABLE_PROFILER: &str = "Profiler.enable";

/// Every function V8 compiled, by script, with its range.
pub const COVERAGE: &str = "Profiler.getBestEffortCoverage";

/// A breakpoint set before its script loaded is placed in it.
pub const RESOLVED: &str = "Debugger.breakpointResolved";

/// V8 paused a thread, which it holds until resumed.
pub const PAUSED: &str = "Debugger.paused";

/// The reason a worker that waited for its debugger first pauses for.
pub const BREAK_ON_START: &str = "Break on start";
pub const RESUME: &str = "Debugger.resume";

/// V8 left its pause. A pause asked for before this is dropped, as V8
/// takes it while it still holds the thread.
pub const RESUMED: &str = "Debugger.resumed";

/// Pauses running JavaScript at its next statement; it cannot be
/// withdrawn (F19).
pub const PAUSE: &str = "Debugger.pause";
pub const STEP_INTO: &str = "Debugger.stepInto";
pub const STEP_OVER: &str = "Debugger.stepOver";
pub const STEP_OUT: &str = "Debugger.stepOut";

/// V8 parsed a script, which it names by id from then on.
pub const SCRIPT_PARSED: &str = "Debugger.scriptParsed";

/// Lists an object's own properties: here, the variables of a paused
/// frame's scope.
pub const GET_PROPERTIES: &str = "Runtime.getProperties";

/// The path of the script V8 names `url`: a module's `file:` URL decoded,
/// or the name itself, as a `CommonJS` script's path or a `node:` name.
pub fn script_path(url: &str) -> std::borrow::Cow<'_, str> {
    super::frames::file_url_path(url)
        .map_or(std::borrow::Cow::Borrowed(url), std::borrow::Cow::Owned)
}

/// Chooses which exceptions V8 pauses at.
pub const PAUSE_ON_EXCEPTIONS: &str = "Debugger.setPauseOnExceptions";
