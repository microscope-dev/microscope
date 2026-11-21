import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, For, Show } from "solid-js";

import { copyText } from "../../clipboard";
import {
  ago,
  type Detail,
  ENVIRONMENT,
  fleetApi,
  HOME,
  lookHash,
  memory,
  otherTags,
  sharedText,
} from "../../fleet";
import { Loading, useLoaded } from "../Orgs";
import { Ask, Dot, OwnerButton, STATE_LABEL, WARNING_LABEL } from "./parts";

const CAPABILITY_LABEL: Record<string, string> = {
  bpf: "BPF",
  perfmon: "perf events",
  sys_ptrace: "ptrace",
  sys_admin: "admin",
};

const EVENT_LABEL: Record<string, string> = {
  agent_died: "Its agent stopped without saying goodbye",
  host_died: "The host restarted without the agent saying goodbye",
  duplicated: "Another machine used the same key",
  split_from: "Started from a copy of another machine's disk",
  rekeyed: "Changed its key",
};

const HIDDEN_LABEL: Record<string, string> = {
  container_bridge: "container bridges",
  left_out: "interfaces left out to keep the report small",
};

const shortFingerprint = (fingerprint: string): string => {
  const [kind, hash] = fingerprint.split(":");
  return hash && hash.length > 12 ? `${kind}:${hash.slice(0, 4)}…${hash.slice(-4)}` : fingerprint;
};

function Fact(props: { name: string; children: JSX.Element }) {
  return (
    <>
      <dt>{props.name}</dt>
      <dd>{props.children}</dd>
    </>
  );
}

function Where(props: { detail: Detail }) {
  const cloud = createMemo(() => props.detail.inventory.cloud);
  const node = createMemo(() => props.detail.inventory.kubernetes?.node);
  const tags = createMemo(() => otherTags(props.detail.tags));
  return (
    <section>
      <h4>Where</h4>
      <dl>
        <Fact name="Environment">{props.detail.tags[ENVIRONMENT] ?? "none"}</Fact>
        <For each={tags()}>
          {([key, value]) => (
            <Fact name={key}>
              <span class="mono">{value}</span>
            </Fact>
          )}
        </For>
        <Show when={node()}>
          {(nodeName) => (
            <Fact name="Kubernetes node">
              <span class="mono">{nodeName()}</span>
            </Fact>
          )}
        </Show>
        <Show when={cloud()}>
          {(c) => (
            <Fact name={c().provider.toUpperCase()}>
              {[c().zone ?? c().region, c().instance_type].filter(Boolean).join(" · ")}
            </Fact>
          )}
        </Show>
        <Show when={cloud()?.instance_id}>
          {(instanceId) => (
            <Fact name="Instance">
              <span class="mono">{instanceId()}</span>
            </Fact>
          )}
        </Show>
      </dl>
    </section>
  );
}

function Network(props: { detail: Detail }) {
  const network = createMemo(() => props.detail.inventory.network);
  const primary = createMemo(() => network()?.primary);
  const hidden = createMemo(() => Object.entries(network()?.hidden ?? {}));
  return (
    <section>
      <h4>Network</h4>
      <Show
        when={network()}
        fallback={<p class="muted">This machine&apos;s agent does not report its network.</p>}
      >
        {(net) => (
          <dl>
            <Show when={primary()}>
              {(p) => (
                <Fact name="Primary">
                  <span class="mono">{p().address}</span>
                  {p().interface ? ` on ${p().interface}` : ""}
                </Fact>
              )}
            </Show>
            <Show when={props.detail.egress_ip}>
              {(egressIp) => (
                <Fact name="Egress">
                  <span class="mono">{egressIp()}</span>
                </Fact>
              )}
            </Show>
            <For each={net().interfaces}>
              {(link) => (
                <Fact name={link.name}>
                  <span class="mono">
                    {[link.mac, ...(link.addresses ?? [])].filter(Boolean).join(" · ") ||
                      "no addresses"}
                  </span>
                  <Show when={link.state && link.state !== "up"}>
                    <span class="muted"> ({link.state})</span>
                  </Show>
                </Fact>
              )}
            </For>
          </dl>
        )}
      </Show>
      <Show when={hidden().length > 0}>
        <p class="muted">
          {hidden()
            .map(([kind, count]) => `${count} ${HIDDEN_LABEL[kind] ?? kind}`)
            .join(", ")}{" "}
          not listed
        </p>
      </Show>
      <Show when={network()?.own_namespace}>
        <p class="machines-warning-text">
          The agent runs in its own network namespace, so these are not the host&apos;s addresses.
        </p>
      </Show>
    </section>
  );
}

function Host(props: { detail: Detail }) {
  const osText = createMemo(() => {
    const system = props.detail.inventory.system;
    if (!system) {
      return null;
    }
    return [system.os, system.kernel && `kernel ${system.kernel}`, system.arch]
      .filter(Boolean)
      .join(" · ");
  });
  const sizeText = createMemo(() => {
    const system = props.detail.inventory.system;
    if (!system || (!system.cpus && !system.memory_mib)) {
      return null;
    }
    return [system.cpus && `${system.cpus} CPUs`, system.memory_mib && memory(system.memory_mib)]
      .filter(Boolean)
      .join(" · ");
  });
  const capabilities = createMemo(() => props.detail.inventory.capabilities);
  const container = createMemo(() => props.detail.inventory.container);
  return (
    <section>
      <h4>Host</h4>
      <dl>
        <Show when={osText() !== null}>
          <Fact name="OS">{osText()}</Fact>
        </Show>
        <Show when={sizeText()}>{(size) => <Fact name="Size">{size()}</Fact>}</Show>
        <Show when={capabilities()}>
          {(caps) => (
            <Fact name="Can record">
              {caps().length === 0
                ? "only its own processes"
                : caps()
                    .map((name) => CAPABILITY_LABEL[name] ?? name)
                    .join(", ")}
            </Fact>
          )}
        </Show>
        <Show when={container()}>
          {(c) => (
            <Fact name="Container">
              {c().runtime}
              {c().host_pids ? ", sees the host's processes" : ", sees only its own processes"}
            </Fact>
          )}
        </Show>
        <Fact name="Agent">
          <span class="mono">{props.detail.agent_version}</span>
        </Fact>
      </dl>
    </section>
  );
}

