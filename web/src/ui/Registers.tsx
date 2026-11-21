import { createMemo, For, Match, Show, Switch } from "solid-js";

import { assert } from "../assert";
import { useRequest } from "../data";
import type { At } from "../focus";
import type { Register, Row } from "../protocol";
import { errorClass, hidden } from "./hidden";
import { LinkedExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

function row(register: Register, innermost: boolean): Row {
  return {
    name: register.name,
    text: register.value ?? "not saved",
    detail: null,
    type: register.role,
    path: `$${register.name}`,
    children: null,
    editable: innermost && register.value !== null,
    memory: null,
    memoryBytes: null,
    textBytes: null,
    stored: false,
    truncated: false,
    drawings: [],
  };
}

export function Registers() {
  const focus = useFocus();
  const why = createMemo(() => hidden(focus(), "Registers"));
  const registers = useRequest("registers", () => {
    const current = focus();
    const reason = why();
    assert(reason !== null || current.at !== null, "registers shown with no stop");
    return reason === null ? (current.at as At) : null;
  });

  return (
    <div class="pane-body registers">
      <LinkedExpansion>
        <Switch fallback={<div class="empty">Reading registers…</div>}>
          <Match when={why()}>{(reason) => <div class="empty">{reason()}</div>}</Match>
          <Match when={registers().error}>
            {(err) => <div class={errorClass(err())}>{err().message}</div>}
          </Match>
          <Match when={registers().data}>
            {(data) => {
              const frame = createMemo(() => {
                const at = focus().at;
                assert(at !== null, "registers listed with no stop");
                return at.frame;
              });
              return (
                <ul class="value-list" data-testid="registers">
                  <For each={data().registers} keyed={(register) => register.name}>
                    {(register) => (
                      <ValueRow
                        row={row(register(), frame() === 0)}
                        parent={`registers@${frame()}`}
                        depth={0}
                        missing={register().value === null ? "absent" : undefined}
                      />
                    )}
                  </For>
                </ul>
              );
            }}
          </Match>
        </Switch>
      </LinkedExpansion>
    </div>
  );
}

export function RegistersBadge() {
  const focus = useFocus();
  return <Show when={focus().at}>{(at) => <span class="count">frame {at().frame}</span>}</Show>;
}
