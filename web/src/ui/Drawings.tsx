import {
  createEffect,
  createMemo,
  createSignal,
  createUniqueId,
  For,
  Match,
  onSettled,
  Show,
  Switch,
  untrack,
} from "solid-js";

import { assert } from "../assert";
import { copyText } from "../clipboard";
import { useRequest } from "../data";
import { type At, drawing, formatPinned, type Look, type Pinned, parsePinned } from "../focus";
import type { Drawing, Row } from "../protocol";
import { shallowEqual } from "../state";
import { flash } from "../tab";
import { serverUrl } from "../target";
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

export interface Card {
  key: string;
  path: string;

  offered: string[] | null;

  chosen: string | null;

  pinned: number | null;
}

let shared: Sandbox | null = null;

let reveal: string | null = null;

export function pin(pinned: Pinned): (look: Look) => Look {
  reveal = formatPinned(pinned);
  return (look) => drawing(look, pinned);
}

export function sandbox(): Sandbox {
  if (shared?.spent) {
    shared.dispose();
    shared = null;
  }
  shared ??= new Sandbox(serverUrl("visualizer-frame"));
  return shared;
}

export function useCards(): () => Card[] {
  const focus = useFocus();
  const why = createMemo(() => hidden(focus()));
  const scopes = useRequest("scopes", () => {
    const current = focus();
    const reason = why();
    assert(reason !== null || current.at !== null, "values shown with no stop");
    return reason === null ? current.at : null;
  });

  // Keep the previous card list while running or refetching scopes so cards do not unmount
  // and lose renderer/canvas state between stops.
  let last: Card[] = [];
  return createMemo(() => {
    const data = scopes().data;
    const pins = focus().look.d;
    const reason = why();
    if ((reason !== null || data === undefined) && last.length > 0) {
      return last;
    }
    const cards: Card[] = [];
    const seen = new Set<string>();
    for (const scope of data?.scopes ?? []) {
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
        const card = cards.find((each) => each.key === key);
        assert(card !== undefined, `pin ${index} seen with no card`);
        if (card.pinned === null) {
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
    assert(seen.size === cards.length, `${cards.length} cards for ${seen.size} keys`);
    last = cards;
    return cards;
  });
}

export function Drawings() {
  const focus = useFocus();
  const cards = useCards();
  const why = createMemo(() => hidden(focus(), "Drawings"));

  return (
    <Show
      when={cards().length > 0}
      fallback={
        <div class="drawings">
          <div class="empty center-message">
            <Show
              when={why()}
              fallback={
                <>
                  No value here has a drawing. Draw one from its row, or give its type a{" "}
                  <code>visualize</code> in a view.
                </>
              }
            >
              {(reason) => <>{reason()}</>}
            </Show>
          </div>
        </div>
      }
    >
      <div class="drawings" data-testid="drawings">
        <For each={cards()} keyed={(card) => card.key}>
          {(card) => <DrawingCard card={card()} />}
        </For>
      </div>
    </Show>
  );
}

type Shown =
  | { stop: number; picture: Picture; renderer: Drawing["renderer"]; inputs: Inputs }
  | { stop: number; live: LiveJob; renderer: Drawing["renderer"]; inputs: Inputs }
  | { stop: number; problem: string; inputs: Inputs | null };

type Inputs = Record<string, Value>;

interface Job {
  stop: number;
  key: string;
  source: RendererCode;
  renderer: Drawing["renderer"];
  inputs: Inputs;
  paths: Record<string, string | null>;
}

const RESIZED = 8;

const tabs = new Map<string, string>();

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
      return `The renderer took longer than ${DRAW_TIMEOUT / 1_000} s, so microscope stopped it.`;
    case "failed":
      return failureText(outcome.failure);
    case "picture":
    case "live":
      return null;
  }
}

const liveJobs = new WeakMap<Job, LiveJob>();

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
  assert(outcome.kind === "picture", `draw outcome ${outcome.kind} unhandled`);
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

function partPath(path: string, select: string): string {
  const whole = /^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*|\[\d+\])*$/u.test(path)
    ? path
    : `(${path})`;
  return select.startsWith("[") ? `${whole}${select}` : `${whole}.${select}`;
}

