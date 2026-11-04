// The Processes pane selects which process the session shows and lets a
// stopped child detach without ending the other processes.

import { useCallback, useState } from "react";

import { assert } from "../assert";
import { controls } from "../model";
import type { Group, Member } from "../protocol";
import { useConnection, useModel } from "../store";
import { flash } from "../tab";
import { useFocus } from "./Workspace";

/** A member's program, by file name, as `ps` names it. */
export function memberName(member: Member): string {
  const word = member.command.split(" ")[0];
  assert(word !== undefined, `member ${member.number} command split into nothing`);
  const program = member.program ?? word;
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
  const ask = useCallback(
    (method: "selectMember" | "detachMember", member: number) => {
      setBusy(true);
      void connection
        .request(method, { member })
        .catch((failure: Error) => flash(failure.message))
        .finally(() => setBusy(false));
    },
    [connection],
  );
  const selectedNumber = group?.selected;
  const detach = useCallback(() => {
    assert(selectedNumber !== undefined, "detaching with no group");
    ask("detachMember", selectedNumber);
  }, [ask, selectedNumber]);
  if (!showsProcesses(group)) {
    return (
      <div className="pane-body">
        <div className="empty">This session debugs one process.</div>
      </div>
    );
  }
  const selected = group.members.find((member) => member.number === group.selected);
  const active: Member[] = [];
  const other: Member[] = [];
  for (const member of group.members) {
    const visible =
      member.state === "running" ||
      member.state === "stopped" ||
      member.number === 1 ||
      member.number === group.selected ||
      member.number === group.stopped;
    (visible ? active : other).push(member);
  }
  const row = (member: Member) => (
    <ProcessRow
      key={member.number}
      member={member}
      on={member.number === group.selected}
      stoppedHere={member.number === group.stopped}
      disabled={!control || busy}
      onAsk={ask}
    />
  );

  return (
    <nav className="pane-body processes" aria-label="Processes" data-testid="processes">
      <ul>{active.map((member) => row(member))}</ul>
      {control && selected && selected.number !== 1 && selected.state === "stopped" && (
        <button
          type="button"
          className="button small"
          disabled={busy}
          title="Let this process run on without microscope; the others stay debugged"
          onClick={detach}
        >
          Detach [{selected.number}]
        </button>
      )}
      {other.length > 0 && (
        <details>
          <summary>Other processes ({other.length})</summary>
          <ul>{other.map((member) => row(member))}</ul>
        </details>
      )}
    </nav>
  );
}

function ProcessRow({
  member,
  on,
  stoppedHere,
  disabled,
  onAsk,
}: {
  member: Member;
  on: boolean;
  stoppedHere: boolean;
  disabled: boolean;
  onAsk: (method: "selectMember", member: number) => void;
}) {
  const debugged = member.state === "running" || member.state === "stopped";
  const onClick = useCallback(
    () => !on && onAsk("selectMember", member.number),
    [on, onAsk, member.number],
  );
  return (
    <li>
      <button
        type="button"
        className={`process ${on ? "on" : ""}`}
        aria-pressed={on}
        disabled={disabled || !debugged}
        title={[
          `[${member.number}] process ${member.pid ?? "not started"}`,
          member.command,
          joinedText(member),
          member.description,
        ]
          .filter(Boolean)
          .join("\n")}
        onClick={onClick}
      >
        <span className="number">[{member.number}]</span> {memberName(member)}
        <span className="muted"> · {joinedText(member)} · </span>
        <span className={`state ${member.state}`}>{STATES[member.state]}</span>
        {stoppedHere && <span> ◂ stopped here</span>}
      </button>
    </li>
  );
}

export function ProcessesBadge() {
  const count = useModel((model) => model.state?.group?.members.length ?? 0);
  return count > 1 ? <span className="count">{count}</span> : null;
}
