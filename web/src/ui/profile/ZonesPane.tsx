import { createMemo, createSignal, For, Show } from "solid-js";

import {
  formatBand,
  type Instance,
  parseBand,
  parseInstance,
  type ProfileLook,
  rangeOf,
  withInstance,
  withLook,
  withoutSite,
} from "../../profile/look";
import { formatCount, formatDuration, formatInstant, largest } from "../../profile/paint";
import { bandOf, bucketSpans, threadName } from "../../profile/zones";
import type { ProfileZoneSite, ProfileZoneStats } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Duration } from "./parts";

type Order = "count" | "total" | "longest";

const ORDERS: { order: Order; label: string }[] = [
  { order: "count", label: "count" },
  { order: "total", label: "total" },
  { order: "longest", label: "longest" },
];

export function ZonesPane() {
  const focus = useProfile();
  const [order, setOrder] = createSignal<Order>("total");
  const sites = createMemo(() => {
    const by = order();
    return focus().summary.zoneSites.toSorted(
      (a, b) => b[by] - a[by] || a.name.localeCompare(b.name),
    );
  });
  const chosen = createMemo(() =>
    focus().summary.zoneSites.find((site) => site.name === focus().look.zs),
  );
  const choose = (name: string) =>
    focus().setLook((prev) => {
      const rest = withoutSite(prev);
      return prev.zs === name ? rest : { ...rest, zs: name };
    }, true);
  return (
    <aside class="pane zones-pane" data-testid="zones-pane" aria-label="Zones">
      <div class="pane-body">
        <Show
          when={sites().length > 0}
          fallback={<div class="muted">This recording has no zones.</div>}
        >
          <div class="zone-sites-list">
            <table class="zone-sites">
              <thead>
                <tr>
                  <th>zone</th>
                  <For each={ORDERS}>
                    {(each) => (
                      <th aria-sort={order() === each.order ? "descending" : "none"}>
                        <OrderButton order={each.order} label={each.label} setOrder={setOrder} />
                      </th>
                    )}
                  </For>
                </tr>
              </thead>
              <tbody>
                <For each={sites()} keyed={(site) => `${site.label}:${site.name}`}>
                  {(site) => (
                    <SiteRow
                      site={site()}
                      chosen={site().name === focus().look.zs}
                      choose={choose}
                    />
                  )}
                </For>
              </tbody>
            </table>
          </div>
        </Show>
        <Show when={chosen()?.name} keyed>
          {(_name) => <Show when={chosen()}>{(site) => <ZoneStats site={site()} />}</Show>}
        </Show>
      </div>
    </aside>
  );
}

function OrderButton(props: { order: Order; label: string; setOrder: (order: Order) => void }) {
  return (
    <button type="button" class="link-button" onClick={() => props.setOrder(props.order)}>
      {props.label}
    </button>
  );
}

function SiteRow(props: {
  site: ProfileZoneSite;
  chosen: boolean;
  choose: (name: string) => void;
}) {
  return (
    <tr class={props.chosen ? "selected" : undefined}>
      <td>
        <button
          type="button"
          class="link-button"
          aria-pressed={props.chosen ? "true" : "false"}
          onClick={() => props.choose(props.site.name)}
        >
          {props.site.name}
        </button>
        <Show when={props.site.label}>
          <span class="dim"> label</span>
        </Show>
        <Show when={props.site.flagged > 0}>
          <span class="warn" title="Zones whose begin or end was not seen, or did not nest">
            {" "}
            {formatCount(props.site.flagged)} flagged
          </span>
        </Show>
      </td>
      <td>{formatCount(props.site.count)}</td>
      <td>
        <Duration ns={props.site.total} />
      </td>
      <td>
        <Duration ns={props.site.longest} />
      </td>
    </tr>
  );
}

const SCOPES: { zf: ProfileLook["zf"]; label: string; title: string }[] = [
  {
    zf: undefined,
    label: "Range only",
    title: "The views read the range, whatever zones it holds",
  },
  {
    zf: "all",
    label: "Inside every one",
    title: "The views read only samples inside this site's zones",
  },
  {
    zf: "band",
    label: "Inside the band",
    title: "The views read only samples inside the band's zones",
  },
];

