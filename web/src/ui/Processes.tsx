// The processes a session debugs together, beside the stop banner: which
// one the panes show, how each joined, and where each is. It appears once a
// program has started a second process the session debugs.

import { useState } from "react";
import { controls } from "../model";
import type { Group, Member } from "../protocol";
import { useConnection, useModel } from "../store";
import { flash } from "../tab";
import { useFocus } from "./Workspace";

/** A member's program, by file name, as `ps` names it. */
export function memberName(member: Member): string {
  const program = member.program ?? member.command.split(" ")[0] ?? "";
  return program.slice(program.lastIndexOf("/") + 1) || `process ${member.pid ?? "?"}`;
}

/** How a member joined, in a few words. */
export function joinedText(member: Member): string {
  switch (member.joined) {
    case "launched":
      return "launched";
    case "attached":
      return "attached";
    case "forked":
      return member.parent === null ? "forked" : `forked by [${member.parent}]`;
    case "executed":
      return member.parent === null ? "executed" : `executed by [${member.parent}]`;
  }
}

const STATES: Record<Member["state"], string> = {
  notStarted: "not started",
  running: "running",
  stopped: "stopped",
  exited: "exited",
  watched: "watched",
  detached: "detached",
  lost: "lost",
};

/** Whether the group has more than its own program's process to show. */
export function showsProcesses(group: Group | null | undefined): group is Group {
  return (group?.members.length ?? 0) > 1;
}

export function Processes() {
  const { state } = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const [busy, setBusy] = useState(false);
  const group = state.group;
  if (!showsProcesses(group)) {
    return null;
  }
  const selected = group.members.find((member) => member.number === group.selected);
  const ask = (method: "selectMember" | "detachMember", member: number) => {
    setBusy(true);
    void connection
      .request(method, { member })
      .catch((failure: Error) => flash(failure.message))
      .finally(() => setBusy(false));
  };

  return (
    <nav className="processes" aria-label="Processes" data-testid="processes">
      <span className="muted">Processes</span>
      <ul>
        {group.members.map((member) => {
          const on = member.number === group.selected;
          const debugged = member.state === "running" || member.state === "stopped";
          return (
            <li key={member.number}>
              <button
                type="button"
                className={`process ${on ? "on" : ""}`}
                aria-pressed={on}
                disabled={!control || busy || !debugged}
                title={[
                  `[${member.number}] process ${member.pid ?? "not started"}`,
                  member.command,
                  joinedText(member),
                  member.description,
                ]
                  .filter(Boolean)
                  .join("\n")}
                onClick={() => !on && ask("selectMember", member.number)}
              >
                <span className="number">[{member.number}]</span> {memberName(member)}
                <span className="muted"> · {joinedText(member)} · </span>
                <span className={`state ${member.state}`}>{STATES[member.state]}</span>
                {member.number === group.stopped && <span> ◂ stopped here</span>}
              </button>
            </li>
          );
        })}
      </ul>
      {control && selected && selected.number !== 1 && selected.state === "stopped" && (
        <button
          type="button"
          className="button small"
          disabled={busy}
          title="Let this process run on without microscope; the others stay debugged"
          onClick={() => ask("detachMember", selected.number)}
        >
          Detach [{selected.number}]
        </button>
      )}
    </nav>
  );
}
