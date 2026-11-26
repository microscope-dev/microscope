// Generated from src/web/protocol.rs by `cargo test`; do not edit.

export const PROTOCOL_VERSION = 2;

export type Role = "control" | "view";

export type Envelope = { id: number, } & ({ "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "selectMember", "params": MemberRef } | { "method": "detachMember", "params": MemberRef } | { "method": "step", "params": Step } | { "method": "stepTargets", "params": ThreadAt } | { "method": "jump", "params": Jump } | { "method": "advance", "params": Advance } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input } | { "method": "scopes", "params": FrameAt } | { "method": "children", "params": ChildrenOf } | { "method": "evaluate", "params": Evaluate } | { "method": "setValue", "params": SetValue } | { "method": "complete", "params": Complete } | { "method": "console", "params": ConsoleLine } | { "method": "disassemble", "params": Disassemble } | { "method": "readMemory", "params": ReadMemory } | { "method": "writeMemory", "params": WriteMemory } | { "method": "registers", "params": FrameAt } | { "method": "addWatchpoint", "params": AddWatchpoint } | { "method": "editWatchpoint", "params": EditWatchpoint } | { "method": "removeWatchpoint", "params": WatchpointRef } | { "method": "signals" } | { "method": "setSignal", "params": SignalPolicy } | { "method": "exceptions" } | { "method": "setException", "params": ExceptionChoice } | { "method": "modules" } | { "method": "functions", "params": FunctionQuery } | { "method": "resolve", "params": SymbolName } | { "method": "tasks", "params": StopAt } | { "method": "draw", "params": Draw } | { "method": "renderer", "params": RendererRef } | { "method": "renderers" } | { "method": "reloadViews" } | { "method": "profile", "params": ProfileRef } | { "method": "profileInspect", "params": ProfileRef } | { "method": "profileTree", "params": ProfileQuery } | { "method": "profileFunctions", "params": ProfileQuery } | { "method": "profileTimeline", "params": ProfileTimelineQuery } | { "method": "profileZones", "params": ProfileZonesQuery } | { "method": "profileZoneStats", "params": ProfileZoneStatsQuery } | { "method": "profileLines", "params": ProfileFunctionQuery } | { "method": "profileCode", "params": ProfileFunctionQuery } | { "method": "recordProfile", "params": RecordProfile } | { "method": "stopRecording", "params": ProfileRef } | { "method": "syscallSummary", "params": SyscallSummaryQuery } | { "method": "syscallHeatmap", "params": SyscallHeatmapQuery } | { "method": "syscallDetail", "params": SyscallDetailQuery } | { "method": "syscallLog", "params": SyscallLogQuery } | { "method": "syscallCall", "params": SyscallCallQuery } | { "method": "syscallFiles", "params": SyscallQuery } | { "method": "syscallNetwork", "params": SyscallQuery } | { "method": "syscallProcesses", "params": SyscallQuery } | { "method": "syscallInFlight", "params": SyscallInFlightQuery } | { "method": "cloudStatus" } | { "method": "cloudLoginStart", "params": CloudLoginStart } | { "method": "cloudLoginPoll", "params": CloudLoginPoll } | { "method": "shareProfile", "params": ShareProfile });

export type Request = { "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "selectMember", "params": MemberRef } | { "method": "detachMember", "params": MemberRef } | { "method": "step", "params": Step } | { "method": "stepTargets", "params": ThreadAt } | { "method": "jump", "params": Jump } | { "method": "advance", "params": Advance } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input } | { "method": "scopes", "params": FrameAt } | { "method": "children", "params": ChildrenOf } | { "method": "evaluate", "params": Evaluate } | { "method": "setValue", "params": SetValue } | { "method": "complete", "params": Complete } | { "method": "console", "params": ConsoleLine } | { "method": "disassemble", "params": Disassemble } | { "method": "readMemory", "params": ReadMemory } | { "method": "writeMemory", "params": WriteMemory } | { "method": "registers", "params": FrameAt } | { "method": "addWatchpoint", "params": AddWatchpoint } | { "method": "editWatchpoint", "params": EditWatchpoint } | { "method": "removeWatchpoint", "params": WatchpointRef } | { "method": "signals" } | { "method": "setSignal", "params": SignalPolicy } | { "method": "exceptions" } | { "method": "setException", "params": ExceptionChoice } | { "method": "modules" } | { "method": "functions", "params": FunctionQuery } | { "method": "resolve", "params": SymbolName } | { "method": "tasks", "params": StopAt } | { "method": "draw", "params": Draw } | { "method": "renderer", "params": RendererRef } | { "method": "renderers" } | { "method": "reloadViews" } | { "method": "profile", "params": ProfileRef } | { "method": "profileInspect", "params": ProfileRef } | { "method": "profileTree", "params": ProfileQuery } | { "method": "profileFunctions", "params": ProfileQuery } | { "method": "profileTimeline", "params": ProfileTimelineQuery } | { "method": "profileZones", "params": ProfileZonesQuery } | { "method": "profileZoneStats", "params": ProfileZoneStatsQuery } | { "method": "profileLines", "params": ProfileFunctionQuery } | { "method": "profileCode", "params": ProfileFunctionQuery } | { "method": "recordProfile", "params": RecordProfile } | { "method": "stopRecording", "params": ProfileRef } | { "method": "syscallSummary", "params": SyscallSummaryQuery } | { "method": "syscallHeatmap", "params": SyscallHeatmapQuery } | { "method": "syscallDetail", "params": SyscallDetailQuery } | { "method": "syscallLog", "params": SyscallLogQuery } | { "method": "syscallCall", "params": SyscallCallQuery } | { "method": "syscallFiles", "params": SyscallQuery } | { "method": "syscallNetwork", "params": SyscallQuery } | { "method": "syscallProcesses", "params": SyscallQuery } | { "method": "syscallInFlight", "params": SyscallInFlightQuery } | { "method": "cloudStatus" } | { "method": "cloudLoginStart", "params": CloudLoginStart } | { "method": "cloudLoginPoll", "params": CloudLoginPoll } | { "method": "shareProfile", "params": ShareProfile };

export type SetName = { name: string, };

export type Share = { role: Role, to: string, };

export type CompletePath = { text: string, };

export type Launch = { program: string, arguments: Array<string>, cwd: string | null, environment: Array<[string, string]>, stopAtEntry: boolean, run: boolean, replace: boolean, };

export type Attach = { pid: number, replace: boolean, };

export type OpenCore = { core: string, executable: string | null, replace: boolean, };

export type Continue = { stop: number | null, };

export type Step = { stop: number, thread: number, task?: TaskKey | null, frame?: number, kind: StepKind, call?: string | null, };

export type Jump = { stop: number, thread: number, task?: TaskKey | null, location: string, };

export type Advance = { stop: number, thread: number, task?: TaskKey | null, frame?: number, location: string, };

export type StepKind = "over" | "into" | "out" | "instruction" | "overInstruction";

export type SetFocus = { focus: Focus | null, };

export type Focus = { url: string, label: string, };

export type ThreadAt = { stop: number, thread: number, task?: TaskKey | null, };

export type SourcePath = { path: string, };

export type AddBreakpoint = { location: string, condition?: string | null, hitCondition?: string | null, logMessage?: string | null, };

export type EditBreakpoint = { id: number, condition?: string | null, hitCondition?: string | null, logMessage?: string | null, };

export type BreakpointRef = { id: number, };

export type MemberRef = { member: number, };

export type Input = { text: string, eof: boolean, };

export type ServerMessage = { "type": "hello" } & Hello | { "type": "state" } & State | { "type": "result", id: number, result: unknown, } | { "type": "error", id: number, error: ErrorBody, } | { "type": "output" } & Output | { "type": "presence" } & Presence | { "type": "notice" } & Notice | { "type": "profiles" } & ProfileList;

export type Hello = { version: number, connection: number, role: Role, name: string, cwd: string, };

export type State = { session: string | null, target: Target | null, busy: string | null, revision: number, inferior: Inferior, threads: Array<Thread>, breakpoints: Array<Breakpoint>, stops: Array<StopEntry>, writes: number, watchpoints: Array<Watchpoint>, settings: number, group: Group | null, };

export type Group = { members: Array<Member>, selected: number, stopped: number | null, };

export type Member = { number: number, pid: number | null, parent: number | null, command: string, joined: Joined, program: string | null, state: MemberState, description: string, };

export type Joined = "launched" | "attached" | "forked" | "executed";

export type MemberState = "notStarted" | "running" | "stopped" | "exited" | "watched" | "detached" | "lost";

export type Target = { kind: TargetKind, program: string, arguments: Array<string>, pid: number | null, };

export type TargetKind = "launch" | "attach" | "core";

export type Inferior = { "state": "notStarted" } | { "state": "running", pid: number, } | { "state": "stopped", pid: number, stop: number, thread: number, reason: StopReason, frame: number, place: Place | null, } | { "state": "exited", description: string, } | { "state": "detached", pid: number, };

export type StopReason = { kind: string, description: string, };

export type Place = { address: string, function: string | null, path: string | null, line: number | null, };

export type StopEntry = { stop: number, thread: number, reason: StopReason, frame: number, place: Place | null, by: string | null, action: string | null, };

export type Breakpoint = { id: number, location: string, condition: string | null, hitCondition: string | null, logMessage: string | null, hits: number, places: Array<Place>, pending?: PendingBreakpoint, };

export type PendingBreakpoint = { message: string, places: Array<PendingBreakpointPlace>, };

export type PendingBreakpointPlace = { place: Place, reason: string, };

export type Thread = { id: number, name: string | null, stopped: boolean, labels: Array<string>, };

export type ErrorBody = { kind: ErrorKind, message: string, };

export type ErrorKind = "staleStop" | "notStopped" | "forbidden" | "busy" | "invalid" | "unsupported" | "failed";

export type Output = { stream: Stream, text: string, };

export type Stream = "stdout" | "stderr" | "log";

export type Presence = { people: Array<Person>, };

export type Person = { connection: number, name: string, role: Role, focus: Focus | null, };

export type Notice = { connection: number, name: string, text: string, };

export type PathCompletions = { entries: Array<PathEntry>, };

export type PathEntry = { text: string, kind: PathKind, };

export type PathKind = "directory" | "executable" | "file";

export type Processes = { processes: Array<Process>, ptraceScope: number | null, };

export type Process = { pid: number, command: string, };

export type ShareLink = { url: string, };

export type Backtrace = { frames: Array<Frame>, folds: Array<FrameRun>, incomplete: string | null, };

export type FrameRun = { first: number, count: number, };

export type Frame = { index: number, name: string, kind: FrameKind, address: string | null, module: string | null, source: SourceLine | null, language: FrameLanguage | null, unfollowed: string | null, };

export type FrameKind = "physical" | "inline" | "signal" | "tailCall" | "async" | "awaited" | "interpreted";

export type FrameLanguage = { mark: string, name: string, };

export type SourceLine = { path: string, line: number, column: number | null, first: number | null, };

export type SourceFiles = { files: Array<string>, entry: SourceLine | null, note: string | null, };

export type FunctionQuery = { query: string, limit?: number | null, };

export type Functions = { functions: Array<FunctionMatch>, more: boolean, };

export type SymbolName = { name: string, };

export type Resolved = { address: string, };

export type FunctionMatch = { name: string, path: string | null, line: number | null, };

export type SourceText = { path: string, read: string, text: string, breakable: Array<number>, };

export type Added = { id: number, };

export type FrameAt = { stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type ChildrenOf = { handle: number, start: number, count: number, };

export type Evaluate = { expression: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type SetValue = { path: string, value: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type Complete = { text: string, stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, };

export type ConsoleLine = { line: string, stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, };

export type Scopes = { scopes: Array<Scope>, };

export type Scope = { key: ScopeKey, name: string, rows: Array<Row>, problem: string | null, note: string | null, };

export type ScopeKey = "args" | "locals" | "closure" | "module" | "global" | "unnamed" | "statics";

export type Row = { name: string, text: string, detail: string | null, type: string | null, path: string | null, children: Children | null, editable: boolean, memory: string | null, memoryBytes: number | null, stored: boolean, textBytes: number | null, truncated: boolean, drawings: Array<string>, };

export type Children = { handle: number, indexed: number | null, named: number | null, };

export type Rows = { rows: Array<Row>, };

export type Completions = { start: number, items: Array<Completion>, };

export type Completion = { label: string, kind: string, };

export type ConsoleResult = { output: string | null, row: Row | null, };

export type Disassemble = { address?: string | null, syntax?: Syntax | null, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type ReadMemory = { stop: number, address: string, count: number, };

export type WriteMemory = { stop: number, address: string, bytes: string, };

export type AddWatchpoint = { target: string, access: WatchAccess, stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, condition?: string | null, hitCondition?: string | null, };

export type EditWatchpoint = { id: number, condition?: string | null, hitCondition?: string | null, };

export type WatchpointRef = { id: number, };

export type WatchAccess = "change" | "write" | "readWrite" | "read";

export type Syntax = "intel" | "att";

export type SignalPolicy = { signal: number, name: string, stop: boolean, print: boolean, pass: boolean, };

export type Watchpoint = { id: number, access: WatchAccess, expression: string | null, address: string, bytes: number, scope: string, condition: string | null, hitCondition: string | null, hits: number, };

export type Disassembled = { function: string | null, marked: string | null, instructions: Array<Instruction>, notes: Array<string>, };

export type Instruction = { address: string, bytes: string, tokens: Array<Token>, invalid: string | null, target: BranchTarget | null, comment: string | null, symbol: string | null, source: SourceLine | null, };

export type Token = { kind: string, text: string, };

export type BranchTarget = { address: string, name: string | null, };

export type Memory = { address: string, bytes: string, unreadable: string | null, };

export type StepTargets = { calls: Array<StepCall>, };

export type StepCall = { call: string, callee: string | null, target: string | null, };

export type Registers = { registers: Array<Register>, };

export type Register = { name: string, value: string | null, bits: number, role: string | null, };

export type Signals = { signals: Array<SignalPolicy>, };

export type Exceptions = { filters: Array<ExceptionFilter>, };

export type ExceptionFilter = { id: string, language: string, label: string, description: string, on: boolean, };

export type ExceptionChoice = { id: string, on: boolean, };

export type Modules = { modules: Array<Module>, };

export type Module = { id: number, name: string, path: string, start: string | null, end: string | null, symbols: string, debugFile: string | null, debugFileProblem: string | null, debugInformationProblem: string | null, };

export type StopAt = { stop: number, };

export type TaskKey = { runtime: number, number: string, };

export type TaskList = { noun: string | null, tasks: Array<Task>, more: boolean, gaps: Array<string>, };

export type Task = { key: TaskKey, state: TaskState, place: string, frame: Frame | null, detail: string | null, labels: string | null, thread: number | null, stackless: boolean, };

export type TaskState = "running" | "runnable" | "blocked" | "exited" | "unknown";

export type Draw = { path: string, renderer: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type RendererRef = { digest: string, };

export type RendererInfo = { name: string, origin: string, digest: string, };

export type RendererSource = { source: string, name: string, origin: string, digest: string, };

export type RendererList = { renderers: Array<RendererInfo>, };

export type Drawing = { renderer: RendererInfo, offered: boolean, inputs: Array<DrawInput> | null, problem: string | null, bytes: number, };

export type DrawInput = { name: string, value: Datum, path: string | null, };

export type Datum = { "t": "bool", b: boolean, } | { "t": "int", i: number, } | { "t": "big", big: string, } | { "t": "float", f: string, } | { "t": "text", s: string, } | { "t": "enum", name: string | null, value: Datum, } | { "t": "sum", variant: string, value: Datum | null, } | { "t": "record", members: Array<[string, Datum]>, } | { "t": "list", items: Array<Datum>, } | { "t": "numbers", kind: NumberKind, offset: number, count: number, } | { "t": "bytes", offset: number, length: number, } | { "t": "entries", entries: Array<[Datum, Datum]>, } | { "t": "null" };

export type NumberKind = "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "f32" | "f64";

export type ProfileList = { profiles: Array<ProfileInfo>, };

export type ProfileInfo = { id: string, name: string, recording: boolean, revision: number, ended: string | null, };

export type ProfileRef = { profile: string, revision?: number, };

export type ProfileQuery = { profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type ProfileDuring = { filter: string, latency?: number | null, row?: SyscallRowKey | null, };

export type ProfileTransform = { op: TransformOp, name: string, };

export type TransformOp = "focus" | "merge" | "drop" | "collapseCycles";

export type ProfileGranularity = "function" | "physical" | "line" | "address";

export type ProfileLook = "code" | "tasks" | "native";

export type ProfileTimelineQuery = { buckets: number, taskLanes: boolean, profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type ProfileSummary = { id: string, name: string, program: string | null, command: ProfileCommand | null, unit: string, source: string | null, samples: number, weight: number, syscalls: number, syscallsLost: SyscallLosses, duration: number, threads: Array<ProfileThread>, lost: number, throttles: number, truncations: Array<ProfileCount>, settings: Array<ProfileSetting>, recorderCpu: number, recorderLoading: number, behind: Array<[number, number]>, budgetChanges: Array<ProfileMarker>, tasks: Array<ProfileTask>, interpreted: boolean, snapshots: number, snapshotPause: number, taskSamples: number, snapshotProblems: Array<ProfileMarker>, waited: number, probes: ProfileProbes, zoneSites: Array<ProfileZoneSite>, labelKeys: Array<string>, probeProblems: Array<ProfileMarker>, };

export type ProfileInspection = { processes: Array<ProfileProcess>, modules: Array<ProfileModule>, depths: Array<number>, overTime: Array<number>, distinctStacks: number, frames: number, framesWithLines: number, counted: number, stackBytes: number, shortened: number, recorderSetup: number, recorderFinishing: number, tables: Array<ProfileTable>, file: ProfileFile | null, };

export type ProfileCommand = { arguments: Array<string> | null, text: string, };

export type ProfileProcess = { pid: number, name: string, command: ProfileCommand | null, };

export type ProfileModule = { path: string, size: number, buildId: string | null, frames: number, };

export type ProfileTable = { name: string, bytes: number, };

export type ProfileFile = { bytes: number | null, chunks: number, truncated: boolean, };

export type ProfileThread = { row: number, pid: number, tid: number, name: string | null, samples: number, weight: number, zones: number, blocked: number, preempted: number, };

export type ProfileCount = { what: string, count: number, };

export type ProfileSetting = { name: string, value: string, };

export type ProfileTree = { nodes: number, weight: number, samples: number, keys: Array<ProfileKey>, base: ProfileBaseWeight | null, };

export type ProfileKey = { name: string, kind: ProfileKeyKind, module: string | null, file: string | null, line: number | null, };

export type ProfileKeyKind = "function" | "line" | "address" | "truncation" | "task" | "group" | "label" | "syscall";

export type ProfileTask = { row: number, name: string, weight: number, waited: number, };

export type ProfileSource = "samples" | "waits";

export type ProfileFunctions = { weight: number, rows: Array<ProfileCost>, keys: Array<ProfileKey>, base: ProfileBaseWeight | null, };

export type ProfileCost = { key: number, own: number, total: number, baseOwn: number, baseTotal: number, };

export type ProfileTimeline = { range: [number, number], lanes: Array<ProfileLane>, };

export type ProfileLane = { thread: number | null, task: number | null, weights: Array<number>, blocked: Array<number>, preempted: Array<number>, syscalls: Array<number>, failed: Array<number>, waits: Array<ProfileWait>, };

export type ProfileWait = { call: string, start: number, end: number, };

export type ProfileMarker = { time: number, text: string, };

export type ProfileProbes = { events: number, overwritten: number, noRing: number, interrupted: number, malformed: number, plotted: number, };

export type ProfileZoneSite = { name: string, label: boolean, count: number, total: number, longest: number, flagged: number, };

export type ProfileZonesQuery = { profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, width: number, site?: string | null, band?: [number, number] | null, };

export type ProfileZones = { range: [number, number], blocks: number, offCpu: boolean, names: Array<string>, marks: Array<ProfileMark>, plots: Array<ProfilePlot>, };

export type ProfileMark = { thread: number, time: number, kind: ProfileMarkKind, name: string, text: string | null, };

export type ProfileFunctionQuery = { function: string, profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type ProfileLines = { weight: number, files: Array<ProfileSourceFile>, };

export type ProfileSourceFile = { path: string | null, lines: Array<[number, number, number]>, first: number, text: Array<string> | null, unread: string | null, };

export type ProfileCode = { weight: number, module: string | null, blocks: Array<ProfileCodeBlock>, unplaced: Array<[number, number, number]>, unavailable: string | null, functions: Array<string>, files: Array<ProfileSourceFile>, unlined: string | null, inlined_into: string | null, };

export type ProfileCodeBlock = { symbol: string, instructions: Array<ProfileInstruction>, stopped: string | null, };

export type ProfileCodeLine = [number, number | null, number];

export type ProfileToken = [string, string];

export type ProfileInstruction = { address: number, tokens: Array<ProfileToken>, target: string | null, own: number, total: number, lines: Array<ProfileCodeLine>, };

export type ProfileBase = { profile: string, range?: [number, number] | null, zones?: ProfileZoneScope | null, };

export type ProfileZoneScope = { name: string, band?: [number, number] | null, outside: boolean, };

export type ProfileBaseWeight = { weight: number, samples: number, };

export type ProfilePlot = { name: string, values: number, unshown: number, bins: Array<[number, number, number, number, number]>, held: number | null, continues: boolean, };

export type ProfileMarkKind = "message" | "frame" | "trigger";

export type ProfileZoneStatsQuery = { profile: string, revision?: number, name: string, range?: [number, number] | null, threads?: Array<number> | null, band?: [number, number] | null, rankOf?: number | null, };

export type ProfileZoneStats = { count: number, total: number, flagged: number, min: number, median: number, p90: number, p99: number, max: number, histogram: Array<[number, number]>, longest: Array<ProfileZoneAt>, banded: [number, number], shorter: number, values: Array<ProfileLabelValue>, };

export type SyscallQuery = { filter: string, latency?: number | null, profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type SyscallGroup = "syscall" | "class" | "process" | "thread" | "path" | "site";

export type SyscallRowKey = { group: SyscallGroup, key: string, };

export type SyscallSummaryQuery = { group: SyscallGroup, filter: string, latency?: number | null, profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type SyscallSummary = { total: number, calls: number, errors: number, lost: SyscallLosses, counted: boolean, engine: string | null, processes: number, rows: Array<SyscallRow>, };

export type SyscallLosses = { dropped: number, unseen: number, capped: number, overflowed: number, };

export type SyscallRow = { key: string, name: string, calls: number, errors: number, timed: number, totalNs: number, p50Ns: number | null, p90Ns: number | null, p99Ns: number | null, maxNs: number, bytes: number, errnos: Array<SyscallCount>, };

export type SyscallCount = { name: string, count: number, };

export type SyscallHeatmapQuery = { buckets: number, filter: string, latency?: number | null, profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type SyscallHeatmap = { start: number, width: number, rows: number, counts: Array<number>, most: number, failed: Array<number>, };

export type SyscallDetailQuery = { call: string, filter: string, latency?: number | null, profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type SyscallDetail = { whole: Array<number>, selected: Array<number>, errnos: Array<SyscallCount>, slowNs: number | null, callers: Array<SyscallCaller>, unplaced: number, };

export type SyscallCaller = { site: string, slow: number, failed: number, };

export type SyscallLogQuery = { offset: number, limit: number, filter: string, latency?: number | null, profile: string, revision?: number, range?: [number, number] | null, threads?: Array<number> | null, source: ProfileSource, tasks?: Array<number | null> | null, byTask: boolean, labels: Array<string>, byLabel?: string | null, transforms: Array<ProfileTransform>, invert: boolean, look: ProfileLook, granularity: ProfileGranularity, zones?: ProfileZoneScope | null, base?: ProfileBase | null, during?: ProfileDuring | null, syscallLeaves: boolean, };

export type SyscallLog = { total: number, lines: Array<SyscallLine>, };

export type SyscallLine = { event: number, time: number, tid: number, pid: number, tokens: Array<SyscallToken>, durationNs: number | null, failed: boolean, call: boolean, };

export type SyscallToken = { kind: string, text: string, };

export type SyscallCallQuery = { profile: string, revision?: number, event: number, };

export type SyscallCall = { arguments: Array<SyscallArgument>, dumps: Array<string>, stack: Array<string>, stackEnds: string | null, entered: number | null, exited: number | null, result: Array<SyscallToken>, site: SyscallSite | null, };

export type SyscallArgument = { name: string, tokens: Array<SyscallToken>, };

export type SyscallSite = { function: string, path: string, line: number, first: number, text: Array<string> | null, unread: string | null, };

export type SyscallFiles = { files: Array<SyscallFile>, };

export type SyscallFile = { path: string, opens: number, failed: number, read: number, written: number, syncNs: number, totalNs: number, };

export type SyscallNetwork = { endpoints: Array<SyscallEndpoint>, };

export type SyscallEndpoint = { endpoint: string, connects: number, failed: number, connectNs: number, sent: number, received: number, };

export type SyscallProcesses = { processes: Array<SyscallProcess>, };

export type SyscallsLevel = "off" | "counts" | "calls" | "full";

export type SyscallsEngine = "bpf" | "ptrace";

export type SyscallInFlightQuery = { profile: string, revision?: number, };

export type SyscallInFlight = { live: boolean, calls: Array<SyscallBlocked>, };

export type SyscallBlocked = { tid: number, pid: number, tokens: Array<SyscallToken>, blockedNs: number, };

export type SyscallProcess = { pid: number, parent: number | null, images: Array<string>, threads: number, calls: number, errors: number, started: number, ended: number | null, end: string | null, };

export type ProfileZoneAt = { thread: number, start: number, end: number, value: string | null, };

export type ProfileLabelValue = { value: string, total: number, count: number, };

export type RecordProfile = { program: string, arguments: Array<string>, cwd?: string | null, environment: Array<[string, string]>, preset: RecordPreset, function?: string | null, window?: number | null, syscalls?: SyscallsLevel | null, syscallEngine?: SyscallsEngine | null, };

export type RecordPreset = "production" | "development" | "maximum";

export type RecordStarted = { profile: string, };

export type CloudLoginStart = { server?: string | null, };

export type CloudLoginStarted = { server: string, verifyUrl: string, userCode: string, pollToken: string, expiresInSecs: number, pollIntervalMs: number, };

export type CloudLoginPoll = { server: string, pollToken: string, };

export type CloudLoginStatus = { status: string, username: string | null, email: string | null, };

export type CloudProject = { id: number, name: string, orgId: number, orgName: string | null, };

export type CloudStatus = { loggedIn: boolean, server: string | null, defaultServer: string, username: string | null, email: string | null, projects: Array<CloudProject>, error: string | null, };

export type ShareProfile = { profile: string, projectId: number, name?: string | null, };

export type SharedProfile = { url: string, sessionId: number, captureId: number, projectName: string, byteLen: number, redactedCount: number, };
