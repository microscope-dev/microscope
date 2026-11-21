import { createMemo, createSignal, For, Match, onSettled, Show, Switch, untrack } from "solid-js";

import { assert } from "../assert";
import { cache, useRequest } from "../data";
import {
  dumpRows,
  hex,
  target as parseTarget,
  printable,
  ROW,
  readAs,
  rowStart,
  rowTarget,
  type Target,
  targetText,
} from "../memory";
import { controls } from "../model";
import type { Row } from "../protocol";
import { useConnection, useModel } from "../store";
import { hidden } from "./hidden";
import { useLook } from "./navigation";
import { useFocus } from "./Workspace";

const ROWS = 16;

const END = 1n << 64n;

const STEPS: Record<string, bigint> = {
  ArrowLeft: -1n,
  ArrowRight: 1n,
  ArrowUp: -BigInt(ROW),
  ArrowDown: BigInt(ROW),
};

export function Memory() {
  const focus = useFocus();
  const keyedShown = createMemo(
    () => {
      const mem = focus().look.mem;
      if (!mem) {
        return null;
      }
      const target = parseTarget(mem);
      return target ? { key: mem, target } : null;
    },
    { equals: (a, b) => a?.key === b?.key },
  );

  return (
    <div class="mem">
      <div class="view-head">
        <GoTo />
        <Show when={keyedShown()}>
          {(entry) => (
            <span class="dim">
              {hex(entry().target.address)}
              {entry().target.bytes ? ` · ${entry().target.bytes} bytes` : ""}
              {entry().target.text ? ` · string: ${entry().target.text} bytes` : ""}
            </span>
          )}
        </Show>
      </div>
      <Show
        when={keyedShown()}
        keyed
        fallback={
          <div class="empty center-message">
            Type an address or an expression above, or open a value&apos;s memory from its row.
          </div>
        }
      >
        {(entry) => <Dump shown={entry.target} />}
      </Show>
    </div>
  );
}

function GoTo() {
  const focus = useFocus();
  const look = useLook();
  const connection = useConnection();
  const [text, setText] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);

  let latest = 0;

  const go = (mem: string) => {
    look((current) => ({ ...current, mem, view: "memory" }), { replace: false });
    setText("");
  };

  const submit = async (event: SubmitEvent) => {
    event.preventDefault();
    const wanted = text().trim();
    const asked = ++latest;
    setError(null);
    const named = parseTarget(wanted);
    if (named) {
      go(targetText(named));
      return;
    }
    const { at, stale } = focus();
    if (!at || stale) {
      setError("Expressions only work while the program is at the stop shown");
      return;
    }
    const settled = await cache.get(connection, "evaluate", { ...at, expression: wanted }).promise;
    const row = settled.ok ? (settled.value as Row) : null;
    if (asked !== latest) {
      return;
    }
    if (!settled.ok) {
      setError(settled.error.message);
    } else if (row?.memory) {
      go(rowTarget(row) ?? row.memory);
    } else {
      setError(`${wanted} has no address in memory`);
    }
  };

  return (
    <form class="goto" onSubmit={(event) => void submit(event)}>
      <input
        class="input quiet"
        aria-label="Go to memory"
        placeholder="0xaddress or an expression"
        value={text()}
        onInput={(event) => setText(event.currentTarget.value)}
      />
      <Show when={error()}>{(err) => <span class="error">{err()}</span>}</Show>
    </form>
  );
}

interface Selection {
  from: bigint;
  to: bigint;
}