function DrawingCard(props: { card: Card }) {
  const focus = useFocus();
  const live = createMemo(() => {
    const { at, stale } = focus();
    return at !== null && stale === null ? at : null;
  });
  const look = useLook();
  const theme = useShownScheme();
  const id = createUniqueId();
  let holder: HTMLElement | undefined;
  let body: HTMLDivElement | undefined;
  const [visible, setVisible] = createSignal(false);
  const [selected, setSelected] = createSignal<string | null>(null);
  const { shown, draw, fail } = useDrawQueue(
    id,
    () => holder ?? null,
    () => body ?? null,
  );

  const evaluated = useRequest("evaluate", () => {
    const currentLive = live();
    return props.card.offered === null && props.card.chosen === null && currentLive
      ? { ...currentLive, expression: props.card.path }
      : null;
  });
  const offered = createMemo(() => props.card.offered ?? evaluated().data?.drawings ?? null);
  const tabKey = createMemo(() => `${focus().session}~${props.card.path}`);
  const [tab, setTabState] = createSignal<string | null>(untrack(() => tabs.get(tabKey()) ?? null));
  const setTab = (name: string) => {
    tabs.set(tabKey(), name);
    setTabState(name);
  };
  const renderer = createMemo(() => {
    const currentTab = tab();
    const currentOffered = offered();
    return (
      props.card.chosen ??
      (currentTab !== null && currentOffered?.includes(currentTab)
        ? currentTab
        : (currentOffered?.[0] ?? null))
    );
  });

  const asked = useRequest("draw", () => {
    const currentLive = live();
    const currentRenderer = renderer();
    return currentLive && currentRenderer !== null && visible()
      ? { ...currentLive, path: props.card.path, renderer: currentRenderer }
      : null;
  });
  const code = useRequest("renderer", () => {
    const currentAsked = asked();
    return currentAsked.data && currentAsked.current
      ? { digest: currentAsked.data.renderer.digest }
      : null;
  });

  const cardKey = createMemo(() => props.card.key);

  createEffect(cardKey, (key) => {
    const element = holder;
    assert(element !== undefined, "drawing card mounted without its element");
    if (reveal === key) {
      reveal = null;
      element.scrollIntoView({ block: "nearest" });
    }
    const observer = new IntersectionObserver((entries) => {
      setVisible(entries.some((entry) => entry.isIntersecting));
    });
    observer.observe(element);
    return () => observer.disconnect();
  });

  onSettled(() => () => shared?.release(id));

  const drawInput = createMemo(
    () => {
      const stop = live()?.stop ?? null;
      const currentAsked = asked();
      const currentCode = code();
      const currentEvaluated = evaluated();
      const drawingData = currentAsked.current ? currentAsked.data : undefined;
      const source = currentCode.current ? currentCode.data : undefined;
      let problem: string | undefined;
      if (currentEvaluated.error) {
        problem = `Cannot evaluate ${props.card.path}: ${currentEvaluated.error.message}`;
      } else if (currentAsked.error) {
        problem = currentAsked.error.message;
      } else if (drawingData && currentCode.error) {
        problem = `Cannot load renderer ${drawingData.renderer.name}: ${currentCode.error.message}`;
      }
      return {
        stop,
        drawingData,
        source,
        problem,
        theme: theme(),
        path: props.card.path,
        session: focus().session,
      };
    },
    { equals: shallowEqual },
  );

  createEffect(
    () => drawInput(),
    ({ stop, drawingData, source, problem, path, session }) => {
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
        key: `${session}~${path}~${drawingData.renderer.name}`,
        source,
        renderer: drawingData.renderer,
        inputs,
        paths: inputPaths(drawingData.inputs),
      });
    },
  );

  const picture = createMemo(() => {
    const current = shown();
    return current && "picture" in current ? current.picture : null;
  });

  createEffect(
    () => picture(),
    (pic) => {
      const element = body;
      assert(element !== undefined, "drawing card mounted without its body");
      element.replaceChildren();
      if (pic) {
        element.append(buildPicture(pic, (path) => setSelected(path)));
      }
    },
  );

  const [table, setTable] = createSignal(false);
  const inputs = createMemo(() => shown()?.inputs ?? null);
  const rows = createMemo(() => {
    const current = inputs();
    return current === null ? null : inputRows(current);
  });

  const dim = createMemo(() => {
    const current = shown();
    const { at, stale } = focus();
    return current !== null && (stale !== null || current.stop !== at?.stop);
  });
  const origin = createMemo(() => {
    const current = shown();
    return current && "renderer" in current ? current.renderer.origin : null;
  });
  const remove = () => {
    const pinned = props.card.pinned;
    if (pinned === null) {
      return;
    }
    look((current) => {
      const d = (current.d ?? []).filter((_, index) => index !== pinned);
      const { d: _d, ...rest } = current;
      return d.length > 0 ? { ...rest, d } : rest;
    });
  };
  const toggleTable = () => setTable((open) => !open);
  const copy = () => {
    const current = rows();
    if (current) {
      void copyCsv(current);
    }
  };
  const deselect = () => setSelected(null);

  const problemText = createMemo(() => {
    const current = shown();
    return current && "problem" in current ? current.problem : null;
  });
  const liveJob = createMemo(() => {
    const current = shown();
    return current && "live" in current ? current.live : null;
  });
  const liveDigest = createMemo(() => liveJob()?.source.digest ?? null);
  const tabsOffered = createMemo(() => {
    const list = offered();
    return list && list.length > 1 && props.card.chosen === null ? list : null;
  });
  const selectedTarget = createMemo(() => {
    const at = live();
    const part = selected();
    return at && part !== null ? { at, select: part } : null;
  });

  return (
    <section
      ref={(node) => {
        holder = node;
      }}
      class={["drawing", { stale: dim() }]}
      aria-label={`Drawing of ${props.card.path}${props.card.chosen === null ? "" : ` as ${props.card.chosen}`}`}
    >
      <header class="drawing-head">
        <span class="drawing-path">{props.card.path}</span>
        <Show
          when={tabsOffered()}
          fallback={<span class="drawing-renderer">{renderer() ?? "…"}</span>}
        >
          {(names) => (
            <fieldset class="segmented small" aria-label={`Drawings of ${props.card.path}`}>
              <For each={names()}>
                {(name) => (
                  <RendererTab name={name} pressed={name === renderer()} choose={setTab} />
                )}
              </For>
            </fieldset>
          )}
        </Show>
        <Show when={origin()}>{(src) => <span class="muted">{originLabel(src())}</span>}</Show>
        <span class="drawing-end">
          <Show when={shown()}>
            {(current) => <span class="muted">stop #{current().stop}</span>}
          </Show>
          <Show when={rows()}>
            <button
              type="button"
              class="link-button"
              aria-pressed={table() ? "true" : "false"}
              onClick={toggleTable}
            >
              Table
            </button>
            <button type="button" class="link-button" onClick={copy}>
              Copy CSV
            </button>
          </Show>
          <DrawAs name={props.card.path} path={props.card.path} memory={null} />
          <Show when={props.card.pinned !== null}>
            <button
              type="button"
              class="icon"
              aria-label={`Stop drawing ${props.card.path}`}
              title="Remove"
              onClick={remove}
            >
              ×
            </button>
          </Show>
        </span>
      </header>
      <Switch>
        <Match when={offered()?.length === 0 && props.card.chosen === null}>
          <div class="drawing-problem">
            No view draws {props.card.path}. Choose a renderer from Draw as…, the pencil.
          </div>
        </Match>
        <Match when={problemText()}>
          {(problem) => (
            <div class="drawing-problem" role="alert">
              {problem()}
            </div>
          )}
        </Match>
      </Switch>
      <div
        ref={(node) => {
          body = node;
        }}
        class="drawing-body"
        hidden={!picture()}
      />
      <Show when={liveDigest()} keyed>
        {(_digest) => (
          <Show when={liveJob()}>
            {(job) => (
              <LiveDrawing
                card={id}
                job={job()}
                active={visible()}
                sandbox={sandbox}
                onSelect={setSelected}
              />
            )}
          </Show>
        )}
      </Show>
      <Show when={picture()?.caption !== undefined}>
        <div class="drawing-caption">{picture()?.caption}</div>
      </Show>
      <Show when={!shown()}>
        <div class="drawing-wait muted">{live() ? "Drawing…" : hidden(focus())}</div>
      </Show>
      <Show when={table() ? rows() : null}>
        {(currentRows) => (
          <InputTable rows={currentRows()} label={`Inputs of ${props.card.path}`} />
        )}
      </Show>
      <Show when={selectedTarget()}>
        {(current) => (
          <Selected
            at={current().at}
            path={partPath(props.card.path, current().select)}
            onClose={deselect}
          />
        )}
      </Show>
    </section>
  );
}

