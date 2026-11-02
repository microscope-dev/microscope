import { type ActionName, action, continueLabel } from "../actions";
import { useModel } from "../store";
import { useTab } from "../tab";
import { COMMANDS, useCommands } from "./commands";
import { People } from "./People";
import { Share } from "./Share";
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

/** The run buttons, where the program is, who else is here, and sharing. */
export function RunToolbar() {
  const hello = useModel((model) => model.hello !== null);
  const control = useModel((model) => model.hello?.role === "control");

  return (
    <div className="toolbar" role="toolbar" aria-label="Run">
      {!control && hello && (
        <span className="pill idle" title="This link can watch the session but not run or stop it">
          View only
        </span>
      )}
      {control && BUTTONS.map((button) => <ActionButton key={button.name} {...button} />)}
      <span className="separator" />
      <Status />
      <People />
      <Share />
    </div>
  );
}

/** One of the run buttons, which runs its command. It reads only what it
 * shows, so output arriving does not render the toolbar again. */
function ActionButton({ name, glyph, label }: { name: ActionName; glyph: string; label?: string }) {
  const { run } = useCommands();
  const at = useTab((current) => current.at);
  const why = useModel((model) => {
    const current = action(name, model, at);
    return current.enabled ? null : current.reason;
  });
  const running = useModel((model) => model.state?.inferior.state === "running");
  const text = useModel((model) => label ?? continueLabel(model));
  const key = COMMANDS.describe(name);
  const primary =
    (name === "continue" || name === "pause") && why === null && (name === "pause" || !running);
  return (
    <button
      type="button"
      className={`tb ${primary ? "primary" : ""}`}
      disabled={why !== null}
      title={`${text}${key ? ` (${key})` : ""}${why === null ? "" : `: ${why}`}`}
      data-action={name}
      onClick={() => run(name)}
    >
      <span className="glyph">{glyph}</span>
      <span className={name === "continue" || name === "pause" ? "" : "label"}>{text}</span>
      {key && <span className="hint">{key}</span>}
    </button>
  );
}
