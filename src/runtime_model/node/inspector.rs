//! The methods of V8's inspector protocol that the debugger sends, and the
//! events it reads, as Node's inspector names them.

/// Lets a node the launch preload holds in `waitForDebugger` run on.
pub const RELEASE: &str = "Runtime.runIfWaitingForDebugger";
