import {
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
  useCallback,
  useMemo,
  useState,
} from "react";

import { assert } from "../assert";
import { firstBreakpointPlace, knownBreakpointPlaces, PENDING } from "../breakpoints";
import { useRequest } from "../data";
import { controls } from "../model";
import type { Breakpoint, ExceptionFilter, WatchAccess, Watchpoint } from "../protocol";
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
  const breakpoints = state.breakpoints;

  return (
    <div className="pane-body">
      <ul className="rows" data-testid="breakpoints">
        {breakpoints.map((breakpoint) =>
          editing === breakpoint.id ? (
            <BreakpointEditor key={breakpoint.id} breakpoint={breakpoint} close={stopEditing} />
          ) : (
            <Row
              key={breakpoint.id}
              breakpoint={breakpoint}
              control={control}
              edit={editBreakpoint}
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
  );
}

const editBreakpoint = (id: number | null) => tab.setState({ editing: id });
const stopEditing = () => editBreakpoint(null);

export function BreakpointsBadge() {
  const count = useModel(
    (model) => (model.state?.breakpoints.length ?? 0) + (model.state?.watchpoints.length ?? 0),
  );
  return count > 0 ? <span className="count">{count}</span> : null;
}

/** Short source location, falling back to the requested location. */
export function placeLabel(breakpoint: Breakpoint): string {
  const place = firstBreakpointPlace(breakpoint);
  if (place?.path && place.line) {
    const count = knownBreakpointPlaces(breakpoint).length;
    assert(count >= 1, `breakpoint ${breakpoint.id} has a first place but ${count} known`);
    const more = count > 1 ? ` +${count - 1}` : "";
    return `${fileName(place.path)}:${place.line}${more}`;
  }
  const asked = /^(.*\/)?([^/]+:\d+)$/u.exec(breakpoint.location);
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
  edit(id: number): void;
}) {
  const connection = useConnection();
  const showBreakpoint = useShowBreakpoint();
  const [error, setError] = useState<string | null>(null);
  const show = useCallback(() => showBreakpoint(breakpoint), [showBreakpoint, breakpoint]);
  const editThis = useCallback(() => edit(breakpoint.id), [edit, breakpoint.id]);
  const remove = useCallback(
    () =>
      connection
        .request("removeBreakpoint", { id: breakpoint.id })
        .catch((failure: Error) => setError(failure.message)),
    [connection, breakpoint.id],
  );
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
          onClick={show}
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
              onClick={editThis}
            >
              ✎
            </button>
            <button
              type="button"
              className="icon"
              title="Remove it"
              aria-label={`Remove breakpoint ${breakpoint.id}`}
              onClick={remove}
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

function BreakpointEditor({ breakpoint, close }: { breakpoint: Breakpoint; close(): void }) {
  const connection = useConnection();
  const save = useCallback(
    (conditions: Conditions) =>
      connection.request("editBreakpoint", {
        id: breakpoint.id,
        condition: conditions.condition,
        hitCondition: conditions.hitCondition,
        logMessage: conditions.logMessage ?? null,
      }),
    [connection, breakpoint.id],
  );
  return (
    <ConditionEditor
      title={placeLabel(breakpoint)}
      initial={breakpoint}
      close={close}
      save={save}
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
  close,
}: {
  title: string;
  initial: Conditions;
  save(conditions: Conditions): Promise<unknown>;
  close(): void;
}) {
  const logs = initial.logMessage !== undefined;
  const [condition, setCondition] = useState(initial.condition ?? "");
  const [hits, setHits] = useState(initial.hitCondition ?? "");
  const [message, setMessage] = useState(initial.logMessage ?? "");
  const [error, setError] = useState<string | null>(null);

  const submit = useCallback(
    (event: FormEvent) => {
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
        .then(close)
        .catch((failure: Error) => setError(failure.message));
    },
    [condition, hits, logs, message, save, close],
  );
  const escape = useCallback(
    (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        close();
      }
    },
    [close],
  );
  const editCondition = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setCondition(event.target.value),
    [],
  );
  const editHits = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setHits(event.target.value),
    [],
  );
  const editMessage = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setMessage(event.target.value),
    [],
  );

  return (
    <li className="editor">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- Escape anywhere in the form closes it */}
      <form className="editor-form" onSubmit={submit} onKeyDown={escape}>
        <div className="editor-title">{title}</div>
        <label>
          <span>Stop if</span>
          <input
            className="input"
            value={condition}
            placeholder="an expression, such as n > 3"
            // oxlint-disable-next-line jsx-a11y/no-autofocus -- the editor opens because the person asked to type
            autoFocus
            onChange={editCondition}
          />
        </label>
        <label>
          <span>Hits</span>
          <input
            className="input"
            value={hits}
            placeholder=">=5, ==3, or %10"
            onChange={editHits}
          />
        </label>
        {logs && (
          <label>
            <span>Log</span>
            <input
              className="input"
              value={message}
              placeholder="a message with {expressions}, instead of stopping"
              onChange={editMessage}
            />
          </label>
        )}
        {error && <div className="error">{error}</div>}
        <div className="actions">
          <button type="submit" className="button primary small">
            Save
          </button>
          <button type="button" className="button small" onClick={close}>
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
  const add = useCallback(
    (event: FormEvent) => {
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
    },
    [text, connection],
  );
  const edit = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setText(event.target.value),
    [],
  );
  return (
    <form className="adder" onSubmit={add}>
      <input
        className="input quiet"
        aria-label="Add a breakpoint"
        placeholder="+ breakpoint at function, file:line, or 0xaddress"
        value={text}
        onChange={edit}
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
  const initial = useMemo(
    () => ({ condition: watchpoint.condition, hitCondition: watchpoint.hitCondition }),
    [watchpoint.condition, watchpoint.hitCondition],
  );
  const open = useCallback(() => setEditing(true), []);
  const close = useCallback(() => setEditing(false), []);
  const save = useCallback(
    (conditions: Conditions) =>
      connection.request("editWatchpoint", {
        id: watchpoint.id,
        condition: conditions.condition,
        hitCondition: conditions.hitCondition,
      }),
    [connection, watchpoint.id],
  );
  const remove = useCallback(
    () =>
      connection
        .request("removeWatchpoint", { id: watchpoint.id })
        .catch((failure: Error) => setError(failure.message)),
    [connection, watchpoint.id],
  );
  if (editing) {
    return (
      <ConditionEditor
        title={watchpoint.expression ?? watchpoint.address}
        initial={initial}
        close={close}
        save={save}
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
              onClick={open}
            >
              ✎
            </button>
            <button
              type="button"
              className="icon"
              title="Remove it"
              aria-label={`Remove watchpoint ${watchpoint.id}`}
              onClick={remove}
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
  const add = useCallback(
    (event: FormEvent) => {
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
    },
    [text, at, stale, connection, access],
  );
  const edit = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setText(event.target.value),
    [],
  );
  const choose = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setAccess(event.target.value as WatchAccess),
    [],
  );
  return (
    <form className="adder watch-adder" onSubmit={add}>
      <input
        className="input quiet"
        aria-label="Add a watchpoint"
        placeholder="+ watch an expression, or 0xaddress:bytes"
        value={text}
        onChange={edit}
      />
      <select className="input quiet" aria-label="Stop on" value={access} onChange={choose}>
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
          <FilterRow key={filter.id} filter={filter} control={control} fail={setError} />
        ))}
      </ul>
    </details>
  );
}

function FilterRow({
  filter,
  control,
  fail,
}: {
  filter: ExceptionFilter;
  control: boolean;
  fail: (error: string | null) => void;
}) {
  const connection = useConnection();
  const toggle = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => {
      fail(null);
      connection
        .request("setException", { id: filter.id, on: event.target.checked })
        .catch((failure: Error) => fail(failure.message));
    },
    [fail, connection, filter.id],
  );
  return (
    <li className="row" title={filter.description}>
      <label>
        <input type="checkbox" checked={filter.on} disabled={!control} onChange={toggle} />{" "}
        {filter.label}
      </label>
    </li>
  );
}
