import { useState } from "react";
import { firstBreakpointPlace, knownBreakpointPlaces } from "../breakpoints";
import { controls } from "../model";
import type { Breakpoint, WatchAccess, Watchpoint } from "../protocol";
import { useConnection, useModel } from "../store";
import { tab, useTab } from "../tab";
import { useShowSource } from "./navigation";
import { fileName } from "./paths";
import { useFocus } from "./Workspace";

/** Every breakpoint and watchpoint, where it landed, and how often it was hit. */
export function Breakpoints() {
  const { state } = useFocus();
  const control = useModel(controls);
  const editing = useTab((current) => current.editing);
  const setEditing = (id: number | null) => tab.setState({ editing: id });
  const breakpoints = state.breakpoints;

  return (
    <section
      className="pane"
      style={{ flex: "0 1 auto", maxHeight: "40%" }}
      aria-label="Breakpoints"
      data-pane="3"
      tabIndex={-1}
    >
      <div className="pane-head">
        Breakpoints <span className="count">{breakpoints.length + state.watchpoints.length}</span>
        <span className="end">Alt+3</span>
      </div>
      <div className="pane-body">
        <ul className="rows" data-testid="breakpoints">
          {breakpoints.map((breakpoint) =>
            editing === breakpoint.id ? (
              <Editor key={breakpoint.id} breakpoint={breakpoint} done={() => setEditing(null)} />
            ) : (
              <Row
                key={breakpoint.id}
                breakpoint={breakpoint}
                control={control}
                edit={() => setEditing(breakpoint.id)}
              />
            ),
          )}
        </ul>
        {state.watchpoints.length > 0 && (
          <ul className="rows" data-testid="watchpoints">
            {state.watchpoints.map((watchpoint) => (
              <WatchRow key={watchpoint.id} watchpoint={watchpoint} control={control} />
            ))}
          </ul>
        )}
        {control && <Adder />}
        {control && <WatchAdder />}
        {!control && breakpoints.length + state.watchpoints.length === 0 && (
          <div className="empty">No breakpoints.</div>
        )}
      </div>
    </section>
  );
}

/** Where a breakpoint is, as briefly as it can be said. */
export function placeLabel(breakpoint: Breakpoint): string {
  const place = firstBreakpointPlace(breakpoint);
  if (place?.path && place.line) {
    const count = knownBreakpointPlaces(breakpoint).length;
    const more = count > 1 ? ` +${count - 1}` : "";
    return `${fileName(place.path)}:${place.line}${more}`;
  }
  return breakpoint.location;
}

/** A breakpoint's active locations, or why it remains pending. */
export function breakpointTitle(breakpoint: Breakpoint): string {
  const places = breakpoint.places
    .map((at) => `${at.function ?? at.address} ${at.path ? `${at.path}:${at.line}` : ""}`)
    .join("\n");
  if (breakpoint.pending) {
    return [places, breakpoint.pending.message].filter(Boolean).join("\n");
  }
  return places || `${breakpoint.location}: no loaded code has it yet`;
}

function Row({
  breakpoint,
  control,
  edit,
}: {
  breakpoint: Breakpoint;
  control: boolean;
  edit(): void;
}) {
  const connection = useConnection();
  const showSource = useShowSource();
  const [error, setError] = useState<string | null>(null);
  const place = firstBreakpointPlace(breakpoint);
  const pending = breakpoint.places.length === 0;
  const kind = breakpoint.logMessage ? "log" : breakpoint.condition ? "conditional" : "plain";
  const detail = breakpoint.logMessage
    ? `log "${breakpoint.logMessage}"`
    : [
        breakpoint.condition && `if ${breakpoint.condition}`,
        breakpoint.hitCondition && `hits ${breakpoint.hitCondition}`,
      ]
        .filter(Boolean)
        .join(" · ");

  return (
    <li className="breakpoint" data-breakpoint={breakpoint.id}>
      <div className="row">
        <span className={`dot ${kind} ${pending ? "pending" : ""}`} aria-hidden="true" />
        <button
          type="button"
          className="link-row"
          title={breakpointTitle(breakpoint)}
          disabled={!place?.path}
          onClick={() => place?.path && place.line && showSource(place.path, place.line)}
        >
          {placeLabel(breakpoint)}
        </button>
        {place?.function && <span className="dim fn-name">{place.function}</span>}
        <span className="end" title="Hits in this process, including those that did not stop">
          {breakpoint.hits} {breakpoint.hits === 1 ? "hit" : "hits"}
        </span>
        {control && (
          <>
            <button
              type="button"
              className="icon"
              title="Edit its condition"
              aria-label={`Edit breakpoint ${breakpoint.id}`}
              onClick={edit}
            >
              ✎
            </button>
            <button
              type="button"
              className="icon"
              title="Remove it"
              aria-label={`Remove breakpoint ${breakpoint.id}`}
              onClick={() =>
                connection
                  .request("removeBreakpoint", { id: breakpoint.id })
                  .catch((failure: Error) => setError(failure.message))
              }
            >
              ×
            </button>
          </>
        )}
      </div>
      {breakpoint.pending && <div className="row sub">{breakpoint.pending.message}</div>}
      {detail && <div className="row sub">{detail}</div>}
      {error && <div className="row sub error">{error}</div>}
    </li>
  );
}

