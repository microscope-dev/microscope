import {
  type KeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { assert } from "../assert";
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

export interface LiveJob {
  stop: number;
  source: RendererCode;
  inputs: unknown;
  paths: Record<string, string | null>;
  previous: unknown;
}

const SHORTEST = 240;
const TALLEST = 560;

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
      return `The renderer stopped responding for ${UNRESPONSIVE / 1_000} s, so microscope stopped it.`;
    case "lost":
      return "The renderer lost its WebGL context.";
    case "failed":
      return failureText(failure.failure);
  }
}

const KEPT = /^(?:F\d+|Escape|Tab)$/u;

export function LiveDrawing({
  card,
  job,
  active,
  sandbox,
  onSelect,
}: {
  card: string;
  job: LiveJob;

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
  const select = useRef(onSelect);
  useLayoutEffect(() => {
    latest.current = job;
    select.current = onSelect;
  });

  const sent = useRef<LiveJob | null>(null);
  const size = useRef<Size | null>(null);
  const tip = useRef<Tip | null>(null);
  const pointer = useRef<MouseEvent | null>(null);

  const loop = useRef({
    started: false,
    wanted: false,
    animating: false,
    inFlight: false,
    request: null as number | null,

    asked: { stop: -1, events: 0 },
  });

  const starts = useRef(0);
  const events = useRef(0);

  const schedule = useCallback(() => {
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
      assert(!state.inFlight, "live frame asked for with one in flight");
      assert(sent.current !== null, "live session started with no job sent");
      assert(session.current !== null, "live frame asked for with no session");
      state.wanted = false;
      state.inFlight = true;
      state.asked = { stop: sent.current.stop, events: events.current };
      session.current.frame(time);
    });
  }, []);
  const want = useCallback(() => {
    loop.current.wanted = true;
    schedule();
  }, [schedule]);
  const counted = useCallback((shown: HTMLCanvasElement) => {
    events.current += 1;
    shown.dataset.sent = String(events.current);
  }, []);
  const forward = useCallback(
    (type: LivePointer["type"], event: PointerEvent | WheelEvent | MouseEvent) => {
      const shown = canvas.current;
      const live = session.current;
      if (shown === null || live === null) {
        return;
      }
      pointer.current = event;
      counted(shown);
      const measured = size.current;
      assert(measured !== null, "live pointer forwarded before the size is measured");
      const bounds = shown.getBoundingClientRect();
      const scale = bounds.width > 0 ? measured.device.width / bounds.width : 1;
      let wheel = 0;
      if (event instanceof WheelEvent) {
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
    },
    [counted],
  );

  useLayoutEffect(() => {
    const element = holder.current;
    assert(element !== null, "live drawing mounted without its holder");
    const resized = () => {
      const next = measure(element);
      const before = size.current;
      size.current = next;
      setHeight(next.css.height);

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
  }, [want]);

  useEffect(() => {
    const shown = canvas.current;
    assert(shown !== null, "live drawing mounted without its canvas");
    if (!active || failure !== null) {
      return;
    }
    const measured = size.current;
    assert(measured !== null, `live card ${card} starts before its size is measured`);
    const state = loop.current;
    state.started = false;
    state.wanted = false;
    state.animating = false;
    state.inFlight = false;
    const first = latest.current;
    sent.current = first;
    events.current = 0;
    starts.current += 1;

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
    // oxlint-disable-next-line react-hooks/exhaustive-deps -- the session reads its job and callbacks through refs; a new renderer is a new component
  }, [active, failure, restarts]);

  useEffect(() => {
    const live = session.current;
    const measured = size.current;
    assert(measured !== null, "live drawing updates before its size is measured");
    if (live === null || sent.current === job) {
      return;
    }
    sent.current = job;
    live.update(job.inputs, liveContext(job, measured));
    want();
  });

  useEffect(() => {
    const element = holder.current;
    assert(element !== null, "live drawing mounted without its holder");
    const made = new Tip(element);
    element.append(made.element);
    tip.current = made;
    return () => {
      made.element.remove();
      tip.current = null;
    };
  }, []);

  useEffect(() => {
    const shown = canvas.current;
    assert(shown !== null, "live drawing mounted without its canvas");
    const wheel = (event: WheelEvent) => {
      if (session.current === null) {
        return;
      }
      event.preventDefault();
      forward("wheel", event);
    };
    shown.addEventListener("wheel", wheel, { passive: false });
    return () => shown.removeEventListener("wheel", wheel);
  }, [forward]);

  const again = useCallback(() => {
    setCaption(null);
    setFailure(null);
    setRestarts((count) => count + 1);
  }, []);
  const restart = failure !== null && (
    <button type="button" className="link-button" onClick={again}>
      Restart
    </button>
  );

  const sized = useMemo(() => ({ height }), [height]);
  const press = useCallback(
    (event: ReactPointerEvent<HTMLCanvasElement>) => {
      event.currentTarget.focus({ preventScroll: true });
      event.currentTarget.setPointerCapture(event.pointerId);
      forward("down", event.nativeEvent);
    },
    [forward],
  );
  const move = useCallback(
    (event: ReactPointerEvent<HTMLCanvasElement>) => forward("move", event.nativeEvent),
    [forward],
  );
  const release = useCallback(
    (event: ReactPointerEvent<HTMLCanvasElement>) => forward("up", event.nativeEvent),
    [forward],
  );
  const leave = useCallback(
    (event: ReactPointerEvent<HTMLCanvasElement>) => {
      tip.current?.hide();
      forward("leave", event.nativeEvent);
    },
    [forward],
  );
  const key = useCallback(
    (event: KeyboardEvent<HTMLCanvasElement>) => {
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

      if (!event.ctrlKey && !event.altKey && !event.metaKey) {
        event.preventDefault();
        event.stopPropagation();
      }
    },
    [counted],
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
          style={sized}
          tabIndex={0}
          data-keys="renderer"
          aria-label={`Live drawing by ${job.source.name}`}
          onPointerDown={press}
          onPointerMove={move}
          onPointerUp={release}
          onPointerLeave={leave}
          onKeyDown={key}
        />
      </div>
      {waiting && (
        <div className="drawing-wait muted">Waiting: four live drawings on screen run at once.</div>
      )}
      {caption !== null && <div className="drawing-caption">{caption}</div>}
    </>
  );
}
