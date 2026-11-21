import { createMemo, createSignal, For, Show } from "solid-js";

import { ago, day, ENVIRONMENT, type EnrollmentKey, fleetApi, setLook, tagText } from "../../fleet";
import { Ask, OwnerButton } from "./parts";

const count = (n: number): string => n.toLocaleString("en-US");

const stops = (n: number): string =>
  n === 1
    ? "If it is running, it stops soon, and it cannot enroll again without a new key."
    : "Any that are running stop soon, and none can enroll again without a new key.";

function RevokeKey(props: { enrollmentKey: EnrollmentKey; done: () => void; cancel: () => void }) {
  const [machines, setMachines] = createSignal(false);
  const n = createMemo(() => props.enrollmentKey.unrevoked);
  const act = () => fleetApi.revokeKey(props.enrollmentKey.id, machines());
  return (
    <Ask
      title={`Revoke key ${props.enrollmentKey.name}?`}
      verb="Revoke key"
      danger
      typeToConfirm={props.enrollmentKey.name}
      act={act}
      done={() => props.done()}
      cancel={() => props.cancel()}
    >
      <p>New machines can no longer enroll with this key.</p>
      <Show when={n() > 0}>
        <p>
          {n() === 1
            ? "The machine it already enrolled is not affected unless you revoke it too."
            : `The ${count(n())} machines it already enrolled are not affected unless you revoke them too.`}
        </p>
        <label class="machines-check">
          <input
            type="checkbox"
            checked={machines()}
            onChange={(event) => setMachines(event.currentTarget.checked)}
          />
          <span>
            {n() === 1
              ? "Also revoke the machine it enrolled"
              : `Also revoke all ${count(n())} machines it enrolled`}
            <span class="machines-check-note">{stops(n())}</span>
          </span>
        </label>
      </Show>
    </Ask>
  );
}

function RevokeMachines(props: {
  enrollmentKey: EnrollmentKey;
  done: () => void;
  cancel: () => void;
}) {
  const n = createMemo(() => props.enrollmentKey.unrevoked);
  const act = () => fleetApi.revokeKey(props.enrollmentKey.id, true);
  return (
    <Ask
      title={
        n() === 1
          ? `Revoke the machine ${props.enrollmentKey.name} enrolled?`
          : `Revoke the ${count(n())} machines ${props.enrollmentKey.name} enrolled?`
      }
      verb={n() === 1 ? "Revoke machine" : "Revoke machines"}
      danger
      typeToConfirm={props.enrollmentKey.name}
      act={act}
      done={() => props.done()}
      cancel={() => props.cancel()}
    >
      <p>{stops(n())}</p>
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
  return (
    <tr class={props.enrollmentKey.revoked_at === null ? "" : "revoked"}>
      <td class="mono">{props.enrollmentKey.name}</td>
      <td>{props.enrollmentKey.tags[ENVIRONMENT] ?? ""}</td>
      <td class="mono">{tagText(props.enrollmentKey.tags)}</td>
      {/* oxlint-disable-next-line jsx-a11y/control-has-associated-label */}
      <td class="n">
        <Show when={props.enrollmentKey.live !== 0} fallback="0">
          <button
            type="button"
            class="link-button"
            title={`Show the live machines ${props.enrollmentKey.name} enrolled`}
            onClick={() => showMachines(props.enrollmentKey, "live")}
          >
            {count(props.enrollmentKey.live)}
          </button>
        </Show>
      </td>
      {/* oxlint-disable-next-line jsx-a11y/control-has-associated-label */}
      <td class="n">
        <Show when={props.enrollmentKey.uses !== 0} fallback="0">
          <button
            type="button"
            class="link-button"
            title={`Show every machine ${props.enrollmentKey.name} enrolled`}
            onClick={() => showMachines(props.enrollmentKey, "any")}
          >
            {count(props.enrollmentKey.uses)}
          </button>
        </Show>
      </td>
      <td class="n">
        {props.enrollmentKey.max_uses === null ? "none" : count(props.enrollmentKey.max_uses)}
      </td>
      <td>
        {props.enrollmentKey.expires_at === null ? "never" : day(props.enrollmentKey.expires_at)}
      </td>
      <td>
        {props.enrollmentKey.last_used_at === null
          ? "never"
          : ago(props.enrollmentKey.last_used_at, props.now)}
      </td>
      <td>{props.enrollmentKey.revoked_at === null ? "" : day(props.enrollmentKey.revoked_at)}</td>
      {/* oxlint-disable-next-line jsx-a11y/control-has-associated-label */}
      <td class="machines-key-action">
        <Show
          when={props.enrollmentKey.revoked_at === null}
          fallback={
            <Show when={props.enrollmentKey.unrevoked > 0}>
              <OwnerButton
                owner={props.owner}
                class="button small danger"
                onClick={() => props.revoke(props.enrollmentKey, true)}
              >
                Revoke machines
              </OwnerButton>
            </Show>
          }
        >
          <OwnerButton
            owner={props.owner}
            class="button small danger"
            onClick={() => props.revoke(props.enrollmentKey, false)}
          >
            Revoke
          </OwnerButton>
        </Show>
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
  const [revoking, setRevoking] = createSignal<{
    key: EnrollmentKey;
    machinesOnly: boolean;
  } | null>(null);
  const revoke = (key: EnrollmentKey, machinesOnly: boolean) => setRevoking({ key, machinesOnly });
  const cancel = () => setRevoking(null);
  const done = () => {
    setRevoking(null);
    props.changed();
  };
  return (
    <Show
      when={props.keys.length > 0}
      fallback={
        <p class="machines-empty">
          No enrollment keys yet. A key lets machines join this organization; make one with New key.
        </p>
      }
    >
      <div class="machines-table">
        <table class="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Environment</th>
              <th>Tags</th>
              <th class="n">Live</th>
              <th class="n">Enrolled</th>
              <th class="n">Limit</th>
              <th>Expires</th>
              <th>Last used</th>
              <th>Revoked</th>
              <th>
                <span class="visually-hidden">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            <For each={props.keys}>
              {(key) => (
                <KeyRow enrollmentKey={key} owner={props.owner} now={props.now} revoke={revoke} />
              )}
            </For>
          </tbody>
        </table>
        <Show when={revoking()}>
          {(target) => (
            <Show
              when={target().machinesOnly}
              fallback={<RevokeKey enrollmentKey={target().key} done={done} cancel={cancel} />}
            >
              <RevokeMachines enrollmentKey={target().key} done={done} cancel={cancel} />
            </Show>
          )}
        </Show>
      </div>
    </Show>
  );
}
