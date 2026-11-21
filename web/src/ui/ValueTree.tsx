import type { JSX } from "@solidjs/web";
import {
  createContext,
  createMemo,
  createSignal,
  For,
  Match,
  Show,
  Switch,
  untrack,
  useContext,
} from "solid-js";

import { assert } from "../assert";
import { useRequest } from "../data";
import { rowTarget } from "../memory";
import { controls } from "../model";
import type { Children, Row } from "../protocol";
import { useConnection, useModel } from "../store";
import { flash } from "../tab";
import { childKey, recall, toggled } from "../tree";
import { DrawAs, pin } from "./Drawings";
import { useLook } from "./navigation";
import { useFocus } from "./Workspace";

const PAGE = 100;

interface Expansion {
  isOpen(key: string): boolean;
  toggle(key: string): void;
  readonly remember: boolean;
}

const ExpansionContext = createContext<Expansion | null>(null);

export function LinkedExpansion(props: { children: JSX.Element }) {
  const focus = useFocus();
  const change = useLook();
  const expansion: Expansion = {
    isOpen: (key) => focus().look.x?.includes(key) ?? false,
    toggle: (key) => change((current) => withOpen(current, toggled(current.x, key))),
    remember: true,
  };
  return <ExpansionContext value={expansion}>{props.children}</ExpansionContext>;
}

export function Earlier(props: { children: JSX.Element; when: boolean }) {
  const parent = useExpansion();
  const expansion: Expansion = {
    isOpen: (key) => parent.isOpen(key),
    toggle: (key) => parent.toggle(key),
    get remember() {
      return !props.when && parent.remember;
    },
  };
  return <ExpansionContext value={expansion}>{props.children}</ExpansionContext>;
}

function withOpen<T extends { x?: string[] }>(look: T, x: string[] | undefined): T {
  const { x: _x, ...rest } = look;
  return (x ? { ...rest, x } : rest) as T;
}

export function LocalExpansion(props: { children: JSX.Element }) {
  const [open, setOpen] = createSignal<string[] | undefined>();
  const expansion: Expansion = {
    isOpen: (key) => open()?.includes(key) ?? false,
    toggle: (key) => setOpen((current) => toggled(current, key)),
    remember: false,
  };
  return <ExpansionContext value={expansion}>{props.children}</ExpansionContext>;
}

function useExpansion(): Expansion {
  const expansion = useContext(ExpansionContext);
  if (!expansion) {
    throw new Error("value rows need an expansion");
  }
  return expansion;
}

export function RowList(props: { rows: readonly Row[]; parent: string; depth: number }) {
  const items = createMemo(() => {
    const seen = new Map<string, number>();
    return props.rows.map((row) => {
      const occurrence = (seen.get(row.name) ?? 0) + 1;
      seen.set(row.name, occurrence);
      return { key: `${row.name}~${occurrence}`, row, occurrence };
    });
  });
  return (
    <For each={items()} keyed={(item) => item.key}>
      {(item) => (
        <ValueRow
          row={item().row}
          parent={props.parent}
          depth={props.depth}
          occurrence={item().occurrence}
        />
      )}
    </For>
  );
}

export type Missing = "failed" | "absent";

interface ValueRowProps {
  row: Row;
  parent: string;
  depth: number;

  actions?: JSX.Element;

  missing?: Missing | undefined;

  occurrence?: number;
}

export function ValueRow(props: ValueRowProps) {
  const focus = useFocus();
  const expansion = useExpansion();
  const key = createMemo(() => {
    const occurrence = props.occurrence ?? 1;
    assert(occurrence >= 1, `value row occurrence ${occurrence}`);
    assert(props.depth >= 0, `value row at depth ${props.depth}`);
    const result = childKey(props.parent, props.row.name, occurrence);
    assert(
      result.startsWith(props.parent) && result.charAt(props.parent.length) === "/",
      "row path outside its parent",
    );
    return result;
  });
  const open = createMemo(() => props.row.children !== null && expansion.isOpen(key()));
  const history = createMemo(() => {
    const { at, stale } = focus();
    let was: string | undefined;
    if (expansion.remember && at && !stale && !props.row.truncated && !props.missing) {
      const currentKey = key();
      was = recall.before(at.stop, currentKey);
      recall.note(at.stop, currentKey, props.row.text);
    }
    return { was, changed: was !== undefined && was !== props.row.text };
  });
  const indent = useIndent(() => props.depth);
  const toggle = () => expansion.toggle(key());

  return (
    <li class="value">
      <div class={["value-row", { truncated: props.row.truncated }]} style={indent()}>
        <Show when={props.row.children} fallback={<span class="twist" />}>
          <button
            type="button"
            class="twist"
            aria-label={`${open() ? "Close" : "Open"} ${props.row.name}`}
            aria-expanded={open() ? "true" : "false"}
            onClick={toggle}
          >
            {open() ? "▾" : "▸"}
          </button>
        </Show>
        <span class="value-name" title={props.row.path ?? props.row.name}>
          {props.row.name}
        </span>
        <ValueText
          row={props.row}
          missing={props.missing}
          changed={history().changed}
          was={history().was}
        />
        <Show when={props.row.type}>{(type) => <span class="value-type">{type()}</span>}</Show>
        <RowActions row={props.row} />
        {props.actions}
      </div>
      <Show when={open() ? props.row.children : null}>
        {(children) => (
          <ul class="value-list">
            <ChildRows of={children()} parent={key()} depth={props.depth + 1} />
          </ul>
        )}
      </Show>
    </li>
  );
}