function RendererTab(props: { name: string; pressed: boolean; choose: (name: string) => void }) {
  return (
    <button
      type="button"
      aria-pressed={props.pressed ? "true" : "false"}
      onClick={() => props.choose(props.name)}
    >
      {props.name}
    </button>
  );
}

/**
 * Coalesces rapid stop/resize updates to at most one in-flight sandbox draw per card, drops
 * out-of-order results (`next.stop >= latest`), and closes superseded ImageBitmaps via `free()`.
 */
function useDrawQueue(
  id: string,
  holder: () => HTMLElement | null,
  body: () => HTMLElement | null,
): {
  shown: () => Shown | null;
  draw(job: Job): void;
  fail(stop: number, problem: string): void;
} {
  const [shown, setShown] = createSignal<Shown | null>(null);

  let current: Shown | null = null;
  let wanted: Job | null = null;
  let busy = false;
  let frame: number | null = null;
  let latest = -1;
  let mounted = true;
  let started: Job | null = null;
  let startedWidth = 0;

  const show = (next: Shown) => {
    if (mounted && next.stop >= latest) {
      latest = next.stop;
      free(current);
      current = next;
      setShown(next);
    } else {
      free(next);
    }
  };

  const pump = () => {
    if (busy || frame !== null || wanted === null) {
      return;
    }
    frame = requestAnimationFrame(() => {
      frame = null;
      const job = wanted;
      wanted = null;
      if (job === null || !mounted) {
        return;
      }
      assert(!busy, `card ${id} starts a draw with one running`);
      busy = true;
      started = job;
      startedWidth = offeredWidth(holder(), body());
      const context = contextFor(previousInputs(job.key, job.stop), job.paths, startedWidth);
      const finish = async () => {
        const outcome = await sandbox().draw(id, job.source, job.inputs, context);
        assert(busy, `card ${id} finished a draw it did not start`);
        busy = false;
        show(shownFor(job, outcome));
        pump();
      };
      void finish();
    });
  };

  const draw = (job: Job) => {
    wanted = job;
    pump();
  };

  const fail = (stop: number, problem: string) => {
    wanted = null;
    show({ stop, problem, inputs: null });
  };

  onSettled(() => {
    mounted = true;
    const element = holder();
    assert(element !== null, "draw queue mounted without its card element");
    const observer = new ResizeObserver(() => {
      const job = started;
      const width = offeredWidth(holder(), body());
      if (job !== null && wanted === null && Math.abs(width - startedWidth) >= RESIZED) {
        wanted = job;
        pump();
      }
    });
    observer.observe(element);
    return () => {
      observer.disconnect();
      mounted = false;
      if (frame !== null) {
        cancelAnimationFrame(frame);
        frame = null;
      }
      free(current);
      current = null;
    };
  });

  return { shown, draw, fail };
}

