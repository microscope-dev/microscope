// A live renderer's canvas: the frames its worker draws, shown while the
// card is on screen. The page asks for each frame on an animation frame,
// one at a time, and only when the renderer wants one; it forwards the
// pointer, and the keys while the canvas has focus, as numbers and flags.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Tip } from "../visualize/draw";
import { isSelect } from "../visualize/picture";
import {
  contextFor,
  failureText,
  type LiveContext,
  type LiveFailure,
  type LivePointer,
  type LiveSession,
  type RendererCode,
  type Sandbox,
  UNRESPONSIVE,
} from "../visualize/sandbox";

/** What a live card draws: one stop's inputs, and the stop's before. */
export interface LiveJob {
  stop: number;
  source: RendererCode;
  inputs: unknown;
  paths: Record<string, string | null>;
  previous: unknown;
}

/** The shortest and tallest a live canvas is, in CSS pixels. */
const SHORTEST = 240;
const TALLEST = 560;
/** The widest or tallest a live canvas's pixels are. */
const MOST_PIXELS = 4096;

interface Size {
  css: { width: number; height: number };
  device: { width: number; height: number };
}

function measure(holder: HTMLElement): Size {
  const width = Math.max(1, holder.clientWidth);
  const height = Math.min(TALLEST, Math.max(SHORTEST, Math.round(width * 0.6)));
  const scale = Math.min(window.devicePixelRatio || 1, 2);
  return {
    css: { width, height },
    device: {
      width: Math.min(MOST_PIXELS, Math.round(width * scale)),
      height: Math.min(MOST_PIXELS, Math.round(height * scale)),
    },
  };
}

function liveContext(job: LiveJob, size: Size): LiveContext {
  return { ...contextFor(job.previous, job.paths, size.css.width), height: size.css.height };
}

function liveProblem(failure: LiveFailure): string {
  switch (failure.kind) {
    case "unresponsive":
      return `The renderer stopped answering for ${UNRESPONSIVE / 1_000} s, so it was stopped.`;
    case "lost":
      return "The renderer lost its WebGL context.";
    case "failed":
      return failureText(failure.failure);
  }
}

/** Keys a renderer never gets: the debugger's function keys, and Escape,
 * which gives focus back to the page. */
const KEPT = /^(?:F\d+|Escape|Tab)$/;

