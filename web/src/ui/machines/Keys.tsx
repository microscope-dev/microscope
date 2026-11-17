import { type ChangeEvent, useCallback, useState } from "react";

import { ago, day, type EnrollmentKey, fleetApi, labelText } from "../../fleet";
import { Ask, OwnerButton } from "./parts";

function RevokeKey(props: { enrollmentKey: EnrollmentKey; done: () => void; cancel: () => void }) {
  const { enrollmentKey, done, cancel } = props;
  const [machines, setMachines] = useState(false);
  const tick = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setMachines(event.target.checked),
    [],
  );
  const act = useCallback(
    () => fleetApi.revokeKey(enrollmentKey.id, machines),
    [enrollmentKey.id, machines],
  );
  const live = enrollmentKey.live;
  return (
    <Ask
      title={`Revoke the key ${enrollmentKey.name}?`}
      verb="Revoke key"
      danger
      act={act}
      done={done}
      cancel={cancel}
    >
      <p>
        New machines can no longer enroll with it.
        {live > 0 &&
          (live === 1
            ? " The live machine it enrolled keeps running unless you revoke it too."
            : ` The ${live} live machines it enrolled keep running unless you revoke them too.`)}
      </p>
      {live > 0 && (
        <label className="machines-check">
          <input type="checkbox" checked={machines} onChange={tick} />
          <span>
            {live === 1 ? "Also revoke that machine." : `Also revoke those ${live} machines.`} Each
            stops within 15 minutes and needs a new key to come back.
          </span>
        </label>
      )}
    </Ask>
  );
}

function KeyRow(props: {
  enrollmentKey: EnrollmentKey;
  owner: boolean;
  now: number;
  revoke: (key: EnrollmentKey) => void;
}) {
  const { enrollmentKey: key, owner, now, revoke } = props;
  const ask = useCallback(() => revoke(key), [revoke, key]);
  return (
    <tr className={key.revoked_at === null ? "" : "revoked"}>
      <td className="mono">{key.name}</td>
      <td>{key.environment ?? ""}</td>
      <td className="mono">{labelText(key.labels ?? {})}</td>
      <td className="n">
        {key.live.toLocaleString("en-US")} / {key.uses.toLocaleString("en-US")}
      </td>
      <td>{key.max_uses === null ? "no limit" : `${key.uses} of ${key.max_uses}`}</td>
      <td>{key.expires_at === null ? "never" : day(key.expires_at)}</td>
      <td>{key.last_used_at === null ? "never" : ago(key.last_used_at, now)}</td>
      <td>
        {key.revoked_at === null ? (
          <OwnerButton owner={owner} className="button small danger" onClick={ask}>
            Revoke
          </OwnerButton>
        ) : (
          <span className="muted">revoked {day(key.revoked_at)}</span>
        )}
      </td>
    </tr>
  );
}

export function KeysView(props: {
  keys: EnrollmentKey[];
  owner: boolean;
  now: number;
  changed: () => void;
}) {
  const { keys, owner, now, changed } = props;
  const [revoking, setRevoking] = useState<EnrollmentKey | null>(null);
  const cancel = useCallback(() => setRevoking(null), []);
  const done = useCallback(() => {
    setRevoking(null);
    changed();
  }, [changed]);
  if (keys.length === 0) {
    return (
      <p className="machines-empty">
        No enrollment keys yet. A key lets machines join this organization; make one with New key.
      </p>
    );
  }
  return (
    <div className="machines-table">
      <table className="table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Environment</th>
            <th>Labels</th>
            <th className="n">Machines live / enrolled</th>
            <th>Uses</th>
            <th>Expires</th>
            <th>Last used</th>
            <th>
              <span className="visually-hidden">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {keys.map((key) => (
            <KeyRow key={key.id} enrollmentKey={key} owner={owner} now={now} revoke={setRevoking} />
          ))}
        </tbody>
      </table>
      {revoking && <RevokeKey enrollmentKey={revoking} done={done} cancel={cancel} />}
    </div>
  );
}
