// Values as a tree. Rows are named by path, so what is open survives the
// next stop: the link lists open paths, and the same paths open again in
// the new stop's rows. A value that changed since the stop before is marked,
// with what it was.

import {
  createContext,
  type FormEvent,
  type KeyboardEvent,
  type ReactNode,
  useCallback,
  useContext,
  useMemo,
  useState,
} from "react";

import { assert } from "../assert";
import { useRequest } from "../data";
import { controls } from "../model";
import type { Children, Row } from "../protocol";
import { useConnection, useModel } from "../store";
import { flash } from "../tab";
import { childKey, recall, toggled } from "../tree";
import { DrawAs, pin } from "./Drawings";
import { useLook } from "./navigation";
import { useFocus } from "./Workspace";

/** How many children one page shows. */
const PAGE = 100;

/** Which rows are open, and whether changes are marked. */
interface Expansion {
  isOpen(key: string): boolean;
  toggle(key: string): void;
  /** Whether to compare values with the stop before. */
  remember: boolean;
}

const ExpansionContext = createContext<Expansion | null>(null);

/** The rows the link opens: Variables and Watch. */
export function LinkedExpansion({ children }: { children: ReactNode }) {
  const { look } = useFocus();
  const change = useLook();
  const open = look.x;
  const expansion = useMemo(
    (): Expansion => ({
      isOpen: (key) => open?.includes(key) ?? false,
      toggle: (key) => change((current) => withOpen(current, toggled(current.x, key))),
      remember: true,
    }),
    [open, change],
  );
  return <ExpansionContext value={expansion}>{children}</ExpansionContext>;
}

/**
 * Rows still showing an earlier answer while this stop's arrives: they are
 * not this stop's values, so nothing compares with them.
 */
export function Earlier({ children, when }: { children: ReactNode; when: boolean }) {
  const expansion = useExpansion();
  const forgetting = useMemo(() => ({ ...expansion, remember: false }), [expansion]);
  if (!when) {
    return children;
  }
  return <ExpansionContext value={forgetting}>{children}</ExpansionContext>;
}

function withOpen<T extends { x?: string[] }>(look: T, x: string[] | undefined): T {
  const { x: _x, ...rest } = look;
  return (x ? { ...rest, x } : rest) as T;
}

/** Rows opened here alone, such as the console's. */
export function LocalExpansion({ children }: { children: ReactNode }) {
  const [open, setOpen] = useState<string[] | undefined>();
  const expansion = useMemo(
    (): Expansion => ({
      isOpen: (key) => open?.includes(key) ?? false,
      toggle: (key) => setOpen((current) => toggled(current, key)),
      remember: false,
    }),
    [open],
  );
  return <ExpansionContext value={expansion}>{children}</ExpansionContext>;
}

function useExpansion(): Expansion {
  const expansion = useContext(ExpansionContext);
  if (!expansion) {
    throw new Error("value rows need an expansion");
  }
  return expansion;
}

/** A list of rows under the path `parent`. */
export function RowList({
  rows,
  parent,
  depth,
}: {
  rows: readonly Row[];
  parent: string;
  depth: number;
}) {
  // Names can repeat, as shadowed variables do; each repeat is its own row.
  const seen = new Map<string, number>();
  return (
    <>
      {rows.map((row) => {
        const occurrence = (seen.get(row.name) ?? 0) + 1;
        seen.set(row.name, occurrence);
        return (
          <ValueRow
            key={`${row.name}~${occurrence}`}
            row={row}
            parent={parent}
            depth={depth}
            occurrence={occurrence}
          />
        );
      })}
    </>
  );
}

/** Why a row's text is no value: it failed to be read, in red, or it was
 * not read or not saved, dimmed. */
export type Missing = "failed" | "absent";

interface ValueRowProps {
  row: Row;
  parent: string;
  depth: number;
  /** Controls at the row's end, such as removing a watch. */
  actions?: ReactNode;
  /** The row says why there is no value. */
  missing?: Missing | undefined;
  /** How many rows with this name come before it, plus one. */
  occurrence?: number;
}