function RowActions(props: { row: Row }) {
  const focus = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const look = useLook();
  const pinDrawing = () => {
    const path = props.row.path;
    assert(path !== null, "drawing a row with no path");
    look(pin({ path, renderer: null }), { replace: false });
  };
  const showMemory = () => {
    const memory = rowTarget(props.row);
    assert(memory !== null, "showing the memory of a row with none");
    look((current) => ({ ...current, mem: memory, view: "memory" }), { replace: false });
  };

  const watching = useModel(
    (model) =>
      model.state?.watchpoints.find(
        (watchpoint) =>
          props.row.memory !== null &&
          BigInt(watchpoint.address) === BigInt(props.row.memory) &&
          watchpoint.bytes === props.row.memoryBytes,
      )?.id ?? null,
  );
  const watch = () => {
    const path = props.row.path;
    assert(path !== null, "watching a row with no path");
    const currentWatching = watching();
    const done =
      currentWatching === null
        ? connection
            .request("addWatchpoint", { ...focus().at, target: path, access: "change" })
            .then(() => flash(`Stopping when ${path} changes`))
        : connection
            .request("removeWatchpoint", { id: currentWatching })
            .then(() => flash(`No longer watching ${path}`));
    done.catch((failure: Error) => flash(failure.message));
  };
  const drawable = createMemo(() =>
    props.row.path !== null && focus().at !== null ? props.row.path : null,
  );
  const hasDraw = createMemo(() => drawable() !== null || props.row.memory !== null);
  const watchable = createMemo(() => {
    const { at, stale } = focus();
    return Boolean(control() && at && !stale && props.row.path !== null && props.row.stored);
  });

  return (
    <Show when={Boolean(props.row.memory) || hasDraw()}>
      <span class="row-actions">
        <Show when={Boolean(props.row.memory) && watchable()}>
          <button
            type="button"
            class="icon"
            aria-label={`Watch ${props.row.name} for changes`}
            aria-pressed={watching() === null ? "false" : "true"}
            title={
              watching() === null ? "Stop when this value changes" : "Stop watching this value"
            }
            onClick={watch}
          >
            <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
              <path
                d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8s-2.4 4.5-6.5 4.5S1.5 8 1.5 8Z"
                fill="none"
                stroke="currentColor"
                stroke-width="1.3"
                stroke-linejoin="round"
              />
              <circle cx="8" cy="8" r="2" fill="currentColor" />
            </svg>
          </button>
        </Show>
        <Show when={hasDraw()}>
          <Show when={drawable() !== null && props.row.drawings.length > 0}>
            <button
              type="button"
              class="link-button"
              aria-label={`Draw ${props.row.name}`}
              title={`Draw with ${props.row.drawings.join(", ")}`}
              onClick={pinDrawing}
            >
              Draw
            </button>
          </Show>
          <DrawAs
            name={props.row.name}
            path={drawable()}
            memory={props.row.memory ? showMemory : null}
          />
        </Show>
      </span>
    </Show>
  );
}

