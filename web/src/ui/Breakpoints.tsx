import { createMemo, createSignal, For, Match, onSettled, Show, Switch, untrack } from "solid-js";

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

export function Breakpoints() {
  const focus = useFocus();
  const control = useModel(controls);
  const editing = useTab((current) => current.editing);
  const breakpoints = createMemo(() => focus().state.breakpoints);
  const watchpoints = createMemo(() => focus().state.watchpoints);

  return (
    <div class="pane-body">
      <ul class="rows" data-testid="breakpoints">
        <For each={breakpoints()} keyed={(breakpoint) => breakpoint.id}>
          {(breakpoint) => (
            <Show
              when={editing() === breakpoint().id}
              fallback={<Row breakpoint={breakpoint()} control={control()} edit={editBreakpoint} />}
            >
              <BreakpointEditor breakpoint={breakpoint()} close={stopEditing} />
            </Show>
          )}
        </For>
      </ul>
      <Show when={watchpoints().length > 0}>
        <ul class="rows" data-testid="watchpoints">
          <For each={watchpoints()} keyed={(watchpoint) => watchpoint.id}>
            {(watchpoint) => <WatchRow watchpoint={watchpoint()} control={control()} />}
          </For>
        </ul>
      </Show>
      <Show when={control()}>
        <Adder />
        <WatchAdder />
      </Show>
      <Exceptions control={control()} />
      <Show when={!control() && breakpoints().length + watchpoints().length === 0}>
        <div class="empty">No breakpoints.</div>
      </Show>
    </div>
  );
}

const editBreakpoint = (id: number | null) => tab.setState({ editing: id });
const stopEditing = () => editBreakpoint(null);

export function BreakpointsBadge() {
  const count = useModel(
    (model) => (model.state?.breakpoints.length ?? 0) + (model.state?.watchpoints.length ?? 0),
  );
  return (
    <Show when={count() > 0}>
      <span class="count">{count()}</span>
    </Show>
  );
}

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

export function breakpointTitle(breakpoint: Breakpoint): string {
  const places = breakpoint.places
    .map((at) => `${at.function ?? at.address} ${at.path ? `${at.path}:${at.line}` : ""}`)
    .join("\n");
  if (breakpoint.pending) {
    return [places, breakpoint.pending.message].filter(Boolean).join("\n");
  }
  return places || `${breakpoint.location}: ${PENDING}`;
}