function Dump(props: { shown: Target }) {
  const focus = useFocus();
  const why = createMemo(() => hidden(focus(), "Bytes"));
  const [moved, setMoved] = createSignal(0);

  const value = createMemo(() => {
    const shown = props.shown;
    const range = shown.bytes
      ? { from: shown.address, to: shown.address + BigInt(shown.bytes - 1) }
      : null;
    assert(
      range === null || range.from <= range.to,
      `value at ${hex(shown.address)} ends before it starts`,
    );
    return range;
  });

  const initialSelection = untrack(() => {
    const range = value();
    const shown = props.shown;
    const string =
      range && shown.text ? { from: range.from, to: range.from + BigInt(shown.text - 1) } : null;
    return string ?? range;
  });

  const [selection, setSelection] = createSignal<Selection | null>(initialSelection);
  const [editing, setEditing] = createSignal<bigint | null>(null);
  const [cursor, setCursor] = createSignal<bigint>(untrack(() => props.shown.address));
  let body: HTMLDivElement | undefined;

  const last = END - BigInt(ROWS * ROW);
  const start = createMemo(() => {
    const currentMoved = moved();
    const first = rowStart(props.shown.address) + BigInt(currentMoved * ROW);
    const bounded = first < 0n ? 0n : first > last ? last : first;
    assert(currentMoved % ROWS === 0, `memory moved ${currentMoved} rows, not a page`);
    assert(bounded % BigInt(ROW) === 0n, `memory starts mid-row at ${hex(bounded)}`);
    return bounded;
  });

  const read = useRequest("readMemory", () => {
    const at = focus().at;
    return why() === null && at
      ? { stop: at.stop, address: hex(start()), count: ROWS * ROW }
      : null;
  });

  const rows = createMemo(() => {
    const data = read().data;
    if (!data) {
      return [];
    }
    const built = dumpRows(start(), data.bytes, ROWS);
    assert(built.length === ROWS, `memory shows ${built.length} rows, not ${ROWS}`);
    assert(built[0]?.address === start(), `memory rows do not start at ${hex(start())}`);
    return built;
  });

  const readable = createMemo((): bigint[] =>
    rows().flatMap((row) =>
      row.bytes.flatMap((byte, column) => (byte === null ? [] : [row.address + BigInt(column)])),
    ),
  );

  const stop = createMemo(() => {
    const currentReadable = readable();
    const currentCursor = cursor();
    return currentReadable.includes(currentCursor) ? currentCursor : currentReadable[0];
  });

  const choose = (address: bigint, extend: boolean) => {
    assert(readable().includes(address), `chose unread byte ${hex(address)}`);
    setCursor(address);
    setSelection((current) =>
      extend && current
        ? {
            from: address < current.from ? address : current.from,
            to: address > current.to ? address : current.to,
          }
        : { from: address, to: address },
    );
  };

  const focusByte = (address: bigint) =>
    body?.querySelector<HTMLElement>(`[data-address="${address}"]`)?.focus();

  const walk = (address: bigint, event: KeyboardEvent) => {
    const step = STEPS[event.key];
    if (step === undefined) {
      return;
    }
    event.preventDefault();
    const next = address + step;
    if (readable().includes(next)) {
      choose(next, event.shiftKey);
      focusByte(next);
    }
  };

  const edit = (address: bigint) => setEditing(address);

  const doneEditing = (address: bigint) => {
    setEditing(null);
    requestAnimationFrame(() => focusByte(address));
  };

  const earlier = () => setMoved((current) => current - ROWS);
  const later = () => setMoved((current) => current + ROWS);

  const within = (range: Selection | null, address: bigint) =>
    range !== null && address >= range.from && address <= range.to;

  return (
    <Switch fallback={<div class="empty center-message">Reading memory…</div>}>
      <Match when={why()}>{(reason) => <div class="empty center-message">{reason()}</div>}</Match>
      <Match when={read().error}>
        {(err) => <div class="empty center-message error">{err().message}</div>}
      </Match>
      <Match when={read().data}>
        {(data) => (
          <>
            <div class="mem-pages">
              <button
                type="button"
                class="button small"
                disabled={start() === 0n}
                onClick={earlier}
              >
                ↑ Earlier
              </button>
              <button
                type="button"
                class="button small"
                disabled={start() === last}
                onClick={later}
              >
                Later ↓
              </button>
              <Show when={data().unreadable}>
                {(unreadable) => (
                  <span class="dim">Nothing can be read from {unreadable()} on.</span>
                )}
              </Show>
            </div>
            <div
              ref={(node) => {
                body = node;
              }}
              class={["mem-body", { stale: !read().current }]}
              data-testid="memory"
            >
              <For each={rows()} keyed={(row) => String(row.address)}>
                {(row) => (
                  <div class="mem-row">
                    <span class="mem-address">{row().address.toString(16).padStart(12, "0")}</span>
                    <span class="mem-bytes">
                      <For each={row().bytes} keyed={false}>
                        {(byte, column) => {
                          const address = createMemo(() => row().address + BigInt(column));
                          return (
                            <Show
                              when={editing() === address()}
                              fallback={
                                <ByteCell
                                  address={address()}
                                  byte={byte()}
                                  inValue={within(value(), address())}
                                  selected={within(selection(), address())}
                                  tabbable={address() === stop()}
                                  onSelect={choose}
                                  onEdit={edit}
                                  onWalk={walk}
                                />
                              }
                            >
                              <ByteEditor address={address()} onDone={doneEditing} />
                            </Show>
                          );
                        }}
                      </For>
                    </span>
                    <span class="mem-text">{row().bytes.map(printable).join("")}</span>
                  </div>
                )}
              </For>
            </div>
            <Readings rows={rows()} selection={selection()} />
          </>
        )}
      </Match>
    </Switch>
  );
}

