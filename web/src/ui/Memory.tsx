// Memory at the address the link names, sixteen bytes to a row. The bytes
// of the value it was opened from are marked, and selected, or only its C
// string's bytes are when it is a character array with room to spare. A
// selection reads as the types its length could be, and controllers write
// bytes in place.

import {
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
  type MouseEvent,
  useCallback,
  useMemo,
  useRef,
  useState,
} from "react";

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

/** Rows shown at once. */
const ROWS = 16;

/** One past the last address. */
const END = 1n << 64n;

/** How far each arrow key moves through the bytes. */
const STEPS: Record<string, bigint> = {
  ArrowLeft: -1n,
  ArrowRight: 1n,
  ArrowUp: -BigInt(ROW),
  ArrowDown: BigInt(ROW),
};

export function Memory() {
  const { look } = useFocus();
  const shown = look.mem ? parseTarget(look.mem) : null;
  return (
    <div className="mem">
      <div className="view-head">
        <GoTo />
        {shown && (
          <span className="dim">
            {hex(shown.address)}
            {shown.bytes ? ` · ${shown.bytes} bytes` : ""}
            {shown.text ? ` · string: ${shown.text} bytes` : ""}
          </span>
        )}
      </div>
      {shown ? (
        // A new address starts over: at its row, with its bytes selected.
        <Dump key={look.mem} shown={shown} />
      ) : (
        <div className="empty center-message">
          Type an address or an expression above, or open a value&apos;s memory from its row.
        </div>
      )}
    </div>
  );
}

/** Goes to an address, or to the memory an expression's value names. */
function GoTo() {
  const { at, stale } = useFocus();
  const look = useLook();
  const connection = useConnection();
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  // Only the latest go-to moves the tab, however its answers are ordered.
  const latest = useRef(0);
  const go = useCallback(
    (mem: string) => {
      look((current) => ({ ...current, mem, view: "memory" }), { replace: false });
      setText("");
    },
    [look],
  );
  const submit = useCallback(
    async (event: FormEvent) => {
      event.preventDefault();
      const wanted = text.trim();
      const asked = ++latest.current;
      setError(null);
      const named = parseTarget(wanted);
      if (named) {
        go(targetText(named));
        return;
      }
      if (!at || stale) {
        setError("Expressions only work while the program is at the stop shown");
        return;
      }
      const settled = await cache.get(connection, "evaluate", { ...at, expression: wanted })
        .promise;
      const row = settled.ok ? (settled.value as Row) : null;
      if (asked !== latest.current) {
        return;
      }
      if (!settled.ok) {
        setError(settled.error.message);
      } else if (row?.memory) {
        go(rowTarget(row) ?? row.memory);
      } else {
        setError(`${wanted} has no address in memory`);
      }
    },
    [text, at, stale, connection, go],
  );
  const edit = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setText(event.target.value),
    [],
  );
  return (
    <form className="goto" onSubmit={submit}>
      <input
        className="input quiet"
        aria-label="Go to memory"
        placeholder="0xaddress or an expression"
        value={text}
        onChange={edit}
      />
      {error && <span className="error">{error}</span>}
    </form>
  );
}

interface Selection {
  from: bigint;
  to: bigint;
}

