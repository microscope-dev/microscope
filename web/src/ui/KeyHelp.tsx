// Every key, as `?` shows it: each command with its function key and its
// letter, for keyboards without function keys.

import { useRef } from "react";
import { BINDINGS, type Command, describe, LABELS } from "../keys";
import { tab, useTab } from "../tab";
import { useDialogFocus } from "./dialog";

export function KeyHelp() {
  const open = useTab((current) => current.help);
  return open ? <Keys /> : null;
}

function Keys() {
  const dialog = useRef<HTMLDivElement>(null);
  useDialogFocus(dialog);
  const commands = [...new Set(BINDINGS.map((binding) => binding.command))] as Command[];
  const done = () => tab.setState({ help: false });
  return (
    <div className="overlay">
      <button
        type="button"
        className="overlay-close"
        aria-label="Close the keys"
        tabIndex={-1}
        onClick={done}
      />
      <div className="card key-help" role="dialog" aria-modal="true" aria-label="Keys" ref={dialog}>
        <div className="card-head">
          <h1>Keys</h1>
          <button type="button" className="icon" aria-label="Close" onClick={done}>
            ×
          </button>
        </div>
        <div className="card-body">
          <p className="muted" style={{ margin: 0 }}>
            Function keys work everywhere; letters only outside text boxes.
          </p>
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
                  <td>{LABELS[command]}</td>
                  <td>{describe(command) && <kbd>{describe(command)}</kbd>}</td>
                  <td>{describe(command, true) && <kbd>{describe(command, true)}</kbd>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
