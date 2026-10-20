// How the debugger handles each signal, and the modules the program has
// loaded.

import { useState } from "react";
import { useRequest } from "../data";
import { controls } from "../model";
import type { SignalPolicy } from "../protocol";
import { useConnection, useModel } from "../store";

/** Whether a signal stops the program, is said, and reaches the program. */
export function Signals() {
  const session = useModel((model) => model.state?.session);
  const signals = useRequest("signals", session ? undefined : null);
  const control = useModel(controls);
  const connection = useConnection();
  const [error, setError] = useState<string | null>(null);
  if (!session) {
    return <div className="empty">Signals apply once something is being debugged.</div>;
  }
  if (signals.error) {
    return <div className="empty error">{signals.error.message}</div>;
  }
  if (!signals.data) {
    return <div className="empty">Reading signal policies…</div>;
  }
  const change = (policy: SignalPolicy, field: "stop" | "print" | "pass", on: boolean) => {
    setError(null);
    connection
      .request("setSignal", { ...policy, [field]: on })
      .catch((failure: Error) => setError(failure.message));
  };
  return (
    <div className="pane-body table-body" data-pane="6" tabIndex={-1}>
      {error && <div className="error">{error}</div>}
      <table className="table" data-testid="signals">
        <thead>
          <tr>
            <th>Signal</th>
            <th>Number</th>
            <th>Stop</th>
            <th>Print</th>
            <th>Pass</th>
          </tr>
        </thead>
        <tbody>
          {signals.data.signals.map((policy) => (
            <tr key={policy.signal}>
              <td>{policy.name}</td>
              <td className="dim">{policy.signal}</td>
              {(["stop", "print", "pass"] as const).map((field) => (
                <td key={field}>
                  <input
                    type="checkbox"
                    aria-label={`${field[0]?.toUpperCase()}${field.slice(1)} on ${policy.name}`}
                    checked={policy[field]}
                    // A signal that stops is always said.
                    disabled={!control || (field === "print" && policy.stop)}
                    onChange={(event) => change(policy, field, event.target.checked)}
                  />
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function Modules() {
  const session = useModel((model) => model.state?.session);
  const modules = useRequest("modules", session ? undefined : null);
  if (modules.error) {
    return <div className="empty error">{modules.error.message}</div>;
  }
  if (!modules.data) {
    return <div className="empty">{session ? "Reading modules…" : "Nothing is loaded."}</div>;
  }
  if (modules.data.modules.length === 0) {
    return <div className="empty">Modules appear once the program runs.</div>;
  }
  return (
    <div className="pane-body table-body" data-pane="6" tabIndex={-1}>
      <table className="table" data-testid="modules">
        <thead>
          <tr>
            <th>Module</th>
            <th>Start</th>
            <th>End</th>
            <th>Symbols</th>
            <th>Path</th>
            <th>Debug file</th>
          </tr>
        </thead>
        <tbody>
          {modules.data.modules.map((module) => (
            <tr key={module.id}>
              <td>{module.name}</td>
              <Bound value={module.start} />
              <Bound value={module.end} />
              <td
                className={
                  module.debugInformationProblem ? "error" : module.symbols === "none" ? "dim" : ""
                }
                title={module.debugInformationProblem ?? undefined}
              >
                {module.symbols}
              </td>
              <td className="dim" title={module.path}>
                {module.path}
              </td>
              <td
                className={module.debugFileProblem ? "error" : "dim"}
                title={
                  module.debugFileProblem
                    ? `cannot use ${module.debugFile}: ${module.debugFileProblem}`
                    : (module.debugFile ?? undefined)
                }
              >
                {module.debugFile ?? "none"}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Where a module starts or ends, which is unknown when microscope could
 * not read the module's file. */
function Bound({ value }: { value: string | null }) {
  return value === null ? (
    <td className="dim" title="microscope could not read this module's file">
      unknown
    </td>
  ) : (
    <td className="mono">{value}</td>
  );
}