function free(shown: Shown | null): void {
  if (shown !== null && "picture" in shown) {
    closeImages(shown.picture);
  }
}

async function copyCsv(rows: InputRows): Promise<void> {
  if (!(await copyText(csv(rows)))) {
    flash("The browser would not copy");
    return;
  }
  flash(`Copied ${rows.count} ${rows.count === 1 ? "value" : "values"} as CSV`);
}

const ROW = 22;

const SHOWN_ROWS = 14;

const MOST_HEIGHT = 4_000_000;
const TABLE_STYLE = { "max-height": `${ROW * (SHOWN_ROWS + 1)}px` };
const ROW_STYLE = { height: `${ROW}px` };

/**
 * Virtualized table for renderer inputs. Clamps total scroll height to `MOST_HEIGHT` and switches
 * to proportional scroll mapping above that cap to stay within browser element height limits.
 */
function InputTable(props: { rows: InputRows; label: string }) {
  const [scrolled, setScrolled] = createSignal(0);

  const layout = createMemo(() => {
    const count = props.rows.count;
    const height = Math.min(count * ROW, MOST_HEIGHT);
    const shownCount = Math.min(count, SHOWN_ROWS + 2);
    const room = Math.max(0, height - shownCount * ROW);
    const top = Math.max(0, Math.min(scrolled(), room));
    const first =
      count * ROW <= MOST_HEIGHT
        ? Math.floor(top / ROW)
        : Math.round((top / Math.max(1, room)) * (count - shownCount));
    assert(
      first <= count - shownCount,
      `table starts at row ${first} of ${count}, showing ${shownCount}`,
    );
    const shown: number[] = [];
    for (let index = first; index < Math.min(count, first + shownCount); index++) {
      shown.push(index);
    }
    const before = count * ROW <= MOST_HEIGHT ? first * ROW : top;
    const after = Math.max(0, height - before - shown.length * ROW);
    return { shown, before, after };
  });

  return (
    <div
      class="drawing-table"
      style={TABLE_STYLE}
      onScroll={(event) => setScrolled(event.currentTarget.scrollTop)}
    >
      <table aria-label={props.label} aria-rowcount={props.rows.count + 1}>
        <thead>
          <tr>
            <th>path</th>
            <th>value</th>
          </tr>
        </thead>
        <tbody>
          <Show when={layout().before > 0}>
            <tr style={{ height: `${layout().before}px` }} />
          </Show>
          <For each={layout().shown}>
            {(index) => {
              const pair = createMemo(() => props.rows.row(index));
              return (
                <tr aria-rowindex={index + 2} style={ROW_STYLE}>
                  <td>{pair()[0]}</td>
                  <td>{pair()[1]}</td>
                </tr>
              );
            }}
          </For>
          <Show when={layout().after > 0}>
            <tr style={{ height: `${layout().after}px` }} />
          </Show>
        </tbody>
      </table>
    </div>
  );
}