export function useShowBreakpoint(): (breakpoint: Breakpoint) => string | null {
  const showSource = useShowSource();
  const look = useLook();
  return (breakpoint) => {
    const place = firstBreakpointPlace(breakpoint);
    if (place?.path && place.line) {
      showSource(place.path, place.line);
      return null;
    }

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

interface Conditions {
  condition: string | null;
  hitCondition: string | null;

  logMessage?: string | null;
}

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

function Row(props: { breakpoint: Breakpoint; control: boolean; edit(id: number): void }) {
  const connection = useConnection();
  const showBreakpoint = useShowBreakpoint();
  const [error, setError] = createSignal<string | null>(null);
  const show = () => showBreakpoint(props.breakpoint);
  const editThis = () => props.edit(props.breakpoint.id);
  const remove = () =>
    connection
      .request("removeBreakpoint", { id: props.breakpoint.id })
      .catch((failure: Error) => setError(failure.message));
  const place = createMemo(() => firstBreakpointPlace(props.breakpoint));
  const pending = createMemo(() => props.breakpoint.places.length === 0);
  const kind = createMemo(() =>
    props.breakpoint.logMessage ? "log" : props.breakpoint.condition ? "conditional" : "plain",
  );
  const detail = createMemo(() => conditionText(props.breakpoint));

  return (
    <li class="breakpoint" data-breakpoint={props.breakpoint.id}>
      <div class="row">
        <span class={["dot", kind(), { pending: pending() }]} aria-hidden="true" />
        <button
          type="button"
          class="link-row"
          title={breakpointTitle(props.breakpoint)}
          disabled={pending() && !place()?.path}
          onClick={show}
        >
          {placeLabel(props.breakpoint)}
        </button>
        <Show when={place()?.function}>{(fn) => <span class="dim fn-name">{fn()}</span>}</Show>
        <span class="end" title="Hits in this process, including those that did not stop">
          {props.breakpoint.hits} {props.breakpoint.hits === 1 ? "hit" : "hits"}
        </span>
        <Show when={props.control}>
          <button
            type="button"
            class="icon"
            title="Edit its condition"
            aria-label={`Edit breakpoint ${props.breakpoint.id}`}
            onClick={editThis}
          >
            ✎
          </button>
          <button
            type="button"
            class="icon"
            title="Remove it"
            aria-label={`Remove breakpoint ${props.breakpoint.id}`}
            onClick={remove}
          >
            ×
          </button>
        </Show>
      </div>
      <Show
        when={props.breakpoint.pending}
        fallback={
          <Show when={pending()}>
            <div class="row sub">{PENDING}</div>
          </Show>
        }
      >
        {(reason) => <div class="row sub">{reason().message}</div>}
      </Show>
      <Show when={detail()}>{(text) => <div class="row sub">{text()}</div>}</Show>
      <Show when={error()}>{(message) => <div class="row sub error">{message()}</div>}</Show>
    </li>
  );
}

function BreakpointEditor(props: { breakpoint: Breakpoint; close(): void }) {
  const connection = useConnection();
  const save = (conditions: Conditions) =>
    connection.request("editBreakpoint", {
      id: props.breakpoint.id,
      condition: conditions.condition,
      hitCondition: conditions.hitCondition,
      logMessage: conditions.logMessage ?? null,
    });
  return (
    <ConditionEditor
      title={placeLabel(props.breakpoint)}
      initial={props.breakpoint}
      close={props.close}
      save={save}
    />
  );
}

function ConditionEditor(props: {
  title: string;
  initial: Conditions;
  save(conditions: Conditions): Promise<unknown>;
  close(): void;
}) {
  const logs = createMemo(() => props.initial.logMessage !== undefined);
  const [condition, setCondition] = createSignal(untrack(() => props.initial.condition ?? ""));
  const [hits, setHits] = createSignal(untrack(() => props.initial.hitCondition ?? ""));
  const [message, setMessage] = createSignal(untrack(() => props.initial.logMessage ?? ""));
  const [error, setError] = createSignal<string | null>(null);
  let first: HTMLInputElement | undefined;

  onSettled(() => {
    first?.focus();
  });

  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    setError(null);
    const conditions: Conditions = {
      condition: condition().trim() || null,
      hitCondition: hits().trim() || null,
    };
    if (logs()) {
      conditions.logMessage = message().trim() || null;
    }
    props
      .save(conditions)
      .then(() => props.close())
      .catch((failure: Error) => setError(failure.message));
  };

  const escape = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      props.close();
    }
  };

  return (
    <li class="editor">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- Escape anywhere in the form closes it */}
      <form class="editor-form" onSubmit={submit} onKeyDown={escape}>
        <div class="editor-title">{props.title}</div>
        <label>
          <span>Stop if</span>
          <input
            ref={(node) => {
              first = node;
            }}
            class="input"
            value={condition()}
            placeholder="an expression, such as n > 3"
            autofocus
            onInput={(event) => setCondition(event.currentTarget.value)}
          />
        </label>
        <label>
          <span>Hits</span>
          <input
            class="input"
            value={hits()}
            placeholder=">=5, ==3, or %10"
            onInput={(event) => setHits(event.currentTarget.value)}
          />
        </label>
        <Show when={logs()}>
          <label>
            <span>Log</span>
            <input
              class="input"
              value={message()}
              placeholder="a message with {expressions}, instead of stopping"
              onInput={(event) => setMessage(event.currentTarget.value)}
            />
          </label>
        </Show>
        <Show when={error()}>{(err) => <div class="error">{err()}</div>}</Show>
        <div class="actions">
          <button type="submit" class="button primary small">
            Save
          </button>
          <button type="button" class="button small" onClick={() => props.close()}>
            Cancel
          </button>
        </div>
      </form>
    </li>
  );
}

function Adder() {
  const connection = useConnection();
  const [text, setText] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const add = (event: SubmitEvent) => {
    event.preventDefault();
    const location = text().trim();
    if (!location) {
      return;
    }
    setError(null);
    connection
      .request("addBreakpoint", { location })
      .then(() => setText(""))
      .catch((failure: Error) => setError(failure.message));
  };
  return (
    <form class="adder" onSubmit={add}>
      <input
        class="input quiet"
        aria-label="Add a breakpoint"
        placeholder="+ breakpoint at function, file:line, or 0xaddress"
        value={text()}
        onInput={(event) => setText(event.currentTarget.value)}
      />
      <Show when={error()}>{(err) => <div class="error">{err()}</div>}</Show>
    </form>
  );
}

const ACCESS: Record<WatchAccess, string> = {
  change: "change",
  write: "write",
  readWrite: "read or write",
  read: "read",
};

const ACCESS_KEYS = Object.keys(ACCESS) as WatchAccess[];

