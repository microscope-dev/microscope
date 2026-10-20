import { useShallow } from "zustand/react/shallow";
import type { Model } from "../model";
import { commandName } from "../model";
import { useModel } from "../store";
import { threadLabel } from "./names";

interface Shown {
  pill: "stopped" | "running" | "idle" | "warn";
  label: string;
  detail: string | null;
}

/** What the status shows: the run state is the loudest thing on screen. */
export function shown(model: Model): Shown {
  if (model.link === "reconnecting" || model.link === "connecting") {
    return {
      pill: "warn",
      label: model.link === "connecting" ? "Connecting" : "Reconnecting",
      detail: model.state?.session ? "showing the last state" : null,
    };
  }
  const state = model.state;
  if (state?.busy) {
    return { pill: "warn", label: "Working", detail: state.busy };
  }
  if (!state?.target) {
    return { pill: "idle", label: "Nothing loaded", detail: null };
  }
  const inferior = state.inferior;
  switch (inferior.state) {
    case "notStarted":
      return { pill: "idle", label: "Not started", detail: "F5 runs it" };
    case "running":
      return { pill: "running", label: "Running", detail: `process ${inferior.pid}` };
    case "stopped": {
      const thread = state.threads.find((candidate) => candidate.id === inferior.thread);
      return {
        pill: "stopped",
        label: state.target.kind === "core" ? "Core dump" : "Stopped",
        detail: `stop #${inferior.stop} · ${inferior.reason.description} · ${threadLabel(inferior.thread, thread?.name)}`,
      };
    }
    case "exited":
      return { pill: "idle", label: "Exited", detail: inferior.description };
    case "detached":
      return { pill: "idle", label: "Detached", detail: `process ${inferior.pid} runs on` };
  }
}

export function Status() {
  // Only what it shows, so output arriving does not render it again.
  const { pill, label, detail } = useModel(useShallow(shown));
  const program = useModel((model) => commandName(model.state));
  const command = useModel((model) => {
    const target = model.state?.target;
    return target ? [target.program, ...target.arguments].join(" ") : undefined;
  });
  return (
    // A status, so a change of run state is read out.
    <div className="status" role="status" data-testid="status">
      {program && (
        <span className="program" title={command}>
          {program}
        </span>
      )}
      <span className={`pill ${pill}`} data-state={pill}>
        {label}
      </span>
      {detail && (
        <span className="detail" title={detail}>
          {detail}
        </span>
      )}
    </div>
  );
}
