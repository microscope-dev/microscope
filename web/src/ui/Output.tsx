import { createSignal, For, Show } from "solid-js";

import { controls } from "../model";
import { useConnection, useModel } from "../store";
import { useFollowBottom } from "./scroll";

export function OutputBadge() {
  const bytes = useModel((model) => model.outputBytes);
  return (
    <Show when={bytes() > 0}>
      <span class="count"> {bytes().toLocaleString()} bytes</span>
    </Show>
  );
}

export function Output() {
  return (
    <div class="output-pane">
      <Written />
      <Input />
    </div>
  );
}

function Written() {
  const output = useModel((model) => model.output);
  const { ref, onScroll } = useFollowBottom<HTMLDivElement>(output);

  return (
    <div class="pane-body" ref={ref} onScroll={onScroll}>
      <Show
        when={output().length > 0}
        fallback={<div class="empty">The program&apos;s output appears here.</div>}
      >
        <pre class="output" data-testid="output">
          <For each={output()} keyed={(piece) => piece.seq}>
            {(piece) => <span class={piece().stream}>{piece().text}</span>}
          </For>
        </pre>
      </Show>
    </div>
  );
}

function Input() {
  const control = useModel(controls);
  const running = useModel(
    (model) =>
      model.state?.inferior.state === "running" || model.state?.inferior.state === "stopped",
  );
  const connection = useConnection();
  const [text, setText] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);

  const send = (eof: boolean) => {
    setError(null);
    const current = text();
    connection
      .request("input", { text: eof ? current : `${current}\n`, eof })
      .then(() => setText(""))
      .catch((failure: Error) => setError(failure.message));
  };

  const onSubmit = (event: SubmitEvent) => {
    event.preventDefault();
    send(false);
  };

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === "d" && event.ctrlKey) {
      event.preventDefault();
      send(true);
    }
  };

  return (
    <Show when={control() && running()}>
      <form class="program-input" onSubmit={onSubmit}>
        <span class="prompt" aria-hidden="true">
          ›
        </span>
        <input
          class="input quiet"
          aria-label="Program input"
          placeholder="input for the program; Enter sends a line, Ctrl+D closes input"
          value={text()}
          onInput={(event) => setText(event.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <Show when={error()}>
          {(err) => (
            <span class="error" title={err()}>
              {err()}
            </span>
          )}
        </Show>
      </form>
    </Show>
  );
}
