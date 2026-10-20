// Memory at the address the link names, sixteen bytes to a row. The bytes
// of the value it was opened from are marked, a selection reads as the
// types its length could be, and controllers write bytes in place.

import { type KeyboardEvent, useRef, useState } from "react";
import { cache, useRequest } from "../data";
import {
  dumpRows,
  hex,
  target as parseTarget,
  printable,
  ROW,
  readAs,
  rowStart,
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
          </span>
        )}
      </div>
      {shown ? (
        // A new address starts over: at its row, with its bytes selected.
        <Dump key={look.mem} shown={shown} />
      ) : (
        <div className="empty center-message">
          Type an address or an expression above, or open a value's memory from its row.
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
  const go = (mem: string) => {
    look((current) => ({ ...current, mem, view: "memory" }), { replace: false });
    setText("");
  };
  return (
    <form
      className="goto"
      onSubmit={(event) => {
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
          setError("Expressions are read at the stop shown, while it is the program's");
          return;
        }
        cache.get(connection, "evaluate", { ...at, expression: wanted }).promise.then((settled) => {
          const row = settled.ok ? (settled.value as Row) : null;
          if (asked !== latest.current) {
            return;
          }
          if (!settled.ok) {
            setError(settled.error.message);
          } else if (!row?.memory) {
            setError(`${wanted} names no memory`);
          } else {
            go(row.memoryBytes ? `${row.memory}:${row.memoryBytes}` : row.memory);
          }
        });
      }}
    >
      <input
        className="input quiet"
        aria-label="Go to memory"
        placeholder="0xaddress or an expression"
        value={text}
        onChange={(event) => setText(event.target.value)}
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
  const [selection, setSelection] = useState<Selection | null>(value);
  const [editing, setEditing] = useState<bigint | null>(null);
  // The byte the keyboard is on, the bytes' one stop in the tab order.
  const [cursor, setCursor] = useState<bigint>(shown.address);
  const body = useRef<HTMLDivElement>(null);

  const first = rowStart(shown.address) + BigInt(moved * ROW);
  const start = first < 0n ? 0n : first;
  const read = useRequest(
    "readMemory",
    why === null && at ? { stop: at.stop, address: hex(start), count: ROWS * ROW } : null,
  );
  if (why) {
    return <div className="empty center-message">{why}</div>;
  }
  if (read.error) {
    return <div className="empty center-message error">{read.error.message}</div>;
  }
  if (!read.data) {
    return <div className="empty center-message">Reading memory…</div>;
  }
  const rows = dumpRows(start, read.data.bytes, ROWS);
  const readable: bigint[] = rows.flatMap((row) =>
    row.bytes.flatMap((byte, column) => (byte === null ? [] : [row.address + BigInt(column)])),
  );
  // Paging can leave the cursor off screen; the first byte shown takes over.
  const stop = readable.includes(cursor) ? cursor : readable[0];
  const within = (range: Selection | null, address: bigint) =>
    range !== null && address >= range.from && address <= range.to;
  const choose = (address: bigint, extend: boolean) => {
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
  // At once, not on the next frame: a held arrow key's repeats must each
  // start from the byte the last one reached.
  const focusByte = (address: bigint) =>
    body.current?.querySelector<HTMLElement>(`[data-address="${address}"]`)?.focus();
  const walk = (address: bigint, event: KeyboardEvent) => {
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
  };

  return (
    <>
      <div className="mem-pages">
        <button type="button" className="button small" onClick={() => setMoved(moved - ROWS)}>
          ↑ Earlier
        </button>
        <button type="button" className="button small" onClick={() => setMoved(moved + ROWS)}>
          Later ↓
        </button>
        {read.data.unreadable && (
          <span className="dim">Nothing can be read from {read.data.unreadable} on.</span>
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
                    <ByteEditor
                      key={String(address)}
                      address={address}
                      done={() => {
                        setEditing(null);
                        // The byte replaces the editor only once drawn.
                        requestAnimationFrame(() => focusByte(address));
                      }}
                    />
                  );
                }
                return (
                  <ByteCell
                    key={String(address)}
                    address={address}
                    byte={byte}
                    className={`${within(value, address) ? "in-value" : ""} ${within(selection, address) ? "selected" : ""}`}
                    tabbable={address === stop}
                    onSelect={(extend) => choose(address, extend)}
                    onEdit={() => setEditing(address)}
                    onWalk={(event) => walk(address, event)}
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
  onSelect(extend: boolean): void;
  onEdit(): void;
  onWalk(event: KeyboardEvent): void;
}) {
  const { stale } = useFocus();
  const control = useModel(controls);
  if (byte === null) {
    return <span className="byte unread">??</span>;
  }
  const text = byte.toString(16).padStart(2, "0");
  const edit = () => control && !stale && onEdit();
  return (
    <button
      type="button"
      className={`byte ${className}`}
      data-address={String(address)}
      aria-label={`${text} at ${hex(address)}`}
      tabIndex={tabbable ? 0 : -1}
      onClick={(event) => onSelect(event.shiftKey)}
      onDoubleClick={edit}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === "F2") {
          event.preventDefault();
          edit();
        } else {
          onWalk(event);
        }
      }}
    >
      {text}
    </button>
  );
}

/** Writes bytes from `address` on: one pair, or several. */
function ByteEditor({ address, done }: { address: bigint; done(): void }) {
  const { at } = useFocus();
  const connection = useConnection();
  const [error, setError] = useState<string | null>(null);
  return (
    <form
      className="byte-edit"
      onSubmit={(event) => {
        event.preventDefault();
        const bytes = String(new FormData(event.currentTarget).get("bytes") ?? "").replaceAll(
          /\s/g,
          "",
        );
        if (!at || !/^([0-9a-f]{2})+$/i.test(bytes)) {
          setError("pairs of hexadecimal digits");
          return;
        }
        connection
          .request("writeMemory", { stop: at.stop, address: hex(address), bytes })
          .then(done)
          .catch((failure: Error) => setError(failure.message));
      }}
    >
      <input
        className="input"
        name="bytes"
        aria-label={`New byte at ${hex(address)}`}
        title="hexadecimal pairs; several write the bytes after it too"
        aria-invalid={error !== null}
        // biome-ignore lint/a11y/noAutofocus: the person asked to edit this byte
        autoFocus
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            done();
          }
        }}
        onBlur={() => error === null && done()}
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
