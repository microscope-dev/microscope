// Settings this browser keeps: the theme, the name others see in a session,
// and saved layouts; and every key, from the command registry.

import { type FormEvent, useId, useState } from "react";

import { isString, read, write } from "../storage";
import { useConnection, useModel } from "../store";
import { flash } from "../tab";
import { chooseTheme, THEMES, useChosenTheme } from "../theme";
import { deleteNamed, layoutNames, loadNamed, saveNamed } from "../workbench/persist";
import { workbench } from "../workbench/store";
import { COMMANDS } from "./commands";
import { applyLayout, KINDS } from "./panes";

export function SettingsBody() {
  const theme = useChosenTheme();
  return (
    <div className="pane-body pane-text settings">
      <h2>Theme</h2>
      <div className="segmented" role="radiogroup" aria-label="Theme">
        {THEMES.map((each) => (
          // biome-ignore lint/a11y/useSemanticElements: a segmented control of three choices
          <button
            key={each}
            type="button"
            role="radio"
            aria-checked={theme === each}
            onClick={() => chooseTheme(each)}
          >
            {{ system: "The system's", light: "Light", dark: "Dark" }[each]}
          </button>
        ))}
      </div>
      <h2>Your name</h2>
      <YourName />
      <h2>Layouts</h2>
      <Layouts />
    </div>
  );
}

function YourName() {
  const id = useId();
  const connection = useConnection();
  const open = useModel((model) => model.link === "open");
  const me = useModel(
    (model) => model.people.find((person) => person.connection === model.hello?.connection)?.name,
  );
  const [name, setName] = useState(() => read("microscope-name", "", isString));
  const [error, setError] = useState<string | null>(null);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    const chosen = name.trim();
    write("microscope-name", chosen);
    setError(null);
    if (open && chosen) {
      connection
        .request("setName", { name: chosen })
        .then(() => flash(`Others see you as ${chosen}`))
        .catch((failure: Error) => setError(failure.message));
    }
  };
  return (
    <form className="pane-form" onSubmit={submit}>
      <div className="field">
        <label htmlFor={id}>Name others see in a session</label>
        <input
          id={id}
          className="input"
          value={name}
          placeholder={me ?? "your name"}
          onChange={(event) => setName(event.target.value)}
        />
      </div>
      {error && <span className="error">{error}</span>}
      <div className="actions">
        <button type="submit" className="button" disabled={!name.trim()}>
          Use this name
        </button>
      </div>
    </form>
  );
}

function Layouts() {
  const id = useId();
  const [names, setNames] = useState(layoutNames);
  const [name, setName] = useState("");
  const save = (event: FormEvent) => {
    event.preventDefault();
    saveNamed(name.trim(), workbench.getState().layout);
    setNames(layoutNames());
    setName("");
  };
  return (
    <>
      {names.length === 0 ? (
        <p className="muted">No layouts are saved yet.</p>
      ) : (
        <ul className="rows">
          {names.map((saved) => (
            <li key={saved} className="layout-row">
              <span>{saved}</span>
              <button
                type="button"
                className="link-button"
                onClick={() => {
                  const loaded = loadNamed(saved, KINDS);
                  if (loaded.ok) {
                    applyLayout(loaded.layout);
                  } else {
                    flash(`The layout ${saved} can't be opened: ${loaded.why}`);
                  }
                }}
              >
                Open
              </button>
              <button
                type="button"
                className="link-button"
                onClick={() => {
                  deleteNamed(saved);
                  setNames(layoutNames());
                }}
              >
                Delete
              </button>
            </li>
          ))}
        </ul>
      )}
      <form className="pane-form" onSubmit={save}>
        <div className="field">
          <label htmlFor={id}>Save this layout as</label>
          <input
            id={id}
            className="input"
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </div>
        <div className="actions">
          <button type="submit" className="button" disabled={!name.trim()}>
            Save
          </button>
        </div>
      </form>
    </>
  );
}

export function KeysBody() {
  const commands = COMMANDS.ids.filter((id) => COMMANDS.get(id).keys.length > 0);
  return (
    <div className="pane-body pane-text">
      <p className="muted">Function keys work everywhere; letters only outside text boxes.</p>
      <table className="table">
        <thead>
          <tr>
            <th>Command</th>
            <th>Key</th>
            <th>Letter</th>
          </tr>
        </thead>
        <tbody>
          {commands.map((command) => (
            <tr key={command}>
              <td>{COMMANDS.get(command).label}</td>
              <td>{COMMANDS.describe(command) && <kbd>{COMMANDS.describe(command)}</kbd>}</td>
              <td>
                {COMMANDS.describe(command, true) && <kbd>{COMMANDS.describe(command, true)}</kbd>}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
