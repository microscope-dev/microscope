// Generated from src/web/protocol.rs by `cargo test`; do not edit.

export const PROTOCOL_VERSION = 1;

export type Role = "control" | "view";

export type Envelope = { 
/**
 * Chosen by the tab; the answer carries it back.
 */
id: number, } & ({ "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "selectMember", "params": MemberRef } | { "method": "detachMember", "params": MemberRef } | { "method": "step", "params": Step } | { "method": "stepTargets", "params": ThreadAt } | { "method": "jump", "params": Jump } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input } | { "method": "scopes", "params": FrameAt } | { "method": "children", "params": ChildrenOf } | { "method": "evaluate", "params": Evaluate } | { "method": "setValue", "params": SetValue } | { "method": "complete", "params": Complete } | { "method": "console", "params": ConsoleLine } | { "method": "disassemble", "params": Disassemble } | { "method": "readMemory", "params": ReadMemory } | { "method": "writeMemory", "params": WriteMemory } | { "method": "registers", "params": FrameAt } | { "method": "addWatchpoint", "params": AddWatchpoint } | { "method": "editWatchpoint", "params": EditWatchpoint } | { "method": "removeWatchpoint", "params": WatchpointRef } | { "method": "signals" } | { "method": "setSignal", "params": SignalPolicy } | { "method": "exceptions" } | { "method": "setException", "params": ExceptionChoice } | { "method": "modules" } | { "method": "functions", "params": FunctionQuery } | { "method": "tasks", "params": StopAt } | { "method": "draw", "params": Draw } | { "method": "renderer", "params": RendererRef } | { "method": "renderers" } | { "method": "reloadViews" } | { "method": "profile", "params": ProfileRef } | { "method": "profileTree", "params": ProfileQuery } | { "method": "profileFunctions", "params": ProfileQuery } | { "method": "profileTimeline", "params": ProfileTimelineQuery } | { "method": "profileZones", "params": ProfileZonesQuery } | { "method": "profileZoneStats", "params": ProfileZoneStatsQuery } | { "method": "profileLines", "params": ProfileFunctionQuery } | { "method": "profileCode", "params": ProfileFunctionQuery } | { "method": "recordProfile", "params": RecordProfile } | { "method": "stopRecording", "params": ProfileRef } | { "method": "syscallSummary", "params": SyscallSummaryQuery } | { "method": "syscallRate", "params": SyscallRateQuery } | { "method": "syscallHeatmap", "params": SyscallHeatmapQuery } | { "method": "syscallDetail", "params": SyscallDetailQuery } | { "method": "syscallLog", "params": SyscallLogQuery } | { "method": "syscallCall", "params": SyscallCallQuery } | { "method": "syscallLanes", "params": SyscallLanesQuery } | { "method": "syscallFiles", "params": SyscallQuery } | { "method": "syscallNetwork", "params": SyscallQuery } | { "method": "syscallProcesses", "params": SyscallQuery } | { "method": "syscallInFlight", "params": SyscallInFlightQuery });

export type Request = { "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "selectMember", "params": MemberRef } | { "method": "detachMember", "params": MemberRef } | { "method": "step", "params": Step } | { "method": "stepTargets", "params": ThreadAt } | { "method": "jump", "params": Jump } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input } | { "method": "scopes", "params": FrameAt } | { "method": "children", "params": ChildrenOf } | { "method": "evaluate", "params": Evaluate } | { "method": "setValue", "params": SetValue } | { "method": "complete", "params": Complete } | { "method": "console", "params": ConsoleLine } | { "method": "disassemble", "params": Disassemble } | { "method": "readMemory", "params": ReadMemory } | { "method": "writeMemory", "params": WriteMemory } | { "method": "registers", "params": FrameAt } | { "method": "addWatchpoint", "params": AddWatchpoint } | { "method": "editWatchpoint", "params": EditWatchpoint } | { "method": "removeWatchpoint", "params": WatchpointRef } | { "method": "signals" } | { "method": "setSignal", "params": SignalPolicy } | { "method": "exceptions" } | { "method": "setException", "params": ExceptionChoice } | { "method": "modules" } | { "method": "functions", "params": FunctionQuery } | { "method": "tasks", "params": StopAt } | { "method": "draw", "params": Draw } | { "method": "renderer", "params": RendererRef } | { "method": "renderers" } | { "method": "reloadViews" } | { "method": "profile", "params": ProfileRef } | { "method": "profileTree", "params": ProfileQuery } | { "method": "profileFunctions", "params": ProfileQuery } | { "method": "profileTimeline", "params": ProfileTimelineQuery } | { "method": "profileZones", "params": ProfileZonesQuery } | { "method": "profileZoneStats", "params": ProfileZoneStatsQuery } | { "method": "profileLines", "params": ProfileFunctionQuery } | { "method": "profileCode", "params": ProfileFunctionQuery } | { "method": "recordProfile", "params": RecordProfile } | { "method": "stopRecording", "params": ProfileRef } | { "method": "syscallSummary", "params": SyscallSummaryQuery } | { "method": "syscallRate", "params": SyscallRateQuery } | { "method": "syscallHeatmap", "params": SyscallHeatmapQuery } | { "method": "syscallDetail", "params": SyscallDetailQuery } | { "method": "syscallLog", "params": SyscallLogQuery } | { "method": "syscallCall", "params": SyscallCallQuery } | { "method": "syscallLanes", "params": SyscallLanesQuery } | { "method": "syscallFiles", "params": SyscallQuery } | { "method": "syscallNetwork", "params": SyscallQuery } | { "method": "syscallProcesses", "params": SyscallQuery } | { "method": "syscallInFlight", "params": SyscallInFlightQuery };

export type SetName = { name: string, };

export type Share = { role: Role, 
/**
 * The page path the link opens, such as `/s/k7q2`.
 */
to: string, };

export type CompletePath = { 
/**
 * The path typed so far, relative to the server's directory or `~`.
 */
text: string, };

export type Launch = { program: string, arguments: Array<string>, 
/**
 * The program's working directory, instead of the server's.
 */
cwd: string | null, 
/**
 * Variables set in the program's environment, in order.
 */
environment: Array<[string, string]>, 
/**
 * Stop at the program's first instruction when it starts.
 */
stopAtEntry: boolean, 
/**
 * Start the program at once instead of waiting for a continue.
 */
run: boolean, 
/**
 * End a current session to make way. Without it, a current session
 * fails the request with `busy`.
 */
replace: boolean, };

export type Attach = { pid: number, replace: boolean, };

export type OpenCore = { core: string, 
/**
 * The executable that wrote the dump, when the recorded one is wrong.
 */
executable: string | null, replace: boolean, };

export type Continue = { 
/**
 * The stop being continued, which must still be current. Absent to
 * start a program that has not started.
 */
stop: number | null, };

export type Step = { 
/**
 * The stop being stepped from, which must still be current.
 */
stop: number, thread: number, 
/**
 * The task stepped, which runs on the thread.
 */
task?: TaskKey | null, 
/**
 * The frame a step out leaves; every other step starts from the
 * innermost frame.
 */
frame?: number, kind: StepKind, 
/**
 * For a step into, the one call of the line to go into, in
 * hexadecimal, as `stepTargets` lists it; the line's other calls run
 * to their returns.
 */
call?: string | null, };

export type Jump = { 
/**
 * The stop the thread is moved at, which must still be current.
 */
stop: number, thread: number, task?: TaskKey | null, 
/**
 * `FILE:LINE`, or another location a breakpoint takes.
 */
location: string, };

export type StepKind = "over" | "into" | "out" | "instruction" | "overInstruction";

export type SetFocus = { 
/**
 * Absent when the tab looks at nothing in particular.
 */
focus: Focus | null, };

export type Focus = { 
/**
 * The page path and query, such as `/s/k7q2/stop/12/t/41872/f/1`.
 */
url: string, 
/**
 * Such as `frame 1, serve_conn`.
 */
label: string, };

export type ThreadAt = { stop: number, thread: number, task?: TaskKey | null, };

export type SourcePath = { 
/**
 * A path as the debug information records it.
 */
path: string, };

export type AddBreakpoint = { 
/**
 * A function, `FILE:LINE`, `FILE:FUNCTION`, or `0xADDRESS`.
 */
location: string, condition?: string | null, 
/**
 * Which hits stop, such as `>=5` or `%10`.
 */
hitCondition?: string | null, 
/**
 * A message to log instead of stopping, with expressions in braces.
 */
logMessage?: string | null, };

export type EditBreakpoint = { id: number, 
/**
 * Absent to remove.
 */
condition?: string | null, hitCondition?: string | null, logMessage?: string | null, };

export type BreakpointRef = { id: number, };

export type MemberRef = { member: number, };

export type Input = { text: string, 
/**
 * Close the input after the text, so the program reads its end.
 */
eof: boolean, };

export type ServerMessage = { "type": "hello" } & Hello | { "type": "state" } & State | { "type": "result", id: number, result: unknown, } | { "type": "error", id: number, error: ErrorBody, } | { "type": "output" } & Output | { "type": "presence" } & Presence | { "type": "notice" } & Notice | { "type": "profiles" } & ProfileList;

export type Hello = { version: number, 
/**
 * This connection's number, unique while the server runs.
 */
connection: number, role: Role, 
/**
 * The name others see until the tab chooses one.
 */
name: string, 
/**
 * The server's working directory, which relative paths start from.
 */
cwd: string, 
/**
 * Whether the server makes links that share the session. One behind a
 * trusted proxy makes none, since the proxy decides who sees its pages.
 */
share: boolean, };

export type State = { 
/**
 * Identifies what is being debugged; a new program, process, or core
 * dump gets a new one. Absent when nothing is.
 */
session: string | null, 
/**
 * What is being debugged.
 */
target: Target | null, 
/**
 * Slow work under way, such as loading debug information.
 */
busy: string | null, 
/**
 * The debugger revision this state reflects.
 */
revision: number, inferior: Inferior, threads: Array<Thread>, breakpoints: Array<Breakpoint>, 
/**
 * The latest stops of this session, oldest first.
 */
stops: Array<StopEntry>, 
/**
 * Counts changes made to the program's values, which make values read
 * earlier at the same stop out of date.
 */
writes: number, watchpoints: Array<Watchpoint>, 
/**
 * Counts changes to settings that publish nothing else, such as
 * signal policies.
 */
settings: number, 
/**
 * The processes debugged together, of which this state describes the
 * selected one; none for a core dump, or when nothing is debugged.
 */
group: Group | null, };

export type Group = { 
/**
 * Every member so far, in the order they joined.
 */
members: Array<Member>, 
/**
 * The member the state describes and requests act on.
 */
selected: number, 
/**
 * The member whose stop stopped the group, while it is stopped there.
 */
stopped: number | null, };

export type Member = { 
/**
 * Its number in the group, from 1 for the program's own process.
 */
number: number, pid: number | null, 
/**
 * The member whose process started it, when one did.
 */
parent: number | null, 
/**
 * Its command line, as it was when it joined.
 */
command: string, joined: Joined, 
/**
 * The program it executed to join, for one that joined by executing.
 */
program: string | null, state: MemberState, 
/**
 * Its state in words, such as `breakpoint 1 at main` or `exited with
 * status 0`.
 */
description: string, };

export type Joined = "launched" | "attached" | "forked" | "executed";

export type MemberState = "notStarted" | "running" | "stopped" | "exited" | "watched" | "detached" | "lost";

export type Target = { kind: TargetKind, 
/**
 * The executable's path.
 */
program: string, 
/**
 * A launched program's arguments.
 */
arguments: Array<string>, 
/**
 * An attached process, or the process a core dump recorded.
 */
pid: number | null, };

export type TargetKind = "launch" | "attach" | "core";

export type Inferior = { "state": "notStarted" } | { "state": "running", pid: number, } | { "state": "stopped", pid: number, 
/**
 * The stop's number, which requests about it carry.
 */
stop: number, 
/**
 * The thread whose event caused the stop.
 */
thread: number, reason: StopReason, 
/**
 * The thread's frame the stop selects, which a tab opens on: the
 * innermost, or the program's frame where a language runtime's
 * agent stopped it.
 */
frame: number, 
/**
 * Where that thread stopped.
 */
place: Place | null, } | { "state": "exited", description: string, } | { "state": "detached", pid: number, };

export type StopReason = { 
/**
 * A short machine-readable kind, such as `breakpoint` or `pause`.
 */
kind: string, 
/**
 * The reason as the CLI says it.
 */
description: string, };

export type Place = { 
/**
 * The instruction's address, in hexadecimal.
 */
address: string, function: string | null, 
/**
 * The source file, as the debug information records it.
 */
path: string | null, line: number | null, };

export type StopEntry = { stop: number, thread: number, reason: StopReason, 
/**
 * The thread's frame the stop selects, and where that frame is.
 */
frame: number, place: Place | null, 
/**
 * Who ran the program to it, when someone did.
 */
by: string | null, 
/**
 * What they did, such as `stepped over`.
 */
action: string | null, };

export type Breakpoint = { id: number, 
/**
 * What it was set at, as it was asked for.
 */
location: string, condition: string | null, hitCondition: string | null, logMessage: string | null, 
/**
 * Hits in the current process, including those that did not stop.
 */
hits: number, 
/**
 * Ready locations; runtime arming problems are described in `pending`.
 */
places: Array<Place>, pending?: PendingBreakpoint, };

export type PendingBreakpoint = { message: string, places: Array<PendingBreakpointPlace>, };

export type PendingBreakpointPlace = { place: Place, reason: string, };

export type Thread = { id: number, name: string | null, stopped: boolean, 
/**
 * What the program's runtimes say of the stopped thread, such as
 * `isolate 1`.
 */
labels: Array<string>, };

export type ErrorBody = { kind: ErrorKind, message: string, };

export type ErrorKind = "staleStop" | "notStopped" | "forbidden" | "busy" | "invalid" | "unsupported" | "failed";

export type Output = { stream: Stream, text: string, };

export type Stream = "stdout" | "stderr" | "log";

export type Presence = { people: Array<Person>, };

export type Person = { connection: number, name: string, role: Role, focus: Focus | null, };

export type Notice = { 
/**
 * Who did it.
 */
connection: number, name: string, 
/**
 * What they did, such as `continued`.
 */
text: string, };

export type PathCompletions = { entries: Array<PathEntry>, };

export type PathEntry = { 
/**
 * The completed text, ending in `/` for a directory.
 */
text: string, kind: PathKind, };

export type PathKind = "directory" | "executable" | "file";

export type Processes = { processes: Array<Process>, 
/**
 * Yama's `ptrace_scope`, when the kernel has it: at 1 or more,
 * attaching to a process that is not a child may be refused.
 */
ptraceScope: number | null, };

export type Process = { pid: number, 
/**
 * The command line, or the process's name when it has none.
 */
command: string, };

export type ShareLink = { url: string, };

export type Backtrace = { frames: Array<Frame>, 
/**
 * The runs of frames of a runtime's or an interpreter's machinery
 * the stack folds into a line each, as the CLI's `bt` does, with the
 * innermost frame shown.
 */
folds: Array<FrameRun>, 
/**
 * Why the stack ends early, when the unwinder could not finish it.
 */
incomplete: string | null, };

export type FrameRun = { first: number, count: number, };

export type Frame = { 
/**
 * Its number, counting from the innermost frame.
 */
index: number, 
/**
 * The function, symbol, or address.
 */
name: string, kind: FrameKind, 
/**
 * The instruction or return address, in hexadecimal; for a suspended
 * task's frame, where it resumes, if that is known.
 */
address: string | null, 
/**
 * The file name of the module the code is in.
 */
module: string | null, source: SourceLine | null, 
/**
 * The language of its code, where its function or interpreter says.
 */
language: FrameLanguage | null, 
/**
 * For a frame that drives frames the stack does not show in full,
 * the note that says so and why.
 */
unfollowed: string | null, };

export type FrameKind = "physical" | "inline" | "signal" | "tailCall" | "async" | "awaited" | "interpreted";

export type FrameLanguage = { mark: string, name: string, };

export type SourceLine = { path: string, line: number, column: number | null, 
/**
 * The line of the same file the frame's function begins at, where its
 * runtime or debug information says.
 */
first: number | null, };

export type SourceFiles = { 
/**
 * Paths as the debug information records them, then a Python
 * program's own files and those its frames recorded, sorted.
 */
files: Array<string>, 
/**
 * Where the program's main function is declared, or a Python program's
 * script begins.
 */
entry: SourceLine | null, 
/**
 * Why the list is not every file, when it is not.
 */
note: string | null, };

export type FunctionQuery = { query: string, 
/**
 * How many to return; 50 when absent, and never more than 200.
 */
limit?: number | null, };

export type Functions = { functions: Array<FunctionMatch>, 
/**
 * Whether more functions match than were returned.
 */
more: boolean, };

export type FunctionMatch = { name: string, 
/**
 * Where it is declared, when the debug information says.
 */
path: string | null, line: number | null, };

export type SourceText = { path: string, 
/**
 * The file read, which a source map may have moved.
 */
read: string, text: string, 
/**
 * The lines a breakpoint can stop at, in order.
 */
breakable: Array<number>, };

export type Added = { id: number, };

export type FrameAt = { stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type ChildrenOf = { 
/**
 * A row's `children.handle`, which belongs to this connection and the
 * row's stop.
 */
handle: number, start: number, count: number, };

export type Evaluate = { expression: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type SetValue = { 
/**
 * The expression that names what changes, a row's `path`.
 */
path: string, 
/**
 * An expression for the new value.
 */
value: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type Complete = { 
/**
 * The line up to the cursor.
 */
text: string, 
/**
 * The frame whose names complete; absent when nothing is stopped.
 */
stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, };

export type ConsoleLine = { line: string, 
/**
 * The frame the line runs in; absent when nothing is stopped.
 */
stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, };

export type Scopes = { scopes: Array<Scope>, };

export type Scope = { key: ScopeKey, name: string, rows: Array<Row>, 
/**
 * Why the scope's rows could not be read, when they could not.
 */
problem: string | null, };

export type ScopeKey = "args" | "locals" | "statics";

export type Row = { name: string, 
/**
 * The value's summary, or why there is none.
 */
text: string, type: string | null, 
/**
 * An expression that evaluates the value again.
 */
path: string | null, children: Children | null, 
/**
 * Whether `setValue` can change it.
 */
editable: boolean, 
/**
 * The address of its natural memory, as hexadecimal.
 */
memory: string | null, 
/**
 * How many bytes the value occupies there, when it is stored there.
 */
memoryBytes: number | null, 
/**
 * A row that only says where reading stopped short.
 */
truncated: boolean, 
/**
 * The renderers of the drawings the value's view offers, each once.
 */
drawings: Array<string>, };

export type Children = { handle: number, 
/**
 * How many children are elements, when it is known.
 */
indexed: number | null, 
/**
 * How many are named, when it is known.
 */
named: number | null, };

export type Rows = { rows: Array<Row>, };

export type Completions = { 
/**
 * Where the completed part starts, in characters.
 */
start: number, items: Array<Completion>, };

export type Completion = { label: string, 
/**
 * `keyword`, `value`, `variable`, or `field`.
 */
kind: string, };

export type ConsoleResult = { output: string | null, row: Row | null, };

export type Disassemble = { 
/**
 * The address to show, as hexadecimal; the frame's own code when
 * absent.
 */
address?: string | null, 
/**
 * Intel unless AT&T is asked for.
 */
syntax?: Syntax | null, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type ReadMemory = { 
/**
 * The stop the bytes are read at, which must be current.
 */
stop: number, 
/**
 * As hexadecimal, such as `0x7ffff7a3e010`.
 */
address: string, count: number, };

export type WriteMemory = { stop: number, address: string, 
/**
 * The bytes, as pairs of hexadecimal digits.
 */
bytes: string, };

export type AddWatchpoint = { 
/**
 * An expression in the frame, or `0xADDRESS:BYTES`.
 */
target: string, access: WatchAccess, 
/**
 * The frame an expression is resolved in; an address range needs none.
 */
stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, condition?: string | null, hitCondition?: string | null, };

export type EditWatchpoint = { id: number, condition?: string | null, hitCondition?: string | null, };

export type WatchpointRef = { id: number, };

export type WatchAccess = "change" | "write" | "readWrite" | "read";

export type Syntax = "intel" | "att";

export type SignalPolicy = { signal: number, 
/**
 * Such as `SIGUSR1`; filled in by the server.
 */
name: string, stop: boolean, print: boolean, pass: boolean, };

export type Watchpoint = { id: number, access: WatchAccess, 
/**
 * The expression it watches, when it was given one.
 */
expression: string | null, address: string, bytes: number, 
/**
 * Such as `thread 41872's frame at 0x7ffe…, until it returns` for a
 * local, which ends with its frame; empty for a global.
 */
scope: string, condition: string | null, hitCondition: string | null, hits: number, };

export type Disassembled = { 
/**
 * The function shown, when the code shown is one.
 */
function: string | null, 
/**
 * The frame's instruction: its program counter, or, in a caller, the
 * call it returns to after.
 */
marked: string | null, instructions: Array<Instruction>, 
/**
 * What the code shown leaves out, such as unreadable memory.
 */
notes: Array<string>, };

export type Instruction = { address: string, 
/**
 * Its bytes, as hexadecimal pairs separated by spaces.
 */
bytes: string, 
/**
 * Its text in pieces; empty when the bytes decode to nothing.
 */
tokens: Array<Token>, 
/**
 * Why there is no instruction, when there is none.
 */
invalid: string | null, 
/**
 * Where a branch goes, which a page can follow.
 */
target: BranchTarget | null, 
/**
 * What its operands name, such as a function or a global.
 */
comment: string | null, 
/**
 * The symbol it is in, with its offset, such as `main+12`.
 */
symbol: string | null, 
/**
 * Its source line, when it starts one.
 */
source: SourceLine | null, };

export type Token = { 
/**
 * `mnemonic`, `prefix`, `keyword`, `register`, `number`, `address`,
 * `punctuation`, or `text`.
 */
kind: string, text: string, };

export type BranchTarget = { address: string, name: string | null, };

export type Memory = { address: string, 
/**
 * The bytes read, as hexadecimal pairs with no separator.
 */
bytes: string, 
/**
 * The first address that could not be read, when the read stopped
 * short.
 */
unreadable: string | null, };

export type StepTargets = { calls: Array<StepCall>, };

export type StepCall = { 
/**
 * The call instruction's address, in hexadecimal, which names it to a
 * step.
 */
call: string, 
/**
 * The function it calls, when something names it.
 */
callee: string | null, 
/**
 * The address a direct call calls, in hexadecimal; none for an
 * indirect call.
 */
target: string | null, };

export type Registers = { registers: Array<Register>, };

export type Register = { name: string, 
/**
 * As hexadecimal; absent where a caller's frame did not save it.
 */
value: string | null, bits: number, 
/**
 * `pc`, `sp`, or `fp`.
 */
role: string | null, };

export type Signals = { signals: Array<SignalPolicy>, };

export type Exceptions = { filters: Array<ExceptionFilter>, };

export type ExceptionFilter = { 
/**
 * The name it is chosen by, such as `python-uncaught`.
 */
id: string, language: string, 
/**
 * A few words naming what stops, and a sentence saying where.
 */
label: string, description: string, 
/**
 * Whether they stop the program.
 */
on: boolean, };

export type ExceptionChoice = { 
/**
 * The filter, such as `python-uncaught`.
 */
id: string, on: boolean, };

export type Modules = { modules: Array<Module>, };

export type Module = { id: number, name: string, path: string, 
/**
 * Where it is loaded, as hexadecimal.
 */
start: string | null, end: string | null, 
/**
 * `debug` with debug information, `symbols` with only a symbol table,
 * or `none`.
 */
symbols: string, 
/**
 * The separate file its debug information came from, when its own
 * file was stripped of it.
 */
debugFile: string | null, 
/**
 * Why the separate debug file found for it could not be used.
 */
debugFileProblem: string | null, 
/**
 * Why not all of its debug information could be read.
 */
debugInformationProblem: string | null, };

export type StopAt = { stop: number, };

export type TaskKey = { runtime: number, number: number, };

export type TaskList = { 
/**
 * What the runtimes call a task, such as `task` or `goroutine`.
 */
noun: string | null, tasks: Array<Task>, 
/**
 * Whether there are more tasks than those listed, which the list
 * leaves out to stay small.
 */
more: boolean, 
/**
 * Why the list may be missing tasks, or describe some wrongly.
 */
gaps: Array<string>, };

export type Task = { key: TaskKey, state: TaskState, 
/**
 * The code the program wrote that it is in, as the CLI says it.
 */
place: string, 
/**
 * The frame of that code in the task's stack, which choosing the task
 * shows; absent for a task that runs only its runtime's code.
 */
frame: Frame | null, 
/**
 * The runtime's own words for what it does or waits for.
 */
detail: string | null, 
/**
 * Labels the program or runtime gave it, such as `{runtime: "local
 * set 3"}`.
 */
labels: string | null, 
/**
 * The thread running it.
 */
thread: number | null, };

export type TaskState = "running" | "runnable" | "blocked" | "exited" | "unknown";

export type Draw = { 
/**
 * The value's `path`, which evaluates it.
 */
path: string, 
/**
 * The renderer: one whose drawing the value's view offers, or else any
 * renderer, which then draws the value itself as its `values`.
 */
renderer: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type RendererRef = { digest: string, };

export type RendererInfo = { name: string, 
/**
 * The file or module record it was loaded from, or `built-in`.
 */
origin: string, digest: string, };

export type RendererSource = { source: string, name: string, 
/**
 * The file or module record it was loaded from, or `built-in`.
 */
origin: string, digest: string, };

export type RendererList = { renderers: Array<RendererInfo>, };

export type Drawing = { renderer: RendererInfo, 
/**
 * Whether the value's view offers the drawing; otherwise the renderer
 * draws the value itself as its `values`.
 */
offered: boolean, 
/**
 * Each input, by name, in order; none when a problem kept any part of
 * them from being read.
 */
inputs: Array<DrawInput> | null, 
/**
 * Why the inputs could not be read whole: the first part that could
 * not, and why.
 */
problem: string | null, 
/**
 * How many bytes the binary frame sent just before this answer holds:
 * the bytes and numbers the inputs' `bytes` and `numbers` lie in.
 */
bytes: number, };

export type DrawInput = { name: string, value: Datum, 
/**
 * The part of the drawn value the input is, when it is one.
 */
path: string | null, };

export type Datum = { "t": "bool", b: boolean, } | { "t": "int", i: number, } | { "t": "big", big: string, } | { "t": "float", f: string, } | { "t": "text", s: string, } | { "t": "enum", name: string | null, value: Datum, } | { "t": "sum", variant: string, value: Datum | null, } | { "t": "record", members: Array<[string, Datum]>, } | { "t": "list", items: Array<Datum>, } | { "t": "numbers", kind: NumberKind, offset: number, count: number, } | { "t": "bytes", offset: number, length: number, } | { "t": "entries", entries: Array<[Datum, Datum]>, } | { "t": "null" };

export type NumberKind = "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "f32" | "f64";

export type ProfileList = { profiles: Array<ProfileInfo>, };

export type ProfileInfo = { 
/**
 * Derived from a file's bytes, so a link names the same profile
 * whenever a server serves it; a recording's is its own.
 */
id: string, 
/**
 * The file it was read from, or the program recorded.
 */
name: string, 
/**
 * Whether it is being recorded, and grows.
 */
recording: boolean, 
/**
 * Counts the profiles published under this id: each answer about it
 * is of the newest.
 */
revision: number, 
/**
 * How a recorded program ended, once it has.
 */
ended: string | null, };

export type ProfileRef = { profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, };

export type ProfileQuery = { profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Samples taken in `[start, end)`, in nanoseconds from the profile's
 * start, or all.
 */
range?: [number, number] | null, 
/**
 * Samples of these threads, by row, or of all.
 */
threads?: Array<number> | null, 
/**
 * What the query aggregates.
 */
source: ProfileSource, 
/**
 * Samples or waits of these tasks, by row, or of any; `null` in the
 * list is what no snapshot named.
 */
tasks?: Array<number | null> | null, 
/**
 * Roots each path at its task.
 */
byTask: boolean, 
/**
 * Samples or waits that carry each of these labels, `key=value`.
 */
labels: Array<string>, 
/**
 * Roots each path, outside any task, at its label of this key.
 */
byLabel?: string | null, 
/**
 * Applied in order, to keys by the names views show.
 */
transforms: Array<ProfileTransform>, 
/**
 * Paths from the innermost frame out: a tree of callers.
 */
invert: boolean, 
/**
 * How each stack's frames are seen.
 */
look: ProfileLook, granularity: ProfileGranularity, 
/**
 * What a tree or functions answer compares itself with.
 */
base?: ProfileBase | null, };

export type ProfileTransform = { op: TransformOp, name: string, };

export type TransformOp = "focus" | "merge" | "drop" | "collapse";

export type ProfileGranularity = "function" | "physical" | "line" | "address";

export type ProfileLook = "logical" | "native" | "python";

export type ProfileTimelineQuery = { 
/**
 * Slices of the query's range, or of the whole profile.
 */
buckets: number, 
/**
 * Lanes of tasks rather than threads; waits are always of tasks.
 */
taskLanes: boolean, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Samples taken in `[start, end)`, in nanoseconds from the profile's
 * start, or all.
 */
range?: [number, number] | null, 
/**
 * Samples of these threads, by row, or of all.
 */
threads?: Array<number> | null, 
/**
 * What the query aggregates.
 */
source: ProfileSource, 
/**
 * Samples or waits of these tasks, by row, or of any; `null` in the
 * list is what no snapshot named.
 */
tasks?: Array<number | null> | null, 
/**
 * Roots each path at its task.
 */
byTask: boolean, 
/**
 * Samples or waits that carry each of these labels, `key=value`.
 */
labels: Array<string>, 
/**
 * Roots each path, outside any task, at its label of this key.
 */
byLabel?: string | null, 
/**
 * Applied in order, to keys by the names views show.
 */
transforms: Array<ProfileTransform>, 
/**
 * Paths from the innermost frame out: a tree of callers.
 */
invert: boolean, 
/**
 * How each stack's frames are seen.
 */
look: ProfileLook, granularity: ProfileGranularity, 
/**
 * What a tree or functions answer compares itself with.
 */
base?: ProfileBase | null, };

export type ProfileSummary = { id: string, name: string, 
/**
 * The program recorded.
 */
program: string | null, 
/**
 * What a sample weighs: `nanoseconds` of CPU time, `executions`,
 * `bytes`, or what an imported profile counts.
 */
unit: string, 
/**
 * What made the samples, such as a clock or an imported pprof file.
 */
source: string | null, 
/**
 * Whether the samples carry their times and threads, which an
 * imported pprof or folded profile's do not.
 */
timed: boolean, samples: number, weight: number, 
/**
 * System calls its trace holds, which the Syscalls view shows.
 */
syscalls: number, 
/**
 * Records of calls the trace's engine lost.
 */
syscallsLost: number, 
/**
 * Nanoseconds from the first sample to the last.
 */
duration: number, threads: Array<ProfileThread>, 
/**
 * Samples the kernel dropped.
 */
lost: number, 
/**
 * Times the kernel throttled sampling.
 */
throttles: number, 
/**
 * Stacks that ended short of their outermost frame, by reason.
 */
truncations: Array<ProfileCount>, 
/**
 * How the profile was recorded, by setting.
 */
settings: Array<ProfileSetting>, 
/**
 * Nanoseconds of CPU time microscope spent recording, and the part of it
 * spent reading debug information.
 */
recorderCpu: number, recorderLoading: number, 
/**
 * Spans not sampled, in nanoseconds from the start.
 */
gaps: Array<[number, number]>, 
/**
 * Each change the budget made, from the start.
 */
budgetChanges: Array<ProfileMarker>, 
/**
 * The tasks task snapshots found, most weight first.
 */
tasks: Array<ProfileTask>, 
/**
 * Whether its stacks hold frames an interpreter runs, which a query's
 * `python` look shows as their code sees them.
 */
interpreted: boolean, 
/**
 * Task snapshots taken, the nanoseconds they held the program
 * stopped, and the samples they named the task of.
 */
snapshots: number, snapshotPause: number, taskSamples: number, 
/**
 * Each time a snapshot failed or missed tasks, from the start.
 */
snapshotProblems: Array<ProfileMarker>, 
/**
 * The weight of every wait the snapshots found.
 */
waited: number, 
/**
 * What the recording read of the program's instrumentation, and lost.
 */
probes: ProfileProbes, 
/**
 * Each zone site name's and label key's zones, most time first.
 */
zoneSites: Array<ProfileZoneSite>, 
/**
 * Every key a sample's or task's label may have.
 */
labelKeys: Array<string>, 
/**
 * Why modules' instrumentation could not be read.
 */
probeProblems: Array<ProfileMarker>, };

export type ProfileThread = { row: number, pid: number, tid: number, name: string | null, samples: number, weight: number, 
/**
 * Nanoseconds off its CPU, blocked and preempted.
 */
blocked: number, preempted: number, };

export type ProfileCount = { what: string, count: number, };

export type ProfileSetting = { name: string, value: string, };

export type ProfileTree = { nodes: number, 
/**
 * The weight of every sample the query kept.
 */
weight: number, samples: number, keys: Array<ProfileKey>, 
/**
 * For a diff, its base's weight, whose own and total weights by node
 * follow the columns as two more of `f64`s.
 */
base: ProfileBaseWeight | null, };

export type ProfileKey = { 
/**
 * How views name it, which transforms take.
 */
name: string, kind: ProfileKeyKind, 
/**
 * The module's file name.
 */
module: string | null, 
/**
 * Where it is in the source.
 */
file: string | null, line: number | null, };

export type ProfileKeyKind = "function" | "line" | "address" | "truncation" | "task" | "group" | "label";

export type ProfileTask = { row: number, 
/**
 * As views name it: its runtime's noun, number, and labels.
 */
name: string, 
/**
 * The weight of the samples that ran it, and the nanoseconds of
 * waiting snapshots found it in.
 */
weight: number, waited: number, };

export type ProfileSource = "samples" | "waits";

export type ProfileFunctions = { weight: number, 
/**
 * Most own weight first, or, for a diff, most changed share first.
 */
rows: Array<ProfileCost>, keys: Array<ProfileKey>, 
/**
 * For a diff, its base's weight.
 */
base: ProfileBaseWeight | null, };

export type ProfileCost = { 
/**
 * An index into `keys`.
 */
key: number, own: number, total: number, 
/**
 * For a diff, the key's weights in its base.
 */
baseOwn: number, baseTotal: number, };

export type ProfileTimeline = { 
/**
 * The range sliced, in nanoseconds from the profile's start.
 */
range: [number, number], lanes: Array<ProfileLane>, };

export type ProfileLane = { thread: number | null, task: number | null, weights: Array<number>, blocked: Array<number>, preempted: Array<number>, };

export type ProfileMarker = { 
/**
 * Nanoseconds from the start.
 */
time: number, text: string, };

export type ProfileProbes = { events: number, 
/**
 * Records threads overwrote before microscope read them.
 */
overwritten: number, 
/**
 * Events no ring could take.
 */
noRing: number, 
/**
 * Events a signal handler dropped mid-write.
 */
interrupted: number, 
/**
 * Records that broke the probes ABI.
 */
malformed: number, 
/**
 * Values plotted.
 */
plotted: number, };

export type ProfileZoneSite = { name: string, 
/**
 * Whether its zones are a label's spans.
 */
label: boolean, count: number, 
/**
 * Nanoseconds, all of them and the longest.
 */
total: number, longest: number, 
/**
 * Zones of it whose begin or end was not seen, or did not nest.
 */
flagged: number, };

export type ProfileZonesQuery = { profile: string, revision?: number, 
/**
 * Nanoseconds from the profile's start, or all of it.
 */
range?: [number, number] | null, 
/**
 * Of these threads, by row, or of all.
 */
threads?: Array<number> | null, 
/**
 * The pixels the range is drawn across: zones under three pixels wide
 * fold together.
 */
width: number, };

export type ProfileZones = { range: [number, number], blocks: number, names: Array<string>, 
/**
 * The program's messages, frames, and triggers in the range.
 */
marks: Array<ProfileMark>, 
/**
 * Each plotted series in the range, by name.
 */
plots: Array<ProfilePlot>, };

export type ProfileMark = { thread: number, 
/**
 * Nanoseconds from the start.
 */
time: number, kind: ProfileMarkKind, 
/**
 * Its site's name.
 */
name: string, text: string | null, };

export type ProfileFunctionQuery = { function: string, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Samples taken in `[start, end)`, in nanoseconds from the profile's
 * start, or all.
 */
range?: [number, number] | null, 
/**
 * Samples of these threads, by row, or of all.
 */
threads?: Array<number> | null, 
/**
 * What the query aggregates.
 */
source: ProfileSource, 
/**
 * Samples or waits of these tasks, by row, or of any; `null` in the
 * list is what no snapshot named.
 */
tasks?: Array<number | null> | null, 
/**
 * Roots each path at its task.
 */
byTask: boolean, 
/**
 * Samples or waits that carry each of these labels, `key=value`.
 */
labels: Array<string>, 
/**
 * Roots each path, outside any task, at its label of this key.
 */
byLabel?: string | null, 
/**
 * Applied in order, to keys by the names views show.
 */
transforms: Array<ProfileTransform>, 
/**
 * Paths from the innermost frame out: a tree of callers.
 */
invert: boolean, 
/**
 * How each stack's frames are seen.
 */
look: ProfileLook, granularity: ProfileGranularity, 
/**
 * What a tree or functions answer compares itself with.
 */
base?: ProfileBase | null, };

export type ProfileLines = { 
/**
 * The weight of every sample selected.
 */
weight: number, files: Array<ProfileSourceFile>, };

export type ProfileSourceFile = { path: string | null, 
/**
 * Each line with samples: its number, own weight, and total weight.
 */
lines: Array<[number, number, number]>, 
/**
 * The file's text from line `first`, around those lines, as the file
 * reads now; `None`, with why, when it cannot be read.
 */
first: number, text: Array<string> | null, unread: string | null, };

export type ProfileCode = { weight: number, module: string | null, blocks: Array<ProfileCodeBlock>, 
/**
 * Sampled addresses no decoded instruction holds: address, own, and
 * total.
 */
unplaced: Array<[number, number, number]>, unavailable: string | null, };

export type ProfileCodeBlock = { symbol: string, instructions: Array<ProfileInstruction>, stopped: string | null, };

export type ProfileInstruction = { address: number, text: string, 
/**
 * The function a direct jump or call goes to.
 */
target: string | null, own: number, total: number, };

export type ProfileBase = { profile: string, 
/**
 * Nanoseconds from that profile's start, or all of it.
 */
range?: [number, number] | null, };

export type ProfileBaseWeight = { weight: number, samples: number, };

export type ProfilePlot = { name: string, 
/**
 * How many values it drew, and how many were not finite.
 */
values: number, unshown: number, 
/**
 * A bin a pixel wide each: the time from the start of its first
 * value, its first, last, lowest, and highest values.
 */
bins: Array<[number, number, number, number, number]>, 
/**
 * The value before the range, which holds into it.
 */
held: number | null, 
/**
 * Whether a value follows the range, so the last holds to its end.
 */
continues: boolean, };

export type ProfileMarkKind = "message" | "frame" | "trigger";

export type ProfileZoneStatsQuery = { profile: string, revision?: number, 
/**
 * A zone site's name, or a label's key.
 */
name: string, range?: [number, number] | null, threads?: Array<number> | null, };

export type ProfileZoneStats = { count: number, total: number, flagged: number, min: number, median: number, p90: number, p99: number, max: number, 
/**
 * Buckets of equal ratio: each its lowest length and how many.
 */
histogram: Array<[number, number]>, 
/**
 * The longest, longest first.
 */
longest: Array<ProfileZoneAt>, 
/**
 * For a label: each value's time and count, most time first.
 */
values: Array<ProfileLabelValue>, };

export type SyscallQuery = { profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Calls that ended in `[start, end)`, in nanoseconds from the
 * profile's start, or all.
 */
range?: [number, number] | null, 
/**
 * strace's selection, as `-e` takes it, expressions apart by spaces:
 * `trace=%file status=failed`.
 */
filter: string, 
/**
 * Calls of these threads, by id, or of all.
 */
threads?: Array<number> | null, 
/**
 * Calls whose duration falls in this log2 bucket, as a heat map's
 * row names it, or all.
 */
latency?: number | null, };

export type SyscallGroup = "syscall" | "class" | "process" | "thread" | "path" | "site";

export type SyscallSummaryQuery = { group: SyscallGroup, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Calls that ended in `[start, end)`, in nanoseconds from the
 * profile's start, or all.
 */
range?: [number, number] | null, 
/**
 * strace's selection, as `-e` takes it, expressions apart by spaces:
 * `trace=%file status=failed`.
 */
filter: string, 
/**
 * Calls of these threads, by id, or of all.
 */
threads?: Array<number> | null, 
/**
 * Calls whose duration falls in this log2 bucket, as a heat map's
 * row names it, or all.
 */
latency?: number | null, };

export type SyscallSummary = { 
/**
 * Every call the trace holds, and those the query selects.
 */
total: number, calls: number, errors: number, 
/**
 * Records the engine lost: counts are lower bounds when above zero.
 */
lost: number, 
/**
 * Whether the kernel counted the calls without reporting them, so
 * that only the summary has them.
 */
counted: boolean, 
/**
 * The engine that traced, `bpf` or `ptrace`.
 */
engine: string | null, processes: number, rows: Array<SyscallRow>, };

export type SyscallRow = { 
/**
 * What the row is of, as a filter or query names it: a call's name,
 * a class, a pid, a tid, a path, or a site.
 */
key: string, name: string, calls: number, errors: number, 
/**
 * Calls with a duration, which the times cover.
 */
timed: number, totalNs: number, p50Ns: number | null, p90Ns: number | null, p99Ns: number | null, maxNs: number, bytes: number, 
/**
 * Failures by errno name, most first.
 */
errnos: Array<SyscallCount>, };

export type SyscallCount = { name: string, count: number, };

export type SyscallRateBy = "class" | "syscall";

export type SyscallRateQuery = { buckets: number, by: SyscallRateBy, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Calls that ended in `[start, end)`, in nanoseconds from the
 * profile's start, or all.
 */
range?: [number, number] | null, 
/**
 * strace's selection, as `-e` takes it, expressions apart by spaces:
 * `trace=%file status=failed`.
 */
filter: string, 
/**
 * Calls of these threads, by id, or of all.
 */
threads?: Array<number> | null, 
/**
 * Calls whose duration falls in this log2 bucket, as a heat map's
 * row names it, or all.
 */
latency?: number | null, };

export type SyscallRate = { 
/**
 * The range's start and each slice's nanoseconds.
 */
start: number, width: number, 
/**
 * Most calls first; the rest past the first few as `other`.
 */
series: Array<SyscallSeries>, errors: Array<number>, };

export type SyscallSeries = { name: string, counts: Array<number>, };

export type SyscallHeatmapQuery = { buckets: number, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Calls that ended in `[start, end)`, in nanoseconds from the
 * profile's start, or all.
 */
range?: [number, number] | null, 
/**
 * strace's selection, as `-e` takes it, expressions apart by spaces:
 * `trace=%file status=failed`.
 */
filter: string, 
/**
 * Calls of these threads, by id, or of all.
 */
threads?: Array<number> | null, 
/**
 * Calls whose duration falls in this log2 bucket, as a heat map's
 * row names it, or all.
 */
latency?: number | null, };

export type SyscallHeatmap = { start: number, width: number, 
/**
 * Rows of log2 latency, as a histogram's buckets.
 */
rows: number, 
/**
 * Each slice's rows of counts, slice after slice.
 */
counts: Array<number>, most: number, };

export type SyscallDetailQuery = { 
/**
 * The call, by name.
 */
call: string, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Calls that ended in `[start, end)`, in nanoseconds from the
 * profile's start, or all.
 */
range?: [number, number] | null, 
/**
 * strace's selection, as `-e` takes it, expressions apart by spaces:
 * `trace=%file status=failed`.
 */
filter: string, 
/**
 * Calls of these threads, by id, or of all.
 */
threads?: Array<number> | null, 
/**
 * Calls whose duration falls in this log2 bucket, as a heat map's
 * row names it, or all.
 */
latency?: number | null, };

export type SyscallDetail = { 
/**
 * Log2 histograms of the whole trace's calls and of the query's.
 */
whole: Array<number>, selected: Array<number>, errnos: Array<SyscallCount>, 
/**
 * What counts as slow: the selection's 90th percentile.
 */
slowNs: number | null, 
/**
 * Where the slow and failed calls came from, most first.
 */
callers: Array<SyscallCaller>, 
/**
 * Slow or failed calls without a stack.
 */
unplaced: number, };

export type SyscallCaller = { 
/**
 * The innermost frame with a source line, or the stack's innermost.
 */
site: string, slow: number, failed: number, };

export type SyscallLogQuery = { offset: number, limit: number, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Calls that ended in `[start, end)`, in nanoseconds from the
 * profile's start, or all.
 */
range?: [number, number] | null, 
/**
 * strace's selection, as `-e` takes it, expressions apart by spaces:
 * `trace=%file status=failed`.
 */
filter: string, 
/**
 * Calls of these threads, by id, or of all.
 */
threads?: Array<number> | null, 
/**
 * Calls whose duration falls in this log2 bucket, as a heat map's
 * row names it, or all.
 */
latency?: number | null, };

export type SyscallLog = { 
/**
 * Lines the query selects.
 */
total: number, lines: Array<SyscallLine>, };

export type SyscallLine = { 
/**
 * The event's place in the trace, which `syscallCall` names it by.
 */
event: number, 
/**
 * Nanoseconds from the profile's start.
 */
time: number, tid: number, pid: number, tokens: Array<SyscallToken>, durationNs: number | null, failed: boolean, 
/**
 * Whether it is a call, which expands; signals and exits do not.
 */
call: boolean, };

export type SyscallToken = { kind: string, text: string, };

export type SyscallCallQuery = { profile: string, revision?: number, event: number, };

export type SyscallCall = { arguments: Array<SyscallArgument>, 
/**
 * Hex dumps of the bytes it read or wrote, as `-e read=` prints them.
 */
dumps: Array<string>, 
/**
 * Its stack's functions, innermost first.
 */
stack: Array<string>, 
/**
 * Why the stack ends early, when it does.
 */
stackEnds: string | null, 
/**
 * When it entered and returned, from the profile's start.
 */
entered: number | null, exited: number | null, 
/**
 * What its result means, as `ENOENT (No such file or directory)`.
 */
result: Array<SyscallToken>, 
/**
 * Where in the source it was made, when its stack says.
 */
site: SyscallSite | null, };

export type SyscallArgument = { name: string, tokens: Array<SyscallToken>, };

export type SyscallSite = { function: string, path: string, line: number, 
/**
 * The text from line `first`, or why it could not be read.
 */
first: number, text: Array<string> | null, unread: string | null, };

export type SyscallLanesQuery = { buckets: number, profile: string, 
/**
 * The revision the tab has seen, which keeps its cached answers
 * apart; the server answers from the newest.
 */
revision?: number, 
/**
 * Calls that ended in `[start, end)`, in nanoseconds from the
 * profile's start, or all.
 */
range?: [number, number] | null, 
/**
 * strace's selection, as `-e` takes it, expressions apart by spaces:
 * `trace=%file status=failed`.
 */
filter: string, 
/**
 * Calls of these threads, by id, or of all.
 */
threads?: Array<number> | null, 
/**
 * Calls whose duration falls in this log2 bucket, as a heat map's
 * row names it, or all.
 */
latency?: number | null, };

export type SyscallLanes = { start: number, width: number, lanes: Array<SyscallLane>, };

export type SyscallLane = { tid: number, pid: number, 
/**
 * Its process's program.
 */
name: string | null, 
/**
 * Each slice's nanoseconds spent in calls, and failed calls.
 */
busy: Array<number>, failed: Array<number>, 
/**
 * Slices one call spans from end to end: the thread was blocked.
 */
blocked: Array<boolean>, };

export type SyscallFiles = { files: Array<SyscallFile>, };

export type SyscallFile = { path: string, opens: number, failed: number, read: number, written: number, 
/**
 * Nanoseconds in `fsync` and `fdatasync`.
 */
syncNs: number, 
/**
 * Nanoseconds in every call on it.
 */
totalNs: number, };

export type SyscallNetwork = { endpoints: Array<SyscallEndpoint>, };

export type SyscallEndpoint = { 
/**
 * What `-y` prints of the socket, as `TCP:[10.0.0.1:5000->10.0.0.2:80]`.
 */
endpoint: string, connects: number, failed: number, 
/**
 * Nanoseconds the connects took.
 */
connectNs: number, sent: number, received: number, };

export type SyscallProcesses = { processes: Array<SyscallProcess>, };

export type RecordSyscalls = { 
/**
 * The calls to trace, as `-e` takes them; every call when empty.
 */
filter: string, 
/**
 * Unwinds each call's stack, as `-k` does.
 */
stacks: boolean, };

export type SyscallInFlightQuery = { profile: string, revision?: number, };

export type SyscallInFlight = { 
/**
 * Whether the trace is still recording; an ended one has none.
 */
live: boolean, calls: Array<SyscallBlocked>, };

export type SyscallBlocked = { tid: number, pid: number, 
/**
 * The call as strace prints its entry.
 */
tokens: Array<SyscallToken>, 
/**
 * How long it had been blocked.
 */
blockedNs: number, };

export type SyscallProcess = { pid: number, parent: number | null, 
/**
 * The programs it ran, in order.
 */
images: Array<string>, threads: number, calls: number, errors: number, 
/**
 * From the profile's start.
 */
started: number, ended: number | null, 
/**
 * How it ended, as strace's last line says it.
 */
end: string | null, };

export type ProfileZoneAt = { thread: number, 
/**
 * Nanoseconds from the start.
 */
start: number, end: number, value: string | null, };

export type ProfileLabelValue = { value: string, total: number, count: number, };

export type RecordProfile = { program: string, arguments: Array<string>, 
/**
 * Where it runs: the server's directory when absent.
 */
cwd?: string | null, 
/**
 * Variables added to its environment, as `[name, value]` pairs.
 */
environment: Array<[string, string]>, preset: RecordPreset, 
/**
 * Samples each call of the function this names instead of CPU time.
 */
function?: string | null, 
/**
 * Keeps only the last this many seconds, which the page then shows
 * and saves.
 */
window?: number | null, 
/**
 * Traces system calls instead of sampling.
 */
syscalls?: RecordSyscalls | null, };

export type RecordPreset = "production" | "development" | "maximum";

export type RecordStarted = { 
/**
 * The profile's id, which stays its own once the recording ends.
 */
profile: string, };