function ByteCell(props: {
  address: bigint;
  byte: number | null;
  inValue: boolean;
  selected: boolean;
  tabbable: boolean;
  onSelect(address: bigint, extend: boolean): void;
  onEdit(address: bigint): void;
  onWalk(address: bigint, event: KeyboardEvent): void;
}) {
  const focus = useFocus();
  const control = useModel(controls);

  const select = (event: MouseEvent) => props.onSelect(props.address, event.shiftKey);

  const edit = () => {
    if (control() && !focus().stale) {
      props.onEdit(props.address);
    }
  };

  const key = (event: KeyboardEvent) => {
    if (event.key === "Enter" || event.key === "F2") {
      event.preventDefault();
      edit();
    } else {
      props.onWalk(props.address, event);
    }
  };

  const text = createMemo(() =>
    props.byte === null ? "" : props.byte.toString(16).padStart(2, "0"),
  );

  return (
    <Show when={props.byte !== null} fallback={<span class="byte unread">??</span>}>
      <button
        type="button"
        class={["byte", { "in-value": props.inValue, selected: props.selected }]}
        data-address={String(props.address)}
        aria-label={`${text()} at ${hex(props.address)}`}
        tabindex={props.tabbable ? 0 : -1}
        onClick={select}
        onDblClick={edit}
        onKeyDown={key}
      >
        {text()}
      </button>
    </Show>
  );
}

function ByteEditor(props: { address: bigint; onDone(address: bigint): void }) {
  const focus = useFocus();
  const connection = useConnection();
  const [error, setError] = createSignal<string | null>(null);
  let input: HTMLInputElement | undefined;

  onSettled(() => {
    input?.focus();
  });

  const finish = () => props.onDone(props.address);

  const submit = (event: SubmitEvent & { currentTarget: HTMLFormElement }) => {
    event.preventDefault();
    const bytes = String(new FormData(event.currentTarget).get("bytes") ?? "").replaceAll(
      /\s/gu,
      "",
    );
    const at = focus().at;
    if (!at || !/^([0-9a-f]{2})+$/iu.test(bytes)) {
      setError("pairs of hexadecimal digits");
      return;
    }
    connection
      .request("writeMemory", { stop: at.stop, address: hex(props.address), bytes })
      .then(finish)
      .catch((failure: Error) => setError(failure.message));
  };

  const escape = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.stopPropagation();
      finish();
    }
  };

  const blur = () => {
    if (error() === null) {
      finish();
    }
  };

  return (
    <form class="byte-edit" onSubmit={submit}>
      <input
        ref={(node) => {
          input = node;
        }}
        class="input"
        name="bytes"
        aria-label={`New byte at ${hex(props.address)}`}
        title="Hex bytes, such as 0a ff. Extra bytes overwrite the ones after it."
        aria-invalid={error() === null ? "false" : "true"}
        autofocus
        onKeyDown={escape}
        onBlur={blur}
      />
      <Show when={error()}>{(err) => <span class="error">{err()}</span>}</Show>
    </form>
  );
}

function Readings(props: { rows: ReturnType<typeof dumpRows>; selection: Selection | null }) {
  const summary = createMemo(() => {
    const selection = props.selection;
    if (!selection) {
      return null;
    }
    assert(
      selection.from <= selection.to,
      `selection ${hex(selection.from)} ends before it starts`,
    );
    const bytes: (number | null)[] = [];
    for (const row of props.rows) {
      for (const [column, byte] of row.bytes.entries()) {
        const address = row.address + BigInt(column);
        if (address >= selection.from && address <= selection.to) {
          bytes.push(byte);
        }
      }
    }
    const count = Number(selection.to - selection.from) + 1;
    assert(bytes.length <= count, `${bytes.length} bytes shown of a ${count} byte selection`);
    const whole = bytes.length === count && bytes.every((byte) => byte !== null);
    return {
      from: selection.from,
      count,
      readings: whole ? readAs(bytes as number[]) : null,
    };
  });

  return (
    <Show
      when={summary()}
      fallback={
        <div class="mem-selection dim" data-testid="selection">
          Click a byte, or shift-click to select several. The arrow keys move, and Enter edits.
        </div>
      }
    >
      {(info) => (
        <div class="mem-selection" data-testid="selection">
          <span class="dim">
            {info().count} {info().count === 1 ? "byte" : "bytes"} at {hex(info().from)}
          </span>
          <Show
            when={info().readings}
            fallback={<span class="dim">not all of it is shown or readable</span>}
          >
            {(readings) => (
              <For each={readings()} keyed={(reading) => reading.as}>
                {(reading) => (
                  <span class="reading">
                    <span class="dim">{reading().as}</span> {reading().text}
                  </span>
                )}
              </For>
            )}
          </Show>
        </div>
      )}
    </Show>
  );
}
