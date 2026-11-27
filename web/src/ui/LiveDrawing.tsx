import { createEffect, createMemo, createSignal, onSettled, Show, untrack } from "solid-js";

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

// Avoid forwarding function keys, Escape, and Tab to the renderer (and avoid preventing default on Tab, which traps keyboard focus).
const KEPT = /^(?:F\d+|Escape|Tab)$/u;

/**
 * Hosts a sandboxed off-thread canvas renderer via `bitmaprenderer` (`ImageBitmap` transfer) with
 * a demand-driven rAF loop that enforces backpressure of at most one frame in flight at a time.
 */
export function LiveDrawing(props: {
  card: string;
  job: LiveJob;
  active: boolean;
  sandbox: () => Sandbox;
  onSelect(path: string): void;
}) {
  let holder: HTMLDivElement | undefined;
  let canvas: HTMLCanvasElement | undefined;
  const [caption, setCaption] = createSignal<string | null>(null);
  const [failure, setFailure] = createSignal<LiveFailure | null>(null);
  const [waiting, setWaiting] = createSignal(false);
  const [restarts, setRestarts] = createSignal(0);
  const [height, setHeight] = createSignal(SHORTEST);

  let session: LiveSession | null = null;
  let sent: LiveJob | null = null;
  let size: Size | null = null;
  let tip: Tip | null = null;
  let pointer: MouseEvent | null = null;

  const loop = {
    started: false,
    wanted: false,
    animating: false,
    inFlight: false,
    request: null as number | null,
    asked: { stop: -1, events: 0 },
  };

  let starts = 0;
  let events = 0;

  const schedule = () => {
    if (!loop.started || loop.inFlight || loop.request !== null) {
      return;
    }
    if (!loop.wanted && !loop.animating) {
      return;
    }
    loop.request = requestAnimationFrame((time) => {
      loop.request = null;
      if (!loop.started || (!loop.wanted && !loop.animating)) {
        return;
      }
      assert(!loop.inFlight, "live frame asked for with one in flight");
      assert(sent !== null, "live session started with no job sent");
      assert(session !== null, "live frame asked for with no session");
      loop.wanted = false;
      loop.inFlight = true;
      loop.asked = { stop: sent.stop, events };
      session.frame(time);
    });
  };

  const want = () => {
    loop.wanted = true;
    schedule();
  };

  const counted = (shown: HTMLCanvasElement) => {
    events += 1;
    shown.dataset.sent = String(events);
  };

  const forward = (type: LivePointer["type"], event: PointerEvent | WheelEvent | MouseEvent) => {
    const shown = canvas;
    const live = session;
    if (shown === undefined || live === null) {
      return;
    }
    pointer = event;
    counted(shown);
    const measured = size;
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
  };

  const resized = () => {
    const element = holder;
    assert(element !== undefined, "live drawing mounted without its holder");
    const next = measure(element);
    const before = size;
    size = next;
    setHeight(next.css.height);

    // Only size the canvas directly before the first frame; later resizes wait for the next ImageBitmap so setting width/height does not clear the canvas.
    const shown = canvas;
    if (shown !== undefined && shown.dataset.stop === undefined) {
      shown.width = next.device.width;
      shown.height = next.device.height;
    }
    if (
      before !== null &&
      (before.device.width !== next.device.width || before.device.height !== next.device.height)
    ) {
      session?.resize(next.device.width, next.device.height);
      want();
    }
  };

  const wheel = (event: WheelEvent) => {
    if (session === null) {
      return;
    }
    event.preventDefault();
    forward("wheel", event);
  };

  onSettled(() => {
    const element = holder;
    const shown = canvas;
    assert(element !== undefined, "live drawing mounted without its holder");
    assert(shown !== undefined, "live drawing mounted without its canvas");

    resized();
    const observer = new ResizeObserver(resized);
    observer.observe(element);

    const made = new Tip(element);
    element.append(made.element);
    tip = made;

    shown.addEventListener("wheel", wheel, { passive: false });

    return () => {
      observer.disconnect();
      made.element.remove();
      tip = null;
      shown.removeEventListener("wheel", wheel);
    };
  });

  const sessionTrigger = createMemo(() => ({
    active: props.active,
    failure: failure(),
    restarts: restarts(),
  }));

  createEffect(
    () => sessionTrigger(),
    ({ active, failure: currentFailure }) => {
      const shown = canvas;
      assert(shown !== undefined, "live drawing mounted without its canvas");
      if (!active || currentFailure !== null) {
        return;
      }
      if (size === null) {
        resized();
      }
      const measured = size;
      const card = untrack(() => props.card);
      assert(measured !== null, `live card ${card} starts before its size is measured`);
      loop.started = false;
      loop.wanted = false;
      loop.animating = false;
      loop.inFlight = false;
      const first = untrack(() => props.job);
      sent = first;
      events = 0;
      starts += 1;

      shown.dataset.session = String(starts);
      shown.dataset.sent = "0";
      delete shown.dataset.stop;
      delete shown.dataset.seen;
      const bitmaps = shown.getContext("bitmaprenderer");
      const live = props
        .sandbox()
        .live(card, first.source, first.inputs, liveContext(first, measured), measured.device, {
          waiting: () => setWaiting(true),
          started: () => {
            setWaiting(false);
            loop.started = true;
            want();
          },
          frame: (bitmap) => {
            loop.inFlight = false;
            if (bitmap !== null) {
              if (shown.width !== bitmap.width || shown.height !== bitmap.height) {
                shown.width = bitmap.width;
                shown.height = bitmap.height;
              }
              bitmaps?.transferFromImageBitmap(bitmap);
            }
            shown.dataset.stop = String(loop.asked.stop);
            shown.dataset.seen = String(loop.asked.events);
            schedule();
          },
          redraw: want,
          animate: (on) => {
            loop.animating = on;
            schedule();
          },
          caption: (text) => setCaption(text),
          hint: (text) => {
            if (text === null || pointer === null) {
              tip?.hide();
            } else {
              tip?.show(text, pointer);
            }
          },
          select: (path) => {
            if (isSelect(path)) {
              props.onSelect(path);
            }
          },
          failed: (why) => {
            loop.started = false;
            setWaiting(false);
            setFailure(why);
          },
        });
      session = live;
      return () => {
        if (loop.request !== null) {
          cancelAnimationFrame(loop.request);
          loop.request = null;
        }
        loop.started = false;
        session = null;
        tip?.hide();
        live.stop();
      };
    },
  );

  createEffect(
    () => props.job,
    (job) => {
      if (session === null || sent === job) {
        return;
      }
      const measured = size;
      assert(measured !== null, "live drawing updates before its size is measured");
      sent = job;
      session.update(job.inputs, liveContext(job, measured));
      want();
    },
  );

  const again = () => {
    setCaption(null);
    setFailure(null);
    setRestarts((count) => count + 1);
  };

  const press = (event: PointerEvent & { currentTarget: HTMLCanvasElement }) => {
    event.currentTarget.focus({ preventScroll: true });
    event.currentTarget.setPointerCapture(event.pointerId);
    forward("down", event);
  };

  const move = (event: PointerEvent & { currentTarget: HTMLCanvasElement }) =>
    forward("move", event);

  const release = (event: PointerEvent & { currentTarget: HTMLCanvasElement }) =>
    forward("up", event);

  const leave = (event: PointerEvent & { currentTarget: HTMLCanvasElement }) => {
    tip?.hide();
    forward("leave", event);
  };

  const key = (event: KeyboardEvent & { currentTarget: HTMLCanvasElement }) => {
    const live = session;
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
  };

  return (
    <>
      <Show when={failure()}>
        {(currentFailure) => (
          <div class="drawing-problem" role="alert">
            {liveProblem(currentFailure())}{" "}
            <button type="button" class="link-button" onClick={again}>
              Restart
            </button>
          </div>
        )}
      </Show>
      <div
        ref={(node) => {
          holder = node;
        }}
        class="drawing-body live"
      >
        <canvas
          ref={(node) => {
            canvas = node;
          }}
          class="live-canvas"
          style={{ height: `${height()}px` }}
          tabindex={0}
          data-keys="renderer"
          aria-label={`Live drawing by ${props.job.source.name}`}
          onPointerDown={press}
          onPointerMove={move}
          onPointerUp={release}
          onPointerOut={leave}
          onKeyDown={key}
        />
      </div>
      <Show when={waiting()}>
        <div class="drawing-wait muted">Waiting: four live drawings on screen run at once.</div>
      </Show>
      <Show when={caption() !== null}>
        <div class="drawing-caption">{caption()}</div>
      </Show>
    </>
  );
}
