import { createMemo, For, Show } from "solid-js";

import { type ActionName, action, continueLabel } from "../actions";
import { useModel } from "../store";
import { useTab } from "../tab";
import { COMMANDS, useCommands } from "./commands";
import { Status } from "./Status";

const BUTTONS: { name: ActionName; glyph: string; label?: string }[] = [
  { name: "continue", glyph: "▶" },
  { name: "pause", glyph: "⏸", label: "Pause" },
  { name: "over", glyph: "↷", label: "Over" },
  { name: "into", glyph: "↓", label: "Into" },
  { name: "out", glyph: "↑", label: "Out" },
  { name: "restart", glyph: "⟲", label: "Restart" },
  { name: "kill", glyph: "■", label: "Kill" },
];

export function RunToolbar() {
  const hello = useModel((model) => model.hello !== null);
  const control = useModel((model) => model.hello?.role === "control");

  return (
    <div class="toolbar" role="toolbar" aria-label="Run">
      <Show when={!control() && hello()}>
        <span class="pill idle" title="This link can watch the session but not run or stop it">
          View only
        </span>
      </Show>
      <Show when={control()}>
        <For each={BUTTONS}>
          {(button) => (
            <ActionButton name={button.name} glyph={button.glyph} label={button.label} />
          )}
        </For>
      </Show>
      <span class="separator" />
      <Status />
    </div>
  );
}

function ActionButton(props: { name: ActionName; glyph: string; label?: string | undefined }) {
  const { run } = useCommands();
  const at = useTab((current) => current.at);
  const why = useModel((model) => {
    const current = action(props.name, model, at());
    return current.enabled ? null : current.reason;
  });
  const running = useModel((model) => model.state?.inferior.state === "running");
  const text = useModel((model) => props.label ?? continueLabel(model));
  const key = createMemo(() => COMMANDS.describe(props.name));
  const onClick = () => run(props.name);
  const primary = createMemo(
    () =>
      (props.name === "continue" || props.name === "pause") &&
      why() === null &&
      (props.name === "pause" || !running()),
  );
  return (
    <button
      type="button"
      class={["tb", { primary: primary() }]}
      disabled={why() !== null}
      title={`${text()}${key() ? ` (${key()})` : ""}${why() === null ? "" : `: ${why()}`}`}
      data-action={props.name}
      onClick={onClick}
    >
      <span class="glyph">{props.glyph}</span>
      <span class={props.name === "continue" || props.name === "pause" ? "" : "label"}>
        {text()}
      </span>
      <Show when={key()}>{(hint) => <span class="hint">{hint()}</span>}</Show>
    </button>
  );
}