function ZoneStats(props: { site: ProfileZoneSite }) {
  const focus = useProfile();
  const range = createMemo(() => rangeOf({ range: focus().look.range }));
  const band = createMemo(() => {
    const zb = focus().look.zb;
    return zb ? parseBand(zb) : null;
  });
  const instance = createMemo(() => {
    const zi = focus().look.zi;
    return zi ? parseInstance(zi) : null;
  });
  const length = createMemo(() => {
    const current = instance();
    return current ? current.span[1] - current.span[0] : null;
  });
  const params = createMemo(() => {
    const current = focus();
    const currentRange = range();
    const currentBand = band();
    const currentLength = length();
    return {
      profile: current.profile,
      revision: current.query.revision ?? 0,
      name: props.site.name,
      ...(currentRange ? { range: currentRange } : {}),
      ...(current.query.threads ? { threads: current.query.threads } : {}),
      ...(currentBand ? { band: currentBand } : {}),
      ...(currentLength === null ? {} : { rankOf: currentLength }),
    };
  });
  const answer = useAnswer("profileZoneStats", params);
  const waits = createMemo(() => focus().look.src === "waits");
  const selectedSpan = (thread: number, start: number, end: number) => {
    const current = instance();
    return (
      current !== null &&
      current.thread === thread &&
      current.span[0] === start &&
      current.span[1] === end
    );
  };
  const rankedLength = createMemo(() => {
    const current = length();
    return instance() && current !== null ? { ns: current } : null;
  });

  const rootByValue = () =>
    focus().setLook(
      (prev) => withLook(prev, "bl", prev.bl === props.site.name ? undefined : props.site.name),
      true,
    );
  const compare = () =>
    focus().setLook((prev) => {
      const { bp: _bp, br: _br, dm: _dm, ...rest } = prev;
      return withLook(rest, "bz", prev.bz === "rest" ? undefined : "rest");
    }, true);
  const clearBand = () =>
    focus().setLook((prev) => {
      const { zb: _zb, bz: _bz, zf, ...rest } = prev;
      return zf === "all" ? { ...rest, zf } : rest;
    }, true);

  return (
    <Show
      when={!answer().error}
      fallback={<div class="empty error">{answer().error?.message}</div>}
    >
      <Show when={answer().data} fallback={<div class="muted">Measuring {props.site.name}…</div>}>
        {(stats) => (
          <div class={`zone-stats ${answer().current ? "" : "stale"}`} data-testid="zone-stats">
            <dl>
              <div>
                <dt>count</dt>
                <dd>{formatCount(stats().count)}</dd>
              </div>
              <div>
                <dt>total</dt>
                <dd>
                  <Duration ns={stats().total} />
                </dd>
              </div>
              <div>
                <dt>mean</dt>
                <dd>
                  <Duration ns={stats().count > 0 ? stats().total / stats().count : 0} />
                </dd>
              </div>
              <div>
                <dt>median</dt>
                <dd>
                  <Duration ns={stats().median} />
                </dd>
              </div>
              <div>
                <dt>p90</dt>
                <dd>
                  <Duration ns={stats().p90} />
                </dd>
              </div>
              <div>
                <dt>p99</dt>
                <dd>
                  <Duration ns={stats().p99} />
                </dd>
              </div>
              <div>
                <dt>max</dt>
                <dd>
                  <Duration ns={stats().max} />
                </dd>
              </div>
              <div>
                <dt>flagged</dt>
                <dd>{formatCount(stats().flagged)}</dd>
              </div>
            </dl>
            <Show
              when={stats().count > 0}
              fallback={
                <div class="muted">
                  {emptyWhy(range() !== null, focus().query.threads?.length ?? 0)}
                </div>
              }
            >
              <Histogram stats={stats()} band={band()} length={length()} name={props.site.name} />
            </Show>
            <Show
              when={band()}
              fallback={
                <div class="muted zone-hint">
                  Drag across the histogram to pick a band of lengths.
                </div>
              }
            >
              {(currentBand) => (
                <div class="zone-band" data-testid="zone-band">
                  <span>
                    Band {formatDuration(currentBand()[0])} – {formatDuration(currentBand()[1])}:{" "}
                    {formatCount(stats().banded[0])} of {formatCount(stats().count)},{" "}
                    <Duration ns={stats().banded[1]} />
                  </span>
                  <button type="button" class="tb ghost" onClick={clearBand}>
                    Clear band
                  </button>
                </div>
              )}
            </Show>
            <Show when={stats().count > 0 ? rankedLength() : null}>
              {(shown) => (
                <div class="zone-rank" data-testid="zone-rank">
                  The zone selected is p{Math.round((100 * stats().shorter) / stats().count)} of{" "}
                  {formatCount(stats().count)}: {formatDuration(shown().ns)}
                </div>
              )}
            </Show>
            <div class="zone-scope">
              <fieldset class="segmented" aria-label="Samples the views read">
                <For each={SCOPES}>
                  {(scope) => (
                    <ScopeButton
                      zf={scope.zf}
                      label={scope.label}
                      title={waits() ? "Zones scope CPU samples, not waits" : scope.title}
                      disabled={waits() || (scope.zf === "band" && !band())}
                    />
                  )}
                </For>
              </fieldset>
              <button
                type="button"
                class="tb ghost"
                aria-pressed={focus().look.bz === "rest" ? "true" : "false"}
                disabled={waits() || !band()}
                title={
                  band()
                    ? "A diff of the band's samples against the rest of this site's"
                    : "Pick a band first"
                }
                onClick={compare}
              >
                Compare the band with the rest
              </button>
            </div>
            <div class="pane-head">{band() ? "Longest in the band" : "Longest"}</div>
            <Show when={stats().longest.length > 0} fallback={<div class="muted">None.</div>}>
              <ul class="zone-longest">
                <For each={stats().longest} keyed={(zone) => `${zone.thread}:${zone.start}`}>
                  {(zone) => (
                    <li>
                      <ZoneSpan
                        thread={zone().thread}
                        start={zone().start}
                        end={zone().end}
                        chosen={selectedSpan(zone().thread, zone().start, zone().end)}
                      />{" "}
                      {zone().value !== null && zone().value !== undefined
                        ? `${props.site.name}=${zone().value} `
                        : ""}
                      <span class="dim">
                        {threadName(focus().summary.threads, zone().thread)} at{" "}
                        {formatInstant(zone().start, Math.max(zone().end - zone().start, 1) / 10)}
                      </span>
                    </li>
                  )}
                </For>
              </ul>
            </Show>
            <Show when={props.site.label}>
              <div class="pane-head">
                Values
                <button
                  type="button"
                  class="tb ghost"
                  aria-pressed={focus().look.bl === props.site.name ? "true" : "false"}
                  title="Group stacks by this label's value"
                  onClick={rootByValue}
                >
                  By {props.site.name}
                </button>
              </div>
              <ul class="zone-values">
                <For each={stats().values} keyed={(value) => value.value}>
                  {(value) => {
                    const label = createMemo(() => `${props.site.name}=${value().value}`);
                    return (
                      <li>
                        <LabelValue
                          label={label()}
                          on={(focus().look.lb ?? []).includes(label())}
                        />{" "}
                        <span class="dim">
                          {formatDuration(value().total)} in {formatCount(value().count)}
                        </span>
                      </li>
                    );
                  }}
                </For>
              </ul>
            </Show>
          </div>
        )}
      </Show>
    </Show>
  );
}

