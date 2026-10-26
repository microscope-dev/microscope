import { useState } from "react";
import { firstBreakpointPlace, knownBreakpointPlaces, PENDING } from "../breakpoints";
import { useRequest } from "../data";
import { controls } from "../model";
import type { Breakpoint, WatchAccess, Watchpoint } from "../protocol";
import { useConnection, useModel } from "../store";
import { tab, useTab } from "../tab";
import { useLook, useShowSource } from "./navigation";
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
              <BreakpointEditor
                key={breakpoint.id}
                breakpoint={breakpoint}
                done={() => setEditing(null)}
              />
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
        <Exceptions control={control} />
        {!control && breakpoints.length + state.watchpoints.length === 0 && (
          <div className="empty">No breakpoints.</div>
        )}
      </div>
    </section>
  );
}

/** Short source location, falling back to the requested location. */
export function placeLabel(breakpoint: Breakpoint): string {
  const place = firstBreakpointPlace(breakpoint);
  if (place?.path && place.line) {
    const count = knownBreakpointPlaces(breakpoint).length;
    const more = count > 1 ? ` +${count - 1}` : "";
    return `${fileName(place.path)}:${place.line}${more}`;
  }
  const asked = /^(.*\/)?([^/]+:\d+)$/.exec(breakpoint.location);
  return asked?.[2] ?? breakpoint.location;
}

/** A breakpoint's active locations, or why it remains pending. */
export function breakpointTitle(breakpoint: Breakpoint): string {
  const places = breakpoint.places
    .map((at) => `${at.function ?? at.address} ${at.path ? `${at.path}:${at.line}` : ""}`)
    .join("\n");
  if (breakpoint.pending) {
    return [places, breakpoint.pending.message].filter(Boolean).join("\n");
  }
  return places || `${breakpoint.location}: ${PENDING}`;
}

/**
 * Shows a breakpoint's code: its line, or its instructions where it has no
 * line. Returns why it cannot, when it cannot.
 */
export function useShowBreakpoint(): (breakpoint: Breakpoint) => string | null {
  const showSource = useShowSource();
  const look = useLook();
  return (breakpoint) => {
    const place = firstBreakpointPlace(breakpoint);
    if (place?.path && place.line) {
      showSource(place.path, place.line);
      return null;
    }
    // A pending place's address is not where the code will load.
    const placed = breakpoint.places[0];
    if (placed) {
      look((current) => ({ ...current, asm: placed.address, view: "disassembly" }), {
        replace: false,
      });
      return null;
    }
    return breakpoint.pending?.message ?? `${breakpoint.location} is ${PENDING}`;
  };
}

/** What stops a breakpoint or watchpoint, besides being reached. */
interface Conditions {
  condition: string | null;
  hitCondition: string | null;
  /** Breakpoints alone log instead of stopping. */
  logMessage?: string | null;
}

/** Conditions as a row says them, or "" when there are none. */
function conditionText(conditions: Conditions): string {
  if (conditions.logMessage) {
    return `log "${conditions.logMessage}"`;
  }
  return [
    conditions.condition && `if ${conditions.condition}`,
    conditions.hitCondition && `hits ${conditions.hitCondition}`,
  ]
    .filter(Boolean)
    .join(" · ");
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
  const showBreakpoint = useShowBreakpoint();
  const [error, setError] = useState<string | null>(null);
  const place = firstBreakpointPlace(breakpoint);
  const pending = breakpoint.places.length === 0;
  const kind = breakpoint.logMessage ? "log" : breakpoint.condition ? "conditional" : "plain";
  const detail = conditionText(breakpoint);

  return (
    <li className="breakpoint" data-breakpoint={breakpoint.id}>
      <div className="row">
        <span className={`dot ${kind} ${pending ? "pending" : ""}`} aria-hidden="true" />
        <button
          type="button"
          className="link-row"
          title={breakpointTitle(breakpoint)}
          disabled={pending && !place?.path}
          onClick={() => showBreakpoint(breakpoint)}
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
      {breakpoint.pending ? (
        <div className="row sub">{breakpoint.pending.message}</div>
      ) : (
        pending && <div className="row sub">{PENDING}</div>
      )}
      {detail && <div className="row sub">{detail}</div>}
      {error && <div className="row sub error">{error}</div>}
    </li>
  );
}

