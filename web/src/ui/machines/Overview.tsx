import { useCallback } from "react";

import { type Group, type Overview, setLook } from "../../fleet";

function GroupRow({ group }: { group: Group }) {
  const open = useCallback(
    () =>
      setLook({
        view: "list",
        q: "",
        state: null,
        environment: group.environment,
        cluster: group.cluster,
        version: null,
        machine: null,
      }),
    [group],
  );
  const quiet = useCallback(
    () =>
      setLook({
        view: "list",
        q: "",
        state: "quiet",
        environment: group.environment,
        cluster: group.cluster,
        version: null,
        machine: null,
      }),
    [group],
  );
  const range =
    group.live_min_7d === group.live_max_7d
      ? String(group.live_max_7d)
      : `${group.live_min_7d} to ${group.live_max_7d}`;
  return (
    <li className="machines-group">
      <button
        type="button"
        className="machines-row"
        aria-label={`${group.environment ?? "no environment"}, ${group.cluster ?? "not Kubernetes"}: ${group.live} live now, ${range} live over the last 7 days, ${group.enrolled_24h} enrolled and ${group.stopped_24h} stopped in 24 hours`}
        onClick={open}
      >
        <span className={group.environment ? "" : "muted"}>
          {group.environment ?? "no environment"}
        </span>
        <span className={group.cluster ? "mono" : "muted"}>
          {group.cluster ?? "not Kubernetes"}
        </span>
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

export function OverviewView({ overview }: { overview: Overview }) {
  if (overview.groups.length === 0) {
    return (
      <p className="machines-empty">
        No machines are live right now. Search above to find ones that stopped.
      </p>
    );
  }
  return (
    <>
      <div className="machines-rows overview">
        <div className="machines-row head" aria-hidden="true">
          <span>Environment</span>
          <span>Cluster</span>
          <span className="n">Live now</span>
          <span className="n">Live range, 7 days</span>
          <span className="n">Enrolled, 24 h</span>
          <span className="n">Stopped, 24 h</span>
        </div>
        <ul aria-label="Groups">
          {overview.groups.map((group) => (
            <GroupRow key={`${group.environment}\u0000${group.cluster}`} group={group} />
          ))}
        </ul>
      </div>
      <p className="note">
        Machines outside Kubernetes group by environment alone. Opening a row shows its machines.
      </p>
    </>
  );
}