/** One value, which opens to its children. */
export function ValueRow({ row, parent, depth, actions, missing, occurrence = 1 }: ValueRowProps) {
  const { at, stale } = useFocus();
  const expansion = useExpansion();
  assert(occurrence >= 1, `value row occurrence ${occurrence}`);
  assert(depth >= 0, `value row at depth ${depth}`);
  const key = childKey(parent, row.name, occurrence);
  assert(
    key.startsWith(parent) && key.charAt(parent.length) === "/",
    "row path outside its parent",
  );
  const open = row.children !== null && expansion.isOpen(key);
  let was: string | undefined;
  if (expansion.remember && at && !stale && !row.truncated && !missing) {
    // Rendering notes what this stop shows, for the stop after it.
    was = recall.before(at.stop, key);
    recall.note(at.stop, key, row.text);
  }
  const changed = was !== undefined && was !== row.text;
  const indent = useIndent(depth);
  const toggle = useCallback(() => expansion.toggle(key), [expansion, key]);

  return (
    <li className="value">
      <div className={`value-row ${row.truncated ? "truncated" : ""}`} style={indent}>
        {row.children ? (
          <button
            type="button"
            className="twist"
            aria-label={`${open ? "Close" : "Open"} ${row.name}`}
            aria-expanded={open}
            onClick={toggle}
          >
            {open ? "▾" : "▸"}
          </button>
        ) : (
          <span className="twist" />
        )}
        <span className="value-name" title={row.path ?? row.name}>
          {row.name}
        </span>
        <ValueText row={row} missing={missing} changed={changed} was={was} />
        {row.type && <span className="value-type">{row.type}</span>}
        <RowActions row={row} />
        {actions}
      </div>
      {open && row.children && (
        <ul className="value-list">
          <ChildRows of={row.children} parent={key} depth={depth + 1} />
        </ul>
      )}
    </li>
  );
}

/**
 * What a value's memory offers: showing it, and, for a value stored there,
 * stopping when it changes.
 */
function RowActions({ row }: { row: Row }) {
  const { at, stale } = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const look = useLook();
  const path = row.path;
  const pinDrawing = useCallback(() => {
    assert(path !== null, "drawing a row with no path");
    look(pin({ path, renderer: null }), { replace: false });
  }, [look, path]);
  const showMemory = useCallback(() => {
    const memory = row.memoryBytes ? `${row.memory}:${row.memoryBytes}` : row.memory;
    assert(memory !== null, "showing the memory of a row with none");
    look((current) => ({ ...current, mem: memory, view: "memory" }), { replace: false });
  }, [look, row.memory, row.memoryBytes]);
  const watch = useCallback(() => {
    assert(row.path !== null, "watching a row with no path");
    void connection
      .request("addWatchpoint", { ...at, target: row.path, access: "change" })
      .then(() => flash(`Stopping when ${row.path} changes`))
      .catch((failure: Error) => flash(failure.message));
  }, [connection, at, row.path]);
  const drawable = path !== null && at !== null ? path : null;
  const draw = (drawable !== null || row.memory !== null) && (
    <>
      {drawable !== null && row.drawings.length > 0 && (
        <button
          type="button"
          className="link-button"
          aria-label={`Draw ${row.name}`}
          title={`Draw with ${row.drawings.join(", ")}`}
          onClick={pinDrawing}
        >
          Draw
        </button>
      )}
      <DrawAs name={row.name} path={drawable} memory={row.memory ? showMemory : null} />
    </>
  );
  if (!row.memory) {
    return draw ? <span className="row-actions">{draw}</span> : null;
  }
  const watchable = control && at && !stale && row.path !== null && row.stored;
  return (
    <span className="row-actions">
      {watchable && (
        <button
          type="button"
          className="icon"
          aria-label={`Watch ${row.name} for changes`}
          title="Stop when this value changes"
          onClick={watch}
        >
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path
              d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8s-2.4 4.5-6.5 4.5S1.5 8 1.5 8Z"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.3"
              strokeLinejoin="round"
            />
            <circle cx="8" cy="8" r="2" fill="currentColor" />
          </svg>
        </button>
      )}
      {draw}
    </span>
  );
}