function ScopeButton(props: {
  zf: ProfileLook["zf"];
  label: string;
  title: string;
  disabled: boolean;
}) {
  const focus = useProfile();
  const pick = () => focus().setLook((prev) => withLook(prev, "zf", props.zf), true);
  return (
    <button
      type="button"
      title={props.title}
      aria-pressed={focus().look.zf === props.zf ? "true" : "false"}
      disabled={props.disabled}
      onClick={pick}
    >
      {props.label}
    </button>
  );
}

function Histogram(props: {
  stats: ProfileZoneStats;
  band: [number, number] | null;
  length: number | null;
  name: string;
}) {
  const focus = useProfile();
  const spans = createMemo(() => bucketSpans(props.stats.histogram, props.stats.max));
  const most = createMemo(() =>
    largest(
      props.stats.histogram.map(([, count = 0]) => count),
      1,
    ),
  );
  let bars: HTMLDivElement | undefined;

  let drag: [number, number] | null = null;
  const [dragging, setDragging] = createSignal<[number, number] | null>(null);
  const bucket = (event: PointerEvent) => {
    const bounds = bars?.getBoundingClientRect();
    const currentSpans = spans();
    if (!bounds || currentSpans.length === 0) {
      return 0;
    }
    const fraction = (event.clientX - bounds.left) / Math.max(bounds.width, 1);
    return Math.min(
      Math.max(Math.floor(fraction * currentSpans.length), 0),
      currentSpans.length - 1,
    );
  };
  const down = (event: PointerEvent & { currentTarget: HTMLDivElement }) => {
    if (event.button !== 0) {
      return;
    }
    event.currentTarget.setPointerCapture(event.pointerId);
    const at = bucket(event);
    drag = [at, at];
    setDragging(drag);
  };
  const move = (event: PointerEvent) => {
    if (drag) {
      drag = [drag[0], bucket(event)];
      setDragging(drag);
    }
  };
  const up = () => {
    const was = drag;
    drag = null;
    setDragging(null);
    if (!was) {
      return;
    }
    const picked = bandOf(spans(), was[0], was[1]);
    focus().setLook((prev) => {
      const same = prev.zb === formatBand(picked);
      const { zb: _zb, bz: _bz, ...rest } = prev;

      if (same) {
        const { zf: _zf, ...unscoped } = rest;
        return rest.zf === "band" ? unscoped : rest;
      }
      return { ...rest, zb: formatBand(picked) };
    }, true);
  };
  const cancel = () => {
    drag = null;
    setDragging(null);
  };

  const picking = createMemo(() => {
    const current = dragging();
    return current ? [Math.min(...current), Math.max(...current)] : null;
  });
  const marked = (at: number) => {
    const [low, high] = spans()[at] ?? [0, 0];
    const currentPicking = picking();
    if (currentPicking) {
      return at >= (currentPicking[0] ?? 0) && at <= (currentPicking[1] ?? 0);
    }
    return props.band ? high >= props.band[0] && low <= props.band[1] : null;
  };
  const rank = createMemo(() => {
    const currentLength = props.length;
    return currentLength === null
      ? -1
      : spans().findIndex(([low, high]) => currentLength >= low && currentLength <= high);
  });
  return (
    <>
      <div
        class="zone-histogram"
        ref={(node) => {
          bars = node;
        }}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a chart of bars, not an image file
        role="img"
        aria-label={`Lengths of ${props.name}${props.band ? `, band ${formatDuration(props.band[0])} – ${formatDuration(props.band[1])}` : ""}`}
        onPointerDown={down}
        onPointerMove={move}
        onPointerUp={up}
        onPointerCancel={cancel}
      >
        <For each={props.stats.histogram} keyed={([low = 0]) => low}>
          {(entry, at) => (
            <Bar
              low={entry()[0] ?? 0}
              count={entry()[1] ?? 0}
              most={most()}
              state={marked(at())}
              here={at() === rank()}
            />
          )}
        </For>
      </div>
      <div class="zone-axis muted">
        <span>{formatDuration(props.stats.min)}</span>
        <span>{formatDuration(props.stats.max)}</span>
      </div>
    </>
  );
}

