import { type ChangeEvent, useCallback, useState } from "react";

import { ago, day, ENVIRONMENT, type EnrollmentKey, fleetApi, setLook, tagText } from "../../fleet";
import { Ask, OwnerButton } from "./parts";

const count = (n: number): string => n.toLocaleString("en-US");

const stops = (n: number): string =>
  n === 1
    ? "If it is running, it stops soon, and it cannot enroll again without a new key."
    : "Any that are running stop soon, and none can enroll again without a new key.";

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
  const n = enrollmentKey.unrevoked;
  return (
    <Ask
      title={`Revoke key ${enrollmentKey.name}?`}
      verb="Revoke key"
      danger
      typeToConfirm={enrollmentKey.name}
      act={act}
      done={done}
      cancel={cancel}
    >
      <p>New machines can no longer enroll with this key.</p>
      {n > 0 && (
        <p>
          {n === 1
            ? "The machine it already enrolled is not affected unless you revoke it too."
            : `The ${count(n)} machines it already enrolled are not affected unless you revoke them too.`}
        </p>
      )}
      {n > 0 && (
        <label className="machines-check">
          <input type="checkbox" checked={machines} onChange={tick} />
          <span>
            {n === 1
              ? "Also revoke the machine it enrolled"
              : `Also revoke all ${count(n)} machines it enrolled`}
            <span className="machines-check-note">{stops(n)}</span>
          </span>
        </label>
      )}
    </Ask>
  );
}

function RevokeMachines(props: {
  enrollmentKey: EnrollmentKey;
  done: () => void;
  cancel: () => void;
}) {
  const { enrollmentKey, done, cancel } = props;
  const act = useCallback(() => fleetApi.revokeKey(enrollmentKey.id, true), [enrollmentKey.id]);
  const n = enrollmentKey.unrevoked;
  return (
    <Ask
      title={
        n === 1
          ? `Revoke the machine ${enrollmentKey.name} enrolled?`
          : `Revoke the ${count(n)} machines ${enrollmentKey.name} enrolled?`
      }
      verb={n === 1 ? "Revoke machine" : "Revoke machines"}
      danger
      typeToConfirm={enrollmentKey.name}
      act={act}
      done={done}
      cancel={cancel}
    >
      <p>{stops(n)}</p>
    </Ask>
  );
}

const showMachines = (key: EnrollmentKey, state: "live" | "any") =>
  setLook({
    view: "list",
    key: key.id,
    q: "",
    state,
    environment: null,
    tags: {},
    version: null,
    machine: null,
  });

function KeyRow(props: {
  enrollmentKey: EnrollmentKey;
  owner: boolean;
  now: number;
  revoke: (key: EnrollmentKey, machinesOnly: boolean) => void;
}) {
  const { enrollmentKey: key, owner, now, revoke } = props;
  const ask = useCallback(() => revoke(key, false), [revoke, key]);
  const askMachines = useCallback(() => revoke(key, true), [revoke, key]);
  const showLive = useCallback(() => showMachines(key, "live"), [key]);
  const showAll = useCallback(() => showMachines(key, "any"), [key]);
  return (
    <tr className={key.revoked_at === null ? "" : "revoked"}>
      <td className="mono">{key.name}</td>
      <td>{key.tags[ENVIRONMENT] ?? ""}</td>
      <td className="mono">{tagText(key.tags)}</td>
      <td className="n">
        {key.live === 0 ? (
          "0"
        ) : (
          <button
            type="button"
            className="link-button"
            title={`Show the live machines ${key.name} enrolled`}
            onClick={showLive}
          >
            {count(key.live)}
          </button>
        )}
      </td>
      <td className="n">
        {key.uses === 0 ? (
          "0"
        ) : (
          <button
            type="button"
            className="link-button"
            title={`Show every machine ${key.name} enrolled`}
            onClick={showAll}
          >
            {count(key.uses)}
          </button>
        )}
      </td>
      <td className="n">{key.max_uses === null ? "none" : count(key.max_uses)}</td>
      <td>{key.expires_at === null ? "never" : day(key.expires_at)}</td>
      <td>{key.last_used_at === null ? "never" : ago(key.last_used_at, now)}</td>
      <td>{key.revoked_at === null ? "" : day(key.revoked_at)}</td>
      <td className="machines-key-action">
        {key.revoked_at === null ? (
          <OwnerButton owner={owner} className="button small danger" onClick={ask}>
            Revoke
          </OwnerButton>
        ) : (
          key.unrevoked > 0 && (
            <OwnerButton owner={owner} className="button small danger" onClick={askMachines}>
              Revoke machines
            </OwnerButton>
          )
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
  const [revoking, setRevoking] = useState<{ key: EnrollmentKey; machinesOnly: boolean } | null>(
    null,
  );
  const revoke = useCallback(
    (key: EnrollmentKey, machinesOnly: boolean) => setRevoking({ key, machinesOnly }),
    [],
  );
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
            <th>Tags</th>
            <th className="n">Live</th>
            <th className="n">Enrolled</th>
            <th className="n">Limit</th>
            <th>Expires</th>
            <th>Last used</th>
            <th>Revoked</th>
            <th>
              <span className="visually-hidden">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {keys.map((key) => (
            <KeyRow key={key.id} enrollmentKey={key} owner={owner} now={now} revoke={revoke} />
          ))}
        </tbody>
      </table>
      {revoking &&
        (revoking.machinesOnly ? (
          <RevokeMachines enrollmentKey={revoking.key} done={done} cancel={cancel} />
        ) : (
          <RevokeKey enrollmentKey={revoking.key} done={done} cancel={cancel} />
        ))}
    </div>
  );
}