function WatchRow(props: { watchpoint: Watchpoint; control: boolean }) {
  const connection = useConnection();
  const [editing, setEditing] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const label = createMemo(
    () => props.watchpoint.expression ?? `${props.watchpoint.address}:${props.watchpoint.bytes}`,
  );
  const detail = createMemo(() => conditionText(props.watchpoint));
  const initial = createMemo(() => ({
    condition: props.watchpoint.condition,
    hitCondition: props.watchpoint.hitCondition,
  }));
  const open = () => setEditing(true);
  const close = () => setEditing(false);
  const save = (conditions: Conditions) =>
    connection.request("editWatchpoint", {
      id: props.watchpoint.id,
      condition: conditions.condition,
      hitCondition: conditions.hitCondition,
    });
  const remove = () =>
    connection
      .request("removeWatchpoint", { id: props.watchpoint.id })
      .catch((failure: Error) => setError(failure.message));

  return (
    <Show
      when={editing()}
      fallback={
        <li class="breakpoint" data-watchpoint={props.watchpoint.id}>
          <div class="row">
            <span class="dot watch" aria-hidden="true" />
            <span
              class="watch-target"
              title={`${props.watchpoint.bytes} bytes at ${props.watchpoint.address}${props.watchpoint.scope ? `; ${props.watchpoint.scope}` : ""}`}
            >
              {label()}
            </span>
            <span class="dim">{ACCESS[props.watchpoint.access]}</span>
            <span class="end">
              {props.watchpoint.hits} {props.watchpoint.hits === 1 ? "hit" : "hits"}
            </span>
            <Show when={props.control}>
              <button
                type="button"
                class="icon"
                title="Edit its condition"
                aria-label={`Edit watchpoint ${props.watchpoint.id}`}
                onClick={open}
              >
                ✎
              </button>
              <button
                type="button"
                class="icon"
                title="Remove it"
                aria-label={`Remove watchpoint ${props.watchpoint.id}`}
                onClick={remove}
              >
                ×
              </button>
            </Show>
          </div>
          <Show when={detail()}>{(text) => <div class="row sub">{text()}</div>}</Show>
          <Show when={error()}>{(err) => <div class="row sub error">{err()}</div>}</Show>
        </li>
      }
    >
      <ConditionEditor
        title={props.watchpoint.expression ?? props.watchpoint.address}
        initial={initial()}
        close={close}
        save={save}
      />
    </Show>
  );
}

function WatchAdder() {
  const focus = useFocus();
  const connection = useConnection();
  const [text, setText] = createSignal("");
  const [access, setAccess] = createSignal<WatchAccess>("change");
  const [error, setError] = createSignal<string | null>(null);
  const add = (event: SubmitEvent) => {
    event.preventDefault();
    const target = text().trim();
    if (!target) {
      return;
    }
    setError(null);
    const { at, stale } = focus();
    const frame = at && !stale ? at : {};
    connection
      .request("addWatchpoint", { ...frame, target, access: access() })
      .then(() => setText(""))
      .catch((failure: Error) => setError(failure.message));
  };
  return (
    <form class="adder watch-adder" onSubmit={add}>
      <input
        class="input quiet"
        aria-label="Add a watchpoint"
        placeholder="+ watch an expression, or 0xaddress:bytes"
        value={text()}
        onInput={(event) => setText(event.currentTarget.value)}
      />
      <select
        class="input quiet"
        aria-label="Stop on"
        value={access()}
        onChange={(event) => setAccess(event.currentTarget.value as WatchAccess)}
      >
        <For each={ACCESS_KEYS}>{(key) => <option value={key}>{ACCESS[key]}</option>}</For>
      </select>
      <Show when={error()}>{(err) => <div class="error">{err()}</div>}</Show>
    </form>
  );
}

function Exceptions(props: { control: boolean }) {
  const session = useModel((model) => model.state?.session);
  const exceptions = useRequest("exceptions", () => (session() ? undefined : null));
  const [error, setError] = createSignal<string | null>(null);
  const filters = createMemo(() => exceptions().data?.filters);
  const onCount = createMemo(() => filters()?.filter((filter) => filter.on).length ?? 0);

  return (
    <Switch>
      <Match when={exceptions().error}>
        {(err) => <div class="error">Exceptions: {err().message}</div>}
      </Match>
      <Match when={filters()}>
        {(list) => (
          <details class="exceptions" data-testid="exceptions">
            <summary>
              Exceptions{" "}
              <span class="count">
                {onCount()} of {list().length} on
              </span>
            </summary>
            <Show when={error()}>{(err) => <div class="error">{err()}</div>}</Show>
            <ul class="rows">
              <For each={list()} keyed={(filter) => filter.id}>
                {(filter) => (
                  <FilterRow filter={filter()} control={props.control} fail={setError} />
                )}
              </For>
            </ul>
          </details>
        )}
      </Match>
    </Switch>
  );
}

function FilterRow(props: {
  filter: ExceptionFilter;
  control: boolean;
  fail: (error: string | null) => void;
}) {
  const connection = useConnection();
  const toggle = (event: Event & { currentTarget: HTMLInputElement }) => {
    props.fail(null);
    connection
      .request("setException", { id: props.filter.id, on: event.currentTarget.checked })
      .catch((failure: Error) => props.fail(failure.message));
  };
  return (
    <li class="row" title={props.filter.description}>
      <label>
        <input
          type="checkbox"
          checked={props.filter.on}
          disabled={!props.control}
          onChange={toggle}
        />{" "}
        {props.filter.label}
      </label>
    </li>
  );
}
