pub use super::exceptions::{filter as exception_filter, pause_state};

pub const RELEASE: &str = "Runtime.runIfWaitingForDebugger";

pub const ENABLE_RUNTIME: &str = "NodeRuntime.enable";

pub const WAITING: &str = "NodeRuntime.waitingForDebugger";

pub const NOTIFY_END: &str = "NodeRuntime.notifyWhenWaitingForDisconnect";

pub const END: &str = "NodeRuntime.waitingForDisconnect";

pub const ENABLE_DEBUGGER: &str = "Debugger.enable";

pub const ENABLE_WORKERS: &str = "NodeWorker.enable";
pub const WORKER_ATTACHED: &str = "NodeWorker.attachedToWorker";
pub const WORKER_DETACHED: &str = "NodeWorker.detachedFromWorker";
pub const SET_BREAKPOINT: &str = "Debugger.setBreakpointByUrl";
pub const REMOVE_BREAKPOINT: &str = "Debugger.removeBreakpoint";

pub const SET_BREAKPOINT_AT: &str = "Debugger.setBreakpoint";

pub const POSSIBLE_BREAKPOINTS: &str = "Debugger.getPossibleBreakpoints";

pub const SCRIPT_SOURCE: &str = "Debugger.getScriptSource";

pub const ENABLE_PROFILER: &str = "Profiler.enable";

pub const COVERAGE: &str = "Profiler.getBestEffortCoverage";

pub const RESOLVED: &str = "Debugger.breakpointResolved";

pub const PAUSED: &str = "Debugger.paused";

pub const BREAK_ON_START: &str = "Break on start";
pub const RESUME: &str = "Debugger.resume";

pub const RESUMED: &str = "Debugger.resumed";

pub const PAUSE: &str = "Debugger.pause";
pub const STEP_INTO: &str = "Debugger.stepInto";
pub const STEP_OVER: &str = "Debugger.stepOver";
pub const STEP_OUT: &str = "Debugger.stepOut";

pub const SCRIPT_PARSED: &str = "Debugger.scriptParsed";

pub const GET_PROPERTIES: &str = "Runtime.getProperties";

pub fn script_path(url: &str) -> std::borrow::Cow<'_, str> {
    super::frames::file_url_path(url)
        .map_or(std::borrow::Cow::Borrowed(url), std::borrow::Cow::Owned)
}

pub const PAUSE_ON_EXCEPTIONS: &str = "Debugger.setPauseOnExceptions";