function originLabel(origin: string): string {
  return origin === "built-in" ? "built-in" : (origin.split("/").at(-1) ?? origin);
}

export function Selected(props: { at: At; path: string; onClose(): void }) {
  const value = useRequest("evaluate", () => ({ ...props.at, expression: props.path }));
  const look = useLook();
  const row = createMemo((): Row | undefined => value().data);
  let dialog: HTMLDivElement | undefined;
  let opener: Element | null = null;

  onSettled(() => {
    opener = document.activeElement;
    dialog?.focus();
  });

  const close = () => {
    const back = opener;
    props.onClose();
    if ((back instanceof HTMLElement || back instanceof SVGElement) && back.isConnected) {
      back.focus();
    }
  };

  const escape = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      close();
    }
  };

  const watch = () => {
    const path = props.path;
    look((current) => ({ ...current, w: [...(current.w ?? []), path] }));
    flash(`Watching ${path}`);
  };

  return (
    // oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- Escape anywhere in the dialog closes it
    <div
      ref={(node) => {
        dialog = node;
      }}
      class="drawing-selected"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a non-modal panel in the card, which a dialog element would not be
      role="dialog"
      aria-label={`Part ${props.path}`}
      tabindex={-1}
      data-keys="escape"
      onKeyDown={escape}
    >
      <div class="drawing-selected-head">
        <span class="drawing-path">{props.path}</span>
        <button type="button" class="link-button" onClick={watch}>
          Watch
        </button>
        <button type="button" class="icon" aria-label="Close" onClick={close}>
          ×
        </button>
      </div>
      <LocalExpansion>
        <ul class="value-list">
          <Switch fallback={<li class="empty">Reading…</li>}>
            <Match when={value().error}>
              {(err) => <li class="empty error">{err().message}</li>}
            </Match>
            <Match when={row()}>
              {(item) => <ValueRow row={item()} parent="drawing" depth={0} />}
            </Match>
          </Switch>
        </ul>
      </LocalExpansion>
    </div>
  );
}