export function LiveDrawing({
  card,
  job,
  active,
  sandbox,
  onSelect,
}: {
  /** The card's id, which names its worker. */
  card: string;
  job: LiveJob;
  /** Whether the card is on screen: the renderer runs only then. */
  active: boolean;
  sandbox: () => Sandbox;
  onSelect(path: string): void;
}) {
  const holder = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [caption, setCaption] = useState<string | null>(null);
  const [failure, setFailure] = useState<LiveFailure | null>(null);
  const [waiting, setWaiting] = useState(false);
  const [restarts, setRestarts] = useState(0);
  const [height, setHeight] = useState(SHORTEST);
  const session = useRef<LiveSession | null>(null);
  const latest = useRef(job);
  latest.current = job;
  const select = useRef(onSelect);
  select.current = onSelect;
  // The job the session last had, so a new one is sent once.
  const sent = useRef<LiveJob | null>(null);
  const size = useRef<Size | null>(null);
  const tip = useRef<Tip | null>(null);
  const pointer = useRef<MouseEvent | null>(null);
  // The frame loop: one frame in flight, asked for on an animation frame
  // when the renderer wants one or animates.
  const loop = useRef({
    started: false,
    wanted: false,
    animating: false,
    inFlight: false,
    request: null as number | null,
    // What the frame in flight shows: the stop, and how many pointer and
    // key events the renderer had by then, which the canvas then names.
    asked: { stop: -1, events: 0 },
  });
  // The sessions started, and the events forwarded in this one.
  const starts = useRef(0);
  const events = useRef(0);

  const schedule = () => {
    const state = loop.current;
    if (!state.started || state.inFlight || state.request !== null) {
      return;
    }
    if (!state.wanted && !state.animating) {
      return;
    }
    state.request = requestAnimationFrame((time) => {
      state.request = null;
      if (!state.started || (!state.wanted && !state.animating)) {
        return;
      }
      state.wanted = false;
      state.inFlight = true;
      state.asked = { stop: sent.current?.stop ?? -1, events: events.current };
      session.current?.frame(time);
    });
  };
  const want = () => {
    loop.current.wanted = true;
    schedule();
  };

  // The canvas takes its size from the card's width as soon as it is laid
  // out, session or not, so the cards around it do not move later; a new
  // size reaches a running renderer.
  // biome-ignore lint/correctness/useExhaustiveDependencies: want reads only refs
  useLayoutEffect(() => {
    const element = holder.current;
    if (element === null) {
      return;
    }
    const resized = () => {
      const next = measure(element);
      const before = size.current;
      size.current = next;
      setHeight(next.css.height);
      // Its pixels too, until a frame sizes them: sizing a canvas clears
      // it, so later a frame of the new size replaces the last at once.
      const shown = canvas.current;
      if (shown !== null && shown.dataset.stop === undefined) {
        shown.width = next.device.width;
        shown.height = next.device.height;
      }
      if (
        before !== null &&
        (before.device.width !== next.device.width || before.device.height !== next.device.height)
      ) {
        session.current?.resize(next.device.width, next.device.height);
        want();
      }
    };
    resized();
    const observer = new ResizeObserver(resized);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  // The session runs while the card is on screen and ends when it leaves;
  // coming back starts it afresh with the inputs of then.
  // biome-ignore lint/correctness/useExhaustiveDependencies: the session reads its job and callbacks through refs; a new renderer is a new component
  useEffect(() => {
    const element = holder.current;
    const shown = canvas.current;
    if (!active || failure !== null || element === null || shown === null) {
      return;
    }
    const measured = size.current ?? measure(element);
    const state = loop.current;
    state.started = false;
    state.wanted = false;
    state.animating = false;
    state.inFlight = false;
    const first = latest.current;
    sent.current = first;
    events.current = 0;
    starts.current += 1;
    // The canvas says which session, stop, and events its frame shows.
    shown.dataset.session = String(starts.current);
    shown.dataset.sent = "0";
    delete shown.dataset.stop;
    delete shown.dataset.seen;
    const bitmaps = shown.getContext("bitmaprenderer");
    const live = sandbox().live(
      card,
      first.source,
      first.inputs,
      liveContext(first, measured),
      measured.device,
      {
        waiting: () => setWaiting(true),
        started: () => {
          setWaiting(false);
          state.started = true;
          want();
        },
        frame: (bitmap) => {
          state.inFlight = false;
          if (bitmap !== null) {
            if (shown.width !== bitmap.width || shown.height !== bitmap.height) {
              shown.width = bitmap.width;
              shown.height = bitmap.height;
            }
            bitmaps?.transferFromImageBitmap(bitmap);
          }
          shown.dataset.stop = String(state.asked.stop);
          shown.dataset.seen = String(state.asked.events);
          schedule();
        },
        redraw: want,
        animate: (on) => {
          state.animating = on;
          schedule();
        },
        caption: (text) => setCaption(text),
        hint: (text) => {
          if (text === null || pointer.current === null) {
            tip.current?.hide();
          } else {
            tip.current?.show(text, pointer.current);
          }
        },
        select: (path) => {
          if (isSelect(path)) {
            select.current(path);
          }
        },
        failed: (why) => {
          state.started = false;
          setWaiting(false);
          setFailure(why);
        },
      },
    );
    session.current = live;
    return () => {
      if (state.request !== null) {
        cancelAnimationFrame(state.request);
        state.request = null;
      }
      state.started = false;
      session.current = null;
      tip.current?.hide();
      live.stop();
    };
  }, [active, failure, restarts]);

  // A new stop, or the same in a new theme, updates the renderer.
  useEffect(() => {
    const live = session.current;
    const measured = size.current;
    if (live === null || measured === null || sent.current === job) {
      return;
    }
    sent.current = job;
    live.update(job.inputs, liveContext(job, measured));
    want();
  });

  // The hover text shows in the page's own tip.
  useEffect(() => {
    const element = holder.current;
    if (element === null) {
      return;
    }
    const made = new Tip(element);
    element.append(made.element);
    tip.current = made;
    return () => {
      made.element.remove();
      tip.current = null;
    };
  }, []);

  // The wheel zooms the drawing, not the page, so it cannot be passive.
  // biome-ignore lint/correctness/useExhaustiveDependencies: forward reads only refs
  useEffect(() => {
    const shown = canvas.current;
    if (shown === null) {
      return;
    }
    const wheel = (event: WheelEvent) => {
      if (session.current === null) {
        return;
      }
      event.preventDefault();
      forward("wheel", event);
    };
    shown.addEventListener("wheel", wheel, { passive: false });
    return () => shown.removeEventListener("wheel", wheel);
  }, []);

  const counted = (shown: HTMLCanvasElement) => {
    events.current += 1;
    shown.dataset.sent = String(events.current);
  };

  const forward = (type: LivePointer["type"], event: PointerEvent | WheelEvent | MouseEvent) => {
    const shown = canvas.current;
    const live = session.current;
    if (shown === null || live === null) {
      return;
    }
    pointer.current = event;
    counted(shown);
    const bounds = shown.getBoundingClientRect();
    const scale = bounds.width > 0 ? (size.current?.device.width ?? 0) / bounds.width : 1;
    let wheel = 0;
    if (event instanceof WheelEvent) {
      // Lines and pages as pixels, as most mice give them.
      const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? bounds.height : 1;
      wheel = event.deltaY * unit;
    }
    live.pointer({
      type,
      x: (event.clientX - bounds.left) * scale,
      y: (event.clientY - bounds.top) * scale,
      dx: event.movementX * scale,
      dy: event.movementY * scale,
      buttons: event.buttons,
      wheel,
      shift: event.shiftKey,
      ctrl: event.ctrlKey,
      alt: event.altKey,
    });
  };

  const restart = failure !== null && (
    <button
      type="button"
      className="link-button"
      onClick={() => {
        setCaption(null);
        setFailure(null);
        setRestarts((count) => count + 1);
      }}
    >
      Restart
    </button>
  );

  return (
    <>
      {failure !== null && (
        <div className="drawing-problem" role="alert">
          {liveProblem(failure)} {restart}
        </div>
      )}
      <div ref={holder} className="drawing-body live">
        <canvas
          ref={canvas}
          className="live-canvas"
          style={{ height }}
          tabIndex={0}
          data-keys="renderer"
          aria-label={`Live drawing by ${job.source.name}`}
          onPointerDown={(event) => {
            // Without scrolling: the press is where the pointer is now.
            event.currentTarget.focus({ preventScroll: true });
            event.currentTarget.setPointerCapture(event.pointerId);
            forward("down", event.nativeEvent);
          }}
          onPointerMove={(event) => forward("move", event.nativeEvent)}
          onPointerUp={(event) => forward("up", event.nativeEvent)}
          onPointerLeave={(event) => {
            tip.current?.hide();
            forward("leave", event.nativeEvent);
          }}
          onKeyDown={(event) => {
            const live = session.current;
            if (live === null || KEPT.test(event.key)) {
              return;
            }
            counted(event.currentTarget);
            live.key({
              key: event.key,
              shift: event.shiftKey,
              ctrl: event.ctrlKey,
              alt: event.altKey,
            });
            // A plain key is the renderer's; a combination is the page's too.
            if (!event.ctrlKey && !event.altKey && !event.metaKey) {
              event.preventDefault();
              event.stopPropagation();
            }
          }}
        />
      </div>
      {waiting && (
        <div className="drawing-wait muted">Waiting: four live drawings on screen run at once.</div>
      )}
      {caption !== null && <div className="drawing-caption">{caption}</div>}
    </>
  );
}
