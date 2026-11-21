import { createSignal, createUniqueId, For, Show } from "solid-js";

import { isString, read, write } from "../storage";
import { useConnection, useModel } from "../store";
import { flash } from "../tab";
import { chooseTheme, type Theme, THEMES, useChosenTheme } from "../theme";
import { deleteNamed, layoutNames, loadNamed, saveNamed } from "../workbench/persist";
import { workbench } from "../workbench/store";
import { COMMANDS } from "./commands";
import { applyLayout, KINDS } from "./panes";

const THEME_LABELS: Record<Theme, string> = {
  system: "The system's",
  light: "Light",
  dark: "Dark",
};

export function SettingsBody() {
  const theme = useChosenTheme();
  return (
    <div class="pane-body pane-text settings">
      <h2>Theme</h2>
      <div class="segmented" role="radiogroup" aria-label="Theme">
        <For each={THEMES}>{(each) => <ThemeButton theme={each} checked={theme() === each} />}</For>
      </div>
      <h2>Your name</h2>
      <YourName />
      <h2>Layouts</h2>
      <Layouts />
    </div>
  );
}

function ThemeButton(props: { theme: Theme; checked: boolean }) {
  return (
    <button
      type="button"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a segmented control of three choices
      role="radio"
      aria-checked={props.checked ? "true" : "false"}
      onClick={() => chooseTheme(props.theme)}
    >
      {THEME_LABELS[props.theme]}
    </button>
  );
}

function YourName() {
  const id = createUniqueId();
  const connection = useConnection();
  const open = useModel((model) => model.link === "open");
  const me = useModel(
    (model) => model.people.find((person) => person.connection === model.hello?.connection)?.name,
  );
  const [name, setName] = createSignal(read("microscope-name", "", isString));
  const [error, setError] = createSignal<string | null>(null);
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    const chosen = name().trim();
    write("microscope-name", chosen);
    setError(null);
    if (open() && chosen) {
      connection
        .request("setName", { name: chosen })
        .then(() => flash(`Others see you as ${chosen}`))
        .catch((failure: Error) => setError(failure.message));
    }
  };
  return (
    <form class="pane-form" onSubmit={submit}>
      <div class="field">
        <label for={id}>Name others see in a session</label>
        <input
          id={id}
          class="input"
          value={name()}
          placeholder={me() ?? "your name"}
          onInput={(event) => setName(event.currentTarget.value)}
        />
      </div>
      <Show when={error()}>{(message) => <span class="error">{message()}</span>}</Show>
      <div class="actions">
        <button type="submit" class="button" disabled={!name().trim()}>
          Use this name
        </button>
      </div>
    </form>
  );
}

function Layouts() {
  const id = createUniqueId();
  const [names, setNames] = createSignal(layoutNames());
  const [name, setName] = createSignal("");
  const save = (event: SubmitEvent) => {
    event.preventDefault();
    saveNamed(name().trim(), workbench.getState().layout);
    setNames(layoutNames());
    setName("");
  };
  const deleted = () => setNames(layoutNames());
  return (
    <>
      <Show when={names().length > 0} fallback={<p class="muted">No layouts are saved yet.</p>}>
        <ul class="rows">
          <For each={names()}>{(saved) => <LayoutRow saved={saved} onDeleted={deleted} />}</For>
        </ul>
      </Show>
      <form class="pane-form" onSubmit={save}>
        <div class="field">
          <label for={id}>Save this layout as</label>
          <input
            id={id}
            class="input"
            value={name()}
            onInput={(event) => setName(event.currentTarget.value)}
          />
        </div>
        <div class="actions">
          <button type="submit" class="button" disabled={!name().trim()}>
            Save
          </button>
        </div>
      </form>
    </>
  );
}

function LayoutRow(props: { saved: string; onDeleted: () => void }) {
  const open = () => {
    const loaded = loadNamed(props.saved, KINDS);
    if (loaded.ok) {
      applyLayout(loaded.layout);
    } else {
      flash(`The layout ${props.saved} can't be opened: ${loaded.why}`);
    }
  };
  const remove = () => {
    deleteNamed(props.saved);
    props.onDeleted();
  };
  return (
    <li class="layout-row">
      <span>{props.saved}</span>
      <button type="button" class="link-button" onClick={open}>
        Open
      </button>
      <button type="button" class="link-button" onClick={remove}>
        Delete
      </button>
    </li>
  );
}

export function KeysBody() {
  const commands = COMMANDS.ids.filter((id) => COMMANDS.get(id).keys.length > 0);
  return (
    <div class="pane-body pane-text">
      <p class="muted">Function keys work everywhere; letters only outside text boxes.</p>
      <table class="table">
        <thead>
          <tr>
            <th>Command</th>
            <th>Key</th>
            <th>Letter</th>
          </tr>
        </thead>
        <tbody>
          <For each={commands}>
            {(command) => (
              <tr>
                <td>{COMMANDS.get(command).label}</td>
                <td>
                  <Show when={COMMANDS.describe(command)}>{(text) => <kbd>{text()}</kbd>}</Show>
                </td>
                <td>
                  <Show when={COMMANDS.describe(command, true)}>
                    {(text) => <kbd>{text()}</kbd>}
                  </Show>
                </td>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </div>
  );
}