export function DrawAs(props: { name: string; path: string | null; memory: (() => void) | null }) {
  const [open, setOpen] = createSignal(false);
  const renderers = useRequest("renderers", () =>
    open() && props.path !== null ? undefined : null,
  );
  const look = useLook();
  let anchor: HTMLSpanElement | undefined;
  let button: HTMLButtonElement | undefined;
  let menu: HTMLDivElement | undefined;
  const loaded = createMemo(() => renderers().data !== undefined || props.memory !== null);

  const close = () => {
    setOpen(false);
    button?.focus();
  };

  const choose = (pinned: Pinned) => {
    close();
    look(pin(pinned), { replace: false });
  };

  createEffect(
    () => open() && loaded(),
    (ready) => {
      if (ready) {
        menu?.querySelector<HTMLElement>("[role=menuitem]")?.focus();
      }
    },
  );

  const pointer = (event: PointerEvent) => {
    if (!anchor?.contains(event.target as Node)) {
      setOpen(false);
    }
  };

  createEffect(
    () => open(),
    (isOpen) => {
      if (!isOpen) {
        return;
      }
      window.addEventListener("pointerdown", pointer);
      return () => window.removeEventListener("pointerdown", pointer);
    },
  );

  const move = (event: KeyboardEvent) => {
    const all = [...(menu?.querySelectorAll<HTMLElement>("[role=menuitem]") ?? [])];
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

  const toggle = () => setOpen((current) => !current);

  const escape = (event: KeyboardEvent) => {
    if (open() && event.key === "Escape") {
      event.preventDefault();
      setOpen(false);
    }
  };

  const targetPath = createMemo(() => (props.path === null ? null : { path: props.path }));

  return (
    <span
      class="anchor"
      ref={(node) => {
        anchor = node;
      }}
    >
      <button
        ref={(node) => {
          button = node;
        }}
        type="button"
        class="icon"
        aria-haspopup="menu"
        aria-expanded={open() ? "true" : "false"}
        aria-label={`Draw ${props.path ?? props.name} as…`}
        title="Draw as…"
        onClick={toggle}
        onKeyDown={escape}
      >
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <path
            d="M10.5 2.5l3 3L5.5 13.5H2.5v-3l8-8ZM9 4l3 3"
            fill="none"
            stroke="currentColor"
            stroke-width="1.3"
            stroke-linejoin="round"
          />
        </svg>
      </button>
      <Show when={open()}>
        {/* oxlint-disable-next-line jsx-a11y/interactive-supports-focus -- focus stays on its items */}
        <div
          ref={(node) => {
            menu = node;
          }}
          class="popover menu"
          role="menu"
          aria-label={`Draw ${props.path ?? props.name} as`}
          data-keys="escape"
          onKeyDown={move}
        >
          <Show when={props.memory}>{(show) => <MemoryItem show={show()} close={close} />}</Show>
          <Show when={targetPath()}>
            {(target) => (
              <>
                <Show when={renderers().error}>
                  {(err) => <div class="empty error">{err().message}</div>}
                </Show>
                <Show when={!renderers().data && !renderers().error}>
                  <div class="empty">Reading…</div>
                </Show>
                <Show when={renderers().data?.renderers}>
                  {(list) => (
                    <For each={list()} keyed={(renderer) => renderer.name}>
                      {(renderer) => (
                        <RendererItem
                          path={target().path}
                          name={renderer().name}
                          origin={renderer().origin}
                          choose={choose}
                        />
                      )}
                    </For>
                  )}
                </Show>
              </>
            )}
          </Show>
        </div>
      </Show>
    </span>
  );
}

function MemoryItem(props: { show: () => void; close: () => void }) {
  const click = () => {
    props.close();
    props.show();
  };
  return (
    <button type="button" role="menuitem" tabindex={-1} class="menu-item" onClick={click}>
      <span>Memory</span>
      <span class="muted">hex</span>
    </button>
  );
}

function RendererItem(props: {
  path: string;
  name: string;
  origin: string;
  choose: (pinned: Pinned) => void;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      tabindex={-1}
      class="menu-item"
      onClick={() => props.choose({ path: props.path, renderer: props.name })}
    >
      <span>{props.name}</span>
      <span class="muted">{originLabel(props.origin)}</span>
    </button>
  );
}

function offeredWidth(card: HTMLElement | null, body: HTMLElement | null): number {
  if (card === null || body === null) {
    return 600;
  }
  const style = getComputedStyle(body);
  // oxlint-disable-next-line unicorn/prefer-number-coercion -- the padding ends in px, which Number rejects
  const padding = Number.parseFloat(style.paddingLeft) + Number.parseFloat(style.paddingRight);
  return Math.floor(card.clientWidth - padding) || 600;
}
