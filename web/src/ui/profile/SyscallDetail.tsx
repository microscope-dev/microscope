import { createEffect, createMemo, For, Show } from "solid-js";

import { withLook } from "../../profile/look";
import { formatCount, formatDuration, largest } from "../../profile/paint";
import { bucketLabel, heatAlpha, shownBuckets } from "../../profile/syscalls";
import type { SyscallDetail, SyscallQuery } from "../../protocol";
import { useShownScheme } from "../../theme";
import { prepare, useWidth } from "./canvas";
import { token, useAnswer, useProfile } from "./context";
import { Unanswered } from "./parts";

export function Detail(props: { call: string; query: SyscallQuery }) {
  const selected = createMemo(() => ({
    ...props.query,
    filter: `${props.query.filter} trace=${props.call}`.trim(),
  }));
  const detail = useAnswer("syscallDetail", () => ({ ...props.query, call: props.call }));
  return (
    <div class="pane syscalls-detail" data-testid="syscall-detail">
      <div class="pane-head">
        <span class="mono">{props.call}</span>
      </div>
      <Heatmap query={selected()} call={props.call} />
      <Show
        when={!detail().error && detail().data}
        fallback={<Unanswered error={detail().error} reading={`Reading ${props.call}'s calls…`} />}
      >
        {(data) => <Histograms data={data()} />}
      </Show>
    </div>
  );
}

function Histograms(props: { data: SyscallDetail }) {
  const bounds = createMemo(() => shownBuckets(props.data.whole, props.data.selected));
  const most = createMemo(() => largest(props.data.selected, largest(props.data.whole, 1)));
  const buckets = createMemo(() => {
    const [first, last] = bounds();
    return last >= first ? props.data.whole.slice(first, last + 1) : [];
  });
  return (
    <>
      <section>
        <h3>Latency</h3>
        <Show when={bounds()[1] < bounds()[0]}>
          <p class="muted">No call was timed.</p>
        </Show>
        <div class="latency-bars">
          <For each={buckets()} keyed={false}>
            {(whole, offset) => {
              const bucket = createMemo(() => bounds()[0] + offset);
              const chosen = createMemo(() => props.data.selected[bucket()] ?? 0);
              return (
                <div
                  class="histogram-row"
                  title={`${formatCount(chosen())} of ${formatCount(whole(), "call")}`}
                >
                  <span class="mono muted">{bucketLabel(bucket())}</span>
                  <span class="histogram-bar">
                    <Fill class="whole" percent={(100 * whole()) / most()} />
                    <Fill class="chosen" percent={(100 * chosen()) / most()} />
                  </span>
                  <span class="num">{formatCount(chosen())}</span>
                </div>
              );
            }}
          </For>
        </div>
      </section>
      <Show when={props.data.errnos.length > 0}>
        <section>
          <h3>Errors</h3>
          <For each={props.data.errnos} keyed={(errno) => errno.name}>
            {(errno) => (
              <div class="histogram-row">
                <span class="mono error-text">{errno().name}</span>
                <span class="histogram-bar">
                  <Fill
                    class="error"
                    percent={(100 * errno().count) / Math.max(1, props.data.errnos[0]?.count ?? 1)}
                  />
                </span>
                <span class="num">{formatCount(errno().count)}</span>
              </div>
            )}
          </For>
        </section>
      </Show>
      <section data-testid="syscall-callers">
        <h3>
          Callers of slow
          {props.data.slowNs === null ? "" : ` (≥ ${formatDuration(props.data.slowNs)})`} and failed
          calls
        </h3>
        <Show when={props.data.callers.length === 0}>
          <p class="muted">No stacks were recorded.</p>
        </Show>
        <For each={props.data.callers} keyed={(caller) => caller.site}>
          {(caller) => (
            <div class="caller">
              <span class="mono">{caller().site}</span>
              <span class="muted">
                {caller().slow > 0 ? `${formatCount(caller().slow)} slow` : ""}
                {caller().slow > 0 && caller().failed > 0 ? ", " : ""}
                <Show when={caller().failed > 0}>
                  <span class="error-text">{formatCount(caller().failed)} failed</span>
                </Show>
              </span>
            </div>
          )}
        </For>
        <Show when={props.data.unplaced > 0}>
          <p class="muted">{formatCount(props.data.unplaced)} had no stack.</p>
        </Show>
      </section>
    </>
  );
}

function Fill(props: { class: string; percent: number }) {
  return <span class={props.class} style={{ width: `${props.percent}%` }} />;
}