function History(props: { detail: Detail; now: number }) {
  const replacesText = createMemo(() => {
    const replaced = props.detail.replaces;
    if (!replaced) {
      return "";
    }
    const when =
      replaced.stopped_at === null
        ? `was last seen ${ago(replaced.last_seen_at, props.now)}`
        : `stopped ${ago(replaced.stopped_at, props.now)}`;
    return ` Replaces ${replaced.hostname}, which ${when} on the same host.`;
  });
  return (
    <section>
      <h4>History</h4>
      <p>
        Enrolled {ago(props.detail.created_at, props.now)}
        {props.detail.key ? ` with key ${props.detail.key.name}.` : "."}
        {replacesText()}
      </p>
      <For each={props.detail.events}>
        {(event) => (
          <p>
            {EVENT_LABEL[event.kind] ?? event.kind}
            {event.count > 1
              ? `, ${event.count} times, last ${ago(event.last_at, props.now)}.`
              : `, ${ago(event.last_at, props.now)}.`}
          </p>
        )}
      </For>
      <For each={props.detail.shares_host_ids}>
        {(shared) => <p class="machines-warning-text">{sharedText(shared)}</p>}
      </For>
    </section>
  );
}

function Actions(props: { detail: Detail; owner: boolean; changed: () => void }) {
  const [asking, setAsking] = createSignal(false);
  const [copied, setCopied] = createSignal<boolean | null>(null);
  const revoke = () => setAsking(true);
  const cancel = () => setAsking(false);
  const revokeIt = () => fleetApi.revoke(props.detail.id);
  const revoked = () => {
    setAsking(false);
    props.changed();
  };
  const copy = () => {
    const link = `${window.location.origin}${window.location.pathname}${window.location.search}#${lookHash({ ...HOME, view: "list", machine: props.detail.id })}`;
    void copyText(link).then(setCopied);
  };
  return (
    <div class="actions">
      <Show when={props.detail.state !== "revoked"}>
        <OwnerButton owner={props.owner} class="button danger" onClick={revoke}>
          Revoke
        </OwnerButton>
      </Show>
      <button type="button" class="button" onClick={copy}>
        Copy link
      </button>
      <Show when={copied() !== null}>
        <output class={copied() ? "muted" : "error"}>
          {copied() ? "Copied" : "Could not copy"}
        </output>
      </Show>
      <Show when={asking()}>
        <Ask
          title={`Revoke ${props.detail.hostname}?`}
          verb="Revoke"
          danger
          act={revokeIt}
          done={revoked}
          cancel={cancel}
        >
          <p>
            Its agent stops soon and cannot come back with its key. To return, it needs a new
            enrollment key.
          </p>
        </Ask>
      </Show>
    </div>
  );
}

function Panel(props: { detail: Detail; owner: boolean; now: number; changed: () => void }) {
  const since = createMemo(() => {
    const d = props.detail;
    return d.state === "revoked" && d.revoked_at !== null
      ? `revoked ${ago(d.revoked_at, props.now)}`
      : d.state === "stopped" && d.stopped_at !== null
        ? `stopped ${ago(d.stopped_at, props.now)}`
        : `seen ${ago(d.last_seen_at, props.now)}`;
  });
  return (
    <>
      <h3 class="mono">{props.detail.hostname}</h3>
      <p class="machines-status">
        <Dot state={props.detail.state} />
        {STATE_LABEL[props.detail.state]}, {since()} · key{" "}
        <span class="mono" title={props.detail.fingerprint}>
          {shortFingerprint(props.detail.fingerprint)}
        </span>
      </p>
      <Show when={props.detail.warnings.length > 0}>
        <p class="machines-warnings">
          <For each={props.detail.warnings}>
            {(warning) => (
              <span class={`machines-warning ${warning}`}>{WARNING_LABEL[warning]}</span>
            )}
          </For>
        </p>
      </Show>
      <Where detail={props.detail} />
      <Network detail={props.detail} />
      <Host detail={props.detail} />
      <History detail={props.detail} now={props.now} />
      <Actions detail={props.detail} owner={props.owner} changed={props.changed} />
    </>
  );
}

export function DetailPanel(props: {
  id: number;
  owner: boolean;
  now: number;
  back: () => void;
  changed: () => void;
}) {
  const loaded = useLoaded(
    () => fleetApi.machine(props.id),
    () => String(props.id),
  );
  const refresh = () => {
    loaded.again();
    props.changed();
  };
  return (
    <aside class="machines-detail" aria-label="Machine">
      <button type="button" class="link-button machines-back" onClick={() => props.back()}>
        Back to the list
      </button>
      <button
        type="button"
        class="machines-detail-close"
        aria-label="Close machine"
        title="Close"
        onClick={() => props.back()}
      >
        ×
      </button>
      <Show when={loaded.data()} fallback={<Loading error={loaded.error()} what="this machine" />}>
        {(detail) => (
          <Panel detail={detail()} owner={props.owner} now={props.now} changed={refresh} />
        )}
      </Show>
    </aside>
  );
}
