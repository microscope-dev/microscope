import type * as P from "./protocol.gen";

export type * from "./protocol.gen";
export { PROTOCOL_VERSION } from "./protocol.gen";

export type Method = P.Request["method"];

export type ParamsOf<M extends Method> =
  Extract<P.Request, { method: M }> extends { params: infer T } ? T : undefined;

export interface Results {
  setName: null;
  share: P.ShareLink;
  completePath: P.PathCompletions;
  processes: P.Processes;
  launch: null;
  attach: null;
  openCore: null;
  end: null;
  continue: null;
  pause: null;
  kill: null;
  restart: null;
  selectMember: null;
  detachMember: null;
  step: null;
  stepTargets: P.StepTargets;
  jump: null;
  advance: null;
  setFocus: null;
  backtrace: P.Backtrace;
  sources: P.SourceFiles;
  source: P.SourceText;
  addBreakpoint: P.Added;
  editBreakpoint: null;
  removeBreakpoint: null;
  input: null;
  scopes: P.Scopes;
  children: P.Rows;
  evaluate: P.Row;
  setValue: P.Row;
  complete: P.Completions;
  console: P.ConsoleResult;
  disassemble: P.Disassembled;
  readMemory: P.Memory;
  writeMemory: null;
  registers: P.Registers;
  addWatchpoint: P.Added;
  editWatchpoint: null;
  removeWatchpoint: null;
  signals: P.Signals;
  setSignal: null;
  exceptions: P.Exceptions;
  setException: null;
  modules: P.Modules;
  functions: P.Functions;
  resolve: P.Resolved;
  tasks: P.TaskList;

  draw: P.Drawing & { payload?: Uint8Array };
  renderer: P.RendererSource;
  renderers: P.RendererList;
  reloadViews: null;
  profile: P.ProfileSummary;
  profileInspect: P.ProfileInspection;

  profileTree: P.ProfileTree & { payload?: Uint8Array };
  profileFunctions: P.ProfileFunctions;
  profileTimeline: P.ProfileTimeline;

  profileZones: P.ProfileZones & { payload?: Uint8Array };
  profileZoneStats: P.ProfileZoneStats;
  profileLines: P.ProfileLines;
  profileCode: P.ProfileCode;
  recordProfile: P.RecordStarted;
  stopRecording: null;
  syscallSummary: P.SyscallSummary;
  syscallHeatmap: P.SyscallHeatmap;
  syscallDetail: P.SyscallDetail;
  syscallLog: P.SyscallLog;
  syscallCall: P.SyscallCall;
  syscallFiles: P.SyscallFiles;
  syscallNetwork: P.SyscallNetwork;
  syscallProcesses: P.SyscallProcesses;
  syscallInFlight: P.SyscallInFlight;
}

type Exactly<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
const resultsCoverEveryMethod: Exactly<keyof Results, Method> = true;
void resultsCoverEveryMethod;

export type FailureKind = P.ErrorKind | "disconnected";

export class RequestError extends Error {
  readonly kind: FailureKind;

  constructor(kind: FailureKind, message: string) {
    super(message);
    this.kind = kind;
    this.name = "RequestError";
  }
}