function Heatmap(props: { query: SyscallQuery; call: string }) {
  const focus = useProfile();
  const [box, width] = useWidth();
  let canvas: HTMLCanvasElement | undefined;
  const scheme = useShownScheme();
  const buckets = createMemo(() => Math.max(1, Math.min(512, Math.floor(width() / 4))));
  const answer = useAnswer("syscallHeatmap", () =>
    width() > 0 ? { ...props.query, buckets: buckets() } : null,
  );
  const data = createMemo(() => answer().data);

  const bounds = createMemo<[number, number]>(() => {
    const current = data();
    if (!current) {
      return [0, -1];
    }
    const rows = Array.from({ length: current.rows }, (_, row) => {
      let count = 0;
      for (let slice = 0; slice * current.rows < current.counts.length; slice++) {
        count += current.counts[slice * current.rows + row] ?? 0;
      }
      return count;
    });
    return shownBuckets(rows);
  });
  const mostFailed = createMemo(() => {
    const current = data();
    return current ? current.failed.reduce((most, each) => Math.max(most, each), 0) : 0;
  });
  const height = 140;

  createEffect(
    () => ({
      data: data(),
      width: width(),
      bounds: bounds(),
      scheme: scheme(),
      mostFailed: mostFailed(),
    }),
    ({
      data: current,
      width: w,
      bounds: [first, last],
      scheme: currentScheme,
      mostFailed: topFailed,
    }) => {
      const context = prepare(canvas, w, height);
      if (!context || !current || last < first) {
        return;
      }
      if (canvas) {
        canvas.dataset.scheme = currentScheme;
      }
      const shown = last - first + 1;
      const cell = height / shown;
      const slices = current.counts.length / current.rows;
      const column = w / slices;
      context.fillStyle = token("--accent");
      for (let slice = 0; slice < slices; slice++) {
        for (let row = first; row <= last; row++) {
          const alpha = heatAlpha(current.counts[slice * current.rows + row] ?? 0, current.most);
          if (alpha > 0) {
            context.globalAlpha = alpha;
            context.fillRect(
              slice * column,
              height - (row - first + 1) * cell,
              Math.max(column, 1),
              cell,
            );
          }
        }
      }

      context.fillStyle = token("--error");
      for (let slice = 0; slice < slices; slice++) {
        for (let row = first; row <= last; row++) {
          const failed = current.failed[slice * current.rows + row] ?? 0;
          if (failed > 0) {
            context.globalAlpha = Math.max(heatAlpha(failed, topFailed), 0.6);
            context.fillRect(
              slice * column,
              height - (row - first + 1) * cell,
              Math.max(column, 1),
              cell,
            );
          }
        }
      }
      context.globalAlpha = 1;
    },
  );

  const openCell = (event: MouseEvent & { currentTarget: HTMLCanvasElement }) => {
    const current = data();
    const [first, last] = bounds();
    if (!current || last < first) {
      return;
    }
    const rect = event.currentTarget.getBoundingClientRect();
    const slices = current.counts.length / current.rows;
    const slice = Math.floor(((event.clientX - rect.left) / rect.width) * slices);

    const row = Math.min(
      Math.max(
        last - Math.floor(((event.clientY - rect.top) / rect.height) * (last - first + 1)),
        first,
      ),
      last,
    );
    if (!current.counts[slice * current.rows + row]) {
      return;
    }
    const start = current.start + slice * current.width;
    const filter = `${focus().look.sf ?? ""} trace=${props.call}`.trim();
    focus().setLook((prev) => {
      let next = withLook(prev, "range", `${start}-${start + current.width}`);
      next = withLook(next, "sl", String(row));
      next = withLook(next, "sf", filter);
      // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
      next = withLook(next, "ev", undefined);
      return withLook(next, "st", "log");
    }, true);
  };

  return (
    <section class="latency-map" data-testid="syscall-heatmap">
      <h3>
        Latency over time
        <Show when={bounds()[1] >= bounds()[0]}>
          <span class="muted">
            {" "}
            · {bucketLabel(bounds()[0])} to {bucketLabel(bounds()[1])}
          </span>
        </Show>
        <Show when={mostFailed() > 0}>
          <span class="failed-key"> · failed</span>
        </Show>
      </h3>
      <div ref={box} class="plot">
        <Show
          when={!answer().error}
          fallback={<div class="empty error">{answer().error?.message}</div>}
        >
          <canvas
            ref={(node) => {
              canvas = node;
            }}
            class="clickable"
            style={{ width: `${width()}px`, height: `${height}px` }}
            // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a canvas drawn here, not an image file
            role="img"
            aria-label="Latency heat map"
            title={
              mostFailed() > 0
                ? "A cell's calls, in the log; red cells hold failed calls"
                : "A cell's calls, in the log"
            }
            onClick={openCell}
          />
        </Show>
      </div>
    </section>
  );
}
