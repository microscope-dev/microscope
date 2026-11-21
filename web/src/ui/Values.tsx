import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { assert } from "../assert";
import { useRequest } from "../data";
import { type At, watching } from "../focus";
import { hidden } from "./hidden";
import { useLook } from "./navigation";
import { Earlier, LinkedExpansion, messageRow, RowList, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

export { errorClass, hidden } from "./hidden";

export function Variables() {
  const focus = useFocus();
  const why = createMemo(() => hidden(focus()));
  const scopes = useRequest("scopes", () => {
    const current = focus();
    const reason = why();
    assert(reason !== null || current.at !== null, "variables shown with no stop");
    return reason === null ? (current.at as At) : null;
  });
  const statics = createMemo(() => focus().look.x?.includes("statics") ?? false);
  const look = useLook();
  const toggleStatics = () => {
    const was = statics();
    look((current) => {
      const x = (current.x ?? []).filter((key) => key !== "statics");
      const next = was ? x : [...x, "statics"];
      const { x: _x, ...rest } = current;
      return next.length > 0 ? { ...rest, x: next } : rest;
    });
  };

  return (
    <div class="pane-body">
      <LinkedExpansion>
        <Switch fallback={<div class="empty">Reading variables…</div>}>
          <Match when={why()}>{(reason) => <div class="empty">{reason()}</div>}</Match>
          <Match when={scopes().error}>
            {(err) => <div class="empty error">{err().message}</div>}
          </Match>
          <Match when={scopes().data}>
            {(data) => (
              <Earlier when={!scopes().current}>
                <ul class="value-list" aria-label="Variables" data-testid="variables">
                  <For each={data().scopes} keyed={(scope) => scope.key}>
                    {(scope) => {
                      const closable = createMemo(() => scope().key === "statics");
                      const open = createMemo(() => !closable() || statics());
                      return (
                        <li class="scope">
                          <div class="scope-head">
                            <Show when={closable()}>
                              <button
                                type="button"
                                class="twist"
                                aria-label={`${open() ? "Close" : "Open"} ${scope().name}`}
                                aria-expanded={open() ? "true" : "false"}
                                onClick={toggleStatics}
                              >
                                {open() ? "▾" : "▸"}
                              </button>
                            </Show>
                            {scope().name}
                            <Show when={scope().rows.length > 0}>
                              <span class="count">{scope().rows.length}</span>
                            </Show>
                          </div>
                          <Show when={open()}>
                            <ul class="value-list">
                              <Show when={scope().problem}>
                                {(problem) => <li class="empty error">{problem()}</li>}
                              </Show>
                              <Show when={scope().note}>
                                {(note) => <li class="empty">{note()}</li>}
                              </Show>
                              <Show
                                when={
                                  scope().rows.length === 0 && !scope().problem && !scope().note
                                }
                              >
                                <li class="empty">none</li>
                              </Show>
                              <RowList rows={scope().rows} parent={scope().key} depth={0} />
                            </ul>
                          </Show>
                        </li>
                      );
                    }}
                  </For>
                </ul>
              </Earlier>
            )}
          </Match>
        </Switch>
      </LinkedExpansion>
    </div>
  );
}

export function Watch() {
  const focus = useFocus();
  const watches = createMemo(() => focus().look.w ?? []);
  const look = useLook();
  const [text, setText] = createSignal("");
  const why = createMemo(() => hidden(focus()));

  const entries = createMemo(() => {
    const list = watches();
    const seen = new Map<string, number>();
    return list.map((expression, index) => {
      const occurrence = (seen.get(expression) ?? 0) + 1;
      seen.set(expression, occurrence);
      return { key: `${expression}~${occurrence}`, expression, occurrence, index };
    });
  });

  const remove = (index: number) => {
    const count = watches().length;
    assert(index >= 0 && index < count, `removing watch ${index} of ${count}`);
    look((current) => {
      const w = (current.w ?? []).filter((_, at) => at !== index);
      const { w: _w, ...rest } = current;
      return w.length > 0 ? { ...rest, w } : rest;
    });
  };
  const add = (event: SubmitEvent) => {
    event.preventDefault();
    const expression = text().trim();
    if (expression) {
      look((current) => watching(current, expression));
      setText("");
    }
  };

  return (
    <div class="pane-body">
      <Show when={why() && watches().length > 0 ? why() : null}>
        {(reason) => <div class="empty">{reason()}</div>}
      </Show>
      <LinkedExpansion>
        <ul class="value-list" aria-label="Watches" data-testid="watches">
          <For each={entries()} keyed={(item) => item.key}>
            {(item) => (
              <WatchRow
                expression={item().expression}
                occurrence={item().occurrence}
                index={item().index}
                why={why()}
                onRemove={remove}
              />
            )}
          </For>
        </ul>
      </LinkedExpansion>
      <form class="watch-add" onSubmit={add}>
        <input
          class="input quiet"
          aria-label="Add a watch"
          placeholder="+ expression"
          value={text()}
          onInput={(event) => setText(event.currentTarget.value)}
        />
      </form>
    </div>
  );
}

export function WatchBadge() {
  const focus = useFocus();
  const count = createMemo(() => focus().look.w?.length ?? 0);
  return (
    <Show when={count() > 0}>
      <span class="count">{count()}</span>
    </Show>
  );
}

function WatchRow(props: {
  expression: string;
  occurrence: number;
  index: number;
  why: string | null;
  onRemove(index: number): void;
}) {
  const focus = useFocus();
  const value = useRequest("evaluate", () => {
    const currentAt = focus().at;
    return props.why === null && currentAt ? { ...currentAt, expression: props.expression } : null;
  });
  const remove = () => (
    <button
      type="button"
      class="icon remove"
      aria-label={`Remove the watch ${props.expression}`}
      title="Remove"
      onClick={() => props.onRemove(props.index)}
    >
      ×
    </button>
  );

  return (
    <Switch
      fallback={
        <Earlier when>
          <ValueRow
            row={messageRow(props.expression, "…")}
            parent="w"
            depth={0}
            occurrence={props.occurrence}
            actions={remove()}
          />
        </Earlier>
      }
    >
      <Match when={props.why !== null}>
        <ValueRow
          row={messageRow(props.expression, "not read")}
          parent="w"
          depth={0}
          occurrence={props.occurrence}
          actions={remove()}
          missing="absent"
        />
      </Match>
      <Match when={value().error}>
        {(err) => (
          <ValueRow
            row={messageRow(props.expression, err().message)}
            parent="w"
            depth={0}
            occurrence={props.occurrence}
            actions={remove()}
            missing="failed"
          />
        )}
      </Match>
      <Match when={value().data}>
        {(row) => (
          <Earlier when={!value().current}>
            <ValueRow
              row={row()}
              parent="w"
              depth={0}
              occurrence={props.occurrence}
              actions={remove()}
            />
          </Earlier>
        )}
      </Match>
    </Switch>
  );
}
