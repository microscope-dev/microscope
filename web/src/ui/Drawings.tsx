// The Drawings view: each argument and local of the frame shown that a
// view draws, then each value the link pins, drawn by its renderer in the
// sandbox. A card keeps its drawing while the program runs, dimmed and
// labelled with its stop, and draws again at the next stop it shows.

import {
  type KeyboardEvent as ReactKeyboardEvent,
  type RefObject,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";
import { base } from "../base";
import { useRequest } from "../data";
import { type At, drawing, formatPinned, type Look, type Pinned, parsePinned } from "../focus";
import type { Drawing, Row } from "../protocol";
import { flash } from "../tab";
import { useShownScheme } from "../theme";
import { DecodeError, decodeInputs, inputPaths, type Value } from "../visualize/decode";
import { buildPicture } from "../visualize/draw";
import { closeImages, type Picture, PictureError, validate } from "../visualize/picture";
import {
  contextFor,
  DRAW_TIMEOUT,
  failureText,
  type Outcome,
  type RendererCode,
  Sandbox,
} from "../visualize/sandbox";
import { csv, type InputRows, inputRows } from "../visualize/table";
import { LiveDrawing, type LiveJob } from "./LiveDrawing";
import { useLook } from "./navigation";
import { hidden } from "./Values";
import { LocalExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

/** One value drawn: where it comes from and which renderers it offers. */
export interface Card {
  key: string;
  path: string;
  /** The renderers the value's view offers, or null until known. */
  offered: string[] | null;
  /** The renderer the link names for it, if any. */
  chosen: string | null;
  /** Its place among the link's pinned values, or null for a variable. */
  pinned: number | null;
}

let shared: Sandbox | null = null;

/** The card just pinned, which comes into view when it appears. */
let reveal: string | null = null;

/** Pins `pinned` to the drawings and brings its card into view. */
export function pin(pinned: Pinned): (look: Look) => Look {
  reveal = formatPinned(pinned);
  return (look) => drawing(look, pinned);
}

/** The tab's sandbox, started the first time something is drawn. */
function sandbox(): Sandbox {
  shared ??= new Sandbox(`${base}visualizer-frame`);
  return shared;
}

/** The drawings the focus shows: the frame's variables that a view draws,
 * then the values the link pins. */
export function useCards(): Card[] {
  const focus = useFocus();
  const why = hidden(focus);
  const scopes = useRequest("scopes", why === null ? (focus.at as At) : null);
  const pins = focus.look.d;
  // While the program runs or its stop has passed, and until the next
  // stop's values arrive, the cards stay as they were, each dimmed until it
  // draws the stop shown. Replacing them would forget what they drew.
  const last = useRef<Card[]>([]);
  return useMemo(() => {
    if ((why !== null || scopes.data === undefined) && last.current.length > 0) {
      return last.current;
    }
    const cards: Card[] = [];
    const seen = new Set<string>();
    for (const scope of scopes.data?.scopes ?? []) {
      if (scope.key === "statics") {
        continue;
      }
      for (const row of scope.rows) {
        if (row.path !== null && row.drawings.length > 0 && !seen.has(row.path)) {
          seen.add(row.path);
          cards.push({
            key: row.path,
            path: row.path,
            offered: row.drawings,
            chosen: null,
            pinned: null,
          });
        }
      }
    }
    (pins ?? []).forEach((entry, index) => {
      const pinned = parsePinned(entry);
      const key = formatPinned(pinned);
      if (seen.has(key)) {
        // A pin of a variable the frame draws pins that variable's card.
        const card = cards.find((each) => each.key === key);
        if (card && card.pinned === null) {
          card.pinned = index;
        }
        return;
      }
      seen.add(key);
      cards.push({
        key,
        path: pinned.path,
        offered: null,
        chosen: pinned.renderer,
        pinned: index,
      });
    });
    last.current = cards;
    return cards;
  }, [scopes.data, pins, why]);
}

export function Drawings() {
  const focus = useFocus();
  const cards = useCards();
  const why = hidden(focus, "Drawings");
  if (cards.length === 0) {
    return (
      <div className="drawings">
        <div className="empty center-message">
          {why ?? (
            <>
              No value here has a drawing. Draw one from its row, or give its type a{" "}
              <code>visualize</code> in a view.
            </>
          )}
        </div>
      </div>
    );
  }
  return (
    <div className="drawings" data-testid="drawings">
      {cards.map((card) => (
        <DrawingCard key={card.key} card={card} />
      ))}
    </div>
  );
}

/** What a card shows: a picture, or why there is none, from one stop, and
 * the inputs its renderer had when it had any. */
type Shown =
  | { stop: number; picture: Picture; renderer: Drawing["renderer"]; inputs: Inputs }
  | { stop: number; live: LiveJob; renderer: Drawing["renderer"]; inputs: Inputs }
  | { stop: number; problem: string; inputs: Inputs | null };

type Inputs = Record<string, Value>;

/** A draw a card wants: the latest replaces any that has not started. */
interface Job {
  stop: number;
  key: string;
  source: RendererCode;
  renderer: Drawing["renderer"];
  inputs: Inputs;
  paths: Record<string, string | null>;
}

/** How many CSS pixels a card's width changes by before it draws again. */
const RESIZED = 8;

/** The drawing chosen among a value's several, by session and path, so it
 * stays chosen at the next stop and in another view. */
const tabs = new Map<string, string>();

/** The inputs a card drew at each stop, for `previous`, by session, path,
 * and renderer: stops count on across sessions. */
const drawn = new Map<string, { stop: number; inputs: Record<string, Value> }[]>();

function previousInputs(key: string, stop: number): Record<string, Value> | null {
  const before = (drawn.get(key) ?? []).filter((entry) => entry.stop < stop);
  return before.at(-1)?.inputs ?? null;
}

function remember(key: string, stop: number, inputs: Record<string, Value>): void {
  const entries = (drawn.get(key) ?? []).filter((entry) => entry.stop !== stop);
  entries.push({ stop, inputs });
  entries.sort((a, b) => a.stop - b.stop);
  drawn.set(key, entries.slice(-4));
}

function outcomeProblem(outcome: Outcome): string | null {
  switch (outcome.kind) {
    case "timeout":
      return `The renderer took longer than ${DRAW_TIMEOUT / 1_000} s, so it was stopped.`;
    case "failed":
      return failureText(outcome.failure);
    case "picture":
    case "live":
      return null;
  }
}

/** Each job a live renderer draws, made once so a job drawn again, as at
 * a new width, is not news to the renderer. */
const liveJobs = new WeakMap<Job, LiveJob>();

/** What a card shows after a draw: its picture, or why there is none. */
function shownFor(job: Job, outcome: Outcome): Shown {
  const { stop, inputs } = job;
  const failed = outcomeProblem(outcome);
  if (failed !== null) {
    return { stop, problem: failed, inputs };
  }
  if (outcome.kind === "live") {
    let live = liveJobs.get(job);
    if (live === undefined) {
      live = {
        stop,
        source: job.source,
        inputs,
        paths: job.paths,
        previous: previousInputs(job.key, stop),
      };
      liveJobs.set(job, live);
    }
    remember(job.key, stop, inputs);
    return { stop, live, renderer: job.renderer, inputs };
  }
  try {
    const picture = validate((outcome as { picture: unknown }).picture);
    remember(job.key, stop, inputs);
    return { stop, picture, renderer: job.renderer, inputs };
  } catch (error) {
    closeImages((outcome as { picture: unknown }).picture);
    return {
      stop,
      problem:
        error instanceof PictureError
          ? `${job.renderer.name}.js returned a picture the page cannot show: ${error.message}`
          : String(error),
      inputs,
    };
  }
}

/** A value's path with a part of it, as `select` names one. */
function partPath(path: string, select: string): string {
  const whole = /^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*|\[\d+\])*$/.test(path) ? path : `(${path})`;
  return select.startsWith("[") ? `${whole}${select}` : `${whole}.${select}`;
}

function DrawingCard({ card }: { card: Card }) {
  const focus = useFocus();
  const { at, stale } = focus;
  const live = at !== null && stale === null ? at : null;
  const look = useLook();
  const theme = useShownScheme();
  const id = useId();
  const holder = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const { shown, draw, fail } = useDrawQueue(id, holder, body);

  // A pinned value with no renderer named draws with what its view offers.
  const evaluated = useRequest(
    "evaluate",
    card.offered === null && card.chosen === null && live
      ? { ...live, expression: card.path }
      : null,
  );
  const offered = card.offered ?? evaluated.data?.drawings ?? null;
  const tabKey = `${focus.session}~${card.path}`;
  const [tab, setTabState] = useState<string | null>(() => tabs.get(tabKey) ?? null);
  const setTab = (name: string) => {
    tabs.set(tabKey, name);
    setTabState(name);
  };
  const renderer =
    card.chosen ?? (tab !== null && offered?.includes(tab) ? tab : (offered?.[0] ?? null));

  const asked = useRequest(
    "draw",
    live && renderer !== null && visible ? { ...live, path: card.path, renderer } : null,
  );
  const code = useRequest(
    "renderer",
    asked.data && asked.current ? { digest: asked.data.renderer.digest } : null,
  );

  // Only cards on screen draw.
  useEffect(() => {
    const element = holder.current;
    if (!element) {
      return;
    }
    if (reveal === card.key) {
      reveal = null;
      element.scrollIntoView({ block: "nearest" });
    }
    const observer = new IntersectionObserver((entries) => {
      setVisible(entries.some((entry) => entry.isIntersecting));
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [card.key]);

  // Each card has its own worker, which ends with it.
  useEffect(() => () => shared?.release(id), [id]);

  const stop = live?.stop ?? null;
  const drawingData = asked.current ? asked.data : undefined;
  const source = code.current ? code.data : undefined;
  // A request that failed says which and why, rather than leave the card
  // waiting for a drawing that will not come.
  let problem: string | undefined;
  if (evaluated.error) {
    problem = `Cannot evaluate ${card.path}: ${evaluated.error.message}`;
  } else if (asked.error) {
    problem = asked.error.message;
  } else if (drawingData && code.error) {
    problem = `Cannot load renderer ${drawingData.renderer.name}: ${code.error.message}`;
  }
  // biome-ignore lint/correctness/useExhaustiveDependencies: a new theme draws again, in its colors
  useEffect(() => {
    if (stop === null) {
      return;
    }
    if (problem !== undefined) {
      fail(stop, problem);
      return;
    }
    if (!drawingData) {
      return;
    }
    if (drawingData.problem !== null || drawingData.inputs === null) {
      fail(stop, `Cannot draw: ${drawingData.problem ?? "no inputs"}`);
      return;
    }
    if (!source) {
      return;
    }
    let inputs: Inputs;
    try {
      inputs = decodeInputs(drawingData.inputs, drawingData.payload ?? new Uint8Array());
    } catch (error) {
      fail(stop, `Cannot draw: ${error instanceof DecodeError ? error.message : String(error)}`);
      return;
    }
    draw({
      stop,
      key: `${focus.session}~${card.path}~${drawingData.renderer.name}`,
      source,
      renderer: drawingData.renderer,
      inputs,
      paths: inputPaths(drawingData.inputs),
    });
  }, [stop, drawingData, source, problem, theme, card.path, focus.session, draw, fail]);

  // The page builds the picture's elements itself.
  const picture = shown && "picture" in shown ? shown.picture : null;
  useEffect(() => {
    const element = body.current;
    if (!element) {
      return;
    }
    element.replaceChildren();
    if (picture) {
      element.append(buildPicture(picture, (path) => setSelected(path)));
    }
  }, [picture]);
  const [table, setTable] = useState(false);
  const inputs = shown?.inputs ?? null;
  const rows = useMemo(() => (inputs === null ? null : inputRows(inputs)), [inputs]);

  const dim = shown !== null && (stale !== null || shown.stop !== at?.stop);
  const origin = shown && "renderer" in shown ? shown.renderer.origin : null;
  const remove =
    card.pinned === null
      ? null
      : () =>
          look((current) => {
            const d = (current.d ?? []).filter((_, index) => index !== card.pinned);
            const { d: _d, ...rest } = current;
            return d.length > 0 ? { ...rest, d } : rest;
          });

  return (
    <section
      ref={holder}
      className={`drawing ${dim ? "stale" : ""}`}
      aria-label={`Drawing of ${card.path}${card.chosen !== null ? ` as ${card.chosen}` : ""}`}
    >
      <header className="drawing-head">
        <span className="drawing-path">{card.path}</span>
        {offered && offered.length > 1 && card.chosen === null ? (
          <fieldset className="segmented small" aria-label={`Drawings of ${card.path}`}>
            {offered.map((name) => (
              <button
                key={name}
                type="button"
                aria-pressed={name === renderer}
                onClick={() => setTab(name)}
              >
                {name}
              </button>
            ))}
          </fieldset>
        ) : (
          <span className="drawing-renderer">{renderer ?? "…"}</span>
        )}
        {origin && <span className="muted">{originLabel(origin)}</span>}
        <span className="drawing-end">
          {shown && <span className="muted">stop #{shown.stop}</span>}
          {rows && (
            <>
              <button
                type="button"
                className="link-button"
                aria-pressed={table}
                onClick={() => setTable(!table)}
              >
                Table
              </button>
              <button type="button" className="link-button" onClick={() => void copyCsv(rows)}>
                Copy CSV
              </button>
            </>
          )}
          <DrawAs path={card.path} />
          {remove && (
            <button
              type="button"
              className="icon"
              aria-label={`Stop drawing ${card.path}`}
              title="Remove"
              onClick={remove}
            >
              ×
            </button>
          )}
        </span>
      </header>
      {offered !== null && offered.length === 0 && card.chosen === null ? (
        <div className="drawing-problem">
          No view draws {card.path}. Choose a renderer with Draw as…
        </div>
      ) : shown && "problem" in shown ? (
        <div className="drawing-problem" role="alert">
          {shown.problem}
        </div>
      ) : null}
      <div ref={body} className="drawing-body" hidden={!picture} />
      {shown && "live" in shown && (
        <LiveDrawing
          key={shown.live.source.digest}
          card={id}
          job={shown.live}
          active={visible}
          sandbox={sandbox}
          onSelect={setSelected}
        />
      )}
      {picture?.caption !== undefined && <div className="drawing-caption">{picture.caption}</div>}
      {!shown && <div className="drawing-wait muted">{live ? "Drawing…" : hidden(focus)}</div>}
      {table && rows && <InputTable rows={rows} label={`Inputs of ${card.path}`} />}
      {selected !== null && live && (
        <Selected
          at={live}
          path={partPath(card.path, selected)}
          onClose={() => setSelected(null)}
        />
      )}
    </section>
  );
}

/** A card's draws, one at a time, each started on an animation frame. The
 * latest job replaces any not yet started, so the stops that arrived
 * meanwhile are skipped, and a drawing never replaces one of a later stop.
 * A card whose width changes draws its last job again at the new width. */
function useDrawQueue(
  id: string,
  holder: RefObject<HTMLElement | null>,
  body: RefObject<HTMLElement | null>,
): { shown: Shown | null; draw(job: Job): void; fail(stop: number, problem: string): void } {
  const [shown, setShown] = useState<Shown | null>(null);
  // What `shown` holds, whose images are freed once it is replaced.
  const current = useRef<Shown | null>(null);
  const wanted = useRef<Job | null>(null);
  const drawing = useRef(false);
  const frame = useRef<number | null>(null);
  const latest = useRef(-1);
  const mounted = useRef(true);
  // The draw last started and the width it was offered.
  const started = useRef<Job | null>(null);
  const startedWidth = useRef(0);

  const queue = useMemo(() => {
    const show = (next: Shown) => {
      if (mounted.current && next.stop >= latest.current) {
        latest.current = next.stop;
        // The page drew the last picture's images already, if it ever will.
        free(current.current);
        current.current = next;
        setShown(next);
      } else {
        free(next);
      }
    };
    const pump = () => {
      if (drawing.current || frame.current !== null || wanted.current === null) {
        return;
      }
      frame.current = requestAnimationFrame(() => {
        frame.current = null;
        const job = wanted.current;
        wanted.current = null;
        if (job === null || !mounted.current) {
          return;
        }
        drawing.current = true;
        started.current = job;
        startedWidth.current = offeredWidth(holder.current, body.current);
        const context = contextFor(
          previousInputs(job.key, job.stop),
          job.paths,
          startedWidth.current,
        );
        void sandbox()
          .draw(id, job.source, job.inputs, context)
          .then((outcome) => {
            drawing.current = false;
            show(shownFor(job, outcome));
            pump();
          });
      });
    };
    return {
      pump,
      draw: (job: Job) => {
        wanted.current = job;
        pump();
      },
      fail: (stop: number, problem: string) => {
        wanted.current = null;
        show({ stop, problem, inputs: null });
      },
    };
  }, [id, holder, body]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      if (frame.current !== null) {
        cancelAnimationFrame(frame.current);
        frame.current = null;
      }
      free(current.current);
      current.current = null;
    };
  }, []);

  useEffect(() => {
    const element = holder.current;
    if (!element) {
      return;
    }
    const observer = new ResizeObserver(() => {
      const job = started.current;
      const width = offeredWidth(holder.current, body.current);
      if (
        job !== null &&
        wanted.current === null &&
        Math.abs(width - startedWidth.current) >= RESIZED
      ) {
        wanted.current = job;
        queue.pump();
      }
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [holder, body, queue]);

  return { shown, draw: queue.draw, fail: queue.fail };
}

/** Frees the images of a picture shown no more. */
function free(shown: Shown | null): void {
  if (shown !== null && "picture" in shown) {
    closeImages(shown.picture);
  }
}

/** Copies every input value as CSV, and says so. */
async function copyCsv(rows: InputRows): Promise<void> {
  const text = csv(rows);
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Pages served over plain HTTP to another machine have no clipboard
    // API, but can still copy a selection.
    const area = document.createElement("textarea");
    area.value = text;
    document.body.append(area);
    area.select();
    const copied = document.execCommand("copy");
    area.remove();
    if (!copied) {
      flash("The browser would not copy");
      return;
    }
  }
  flash(`Copied ${rows.count} ${rows.count === 1 ? "value" : "values"} as CSV`);
}

/** The height of one of the Table's rows, in pixels. */
const ROW = 22;
/** How many of the Table's rows show at once. */
const SHOWN_ROWS = 14;
/** The tallest the Table's rows may be together, well within what every
 * browser lays out: beyond it, scrolling moves through the rows in
 * proportion. */
const MOST_HEIGHT = 4_000_000;

/** Every input value, a path and its value per row, of which only the rows
 * scrolled to are in the page. */
function InputTable({ rows, label }: { rows: InputRows; label: string }) {
  const [scrolled, setScrolled] = useState(0);
  const height = Math.min(rows.count * ROW, MOST_HEIGHT);
  const shownCount = Math.min(rows.count, SHOWN_ROWS + 2);
  const room = Math.max(0, height - shownCount * ROW);
  const top = Math.min(scrolled, room);
  const first =
    rows.count * ROW <= MOST_HEIGHT
      ? Math.floor(top / ROW)
      : Math.round((top / Math.max(1, room)) * (rows.count - shownCount));
  const shown: number[] = [];
  for (let index = first; index < Math.min(rows.count, first + shownCount); index++) {
    shown.push(index);
  }
  const before = rows.count * ROW <= MOST_HEIGHT ? first * ROW : top;
  const after = Math.max(0, height - before - shown.length * ROW);
  return (
    <div
      className="drawing-table"
      style={{ maxHeight: ROW * (SHOWN_ROWS + 1) }}
      onScroll={(event) => setScrolled(event.currentTarget.scrollTop)}
    >
      <table aria-label={label} aria-rowcount={rows.count + 1}>
        <thead>
          <tr>
            <th>path</th>
            <th>value</th>
          </tr>
        </thead>
        <tbody>
          {before > 0 && <tr style={{ height: before }} />}
          {shown.map((index) => {
            const [path, value] = rows.row(index);
            return (
              <tr key={index} aria-rowindex={index + 2} style={{ height: ROW }}>
                <td>{path}</td>
                <td>{value}</td>
              </tr>
            );
          })}
          {after > 0 && <tr style={{ height: after }} />}
        </tbody>
      </table>
    </div>
  );
}

/** Where a renderer comes from, as a card names it. */
function originLabel(origin: string): string {
  return origin === "built-in" ? "built-in" : (origin.split("/").at(-1) ?? origin);
}

/** A part of the drawn value, as a row, after a click on its shape. Focus
 * moves into it, and back to where it was when it closes. */
export function Selected({ at, path, onClose }: { at: At; path: string; onClose(): void }) {
  const value = useRequest("evaluate", { ...at, expression: path });
  const look = useLook();
  const row: Row | undefined = value.data;
  const dialog = useRef<HTMLDivElement>(null);
  const opener = useRef<Element | null>(null);
  useEffect(() => {
    opener.current = document.activeElement;
    dialog.current?.focus();
  }, []);
  const close = () => {
    const back = opener.current;
    onClose();
    if ((back instanceof HTMLElement || back instanceof SVGElement) && back.isConnected) {
      back.focus();
    }
  };
  return (
    <div
      ref={dialog}
      className="drawing-selected"
      role="dialog"
      aria-label={`Part ${path}`}
      tabIndex={-1}
      // Escape closes it rather than leave the view.
      data-keys="escape"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          close();
        }
      }}
    >
      <div className="drawing-selected-head">
        <span className="drawing-path">{path}</span>
        <button
          type="button"
          className="link-button"
          onClick={() => {
            look((current) => ({ ...current, w: [...(current.w ?? []), path] }));
            flash(`Watching ${path}`);
          }}
        >
          Watch
        </button>
        <button type="button" className="icon" aria-label="Close" onClick={close}>
          ×
        </button>
      </div>
      <LocalExpansion>
        <ul className="value-list">
          {value.error ? (
            <li className="empty error">{value.error.message}</li>
          ) : row ? (
            <ValueRow row={row} parent="drawing" depth={0} />
          ) : (
            <li className="empty">Reading…</li>
          )}
        </ul>
      </LocalExpansion>
    </div>
  );
}

/** Draws a value with any renderer, pinning it to the drawings. */
export function DrawAs({ path }: { path: string }) {
  const [open, setOpen] = useState(false);
  const renderers = useRequest("renderers", open ? undefined : null);
  const look = useLook();
  const anchor = useRef<HTMLSpanElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const loaded = renderers.data !== undefined;
  const items = () => [...(menu.current?.querySelectorAll<HTMLElement>("[role=menuitem]") ?? [])];
  const close = () => {
    setOpen(false);
    button.current?.focus();
  };
  const choose = (pinned: Pinned) => {
    close();
    look(pin(pinned), { replace: false });
  };

  // Focus moves to the first renderer once the list arrives.
  useEffect(() => {
    if (open && loaded) {
      menu.current?.querySelector<HTMLElement>("[role=menuitem]")?.focus();
    }
  }, [open, loaded]);

  // A click elsewhere puts it away.
  useEffect(() => {
    if (!open) {
      return;
    }
    const pointer = (event: PointerEvent) => {
      if (!anchor.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    window.addEventListener("pointerdown", pointer);
    return () => window.removeEventListener("pointerdown", pointer);
  }, [open]);

  const move = (event: ReactKeyboardEvent) => {
    const all = items();
    const at = all.indexOf(document.activeElement as HTMLElement);
    const to =
      event.key === "ArrowDown"
        ? (at + 1) % all.length
        : event.key === "ArrowUp"
          ? (at - 1 + all.length) % all.length
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? all.length - 1
              : null;
    if (event.key === "Escape") {
      event.preventDefault();
      close();
    } else if (event.key === "Tab") {
      setOpen(false);
    } else if (to !== null && all.length > 0) {
      event.preventDefault();
      all[to]?.focus();
    }
  };

  return (
    <span className="anchor" ref={anchor}>
      <button
        ref={button}
        type="button"
        className="link-button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Draw ${path} as…`}
        onClick={() => setOpen(!open)}
        onKeyDown={(event) => {
          if (open && event.key === "Escape") {
            event.preventDefault();
            setOpen(false);
          }
        }}
      >
        Draw as…
      </button>
      {open && (
        <div
          ref={menu}
          className="popover menu"
          role="menu"
          aria-label={`Draw ${path} as`}
          // Escape closes it rather than leave the view.
          data-keys="escape"
          onKeyDown={move}
        >
          {renderers.error && <div className="empty error">{renderers.error.message}</div>}
          {!renderers.data && !renderers.error && <div className="empty">Reading…</div>}
          {renderers.data?.renderers.map((renderer) => (
            <button
              key={renderer.name}
              type="button"
              role="menuitem"
              tabIndex={-1}
              className="menu-item"
              onClick={() => choose({ path, renderer: renderer.name })}
            >
              <span>{renderer.name}</span>
              <span className="muted">{originLabel(renderer.origin)}</span>
            </button>
          ))}
        </div>
      )}
    </span>
  );
}

/** The CSS pixels a card's body offers a picture. It is measured on the
 * card, because the body stays hidden until it has a picture. */
function offeredWidth(card: HTMLElement | null, body: HTMLElement | null): number {
  if (card === null || body === null) {
    return 600;
  }
  const style = getComputedStyle(body);
  const padding = Number.parseFloat(style.paddingLeft) + Number.parseFloat(style.paddingRight);
  return Math.floor(card.clientWidth - padding) || 600;
}