function Dump({ shown }: { shown: Target }) {
  const focus = useFocus();
  const { at } = focus;
  const why = hidden(focus, "Bytes");
  const [moved, setMoved] = useState(0);
  const value = shown.bytes
    ? { from: shown.address, to: shown.address + BigInt(shown.bytes - 1) }
    : null;
  assert(
    value === null || value.from <= value.to,
    `value at ${hex(shown.address)} ends before it starts`,
  );
  // A character array opens with its string selected, so the selection
  // reads as the text; the array's spare bytes stay marked around it.
  const string =
    value && shown.text ? { from: value.from, to: value.from + BigInt(shown.text - 1) } : null;
  const [selection, setSelection] = useState<Selection | null>(string ?? value);
  const [editing, setEditing] = useState<bigint | null>(null);
  // The byte the keyboard is on, the bytes' one stop in the tab order.
  const [cursor, setCursor] = useState<bigint>(shown.address);
  const body = useRef<HTMLDivElement>(null);

  // Paging stops at either end of the address space.
  const first = rowStart(shown.address) + BigInt(moved * ROW);
  const last = END - BigInt(ROWS * ROW);
  const start = first < 0n ? 0n : first > last ? last : first;
  assert(moved % ROWS === 0, `memory moved ${moved} rows, not a page`);
  assert(start % BigInt(ROW) === 0n, `memory starts mid-row at ${hex(start)}`);
  const read = useRequest(
    "readMemory",
    why === null && at ? { stop: at.stop, address: hex(start), count: ROWS * ROW } : null,
  );
  const data = read.data;
  const rows = useMemo(() => (data ? dumpRows(start, data.bytes, ROWS) : []), [start, data]);
  const readable = useMemo(
    (): bigint[] =>
      rows.flatMap((row) =>
        row.bytes.flatMap((byte, column) => (byte === null ? [] : [row.address + BigInt(column)])),
      ),
    [rows],
  );
  const choose = useCallback(
    (address: bigint, extend: boolean) => {
      assert(readable.includes(address), `chose unread byte ${hex(address)}`);
      setCursor(address);
      setSelection((current) =>
        extend && current
          ? {
              from: address < current.from ? address : current.from,
              to: address > current.to ? address : current.to,
            }
          : { from: address, to: address },
      );
    },
    [readable],
  );
  // At once, not on the next frame: a held arrow key's repeats must each
  // start from the byte the last one reached.
  const focusByte = useCallback(
    (address: bigint) =>
      body.current?.querySelector<HTMLElement>(`[data-address="${address}"]`)?.focus(),
    [],
  );
  const walk = useCallback(
    (address: bigint, event: KeyboardEvent) => {
      const step = STEPS[event.key];
      if (step === undefined) {
        return;
      }
      event.preventDefault();
      const next = address + step;
      if (readable.includes(next)) {
        choose(next, event.shiftKey);
        focusByte(next);
      }
    },
    [readable, choose, focusByte],
  );
  const edit = useCallback((address: bigint) => setEditing(address), []);
  const doneEditing = useCallback(
    (address: bigint) => {
      setEditing(null);
      // The byte replaces the editor only once drawn.
      requestAnimationFrame(() => focusByte(address));
    },
    [focusByte],
  );
  const earlier = useCallback(() => setMoved(moved - ROWS), [moved]);
  const later = useCallback(() => setMoved(moved + ROWS), [moved]);

  if (why) {
    return <div className="empty center-message">{why}</div>;
  }
  if (read.error) {
    return <div className="empty center-message error">{read.error.message}</div>;
  }
  if (!data) {
    return <div className="empty center-message">Reading memory…</div>;
  }
  assert(rows.length === ROWS, `memory shows ${rows.length} rows, not ${ROWS}`);
  assert(rows[0]?.address === start, `memory rows do not start at ${hex(start)}`);
  // Paging can leave the cursor off screen; the first byte shown takes over.
  const stop = readable.includes(cursor) ? cursor : readable[0];
  const within = (range: Selection | null, address: bigint) =>
    range !== null && address >= range.from && address <= range.to;

  return (
    <>
      <div className="mem-pages">
        <button type="button" className="button small" disabled={start === 0n} onClick={earlier}>
          ↑ Earlier
        </button>
        <button type="button" className="button small" disabled={start === last} onClick={later}>
          Later ↓
        </button>
        {data.unreadable && (
          <span className="dim">Nothing can be read from {data.unreadable} on.</span>
        )}
      </div>
      <div ref={body} className={`mem-body ${read.current ? "" : "stale"}`} data-testid="memory">
        {rows.map((row) => (
          <div key={String(row.address)} className="mem-row">
            <span className="mem-address">{row.address.toString(16).padStart(12, "0")}</span>
            <span className="mem-bytes">
              {row.bytes.map((byte, column) => {
                const address = row.address + BigInt(column);
                if (editing === address) {
                  return (
                    <ByteEditor key={String(address)} address={address} onDone={doneEditing} />
                  );
                }
                return (
                  <ByteCell
                    key={String(address)}
                    address={address}
                    byte={byte}
                    className={`${within(value, address) ? "in-value" : ""} ${within(selection, address) ? "selected" : ""}`}
                    tabbable={address === stop}
                    onSelect={choose}
                    onEdit={edit}
                    onWalk={walk}
                  />
                );
              })}
            </span>
            <span className="mem-text">{row.bytes.map(printable).join("")}</span>
          </div>
        ))}
      </div>
      <Readings rows={rows} selection={selection} />
    </>
  );
}

