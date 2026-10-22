// Typed access to the generated protocol: which methods exist, what each
// takes, and what each answers.

import type * as P from "./protocol.gen";

export type * from "./protocol.gen";
export { PROTOCOL_VERSION } from "./protocol.gen";

export type Method = P.Request["method"];

/** The parameters a method takes, or `undefined` for none. */
export type ParamsOf<M extends Method> =
  Extract<P.Request, { method: M }> extends { params: infer T } ? T : undefined;

/** What each method answers. */
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
  tasks: P.TaskList;
  /** With the bytes its inputs' numbers and bytes lie in, when any. */
  draw: P.Drawing & { payload?: Uint8Array };
  renderer: P.RendererSource;
  renderers: P.RendererList;
  reloadViews: null;
  profile: P.ProfileSummary;
  /** With the call tree's columns. */
  profileTree: P.ProfileTree & { payload?: Uint8Array };
  profileFunctions: P.ProfileFunctions;
  profileTimeline: P.ProfileTimeline;
  /** With the zone blocks' columns. */
  profileZones: P.ProfileZones & { payload?: Uint8Array };
  profileZoneStats: P.ProfileZoneStats;
  profileLines: P.ProfileLines;
  profileCode: P.ProfileCode;
  recordProfile: P.RecordStarted;
  stopRecording: null;
  syscallSummary: P.SyscallSummary;
  syscallRate: P.SyscallRate;
  syscallHeatmap: P.SyscallHeatmap;
  syscallDetail: P.SyscallDetail;
  syscallLog: P.SyscallLog;
  syscallCall: P.SyscallCall;
  syscallLanes: P.SyscallLanes;
  syscallFiles: P.SyscallFiles;
  syscallNetwork: P.SyscallNetwork;
  syscallProcesses: P.SyscallProcesses;
  syscallInFlight: P.SyscallInFlight;
}

// Fails to compile when a method has no entry in Results, or Results names
// one the server does not have.
type Exactly<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
const resultsCoverEveryMethod: Exactly<keyof Results, Method> = true;
void resultsCoverEveryMethod;

/** Kinds of failure the page handles, plus the connection's own. */
export type FailureKind = P.ErrorKind | "disconnected";

/** A request that failed, with the server's explanation. */
export class RequestError extends Error {
  readonly kind: FailureKind;

  constructor(kind: FailureKind, message: string) {
    super(message);
    this.kind = kind;
    this.name = "RequestError";
  }
}
