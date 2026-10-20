import { useMemo } from "react";
import { formatRange, rangeOf, withLook } from "../../profile/look";
import { formatCount, formatDuration, formatInstant, largest } from "../../profile/paint";
import { threadName } from "../../profile/zones";
import type { ProfileZoneSite } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Duration } from "./parts";

/** Every zone site and label key, and one's spread of lengths. */
export function FindZone() {
  const { summary, look, setLook } = useProfile();
  const chosen = summary.zoneSites.find((site) => site.name === look.zs);
  return (
    <div className="pane find-zone" data-testid="find-zone">
      <div className="pane-head">
        Find zone<span className="count">{formatCount(summary.zoneSites.length)}</span>
      </div>
      <div className="pane-body">
        <table className="zone-sites">
          <thead>
            <tr>
              <th>zone</th>
              <th>count</th>
              <th>total</th>
              <th>longest</th>
            </tr>
          </thead>
          <tbody>
            {summary.zoneSites.map((site) => (
              <SiteRow
                key={`${site.label}:${site.name}`}
                site={site}
                chosen={site.name === look.zs}
                choose={() =>
                  setLook((look) =>
                    withLook(look, "zs", look.zs === site.name ? undefined : site.name),
                  )
                }
              />
            ))}
          </tbody>
        </table>
        {chosen ? <ZoneStats site={chosen} /> : null}
      </div>
    </div>
  );
}

function SiteRow({
  site,
  chosen,
  choose,
}: {
  site: ProfileZoneSite;
  chosen: boolean;
  choose: () => void;
}) {
  return (
    <tr className={chosen ? "selected" : undefined}>
      <td>
        <button type="button" className="link-button" aria-pressed={chosen} onClick={choose}>
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

/** One site's zones in the range: their percentiles, a histogram of
 * lengths, the longest, and for a label, its values, each a filter. */
function ZoneStats({ site }: { site: ProfileZoneSite }) {
  const { profile, query, look, setLook, summary } = useProfile();
  const selected = rangeOf(look);
  const params = useMemo(
    () => ({
      profile,
      revision: query.revision ?? 0,
      name: site.name,
      ...(selected ? { range: selected } : {}),
      ...(query.threads ? { threads: query.threads } : {}),
    }),
    [profile, query.revision, site.name, selected?.[0], selected?.[1], query.threads],
  );
  const answer = useAnswer("profileZoneStats", params);
  const stats = answer.data;
  if (answer.error) {
    return <div className="empty error">{answer.error.message}</div>;
  }
  if (!stats) {
    return <div className="muted">Measuring {site.name}…</div>;
  }
  const most = largest(
    stats.histogram.map(([, count]) => count),
    1,
  );
  return (
    <div className="zone-stats" data-testid="zone-stats">
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
      <div className="zone-histogram" role="img" aria-label={`Lengths of ${site.name}`}>
        {stats.histogram.map(([low, count]) => (
          <span
            key={low}
            className="bar"
            style={{ height: `${(100 * count) / most}%` }}
            title={`${formatCount(count)} from ${formatDuration(low)}`}
          />
        ))}
      </div>
      <div className="zone-axis muted">
        <span>{formatDuration(stats.min)}</span>
        <span>{formatDuration(stats.max)}</span>
      </div>
      <div className="pane-head">Longest</div>
      <ul className="zone-longest">
        {stats.longest.map((zone) => (
          <li key={`${zone.thread}:${zone.start}`}>
            <button
              type="button"
              className="link-button"
              title="Select this zone's span"
              onClick={() =>
                setLook(
                  (look) =>
                    withLook(
                      look,
                      "range",
                      formatRange([zone.start, Math.max(zone.end, zone.start + 1)]),
                    ),
                  true,
                )
              }
            >
              {formatDuration(zone.end - zone.start)}
            </button>{" "}
            {zone.value !== null && zone.value !== undefined ? `${site.name}=${zone.value} ` : ""}
            <span className="dim">
              {threadName(summary.threads, zone.thread)} at{" "}
              {formatInstant(zone.start, Math.max(zone.end - zone.start, 1) / 10)}
            </span>
          </li>
        ))}
      </ul>
      {site.label ? (
        <>
          <div className="pane-head">
            Values
            <button
              type="button"
              className="tb ghost"
              aria-pressed={look.bl === site.name}
              title="Root each stack at its value of this label"
              onClick={() =>
                setLook(
                  (look) => withLook(look, "bl", look.bl === site.name ? undefined : site.name),
                  true,
                )
              }
            >
              By {site.name}
            </button>
          </div>
          <ul className="zone-values">
            {stats.values.map((value) => {
              const label = `${site.name}=${value.value}`;
              const on = (look.lb ?? []).includes(label);
              return (
                <li key={value.value}>
                  <button
                    type="button"
                    className="link-button"
                    aria-pressed={on}
                    title="Keep only samples that carry this label"
                    onClick={() =>
                      setLook((look) => {
                        const kept = (look.lb ?? []).filter((each) => each !== label);
                        const next = on ? kept : [...kept, label];
                        return withLook(look, "lb", next.length > 0 ? next : undefined);
                      }, true)
                    }
                  >
                    {label}
                  </button>{" "}
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
