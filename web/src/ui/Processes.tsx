import { createMemo, createSignal, For, Show } from "solid-js";

import { assert } from "../assert";
import { controls } from "../model";
import type { Group, Member } from "../protocol";
import { useConnection, useModel } from "../store";
import { flash } from "../tab";
import { useFocus } from "./Workspace";

export function memberName(member: Member): string {
  const word = member.command.split(" ")[0];
  assert(word !== undefined, `member ${member.number} command split into nothing`);
  const program = member.program ?? word;
  return program.slice(program.lastIndexOf("/") + 1) || `process ${member.pid ?? "?"}`;
}

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

export function showsProcesses(group: Group | null | undefined): group is Group {
  return (group?.members.length ?? 0) > 1;
}

export function Processes() {
  const focus = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const [busy, setBusy] = createSignal(false);

  const ask = (method: "selectMember" | "detachMember", member: number) => {
    setBusy(true);
    void connection
      .request(method, { member })
      .catch((failure: Error) => flash(failure.message))
      .finally(() => setBusy(false));
  };

  const split = createMemo(() => {
    const group = focus().state.group;
    if (!showsProcesses(group)) {
      return null;
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
    return { group, selected, active, other };
  });

  const detach = () => {
    const selectedNumber = split()?.group.selected;
    assert(selectedNumber !== undefined, "detaching with no group");
    ask("detachMember", selectedNumber);
  };

  const detachable = createMemo(() => {
    const current = split()?.selected;
    return control() && current && current.number !== 1 && current.state === "stopped"
      ? current
      : null;
  });

  return (
    <Show
      when={split()}
      fallback={
        <div class="pane-body">
          <div class="empty">This session debugs one process.</div>
        </div>
      }
    >
      {(data) => (
        <nav class="pane-body processes" aria-label="Processes" data-testid="processes">
          <ul>
            <For each={data().active} keyed={(member) => member.number}>
              {(member) => (
                <ProcessRow
                  member={member()}
                  on={member().number === data().group.selected}
                  stoppedHere={member().number === data().group.stopped}
                  disabled={!control() || busy()}
                  onAsk={ask}
                />
              )}
            </For>
          </ul>
          <Show when={detachable()}>
            {(selected) => (
              <button
                type="button"
                class="button small"
                disabled={busy()}
                title="Stop debugging this process and let it run. The others stay attached."
                onClick={detach}
              >
                Detach [{selected().number}]
              </button>
            )}
          </Show>
          <Show when={data().other.length > 0}>
            <details>
              <summary>Other processes ({data().other.length})</summary>
              <ul>
                <For each={data().other} keyed={(member) => member.number}>
                  {(member) => (
                    <ProcessRow
                      member={member()}
                      on={member().number === data().group.selected}
                      stoppedHere={member().number === data().group.stopped}
                      disabled={!control() || busy()}
                      onAsk={ask}
                    />
                  )}
                </For>
              </ul>
            </details>
          </Show>
        </nav>
      )}
    </Show>
  );
}

function ProcessRow(props: {
  member: Member;
  on: boolean;
  stoppedHere: boolean;
  disabled: boolean;
  onAsk: (method: "selectMember", member: number) => void;
}) {
  const debugged = createMemo(
    () => props.member.state === "running" || props.member.state === "stopped",
  );
  const onClick = () => {
    if (!props.on) {
      props.onAsk("selectMember", props.member.number);
    }
  };
  return (
    <li>
      <button
        type="button"
        class={["process", { on: props.on }]}
        aria-pressed={props.on ? "true" : "false"}
        disabled={props.disabled || !debugged()}
        title={[
          `[${props.member.number}] process ${props.member.pid ?? "not started"}`,
          props.member.command,
          joinedText(props.member),
          props.member.description,
        ]
          .filter(Boolean)
          .join("\n")}
        onClick={onClick}
      >
        <span class="number">[{props.member.number}]</span> {memberName(props.member)}
        <span class="muted"> · {joinedText(props.member)} · </span>
        <span class={`state ${props.member.state}`}>{STATES[props.member.state]}</span>
        <Show when={props.stoppedHere}>
          <span> ◂ stopped here</span>
        </Show>
      </button>
    </li>
  );
}

export function ProcessesBadge() {
  const count = useModel((model) => model.state?.group?.members.length ?? 0);
  return (
    <Show when={count() > 1}>
      <span class="count">{count()}</span>
    </Show>
  );
}