function Bar(props: {
  low: number;
  count: number;
  most: number;
  state: boolean | null;
  here: boolean;
}) {
  const kind = createMemo(() =>
    props.state === null ? "" : props.state ? " banded" : " unbanded",
  );
  return (
    <span
      class={`bar${kind()}${props.here ? " here" : ""}`}
      style={{ "--share": String(props.count / props.most) }}
      title={`${formatCount(props.count)} from ${formatDuration(props.low)}`}
    />
  );
}

function ZoneSpan(props: { thread: number; start: number; end: number; chosen: boolean }) {
  const focus = useProfile();
  const select = () => {
    const instance: Instance = { thread: props.thread, span: [props.start, props.end] };
    focus().setLook((prev) => withInstance(prev, props.chosen ? null : instance), true);
  };
  return (
    <button
      type="button"
      class="link-button"
      aria-pressed={props.chosen ? "true" : "false"}
      title="Select this zone: the views read its span"
      onClick={select}
    >
      {formatDuration(props.end - props.start)}
    </button>
  );
}

function LabelValue(props: { label: string; on: boolean }) {
  const focus = useProfile();
  const toggle = () =>
    focus().setLook((prev) => {
      const kept = (prev.lb ?? []).filter((each) => each !== props.label);
      const next = props.on ? kept : [...kept, props.label];
      return withLook(prev, "lb", next.length > 0 ? next : undefined);
    }, true);
  return (
    <button
      type="button"
      class="link-button"
      aria-pressed={props.on ? "true" : "false"}
      title="Keep only samples with this label"
      onClick={toggle}
    >
      {props.label}
    </button>
  );
}

function emptyWhy(ranged: boolean, threads: number): string {
  const where = [
    ...(ranged ? ["in the range"] : []),
    ...(threads > 0 ? [`on the ${threads === 1 ? "thread" : "threads"} selected`] : []),
  ];
  return where.length > 0 ? `None of its zones are ${where.join(" or ")}.` : "It has no zones.";
}
