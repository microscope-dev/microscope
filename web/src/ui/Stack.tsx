import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { assert } from "../assert";
import { unreached } from "../follow";
import type { Backtrace, Frame } from "../protocol";
import { stackRows } from "../stack";
import { threadLabel } from "./names";
import { useGo } from "./navigation";
import { fileName } from "./paths";
import { useFocus } from "./Workspace";

const NONE_OPEN: ReadonlySet<number> = new Set();

export function Stack() {
  const focus = useFocus();
  const go = useGo();

  const [opened, setOpened] = createSignal<{
    trace?: Backtrace | undefined;
    runs: ReadonlySet<number>;
  }>({
    runs: new Set(),
  });
  const runs = createMemo(() => (opened().trace === focus().trace ? opened().runs : NONE_OPEN));
  const toggle = (first: number) => {
    const next = new Set(runs());
    if (!next.delete(first)) {
      next.add(first);
    }
    setOpened({ trace: focus().trace, runs: next });
  };
  const choose = (frame: number) => {
    const currentAt = focus().at;
    assert(currentAt !== null, `frame ${frame} chosen with no stop`);
    go({ ...currentAt, frame });
  };
  const rows = createMemo(() => {
    const { at, trace } = focus();
    return at && trace ? stackRows(trace, at.frame, runs()) : [];
  });

  return (
    <div class={["pane-body", { stale: Boolean(focus().stale && focus().stale !== "passed") }]}>
      <Switch>
        <Match when={!focus().at}>
          <div class="empty">The stack appears when the program stops.</div>
        </Match>
        <Match when={focus().trace}>
          {(trace) => (
            <ul class="rows" data-testid="stack">
              <For
                each={rows()}
                keyed={(row) =>
                  row.kind === "fold" ? `fold ${row.run.first}` : `frame ${row.frame.index}`
                }
              >
                {(row) => {
                  const foldRow = createMemo(() => {
                    const current = row();
                    if (current.kind !== "fold") {
                      return null;
                    }
                    assert(
                      current.run.count > 0,
                      `fold at frame ${current.run.first} holds ${current.run.count} frames`,
                    );
                    return current;
                  });
                  const frameRow = createMemo(() => {
                    const current = row();
                    return current.kind === "frame" ? current.frame : null;
                  });
                  return (
                    <Switch>
                      <Match when={foldRow()}>
                        {(fold) => (
                          <FoldRow
                            first={fold().run.first}
                            count={fold().run.count}
                            open={fold().open}
                            onToggle={toggle}
                          />
                        )}
                      </Match>
                      <Match when={frameRow()}>
                        {(frame) => (
                          <>
                            <Show when={frame().unfollowed}>
                              {(unfollowed) => (
                                <li class="row dim" title={unfollowed()}>
                                  {unfollowed()}
                                </li>
                              )}
                            </Show>
                            <FrameRow
                              frame={frame()}
                              selected={frame().index === focus().at?.frame}
                              onChoose={choose}
                            />
                          </>
                        )}
                      </Match>
                    </Switch>
                  );
                }}
              </For>
              <Show when={trace().incomplete}>
                {(incomplete) => (
                  <li class="row dim" title={incomplete()}>
                    the stack ends here: {incomplete()}
                  </li>
                )}
              </Show>
            </ul>
          )}
        </Match>
        <Match when={focus().at}>
          {(at) => (
            <div class="empty">
              {focus().stale === "passed"
                ? unreached(focus().state, at().stop)
                  ? `This session has not reached stop #${at().stop}.`
                  : `Stop #${at().stop} has passed, so its stack is no longer available.`
                : "Reading the stack…"}
            </div>
          )}
        </Match>
      </Switch>
    </div>
  );
}

function FoldRow(props: {
  first: number;
  count: number;
  open: boolean;
  onToggle: (first: number) => void;
}) {
  return (
    <li>
      <button
        type="button"
        class="row button-row fold"
        aria-expanded={props.open ? "true" : "false"}
        title={`Frames ${props.first}–${props.first + props.count - 1}: the runtime's own`}
        onClick={() => props.onToggle(props.first)}
      >
        <span class="dim index">{props.open ? "▾" : "▸"}</span>
        <span class="dim">
          {props.count} frames of the runtime{props.open ? "" : " folded"}
        </span>
      </button>
    </li>
  );
}

function FrameRow(props: { frame: Frame; selected: boolean; onChoose: (frame: number) => void }) {
  return (
    <li>
      <button
        type="button"
        class={["row button-row", { selected: props.selected }]}
        aria-current={props.selected ? "true" : undefined}
        title={
          props.frame.address ? `${props.frame.name} at ${props.frame.address}` : props.frame.name
        }
        onClick={() => props.onChoose(props.frame.index)}
      >
        <span class="dim index">{props.frame.index}</span>
        <span class="mark" title={props.frame.language?.name}>
          {props.frame.language?.mark}
        </span>
        <span class={props.frame.source ? "fn" : "fn subtle"}>
          {props.frame.name}
          <Show when={props.frame.kind === "inline"}>
            <span class="dim"> inlined</span>
          </Show>
          <Show when={props.frame.kind === "tailCall"}>
            <span class="dim"> tail call</span>
          </Show>
          <Show when={props.frame.kind === "async"}>
            <span class="dim"> async</span>
          </Show>
        </span>
        <span class="end">{where(props.frame)}</span>
      </button>
    </li>
  );
}

export function StackBadge() {
  const focus = useFocus();
  const task = createMemo(() => focus().at?.task ?? null);
  const thread = createMemo(() => {
    const { state, at } = focus();
    return state.threads.find((candidate) => candidate.id === at?.thread) ?? null;
  });
  return (
    <Switch>
      <Match when={task()}>{(current) => <span class="count">task {current().number}</span>}</Match>
      <Match when={thread()}>
        {(current) => <span class="count">{threadLabel(current().id, current().name)}</span>}
      </Match>
    </Switch>
  );
}

export function where(frame: Frame): string {
  if (frame.source) {
    const column = frame.source.column === null ? "" : `:${frame.source.column}`;
    return `${fileName(frame.source.path)}:${frame.source.line}${column}`;
  }
  return frame.module ?? frame.address ?? "";
}
