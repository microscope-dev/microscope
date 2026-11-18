import { type ReactNode, useCallback, useState } from "react";

import { copyText } from "../../clipboard";
import {
  ago,
  type Detail,
  ENVIRONMENT,
  fleetApi,
  lookHash,
  HOME,
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

function Fact({ name, children }: { name: string; children: ReactNode }) {
  return (
    <>
      <dt>{name}</dt>
      <dd>{children}</dd>
    </>
  );
}

function Where({ detail }: { detail: Detail }) {
  const cloud = detail.inventory.cloud;
  const node = detail.inventory.kubernetes?.node;
  return (
    <section>
      <h4>Where</h4>
      <dl>
        <Fact name="Environment">{detail.tags[ENVIRONMENT] ?? "none"}</Fact>
        {otherTags(detail.tags).map(([key, value]) => (
          <Fact key={key} name={key}>
            <span className="mono">{value}</span>
          </Fact>
        ))}
        {node && (
          <Fact name="Kubernetes node">
            <span className="mono">{node}</span>
          </Fact>
        )}
        {cloud && (
          <Fact name={cloud.provider.toUpperCase()}>
            {[cloud.zone ?? cloud.region, cloud.instance_type].filter(Boolean).join(" · ")}
          </Fact>
        )}
        {cloud?.instance_id && (
          <Fact name="Instance">
            <span className="mono">{cloud.instance_id}</span>
          </Fact>
        )}
      </dl>
    </section>
  );
}

function Network({ detail }: { detail: Detail }) {
  const network = detail.inventory.network;
  const primary = network?.primary;
  const hidden = Object.entries(network?.hidden ?? {});
  return (
    <section>
      <h4>Network</h4>
      {network ? (
        <dl>
          {primary && (
            <Fact name="Primary">
              <span className="mono">{primary.address}</span>
              {primary.interface && ` on ${primary.interface}`}
            </Fact>
          )}
          {detail.egress_ip && (
            <Fact name="Egress">
              <span className="mono">{detail.egress_ip}</span>
            </Fact>
          )}
          {network.interfaces.map((link) => (
            <Fact key={link.name} name={link.name}>
              <span className="mono">
                {[link.mac, ...(link.addresses ?? [])].filter(Boolean).join(" · ") ||
                  "no addresses"}
              </span>
              {link.state && link.state !== "up" && <span className="muted"> ({link.state})</span>}
            </Fact>
          ))}
        </dl>
      ) : (
        <p className="muted">This machine&apos;s agent does not report its network.</p>
      )}
      {hidden.length > 0 && (
        <p className="muted">
          {hidden.map(([kind, count]) => `${count} ${HIDDEN_LABEL[kind] ?? kind}`).join(", ")} not
          listed
        </p>
      )}
      {network?.own_namespace && (
        <p className="machines-warning-text">
          The agent runs in its own network namespace, so these are not the host&apos;s addresses.
        </p>
      )}
    </section>
  );
}

function Host({ detail }: { detail: Detail }) {
  const system = detail.inventory.system;
  const capabilities = detail.inventory.capabilities;
  const container = detail.inventory.container;
  return (
    <section>
      <h4>Host</h4>
      <dl>
        {system && (
          <Fact name="OS">
            {[system.os, system.kernel && `kernel ${system.kernel}`, system.arch]
              .filter(Boolean)
              .join(" · ")}
          </Fact>
        )}
        {system && (system.cpus || system.memory_mib) && (
          <Fact name="Size">
            {[system.cpus && `${system.cpus} CPUs`, system.memory_mib && memory(system.memory_mib)]
              .filter(Boolean)
              .join(" · ")}
          </Fact>
        )}
        {capabilities && (
          <Fact name="Can record">
            {capabilities.length === 0
              ? "only its own processes"
              : capabilities.map((name) => CAPABILITY_LABEL[name] ?? name).join(", ")}
          </Fact>
        )}
        {container && (
          <Fact name="Container">
            {container.runtime}
            {container.host_pids ? ", sees the host's processes" : ", sees only its own processes"}
          </Fact>
        )}
        <Fact name="Agent">
          <span className="mono">{detail.agent_version}</span>
        </Fact>
      </dl>
    </section>
  );
}

function History({ detail, now }: { detail: Detail; now: number }) {
  return (
    <section>
      <h4>History</h4>
      <p>
        Enrolled {ago(detail.created_at, now)}
        {detail.key ? ` with key ${detail.key.name}.` : "."}
        {detail.replaces &&
          ` Replaces ${detail.replaces.hostname}, which ${
            detail.replaces.stopped_at === null
              ? `was last seen ${ago(detail.replaces.last_seen_at, now)}`
              : `stopped ${ago(detail.replaces.stopped_at, now)}`
          } on the same host.`}
      </p>
      {detail.events.map((event) => (
        <p key={event.kind}>
          {EVENT_LABEL[event.kind] ?? event.kind}
          {event.count > 1
            ? `, ${event.count} times, last ${ago(event.last_at, now)}.`
            : `, ${ago(event.last_at, now)}.`}
        </p>
      ))}
      {detail.shares_host_ids.map((shared) => (
        <p key={`${shared.kind} ${shared.source}`} className="machines-warning-text">
          {sharedText(shared)}
        </p>
      ))}
    </section>
  );
}

function Actions(props: { detail: Detail; owner: boolean; changed: () => void }) {
  const { detail, owner, changed } = props;
  const [asking, setAsking] = useState(false);
  const [copied, setCopied] = useState<boolean | null>(null);
  const revoke = useCallback(() => setAsking(true), []);
  const cancel = useCallback(() => setAsking(false), []);
  const revokeIt = useCallback(() => fleetApi.revoke(detail.id), [detail.id]);
  const revoked = useCallback(() => {
    setAsking(false);
    changed();
  }, [changed]);
  const copy = useCallback(() => {
    const link = `${window.location.origin}${window.location.pathname}${window.location.search}#${lookHash({ ...HOME, view: "list", machine: detail.id })}`;
    void copyText(link).then(setCopied);
  }, [detail.id]);
  return (
    <div className="actions">
      {detail.state !== "revoked" && (
        <OwnerButton owner={owner} className="button danger" onClick={revoke}>
          Revoke
        </OwnerButton>
      )}
      <button type="button" className="button" onClick={copy}>
        Copy link
      </button>
      {copied !== null && (
        <output className={copied ? "muted" : "error"}>
          {copied ? "Copied" : "Could not copy"}
        </output>
      )}
      {asking && (
        <Ask
          title={`Revoke ${detail.hostname}?`}
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
      )}
    </div>
  );
}

function Panel(props: { detail: Detail; owner: boolean; now: number; changed: () => void }) {
  const { detail, now } = props;
  const since =
    detail.state === "revoked" && detail.revoked_at !== null
      ? `revoked ${ago(detail.revoked_at, now)}`
      : detail.state === "stopped" && detail.stopped_at !== null
        ? `stopped ${ago(detail.stopped_at, now)}`
        : `seen ${ago(detail.last_seen_at, now)}`;
  return (
    <>
      <h3 className="mono">{detail.hostname}</h3>
      <p className="machines-status">
        <Dot state={detail.state} />
        {STATE_LABEL[detail.state]}, {since} · key{" "}
        <span className="mono" title={detail.fingerprint}>
          {shortFingerprint(detail.fingerprint)}
        </span>
      </p>
      {detail.warnings.length > 0 && (
        <p className="machines-warnings">
          {detail.warnings.map((warning) => (
            <span key={warning} className={`machines-warning ${warning}`}>
              {WARNING_LABEL[warning]}
            </span>
          ))}
        </p>
      )}
      <Where detail={detail} />
      <Network detail={detail} />
      <Host detail={detail} />
      <History detail={detail} now={now} />
      <Actions detail={detail} owner={props.owner} changed={props.changed} />
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
  const { id, changed } = props;
  const loaded = useLoaded(() => fleetApi.machine(id), String(id));
  const { again } = loaded;
  const refresh = useCallback(() => {
    again();
    changed();
  }, [again, changed]);
  return (
    <aside className="machines-detail" aria-label="Machine">
      <button type="button" className="link-button machines-back" onClick={props.back}>
        Back to the list
      </button>
      <button
        type="button"
        className="machines-detail-close"
        aria-label="Close machine"
        title="Close"
        onClick={props.back}
      >
        ×
      </button>
      {loaded.data ? (
        <Panel detail={loaded.data} owner={props.owner} now={props.now} changed={refresh} />
      ) : (
        <Loading error={loaded.error} what="this machine" />
      )}
    </aside>
  );
}