function BreakpointEditor({ breakpoint, done }: { breakpoint: Breakpoint; done(): void }) {
  const connection = useConnection();
  return (
    <ConditionEditor
      title={placeLabel(breakpoint)}
      initial={breakpoint}
      done={done}
      save={(conditions) =>
        connection.request("editBreakpoint", {
          id: breakpoint.id,
          condition: conditions.condition,
          hitCondition: conditions.hitCondition,
          logMessage: conditions.logMessage ?? null,
        })
      }
    />
  );
}

/**
 * Edits conditions in place: a condition, a hit condition, and, when
 * `initial` has the field, a log message.
 */
function ConditionEditor({
  title,
  initial,
  save,
  done,
}: {
  title: string;
  initial: Conditions;
  save(conditions: Conditions): Promise<unknown>;
  done(): void;
}) {
  const logs = initial.logMessage !== undefined;
  const [condition, setCondition] = useState(initial.condition ?? "");
  const [hits, setHits] = useState(initial.hitCondition ?? "");
  const [message, setMessage] = useState(initial.logMessage ?? "");
  const [error, setError] = useState<string | null>(null);

  return (
    <li className="editor">
      <form
        className="editor-form"
        onSubmit={(event) => {
          event.preventDefault();
          setError(null);
          const conditions: Conditions = {
            condition: condition.trim() || null,
            hitCondition: hits.trim() || null,
          };
          if (logs) {
            conditions.logMessage = message.trim() || null;
          }
          save(conditions)
            .then(done)
            .catch((failure: Error) => setError(failure.message));
        }}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            done();
          }
        }}
      >
        <div className="editor-title">{title}</div>
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
        {logs && (
          <label>
            <span>Log</span>
            <input
              className="input"
              value={message}
              placeholder="a message with {expressions}, instead of stopping"
              onChange={(event) => setMessage(event.target.value)}
            />
          </label>
        )}
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
  // The tab store holds one breakpoint's editor; a watchpoint's is its own.
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const label = watchpoint.expression ?? `${watchpoint.address}:${watchpoint.bytes}`;
  const detail = conditionText(watchpoint);
  if (editing) {
    return (
      <ConditionEditor
        title={watchpoint.expression ?? watchpoint.address}
        initial={{ condition: watchpoint.condition, hitCondition: watchpoint.hitCondition }}
        done={() => setEditing(false)}
        save={(conditions) =>
          connection.request("editWatchpoint", {
            id: watchpoint.id,
            condition: conditions.condition,
            hitCondition: conditions.hitCondition,
          })
        }
      />
    );
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

/**
 * Exception filters start collapsed because most apply to other languages.
 * The summary counts enabled filters.
 */
function Exceptions({ control }: { control: boolean }) {
  const session = useModel((model) => model.state?.session);
  const exceptions = useRequest("exceptions", session ? undefined : null);
  const connection = useConnection();
  const [error, setError] = useState<string | null>(null);
  const filters = exceptions.data?.filters;
  if (exceptions.error) {
    return <div className="error">Exceptions: {exceptions.error.message}</div>;
  }
  if (!filters) {
    return null;
  }
  const on = filters.filter((filter) => filter.on).length;
  return (
    <details className="exceptions" data-testid="exceptions">
      <summary>
        Exceptions{" "}
        <span className="count">
          {on} of {filters.length} on
        </span>
      </summary>
      {error && <div className="error">{error}</div>}
      <ul className="rows">
        {filters.map((filter) => (
          <li key={filter.id} className="row" title={filter.description}>
            <label>
              <input
                type="checkbox"
                checked={filter.on}
                disabled={!control}
                onChange={(event) => {
                  setError(null);
                  connection
                    .request("setException", { id: filter.id, on: event.target.checked })
                    .catch((failure: Error) => setError(failure.message));
                }}
              />{" "}
              {filter.label}
            </label>
          </li>
        ))}
      </ul>
    </details>
  );
}
