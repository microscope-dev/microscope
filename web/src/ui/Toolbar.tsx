import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { type ActionName, action, continueLabel } from "../actions";
import { describe } from "../keys";
import { targetName } from "../model";
import { useConnection, useModel, useStoreApi } from "../store";
import { useTab } from "../tab";
import { chooseTheme, THEMES, type Theme, useChosenTheme } from "../theme";
import { useCommands } from "./commands";
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

export function Toolbar() {
  const hello = useModel((model) => model.hello !== null);
  const control = useModel((model) => model.hello?.role === "control");
  const debugging = useModel((model) => Boolean(targetName(model.state)));
  const navigate = useNavigate();
  const run = useCommands();
  const [failure, setFailure] = useState<string | null>(null);

  return (
    <header className="toolbar">
      <Link to="/" className="brand" aria-label="microscope home">
        <img src="/favicon.svg" alt="" />
        microscope
      </Link>
      {!control && hello && (
        <span className="pill idle" title="This link can watch the session but not run or stop it">
          View only
        </span>
      )}
      {control &&
        BUTTONS.map((button) => (
          <ActionButton key={button.name} {...button} onFailure={setFailure} />
        ))}
      <span className="separator" />
      <Status />
      {failure && (
        <span className="pill warn" role="alert" title={failure}>
          {failure.length > 60 ? `${failure.slice(0, 60)}…` : failure}
        </span>
      )}
      <span className="spacer" />
      <People />
      <Share />
      {control && (
        <button
          type="button"
          className="tb ghost"
          onClick={() => navigate({ to: "/pick" })}
          title="Launch, attach, or open a core dump"
        >
          {debugging ? "Debug something else" : "Debug something"}
        </button>
      )}
      <ThemeButton />
      <button
        type="button"
        className="tb ghost"
        aria-label="Every key"
        title="Every key (?)"
        onClick={() => run("help")}
      >
        ?
      </button>
    </header>
  );
}

/** One of the run buttons. It reads only what it shows, so output arriving
 * does not render the toolbar again. */
function ActionButton({
  name,
  glyph,
  label,
  onFailure,
}: {
  name: ActionName;
  glyph: string;
  label?: string;
  onFailure(failure: string | null): void;
}) {
  const store = useStoreApi();
  const connection = useConnection();
  const at = useTab((current) => current.at);
  const why = useModel((model) => {
    const current = action(name, model, at);
    return current.enabled ? null : current.reason;
  });
  const running = useModel((model) => model.state?.inferior.state === "running");
  const text = useModel((model) => label ?? continueLabel(model));
  const key = describe(name);
  const primary =
    (name === "continue" || name === "pause") && why === null && (name === "pause" || !running);
  return (
    <button
      type="button"
      className={`tb ${primary ? "primary" : ""}`}
      disabled={why !== null}
      title={`${text}${key ? ` (${key})` : ""}${why === null ? "" : `: ${why}`}`}
      data-action={name}
      onClick={() => {
        const current = action(name, store.getState(), at);
        if (current.enabled) {
          onFailure(null);
          current.run(connection).catch((error: Error) => onFailure(error.message));
        }
      }}
    >
      <span className="glyph">{glyph}</span>
      <span className={name === "continue" || name === "pause" ? "" : "label"}>{text}</span>
      {key && <span className="hint">{key}</span>}
    </button>
  );
}

/** A circle, half filled for the system's theme, empty for light, full for dark. */
function ThemeGlyph({ theme }: { theme: Theme }) {
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
      <circle
        cx="7"
        cy="7"
        r="5.5"
        fill={theme === "dark" ? "currentColor" : "none"}
        stroke="currentColor"
        strokeWidth="1.5"
      />
      {theme === "system" && <path d="M7 1.5a5.5 5.5 0 0 1 0 11z" fill="currentColor" />}
    </svg>
  );
}

/** Cycles the theme: the system's, light, then dark. */
function ThemeButton() {
  const theme = useChosenTheme();
  const next = THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length] as Theme;
  return (
    <button
      type="button"
      className="tb ghost"
      aria-label={`Theme: ${theme}`}
      title={`Theme: ${theme}; click for ${next}`}
      onClick={() => chooseTheme(next)}
    >
      <ThemeGlyph theme={theme} />
    </button>
  );
}