/** Edits a breakpoint's condition, hit condition, and log message in place. */
function Editor({ breakpoint, done }: { breakpoint: Breakpoint; done(): void }) {
  const connection = useConnection();
  const [condition, setCondition] = useState(breakpoint.condition ?? "");
  const [hits, setHits] = useState(breakpoint.hitCondition ?? "");
  const [message, setMessage] = useState(breakpoint.logMessage ?? "");
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setError(null);
    const options = {
      condition: condition.trim() || null,
      hitCondition: hits.trim() || null,
      logMessage: message.trim() || null,
    };
    try {
      await connection.request("editBreakpoint", { id: breakpoint.id, ...options });
      done();
    } catch (failure) {
      setError((failure as Error).message);
    }
  };

  return (
    <li className="editor">
      <form
        className="editor-form"
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            done();
          }
        }}
      >
        <div className="editor-title">{placeLabel(breakpoint)}</div>
        <label>
          <span>Stop if</span>
          <input
            className="input"
            value={condition}
            placeholder="an expression, such as n > 3"
            // biome-ignore lint/a11y/noAutofocus: the editor opens because the person asked to type
            autoFocus
            onChange={(event) => setCondition(event.target.value)}
          />
        </label>
        <label>
          <span>Hits</span>
          <input
            className="input"
            value={hits}
            placeholder=">=5, ==3, or %10"
            onChange={(event) => setHits(event.target.value)}
          />
        </label>
        <label>
          <span>Log</span>
          <input
            className="input"
            value={message}
            placeholder="a message with {expressions}, instead of stopping"
            onChange={(event) => setMessage(event.target.value)}
          />
        </label>
        {error && <div className="error">{error}</div>}
        <div className="actions">
          <button type="submit" className="button primary small">
            Save
          </button>
          <button type="button" className="button small" onClick={done}>
            Cancel
          </button>
        </div>
      </form>
    </li>
  );
}

/** Adds a breakpoint at whatever is typed. */
function Adder() {
  const connection = useConnection();
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  return (
    <form
      className="adder"
      onSubmit={(event) => {
        event.preventDefault();
        const location = text.trim();
        if (!location) {
          return;
        }
        setError(null);
        connection
          .request("addBreakpoint", { location })
          .then(() => setText(""))
          .catch((failure: Error) => setError(failure.message));
      }}
    >
      <input
        className="input quiet"
        aria-label="Add a breakpoint"
        placeholder="+ breakpoint at function, file:line, or 0xaddress"
        value={text}
        onChange={(event) => setText(event.target.value)}
      />
      {error && <div className="error">{error}</div>}
    </form>
  );
}

const ACCESS: Record<WatchAccess, string> = {
  change: "change",
  write: "write",
  readWrite: "read or write",
  read: "read",
};

