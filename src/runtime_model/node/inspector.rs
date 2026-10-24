//! The methods of V8's inspector protocol that the debugger sends, and the
//! events it reads, as Node's inspector names them.

/// Lets a node the launch preload holds in `waitForDebugger` run on.
pub const RELEASE: &str = "Runtime.runIfWaitingForDebugger";

/// Asks Node to report the program's end, rather than wait silently for
/// the debugger to leave.
pub const NOTIFY_END: &str = "NodeRuntime.notifyWhenWaitingForDisconnect";

/// The program has ended, and Node waits for the debugger to leave.
pub const END: &str = "NodeRuntime.waitingForDisconnect";

pub const ENABLE_DEBUGGER: &str = "Debugger.enable";
pub const SET_BREAKPOINT: &str = "Debugger.setBreakpointByUrl";
pub const REMOVE_BREAKPOINT: &str = "Debugger.removeBreakpoint";

/// A breakpoint set before its script loaded is placed in it.
pub const RESOLVED: &str = "Debugger.breakpointResolved";

/// V8 paused a thread, which it holds until resumed.
pub const PAUSED: &str = "Debugger.paused";
pub const RESUME: &str = "Debugger.resume";

/// Lists an object's own properties: here, the variables of a paused
/// frame's scope.
pub const GET_PROPERTIES: &str = "Runtime.getProperties";