/** A value's text, which a double click edits when the value can change. */
function ValueText({
  row,
  missing,
  changed,
  was,
}: {
  row: Row;
  missing: Missing | undefined;
  changed: boolean;
  was: string | undefined;
}) {
  const { at, stale } = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const editable = row.editable && row.path !== null && control && at !== null && !stale;
  const path = row.path;
  const submit = useCallback(
    (event: FormEvent<HTMLFormElement>) => {
      event.preventDefault();
      const value = new FormData(event.currentTarget).get("value");
      if (typeof value !== "string" || value.trim() === "") {
        return;
      }
      assert(at !== null && path !== null, "editing a value with no stop or path");
      setError(null);
      connection
        .request("setValue", { ...at, path, value })
        .then(() => setEditing(false))
        .catch((failure: Error) => setError(failure.message));
    },
    [at, path, connection],
  );
  const escape = useCallback((event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.stopPropagation();
      setEditing(false);
    }
  }, []);
  const blur = useCallback(() => !error && setEditing(false), [error]);
  const edit = useCallback(() => setEditing(true), []);
  const editByKey = useCallback((event: KeyboardEvent) => {
    if (event.key === "Enter" || event.key === "F2") {
      event.preventDefault();
      setEditing(true);
    }
  }, []);

  if (editing && at && row.path) {
    return (
      <form className="value-edit" onSubmit={submit}>
        <input
          className="input"
          name="value"
          aria-label={`New value of ${row.name}`}
          defaultValue={row.text}
          // oxlint-disable-next-line jsx-a11y/no-autofocus -- the person asked to edit this value
          autoFocus
          onKeyDown={escape}
          onBlur={blur}
        />
        {error && <span className="error">{error}</span>}
      </form>
    );
  }
  const className = `value-text ${missing === "failed" ? "error" : (missing ?? "")} ${changed ? "changed" : ""}`;
  if (!editable) {
    return (
      <span className={className} title={changed ? `was ${was}` : (row.detail ?? row.text)}>
        {row.text}
        {changed && <span className="was"> was {was}</span>}
      </span>
    );
  }
  return (
    <button
      type="button"
      className={`${className} editable`}
      title={`${row.detail ? `${row.detail}\n\n` : ""}${changed ? `was ${was}; ` : ""}double-click to change`}
      onDoubleClick={edit}
      onKeyDown={editByKey}
    >
      {row.text}
      {changed && <span className="was"> was {was}</span>}
    </button>
  );
}

/** A value's children, a page at a time. */
function ChildRows({ of, parent, depth }: { of: Children; parent: string; depth: number }) {
  const [pages, setPages] = useState(1);
  const known = of.indexed !== null || of.named !== null;
  const total = (of.indexed ?? 0) + (of.named ?? 0);
  const shown = Math.min(pages * PAGE, known ? total : PAGE);
  const starts = Array.from({ length: Math.ceil(shown / PAGE) }, (_, index) => index * PAGE);
  const more = useMemo(() => ({ marginLeft: 6 + depth * 14 }), [depth]);
  const showMore = useCallback(() => setPages(pages + 1), [pages]);
  return (
    <>
      {starts.map((start) => (
        <ChildPage key={start} handle={of.handle} start={start} parent={parent} depth={depth} />
      ))}
      {known && shown < total && (
        <li className="value">
          <button type="button" className="more" style={more} onClick={showMore}>
            Show {Math.min(PAGE, total - shown)} more of {total.toLocaleString()}
          </button>
        </li>
      )}
    </>
  );
}

function ChildPage({
  handle,
  start,
  parent,
  depth,
}: {
  handle: number;
  start: number;
  parent: string;
  depth: number;
}) {
  const { data, error, current } = useRequest("children", { handle, start, count: PAGE });
  const indent = useIndent(depth);
  if (error) {
    return (
      <li className="value">
        <div className="value-row" style={indent}>
          <span className="value-text error">{error.message}</span>
        </div>
      </li>
    );
  }
  if (!data) {
    return (
      <li className="value">
        <div className="value-row dim" style={indent}>
          …
        </div>
      </li>
    );
  }
  return (
    <Earlier when={!current}>
      <RowList rows={data.rows} parent={parent} depth={depth} />
    </Earlier>
  );
}

function useIndent(depth: number) {
  return useMemo(() => ({ paddingLeft: 6 + depth * 14 }), [depth]);
}

/** A row that only says something, such as why a watch failed. */
export function messageRow(name: string, text: string): Row {
  return {
    name,
    text,
    detail: null,
    type: null,
    path: null,
    children: null,
    editable: false,
    memory: null,
    memoryBytes: null,
    stored: false,
    truncated: false,
    drawings: [],
  };
}
