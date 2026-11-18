import { useCallback, useMemo } from "react";

import { type Group, type Overview, setLook } from "../../fleet";
import { ANY, Chip } from "./parts";

function GroupRow({ group, split }: { group: Group; split: string | null }) {
  const show = useCallback(
    (state: "quiet" | null) =>
      setLook({
        view: "list",
        q: "",
        state,
        environment: group.environment,
        tags: split === null ? {} : { [split]: group.value ?? "" },
        version: null,
        machine: null,
      }),
    [group, split],
  );
  const open = useCallback(() => show(null), [show]);
  const quiet = useCallback(() => show("quiet"), [show]);
  const range =
    group.live_min_7d === group.live_max_7d
      ? String(group.live_max_7d)
      : `${group.live_min_7d} to ${group.live_max_7d}`;
  const value = group.value ?? `no ${split}`;
  return (
    <li className="machines-group">
      <button
        type="button"
        className="machines-row"
        aria-label={`${group.environment}${split === null ? "" : `, ${value}`}: ${group.live} live now, ${range} live over the last 7 days, ${group.enrolled_24h} enrolled and ${group.stopped_24h} stopped in 24 hours`}
        onClick={open}
      >
        <span>{group.environment}</span>
        {split !== null && <span className={group.value ? "mono" : "muted"}>{value}</span>}
        <span className="n">{group.live}</span>
        <span className="n">{range}</span>
        <span className="n">{group.enrolled_24h}</span>
        <span className="n">{group.stopped_24h}</span>
      </button>
      {group.went_quiet_24h > 0 && (
        <button type="button" className="machines-quiet" onClick={quiet}>
          {group.went_quiet_24h} went quiet
        </button>
      )}
    </li>
  );
}

const splitName = (value: string) => (value === ANY ? "environment only" : value);

function SplitPicker({ overview }: { overview: Overview }) {
  const keys = useMemo(() => Object.keys(overview.tags), [overview.tags]);
  const pick = useCallback(
    (value: string) => setLook({ split: value === ANY ? null : value, machine: null }),
    [],
  );
  if (keys.length === 0 && overview.split === null) {
    return null;
  }
  return (
    <div className="machines-chips">
      <Chip
        label="Split by"
        value={overview.split ?? ANY}
        options={keys}
        names={splitName}
        pick={pick}
      />
    </div>
  );
}

export function OverviewView({ overview }: { overview: Overview }) {
  const split = overview.split;
  if (overview.groups.length === 0) {
    return (
      <p className="machines-empty">
        No machines are live right now. Search above to find ones that stopped.
      </p>
    );
  }
  return (
    <>
      <SplitPicker overview={overview} />
      <div className={`machines-rows overview${split === null ? " whole" : ""}`}>
        <div className="machines-row head" aria-hidden="true">
          <span>Environment</span>
          {split !== null && <span>{split}</span>}
          <span className="n">Live now</span>
          <span className="n">Live range, 7 days</span>
          <span className="n">Enrolled, 24 h</span>
          <span className="n">Stopped, 24 h</span>
        </div>
        <ul aria-label="Groups">
          {overview.groups.map((group) => (
            <GroupRow
              key={`${group.environment}\u0000${group.value ?? ""}`}
              group={group}
              split={split}
            />
          ))}
        </ul>
      </div>
    </>
  );
}