function ValueText(props: {
  row: Row;
  missing: Missing | undefined;
  changed: boolean;
  was: string | undefined;
}) {
  const focus = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const [editing, setEditing] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const editable = createMemo(() => {
    const { at, stale } = focus();
    return props.row.editable && props.row.path !== null && control() && at !== null && !stale;
  });
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    const value = new FormData(event.currentTarget as HTMLFormElement).get("value");
    if (typeof value !== "string" || value.trim() === "") {
      return;
    }
    const currentAt = focus().at;
    const path = props.row.path;
    assert(currentAt !== null && path !== null, "editing a value with no stop or path");
    setError(null);
    connection
      .request("setValue", { ...currentAt, path, value })
      .then(() => setEditing(false))
      .catch((failure: Error) => setError(failure.message));
  };
  const escape = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.stopPropagation();
      setEditing(false);
    }
  };
  const blur = () => {
    if (!error()) {
      setEditing(false);
    }
  };
  const edit = () => setEditing(true);
  const editByKey = (event: KeyboardEvent) => {
    if (event.key === "Enter" || event.key === "F2") {
      event.preventDefault();
      setEditing(true);
    }
  };
  const textClass = createMemo(
    () =>
      `value-text ${props.missing === "failed" ? "error" : (props.missing ?? "")} ${props.changed ? "changed" : ""}`,
  );

  return (
    <Switch
      fallback={
        <button
          type="button"
          class={`${textClass()} editable`}
          title={`${props.row.detail ? `${props.row.detail}\n\n` : ""}${props.changed ? `was ${props.was}; ` : ""}double-click to change`}
          onDblClick={edit}
          onKeyDown={editByKey}
        >
          {props.row.text}
          <Show when={props.changed}>
            <span class="was"> was {props.was}</span>
          </Show>
        </button>
      }
    >
      <Match when={editing() && focus().at !== null && props.row.path !== null}>
        <form class="value-edit" onSubmit={submit}>
          <input
            class="input"
            name="value"
            aria-label={`New value of ${props.row.name}`}
            value={untrack(() => props.row.text)}
            autofocus
            ref={(node) => {
              queueMicrotask(() => node.focus());
            }}
            onKeyDown={escape}
            onBlur={blur}
          />
          <Show when={error()}>{(message) => <span class="error">{message()}</span>}</Show>
        </form>
      </Match>
      <Match when={!editable()}>
        <span
          class={textClass()}
          title={props.changed ? `was ${props.was}` : (props.row.detail ?? props.row.text)}
        >
          {props.row.text}
          <Show when={props.changed}>
            <span class="was"> was {props.was}</span>
          </Show>
        </span>
      </Match>
    </Switch>
  );
}

function ChildRows(props: { of: Children; parent: string; depth: number }) {
  const [pages, setPages] = createSignal(1);
  const known = createMemo(() => props.of.indexed !== null || props.of.named !== null);
  const total = createMemo(() => (props.of.indexed ?? 0) + (props.of.named ?? 0));
  const shown = createMemo(() => Math.min(pages() * PAGE, known() ? total() : PAGE));
  const starts = createMemo(() =>
    Array.from({ length: Math.ceil(shown() / PAGE) }, (_, index) => index * PAGE),
  );
  const more = createMemo(() => ({ "margin-left": `${6 + props.depth * 14}px` }));
  const showMore = () => setPages((current) => current + 1);
  return (
    <>
      <For each={starts()}>
        {(start) => (
          <ChildPage
            handle={props.of.handle}
            start={start}
            parent={props.parent}
            depth={props.depth}
          />
        )}
      </For>
      <Show when={known() && shown() < total()}>
        <li class="value">
          <button type="button" class="more" style={more()} onClick={showMore}>
            Show {Math.min(PAGE, total() - shown())} more of {total().toLocaleString()}
          </button>
        </li>
      </Show>
    </>
  );
}

function ChildPage(props: { handle: number; start: number; parent: string; depth: number }) {
  const result = useRequest("children", () => ({
    handle: props.handle,
    start: props.start,
    count: PAGE,
  }));
  const indent = useIndent(() => props.depth);
  return (
    <Switch
      fallback={
        <li class="value">
          <div class="value-row dim" style={indent()}>
            …
          </div>
        </li>
      }
    >
      <Match when={result().error}>
        {(err) => (
          <li class="value">
            <div class="value-row" style={indent()}>
              <span class="value-text error">{err().message}</span>
            </div>
          </li>
        )}
      </Match>
      <Match when={result().data}>
        {(data) => (
          <Earlier when={!result().current}>
            <RowList rows={data().rows} parent={props.parent} depth={props.depth} />
          </Earlier>
        )}
      </Match>
    </Switch>
  );
}

function useIndent(depth: () => number) {
  return createMemo(() => ({ "padding-left": `${6 + depth() * 14}px` }));
}

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
    textBytes: null,
    stored: false,
    truncated: false,
    drawings: [],
  };
}