/** A watchpoint: what it watches, when it stops, and how often it was hit. */
function WatchRow({ watchpoint, control }: { watchpoint: Watchpoint; control: boolean }) {
  const connection = useConnection();
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const label = watchpoint.expression ?? `${watchpoint.address}:${watchpoint.bytes}`;
  const detail = [
    watchpoint.condition && `if ${watchpoint.condition}`,
    watchpoint.hitCondition && `hits ${watchpoint.hitCondition}`,
  ]
    .filter(Boolean)
    .join(" · ");
  if (editing) {
    return <WatchEditor watchpoint={watchpoint} done={() => setEditing(false)} />;
  }
  return (
    <li className="breakpoint" data-watchpoint={watchpoint.id}>
      <div className="row">
        <span className="dot watch" aria-hidden="true" />
        <span
          className="watch-target"
          title={`${watchpoint.bytes} bytes at ${watchpoint.address}${watchpoint.scope ? `; ${watchpoint.scope}` : ""}`}
        >
          {label}
        </span>
        <span className="dim">{ACCESS[watchpoint.access]}</span>
        <span className="end">
          {watchpoint.hits} {watchpoint.hits === 1 ? "hit" : "hits"}
        </span>
        {control && (
          <>
            <button
              type="button"
              className="icon"
              title="Edit its condition"
              aria-label={`Edit watchpoint ${watchpoint.id}`}
              onClick={() => setEditing(true)}
            >
              ✎
            </button>
            <button
              type="button"
              className="icon"
              title="Remove it"
              aria-label={`Remove watchpoint ${watchpoint.id}`}
              onClick={() =>
                connection
                  .request("removeWatchpoint", { id: watchpoint.id })
                  .catch((failure: Error) => setError(failure.message))
              }
            >
              ×
            </button>
          </>
        )}
      </div>
      {detail && <div className="row sub">{detail}</div>}
      {error && <div className="row sub error">{error}</div>}
    </li>
  );
}

/** Edits a watchpoint's condition and hit condition in place. */
function WatchEditor({ watchpoint, done }: { watchpoint: Watchpoint; done(): void }) {
  const connection = useConnection();
  const [condition, setCondition] = useState(watchpoint.condition ?? "");
  const [hits, setHits] = useState(watchpoint.hitCondition ?? "");
  const [error, setError] = useState<string | null>(null);
  return (
    <li className="editor">
      <form
        className="editor-form"
        onSubmit={(event) => {
          event.preventDefault();
          setError(null);
          connection
            .request("editWatchpoint", {
              id: watchpoint.id,
              condition: condition.trim() || null,
              hitCondition: hits.trim() || null,
            })
            .then(done)
            .catch((failure: Error) => setError(failure.message));
        }}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            done();
          }
        }}
      >
        <div className="editor-title">{watchpoint.expression ?? watchpoint.address}</div>
        <label>
          <span>Stop if</span>
          <input
            className="input"
            value={condition}
            placeholder="an expression, such as n > 3"
            // biome-ignore lint/a11y/noAutofocus: the editor opens because the person asked to type
            autoFocus
            onChange={(event) => setCondition(event.target.value)}
          />
        </label>
        <label>
          <span>Hits</span>
          <input
            className="input"
            value={hits}
            placeholder=">=5, ==3, or %10"
            onChange={(event) => setHits(event.target.value)}
          />
        </label>
        {error && <div className="error">{error}</div>}
        <div className="actions">
          <button type="submit" className="button primary small">
            Save
          </button>
          <button type="button" className="button small" onClick={done}>
            Cancel
          </button>
        </div>
      </form>
    </li>
  );
}

/**
 * Adds a watchpoint on an expression, read in the frame shown, or on bytes
 * at an address.
 */
function WatchAdder() {
  const { at, stale } = useFocus();
  const connection = useConnection();
  const [text, setText] = useState("");
  const [access, setAccess] = useState<WatchAccess>("change");
  const [error, setError] = useState<string | null>(null);
  return (
    <form
      className="adder watch-adder"
      onSubmit={(event) => {
        event.preventDefault();
        const target = text.trim();
        if (!target) {
          return;
        }
        setError(null);
        const frame = at && !stale ? at : {};
        connection
          .request("addWatchpoint", { ...frame, target, access })
          .then(() => setText(""))
          .catch((failure: Error) => setError(failure.message));
      }}
    >
      <input
        className="input quiet"
        aria-label="Add a watchpoint"
        placeholder="+ watch an expression, or 0xaddress:bytes"
        value={text}
        onChange={(event) => setText(event.target.value)}
      />
      <select
        className="input quiet"
        aria-label="Stop on"
        value={access}
        onChange={(event) => setAccess(event.target.value as WatchAccess)}
      >
        {(Object.keys(ACCESS) as WatchAccess[]).map((key) => (
          <option key={key} value={key}>
            {ACCESS[key]}
          </option>
        ))}
      </select>
      {error && <div className="error">{error}</div>}
    </form>
  );
}
