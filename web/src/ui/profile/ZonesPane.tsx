import {
  type CSSProperties,
  type PointerEvent,
  useCallback,
  useMemo,
  useRef,
  useState,
} from "react";

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

/** Every zone site and label key, and the chosen one's spread of lengths,
 * which picks the samples the views read. */
export function ZonesPane() {
  const { summary, look, setLook } = useProfile();
  const [order, setOrder] = useState<Order>("total");
  const sites = useMemo(
    () => summary.zoneSites.toSorted((a, b) => b[order] - a[order] || a.name.localeCompare(b.name)),
    [summary.zoneSites, order],
  );
  const chosen = summary.zoneSites.find((site) => site.name === look.zs);
  const choose = useCallback(
    (name: string) =>
      setLook((prev) => {
        // The zone selected, if any, is one of the old site's.
        const rest = withoutSite(prev);
        return prev.zs === name ? rest : { ...rest, zs: name };
      }, true),
    [setLook],
  );
  return (
    <aside className="pane zones-pane" data-testid="zones-pane" aria-label="Zones">
      <div className="pane-body">
        {sites.length === 0 ? (
          <div className="muted">This recording has no zones.</div>
        ) : (
          <div className="zone-sites-list">
            <table className="zone-sites">
              <thead>
                <tr>
                  <th>zone</th>
                  {ORDERS.map(({ order: each, label }) => (
                    <th key={each} aria-sort={order === each ? "descending" : "none"}>
                      <OrderButton order={each} label={label} setOrder={setOrder} />
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {sites.map((site) => (
                  <SiteRow
                    key={`${site.label}:${site.name}`}
                    site={site}
                    chosen={site.name === look.zs}
                    choose={choose}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
        {chosen ? <ZoneStats key={chosen.name} site={chosen} /> : null}
      </div>
    </aside>
  );
}

function OrderButton({
  order,
  label,
  setOrder,
}: {
  order: Order;
  label: string;
  setOrder: (order: Order) => void;
}) {
  const click = useCallback(() => setOrder(order), [setOrder, order]);
  return (
    <button type="button" className="link-button" onClick={click}>
      {label}
    </button>
  );
}

function SiteRow({
  site,
  chosen,
  choose,
}: {
  site: ProfileZoneSite;
  chosen: boolean;
  choose: (name: string) => void;
}) {
  const click = useCallback(() => choose(site.name), [choose, site.name]);
  return (
    <tr className={chosen ? "selected" : undefined}>
      <td>
        <button type="button" className="link-button" aria-pressed={chosen} onClick={click}>
          {site.name}
        </button>
        {site.label ? <span className="dim"> label</span> : null}
        {site.flagged > 0 ? (
          <span className="warn" title="Zones whose begin or end was not seen, or did not nest">
            {" "}
            {formatCount(site.flagged)} flagged
          </span>
        ) : null}
      </td>
      <td>{formatCount(site.count)}</td>
      <td>
        <Duration ns={site.total} />
      </td>
      <td>
        <Duration ns={site.longest} />
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

/** One site's zones in the range: their percentiles, a histogram of
 * lengths to pick a band from, which samples the views read, the longest
 * in the band, and for a label, its values, each a filter. */
function ZoneStats({ site }: { site: ProfileZoneSite }) {
  const { profile, query, look, setLook, summary } = useProfile();
  const range = useMemo(() => rangeOf({ range: look.range }), [look.range]);
  const band = useMemo(() => (look.zb ? parseBand(look.zb) : null), [look.zb]);
  const instance = useMemo(() => (look.zi ? parseInstance(look.zi) : null), [look.zi]);
  const length = instance ? instance.span[1] - instance.span[0] : null;
  const params = useMemo(
    () => ({
      profile,
      revision: query.revision ?? 0,
      name: site.name,
      ...(range ? { range } : {}),
      ...(query.threads ? { threads: query.threads } : {}),
      ...(band ? { band } : {}),
      ...(length === null ? {} : { rankOf: length }),
    }),
    [profile, query.revision, site.name, range, query.threads, band, length],
  );
  const answer = useAnswer("profileZoneStats", params);
  const stats = answer.data;
  const waits = look.src === "waits";

  const rootByValue = useCallback(
    () =>
      setLook((prev) => withLook(prev, "bl", prev.bl === site.name ? undefined : site.name), true),
    [setLook, site.name],
  );
  const compare = useCallback(
    () =>
      setLook((prev) => {
        const { bp: _bp, br: _br, dm: _dm, ...rest } = prev;
        return withLook(rest, "bz", prev.bz === "rest" ? undefined : "rest");
      }, true),
    [setLook],
  );
  const clearBand = useCallback(
    () =>
      setLook((prev) => {
        const { zb: _zb, bz: _bz, zf, ...rest } = prev;
        return zf === "all" ? { ...rest, zf } : rest;
      }, true),
    [setLook],
  );

  if (answer.error) {
    return <div className="empty error">{answer.error.message}</div>;
  }
  if (!stats) {
    return <div className="muted">Measuring {site.name}…</div>;
  }
  return (
    <div className={`zone-stats ${answer.current ? "" : "stale"}`} data-testid="zone-stats">
      <dl>
        {(
          [
            ["count", formatCount(stats.count)],
            ["total", <Duration key="total" ns={stats.total} />],
            ["mean", <Duration key="mean" ns={stats.count > 0 ? stats.total / stats.count : 0} />],
            ["median", <Duration key="median" ns={stats.median} />],
            ["p90", <Duration key="p90" ns={stats.p90} />],
            ["p99", <Duration key="p99" ns={stats.p99} />],
            ["max", <Duration key="max" ns={stats.max} />],
            ["flagged", formatCount(stats.flagged)],
          ] as const
        ).map(([term, value]) => (
          <div key={term}>
            <dt>{term}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
      {stats.count === 0 ? (
        <div className="muted">{emptyWhy(range !== null, query.threads?.length ?? 0)}</div>
      ) : (
        <Histogram stats={stats} band={band} length={length} name={site.name} />
      )}
      {band ? (
        <div className="zone-band" data-testid="zone-band">
          <span>
            Band {formatDuration(band[0])} – {formatDuration(band[1])}:{" "}
            {formatCount(stats.banded[0])} of {formatCount(stats.count)},{" "}
            <Duration ns={stats.banded[1]} />
          </span>
          <button type="button" className="tb ghost" onClick={clearBand}>
            Clear band
          </button>
        </div>
      ) : (
        <div className="muted zone-hint">Drag across the histogram to pick a band of lengths.</div>
      )}
      {instance && length !== null && stats.count > 0 ? (
        <div className="zone-rank" data-testid="zone-rank">
          The zone selected is p{Math.round((100 * stats.shorter) / stats.count)} of{" "}
          {formatCount(stats.count)}: {formatDuration(length)}
        </div>
      ) : null}
      <div className="zone-scope">
        <fieldset className="segmented" aria-label="Samples the views read">
          {SCOPES.map(({ zf, label, title }) => (
            <ScopeButton
              key={label}
              zf={zf}
              label={label}
              title={waits ? "Zones scope CPU samples, not waits" : title}
              disabled={waits || (zf === "band" && !band)}
            />
          ))}
        </fieldset>
        <button
          type="button"
          className="tb ghost"
          aria-pressed={look.bz === "rest"}
          disabled={waits || !band}
          title={
            band
              ? "A diff of the band's samples against the rest of this site's"
              : "Pick a band first"
          }
          onClick={compare}
        >
          Compare the band with the rest
        </button>
      </div>
      <div className="pane-head">{band ? "Longest in the band" : "Longest"}</div>
      {stats.longest.length === 0 ? (
        <div className="muted">None.</div>
      ) : (
        <ul className="zone-longest">
          {stats.longest.map((zone) => (
            <li key={`${zone.thread}:${zone.start}`}>
              <ZoneSpan
                thread={zone.thread}
                start={zone.start}
                end={zone.end}
                chosen={
                  instance?.thread === zone.thread &&
                  instance.span[0] === zone.start &&
                  instance.span[1] === zone.end
                }
              />{" "}
              {zone.value !== null && zone.value !== undefined ? `${site.name}=${zone.value} ` : ""}
              <span className="dim">
                {threadName(summary.threads, zone.thread)} at{" "}
                {formatInstant(zone.start, Math.max(zone.end - zone.start, 1) / 10)}
              </span>
            </li>
          ))}
        </ul>
      )}
      {site.label ? (
        <>
          <div className="pane-head">
            Values
            <button
              type="button"
              className="tb ghost"
              aria-pressed={look.bl === site.name}
              title="Group stacks by this label's value"
              onClick={rootByValue}
            >
              By {site.name}
            </button>
          </div>
          <ul className="zone-values">
            {stats.values.map((value) => {
              const label = `${site.name}=${value.value}`;
              return (
                <li key={value.value}>
                  <LabelValue label={label} on={(look.lb ?? []).includes(label)} />{" "}
                  <span className="dim">
                    {formatDuration(value.total)} in {formatCount(value.count)}
                  </span>
                </li>
              );
            })}
          </ul>
        </>
      ) : null}
    </div>
  );
}

function ScopeButton({
  zf,
  label,
  title,
  disabled,
}: {
  zf: ProfileLook["zf"];
  label: string;
  title: string;
  disabled: boolean;
}) {
  const { look, setLook } = useProfile();
  const pick = useCallback(() => setLook((prev) => withLook(prev, "zf", zf), true), [setLook, zf]);
  return (
    <button
      type="button"
      title={title}
      aria-pressed={look.zf === zf}
      disabled={disabled}
      onClick={pick}
    >
      {label}
    </button>
  );
}

/** Lengths in buckets of equal ratio. A click picks one bucket's lengths
 * as the band, and a drag across several picks theirs; the band's buckets
 * stand out, and the selected zone's length is marked. */
function Histogram({
  stats,
  band,
  length,
  name,
}: {
  stats: ProfileZoneStats;
  band: [number, number] | null;
  length: number | null;
  name: string;
}) {
  const { setLook } = useProfile();
  const spans = useMemo(() => bucketSpans(stats.histogram, stats.max), [stats]);
  const most = useMemo(
    () =>
      largest(
        stats.histogram.map(([, count = 0]) => count),
        1,
      ),
    [stats],
  );
  const bars = useRef<HTMLDivElement>(null);
  // Read by each event as it comes, not as the last render saw it, so a
  // release in the same frame as a move ends where the move did.
  const drag = useRef<[number, number] | null>(null);
  const [dragging, setDragging] = useState<[number, number] | null>(null);
  const bucket = useCallback(
    (event: PointerEvent<HTMLElement>) => {
      const bounds = bars.current?.getBoundingClientRect();
      if (!bounds || spans.length === 0) {
        return 0;
      }
      const fraction = (event.clientX - bounds.left) / Math.max(bounds.width, 1);
      return Math.min(Math.max(Math.floor(fraction * spans.length), 0), spans.length - 1);
    },
    [spans.length],
  );
  const down = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) {
        return;
      }
      event.currentTarget.setPointerCapture(event.pointerId);
      const at = bucket(event);
      drag.current = [at, at];
      setDragging(drag.current);
    },
    [bucket],
  );
  const move = useCallback(
    (event: PointerEvent<HTMLDivElement>) => {
      if (drag.current) {
        drag.current = [drag.current[0], bucket(event)];
        setDragging(drag.current);
      }
    },
    [bucket],
  );
  const up = useCallback(() => {
    const was = drag.current;
    drag.current = null;
    setDragging(null);
    if (!was) {
      return;
    }
    const picked = bandOf(spans, was[0], was[1]);
    setLook((prev) => {
      const same = prev.zb === formatBand(picked);
      const { zb: _zb, bz: _bz, ...rest } = prev;
      // Picking the band again lets it go; a scope of the band follows it.
      if (same) {
        const { zf: _zf, ...unscoped } = rest;
        return rest.zf === "band" ? unscoped : rest;
      }
      return { ...rest, zb: formatBand(picked) };
    }, true);
  }, [spans, setLook]);
  const cancel = useCallback(() => {
    drag.current = null;
    setDragging(null);
  }, []);

  const picking = dragging ? [Math.min(...dragging), Math.max(...dragging)] : null;
  const marked = (at: number) => {
    const [low, high] = spans[at] ?? [0, 0];
    if (picking) {
      return at >= (picking[0] ?? 0) && at <= (picking[1] ?? 0);
    }
    return band ? high >= band[0] && low <= band[1] : null;
  };
  const rank =
    length === null ? -1 : spans.findIndex(([low, high]) => length >= low && length <= high);
  return (
    <>
      <div
        className="zone-histogram"
        ref={bars}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a chart of bars, not an image file
        role="img"
        aria-label={`Lengths of ${name}${band ? `, band ${formatDuration(band[0])} – ${formatDuration(band[1])}` : ""}`}
        onPointerDown={down}
        onPointerMove={move}
        onPointerUp={up}
        onPointerCancel={cancel}
      >
        {stats.histogram.map(([low = 0, count = 0], at) => (
          <Bar
            key={low}
            low={low}
            count={count}
            most={most}
            state={marked(at)}
            here={at === rank}
          />
        ))}
      </div>
      <div className="zone-axis muted">
        <span>{formatDuration(stats.min)}</span>
        <span>{formatDuration(stats.max)}</span>
      </div>
    </>
  );
}

function Bar({
  low,
  count,
  most,
  state,
  here,
}: {
  low: number;
  count: number;
  most: number;
  /** In the band, out of it, or no band. */
  state: boolean | null;
  here: boolean;
}) {
  const style = useMemo(
    () => ({ "--share": String(count / most) }) as CSSProperties,
    [count, most],
  );
  const kind = state === null ? "" : state ? " banded" : " unbanded";
  return (
    <span
      className={`bar${kind}${here ? " here" : ""}`}
      style={style}
      title={`${formatCount(count)} from ${formatDuration(low)}`}
    />
  );
}

/** A zone's length, which selects it: the views read its span, and the
 * lanes show it. */
function ZoneSpan({
  thread,
  start,
  end,
  chosen,
}: {
  thread: number;
  start: number;
  end: number;
  chosen: boolean;
}) {
  const { setLook } = useProfile();
  const select = useCallback(() => {
    const instance: Instance = { thread, span: [start, end] };
    setLook((prev) => withInstance(prev, chosen ? null : instance), true);
  }, [setLook, thread, start, end, chosen]);
  return (
    <button
      type="button"
      className="link-button"
      aria-pressed={chosen}
      title="Select this zone: the views read its span"
      onClick={select}
    >
      {formatDuration(end - start)}
    </button>
  );
}

/** A label's value, which keeps only the samples that carry it. */
function LabelValue({ label, on }: { label: string; on: boolean }) {
  const { setLook } = useProfile();
  const toggle = useCallback(
    () =>
      setLook((prev) => {
        const kept = (prev.lb ?? []).filter((each) => each !== label);
        const next = on ? kept : [...kept, label];
        return withLook(prev, "lb", next.length > 0 ? next : undefined);
      }, true),
    [setLook, label, on],
  );
  return (
    <button
      type="button"
      className="link-button"
      aria-pressed={on}
      title="Keep only samples with this label"
      onClick={toggle}
    >
      {label}
    </button>
  );
}

/** Why a site's stats hold no zones: what the views read leaves them out. */
function emptyWhy(ranged: boolean, threads: number): string {
  const where = [
    ...(ranged ? ["in the range"] : []),
    ...(threads > 0 ? [`on the ${threads === 1 ? "thread" : "threads"} selected`] : []),
  ];
  return where.length > 0 ? `None of its zones are ${where.join(" or ")}.` : "It has no zones.";
}
