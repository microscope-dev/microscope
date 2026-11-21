import { createMemo, For, Show } from "solid-js";

import { assert } from "../assert";
import { openingFrame } from "../follow";
import type { Inferior, Thread } from "../protocol";
import { threadLabel } from "./names";
import { useGo } from "./navigation";
import { useFocus } from "./Workspace";

export function Threads() {
  const focus = useFocus();
  const go = useGo();
  const inferior = createMemo(() => focus().state.inferior);
  const stopping = createMemo(() => {
    const current = inferior();
    return current.state === "stopped" ? current.thread : null;
  });
  const shown = createMemo(() => {
    const currentAt = focus().at;
    return currentAt?.task ? null : (currentAt?.thread ?? stopping());
  });
  const choosable = createMemo(() => focus().at !== null && focus().stale === null);
  const choose = (thread: number) => {
    const { at, state } = focus();
    assert(at !== null, `thread ${thread} chosen with no stop`);
    go({ stop: at.stop, thread, frame: openingFrame(state, at.stop, thread) });
  };

  return (
    <div class={["pane-body", { stale: Boolean(focus().stale) }]}>
      <Show
        when={focus().state.threads.length > 0}
        fallback={<div class="empty">{emptyThreads(inferior())}</div>}
      >
        <ul class="rows" data-testid="threads">
          <For each={focus().state.threads} keyed={(thread) => thread.id}>
            {(thread) => (
              <ThreadRow
                thread={thread()}
                shown={thread().id === shown()}
                stopping={thread().id === stopping()}
                choosable={choosable()}
                onChoose={choose}
              />
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}

function ThreadRow(props: {
  thread: Thread;
  shown: boolean;
  stopping: boolean;
  choosable: boolean;
  onChoose: (thread: number) => void;
}) {
  return (
    <li>
      <button
        type="button"
        class={["row button-row", { selected: props.shown }]}
        disabled={!props.choosable}
        aria-current={props.shown ? "true" : undefined}
        onClick={() => props.onChoose(props.thread.id)}
      >
        <span>{threadLabel(props.thread.id, props.thread.name)}</span>
        <For each={props.thread.labels}>{(label) => <span class="dim">{label}</span>}</For>
        <span class="end">
          {props.stopping ? "◂ stopped here" : props.thread.stopped ? "stopped" : "running"}
        </span>
      </button>
    </li>
  );
}

function emptyThreads(inferior: Inferior): string {
  switch (inferior.state) {
    case "notStarted":
      return "No threads until the program runs.";
    case "running":
      return "No threads yet.";
    case "stopped":
      return "No threads.";
    case "exited":
      return "The program exited.";
    case "detached":
      return "microscope let the process go.";
  }
}