/** One byte; Enter or F2 edits it, as a double click does. */
function ByteCell({
  address,
  byte,
  className,
  tabbable,
  onSelect,
  onEdit,
  onWalk,
}: {
  address: bigint;
  byte: number | null;
  className: string;
  tabbable: boolean;
  onSelect(address: bigint, extend: boolean): void;
  onEdit(address: bigint): void;
  onWalk(address: bigint, event: KeyboardEvent): void;
}) {
  const { stale } = useFocus();
  const control = useModel(controls);
  const select = useCallback(
    (event: MouseEvent) => onSelect(address, event.shiftKey),
    [onSelect, address],
  );
  const edit = useCallback(
    () => control && !stale && onEdit(address),
    [control, stale, onEdit, address],
  );
  const key = useCallback(
    (event: KeyboardEvent) => {
      if (event.key === "Enter" || event.key === "F2") {
        event.preventDefault();
        edit();
      } else {
        onWalk(address, event);
      }
    },
    [edit, onWalk, address],
  );
  if (byte === null) {
    return <span className="byte unread">??</span>;
  }
  const text = byte.toString(16).padStart(2, "0");
  return (
    <button
      type="button"
      className={`byte ${className}`}
      data-address={String(address)}
      aria-label={`${text} at ${hex(address)}`}
      tabIndex={tabbable ? 0 : -1}
      onClick={select}
      onDoubleClick={edit}
      onKeyDown={key}
    >
      {text}
    </button>
  );
}

/** Writes bytes from `address` on: one pair, or several. */
function ByteEditor({ address, onDone }: { address: bigint; onDone(address: bigint): void }) {
  const { at } = useFocus();
  const connection = useConnection();
  const [error, setError] = useState<string | null>(null);
  const finish = useCallback(() => onDone(address), [onDone, address]);
  const submit = useCallback(
    (event: FormEvent<HTMLFormElement>) => {
      event.preventDefault();
      const bytes = String(new FormData(event.currentTarget).get("bytes") ?? "").replaceAll(
        /\s/gu,
        "",
      );
      if (!at || !/^([0-9a-f]{2})+$/iu.test(bytes)) {
        setError("pairs of hexadecimal digits");
        return;
      }
      connection
        .request("writeMemory", { stop: at.stop, address: hex(address), bytes })
        .then(finish)
        .catch((failure: Error) => setError(failure.message));
    },
    [at, connection, address, finish],
  );
  const escape = useCallback(
    (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        finish();
      }
    },
    [finish],
  );
  const blur = useCallback(() => error === null && finish(), [error, finish]);
  return (
    <form className="byte-edit" onSubmit={submit}>
      <input
        className="input"
        name="bytes"
        aria-label={`New byte at ${hex(address)}`}
        title="Hex bytes, such as 0a ff. Extra bytes overwrite the ones after it."
        aria-invalid={error !== null}
        // oxlint-disable-next-line jsx-a11y/no-autofocus -- the person asked to edit this byte
        autoFocus
        onKeyDown={escape}
        onBlur={blur}
      />
      {error && <span className="error">{error}</span>}
    </form>
  );
}

/** The selected bytes, read as the types their length could be. */
function Readings({
  rows,
  selection,
}: {
  rows: ReturnType<typeof dumpRows>;
  selection: Selection | null;
}) {
  if (!selection) {
    return (
      <div className="mem-selection dim" data-testid="selection">
        Click a byte, or shift-click to select several. The arrow keys move, and Enter edits.
      </div>
    );
  }
  assert(selection.from <= selection.to, `selection ${hex(selection.from)} ends before it starts`);
  const bytes: (number | null)[] = [];
  for (const row of rows) {
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
  return (
    <div className="mem-selection" data-testid="selection">
      <span className="dim">
        {count} {count === 1 ? "byte" : "bytes"} at {hex(selection.from)}
      </span>
      {whole ? (
        readAs(bytes as number[]).map((reading) => (
          <span key={reading.as} className="reading">
            <span className="dim">{reading.as}</span> {reading.text}
          </span>
        ))
      ) : (
        <span className="dim">not all of it is shown or readable</span>
      )}
    </div>
  );
}
