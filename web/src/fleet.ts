import { createStore, useStore } from "zustand";

import { assert } from "./assert";
import { request, type Role } from "./orgs";

export type MachineState = "live" | "quiet" | "stopped" | "revoked";

export type Warning =
  | "went_quiet"
  | "duplicated_key"
  | "old_version"
  | "cannot_record_syscalls"
  | "not_on_host_network";

export type Tags = Record<string, string>;

export const ENVIRONMENT = "environment";

export interface Group {
  environment: string;
  value: string | null;
  live: number;
  quiet: number;
  stopped: number;
  enrolled_24h: number;
  stopped_24h: number;
  went_quiet_24h: number;
  live_min_7d: number;
  live_max_7d: number;
}

export interface Overview {
  org_id: number;
  role: Role;
  agents_on: boolean;
  computed_at: number;
  live_window_secs: number;
  newest_version: string | null;
  total: { live: number; quiet: number; stopped: number; revoked: number };
  split: string | null;
  groups: Group[];
  environments: string[];
  tags: Record<string, string[]>;
  versions: string[];
}

export interface KeyRef {
  id: number;
  name: string;
}

export interface Machine {
  id: number;
  hostname: string;
  state: MachineState;
  primary_ip: string | null;
  tags: Tags;
  agent_version: string;
  created_at: number;
  last_seen_at: number;
  stopped_at: number | null;
  revoked_at: number | null;
  key: KeyRef | null;
  warnings: Warning[];
  matched: string | null;
}

export interface MachineList {
  machines: Machine[];
  next: string | null;
  newest_version: string | null;
}

export interface Interface {
  name: string;
  kind: string;
  state?: string;
  mac?: string;
  mac_local?: boolean;
  mtu?: number;
  addresses?: string[];
}

export interface Inventory {
  hostname?: string;
  agent_version?: string;
  tags?: Tags;
  cloud?: {
    provider: string;
    region?: string;
    zone?: string;
    instance_type?: string;
    instance_id?: string;
    private_ips?: string[];
    public_ips?: string[];
  };
  kubernetes?: { node?: string; node_ips?: string[] };
  network?: {
    interfaces: Interface[];
    primary?: { address: string; interface?: string };
    hidden?: Record<string, number>;
    addresses_left_out?: number;
    own_namespace?: boolean;
  };
  system?: {
    kernel?: string;
    os?: string;
    arch?: string;
    cpus?: number;
    memory_mib?: number;
  };
  capabilities?: string[];
  container?: { runtime: string; host_pids: boolean };
}

export interface MachineEvent {
  kind: string;
  count: number;
  first_at: number;
  last_at: number;
}

export interface Detail {
  id: number;
  org_id: number;
  hostname: string;
  state: MachineState;
  fingerprint: string;
  tags: Tags;
  agent_version: string;
  primary_ip: string | null;
  egress_ip: string | null;
  created_at: number;
  last_seen_at: number;
  stopped_at: number | null;
  revoked_at: number | null;
  key: KeyRef | null;
  inventory: Inventory;
  events: MachineEvent[];
  replaces: {
    id: number;
    hostname: string;
    state: MachineState;
    stopped_at: number | null;
    last_seen_at: number;
  } | null;
  shares_host_ids: SharedHostId[];
  warnings: Warning[];
}

export interface SharedHostId {
  kind: "same_image" | "same_machine";
  source: string;
  live_machines: number;
}

export interface EnrollmentKey {
  id: number;
  name: string;
  tags: Tags;
  live: number;
  unrevoked: number;
  uses: number;
  max_uses: number | null;
  created_at: number;
  created_by: string | null;
  expires_at: number | null;
  last_used_at: number | null;
  revoked_at: number | null;
}

export interface CreatedKey extends EnrollmentKey {
  secret: string;
  enroll_url: string | null;
}

export interface Arrival {
  id: number;
  hostname: string;
  primary_ip: string | null;
  tags: Tags;
  created_at: number;
}

export interface NewKey {
  org_id?: number;
  name: string;
  tags: Tags;
  max_uses?: number;
  expires_at?: number;
}

const query = (params: Record<string, string | number | null | undefined>): string => {
  const kept = Object.entries(params).filter(
    (entry): entry is [string, string | number] => entry[1] !== null && entry[1] !== undefined,
  );
  return kept.length === 0 ? "" : `?${new URLSearchParams(kept.map(([k, v]) => [k, String(v)]))}`;
};

export const fleetApi = {
  overview: (org: number | null, split: string | null) =>
    request<Overview>("GET", `/api/fleet${query({ org, split })}`),
  machines: (look: Look, after: string | null) =>
    request<MachineList>(
      "GET",
      `/api/fleet/machines${query({
        org: look.org,
        q: look.q.trim() || null,
        state: look.state,
        environment: look.environment,
        tags: tagFilter(look.tags) || null,
        version: look.version,
        key: look.key,
        after,
      })}`,
    ),
  machine: (id: number) => request<Detail>("GET", `/api/fleet/machines/${id}`),
  remove: (id: number) => request<undefined>("DELETE", `/api/fleet/machines/${id}`),
  revoke: (id: number) => request<undefined>("POST", `/api/fleet/machines/${id}/revoke`, null),
  keys: (org: number | null) => request<EnrollmentKey[]>("GET", `/api/fleet/keys${query({ org })}`),
  createKey: (key: NewKey) => request<CreatedKey>("POST", "/api/fleet/keys", key),
  revokeKey: (id: number, machines: boolean) =>
    request<{ revoked_machines: number }>("POST", `/api/fleet/keys/${id}/revoke`, { machines }),
  arrivals: (key: number) => request<Arrival[]>("GET", `/api/fleet/keys/${key}/machines`),
};

export type View = "overview" | "list" | "keys" | "add" | "newkey";

export type StateChoice = MachineState | "any";

export interface Look {
  view: View;
  org: number | null;
  q: string;
  state: StateChoice | null;
  environment: string | null;
  tags: Tags;
  version: string | null;
  key: number | null;
  split: string | null;
  machine: number | null;
}

export const HOME: Look = {
  view: "overview",
  org: null,
  q: "",
  state: null,
  environment: null,
  tags: {},
  version: null,
  key: null,
  split: null,
  machine: null,
};

const PATHS: Record<View, string> = {
  overview: "machines",
  list: "machines/list",
  keys: "machines/keys",
  add: "machines/add",
  newkey: "machines/keys/new",
};

const STATES: readonly StateChoice[] = ["live", "quiet", "stopped", "revoked", "any"];

const FILTERS = ["environment", "version"] as const;

const tagFilter = (tags: Tags): string =>
  Object.entries(tags)
    .map(([key, value]) => `${key}=${value}`)
    .join(",");

function parseTagFilter(text: string | null): Tags {
  const tags = new Map<string, string>();
  for (const pair of (text ?? "").split(",")) {
    const at = pair.indexOf("=");
    const key = pair.slice(0, at);
    if (at > 0 && key !== ENVIRONMENT) {
      tags.set(key, pair.slice(at + 1));
    }
  }
  return Object.fromEntries(tags);
}

export function lookHash(look: Look): string {
  const path = look.machine === null ? PATHS[look.view] : `machines/m/${look.machine}`;
  const params = new URLSearchParams();
  if (look.org !== null) {
    params.set("org", String(look.org));
  }
  if (look.q !== "") {
    params.set("q", look.q);
  }
  if (look.state !== null) {
    params.set("state", look.state);
  }
  for (const name of FILTERS) {
    const value = look[name];
    if (value !== null) {
      params.set(name, value);
    }
  }
  if (Object.keys(look.tags).length > 0) {
    params.set("tags", tagFilter(look.tags));
  }
  if (look.key !== null) {
    params.set("key", String(look.key));
  }
  if (look.split !== null) {
    params.set("split", look.split);
  }
  const search = params.toString();
  return search === "" ? path : `${path}?${search}`;
}

const id = (text: string | null): number | null =>
  text !== null && /^[1-9]\d{0,15}$/u.test(text) ? Number(text) : null;

export function parseLook(hash: string): Look | null {
  const text = hash.startsWith("#") ? hash.slice(1) : hash;
  const at = text.indexOf("?");
  const path = at === -1 ? text : text.slice(0, at);
  const params = new URLSearchParams(at === -1 ? "" : text.slice(at + 1));

  const machineMatch = /^machines\/m\/([1-9]\d{0,15})$/u.exec(path);
  const view = machineMatch
    ? "list"
    : (Object.entries(PATHS) as [View, string][]).find(([, known]) => known === path)?.[0];
  if (view === undefined) {
    return null;
  }

  const state = params.get("state");
  const filter = (name: (typeof FILTERS)[number]) => {
    const value = params.get(name);
    return value === null || value === "" ? null : value;
  };
  return {
    view,
    org: id(params.get("org")),
    q: params.get("q") ?? "",
    state: STATES.find((known) => known === state) ?? null,
    environment: filter("environment"),
    tags: parseTagFilter(params.get("tags")),
    version: filter("version"),
    key: id(params.get("key")),
    split: params.get("split") || null,
    machine: machineMatch ? Number(machineMatch[1]) : null,
  };
}

export const filtered = (look: Look): boolean =>
  look.state !== null ||
  look.key !== null ||
  Object.keys(look.tags).length > 0 ||
  FILTERS.some((name) => look[name] !== null);

export const clearFilters = (look: Look): Look => ({
  ...look,
  state: null,
  environment: null,
  tags: {},
  version: null,
  key: null,
});

export const shownState = (look: Look): StateChoice =>
  look.state ?? (look.q.trim() === "" ? "live" : "any");

export interface WindowState {
  open: boolean;
  look: Look;
  waiting: Look | null;
  pushed: boolean;
}

export const machinesWindow = createStore<WindowState>(() => ({
  open: false,
  look: HOME,
  waiting: null,
  pushed: false,
}));

export function useMachinesWindow<T>(pick: (window: WindowState) => T): T {
  return useStore(machinesWindow, pick);
}

export function openMachines(look: Partial<Look>): void {
  const now = machinesWindow.getState();
  const base = now.open ? now.look : { ...HOME, org: now.look.org };
  machinesWindow.setState({ open: true, look: { ...base, ...look }, waiting: null });
}

export function setLook(change: Partial<Look>): void {
  const now = machinesWindow.getState();
  assert(now.open, "the machines window changed its look while shut");
  machinesWindow.setState({ look: { ...now.look, ...change } });
}

export function closeMachines(): void {
  machinesWindow.setState({ open: false, waiting: null });
}

export function forgetPushed(): void {
  machinesWindow.setState({ pushed: false });
}

const MINUTE = 60;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

export function day(at: number): string {
  const date = new Date(at * 1000);
  return `${date.getUTCDate()} ${MONTHS[date.getUTCMonth()]} ${date.getUTCFullYear()}`;
}

export function ago(at: number, now: number): string {
  const secs = Math.max(0, Math.floor(now - at));
  if (secs < 10) {
    return "just now";
  }
  if (secs < MINUTE) {
    return `${secs} s ago`;
  }
  if (secs < HOUR) {
    return `${Math.floor(secs / MINUTE)} min ago`;
  }
  if (secs < DAY) {
    const hours = Math.floor(secs / HOUR);
    return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
  }
  const days = Math.floor(secs / DAY);
  if (days === 1) {
    return "yesterday";
  }
  if (days < 60) {
    return `${days} days ago`;
  }
  return day(at);
}

export function memory(mib: number): string {
  return mib >= 1024 ? `${Math.round(mib / 1024)} GB` : `${mib} MB`;
}

export function sharedText({ kind, source, live_machines: count }: SharedHostId): string {
  const one = count === 1;
  if (kind === "same_machine") {
    return one
      ? `One other live agent reports the same ${source}, so it runs on this machine too.`
      : `${count} other live agents report the same ${source}, so they run on this machine too.`;
  }
  return `${one ? "One other live machine has" : `${count} other live machines have`} the same ${source}. They may be copies of one disk image that was not emptied before capture.`;
}

const TAG_KEY = /^[A-Za-z0-9._\-/]{1,63}$/u;

export function parseTags(text: string): Tags | string {
  const tags = new Map<string, string>();
  for (const part of text.split(",")) {
    const pair = part.trim();
    if (pair === "") {
      continue;
    }
    const at = pair.indexOf("=");
    const key = at <= 0 ? "" : pair.slice(0, at).trim();
    const value = at <= 0 ? "" : pair.slice(at + 1).trim();
    if (key === "" || value === "") {
      return `write ${pair} as key=value`;
    }
    if (!TAG_KEY.test(key)) {
      return `${key} can only use letters, digits, and . _ - /`;
    }
    if (key === ENVIRONMENT) {
      return "set the environment in its own field";
    }
    if (tags.has(key)) {
      return `${key} is set twice`;
    }
    tags.set(key, value);
  }
  return Object.fromEntries(tags);
}

export type Install = "kubernetes" | "docker" | "systemd";

export const INSTALLS: readonly Install[] = ["kubernetes", "docker", "systemd"];

export const PLACEHOLDER = "ENROLL_URL";

const shell = (text: string): string => `'${text.replaceAll("'", `'\\''`)}'`;

export function snippet(install: Install, enroll: string): string {
  switch (install) {
    case "kubernetes":
      return [
        "kubectl create secret generic microscope-enroll \\",
        `  --from-literal=enroll=${shell(enroll)}`,
        "helm install microscope microscope/agent \\",
        "  --set enroll.secretName=microscope-enroll \\",
        "  --set tags.cluster=your-cluster-name",
      ].join("\n");
    case "docker":
      return [
        "docker run -d --name microscope-agent --pid host --network host --privileged \\",
        "  -v /var/lib/microscope:/var/lib/microscope \\",
        `  -e MICROSCOPE_ENROLL=${shell(enroll)} \\`,
        "  -e MICROSCOPE_TAGS=cluster=your-cluster-name \\",
        "  ghcr.io/microscope-dev/microscope",
      ].join("\n");
    case "systemd":
      return [
        "sudo install -d -m 0700 /etc/microscope",
        "sudo tee /etc/microscope/agent.toml > /dev/null <<'EOF'",
        `enroll = ${JSON.stringify(enroll)}`,
        "",
        "[tags]",
        'cluster = "your-cluster-name"',
        "EOF",
        "sudo systemctl enable --now microscope-agent",
      ].join("\n");
  }
}

export const otherTags = (tags: Tags): [string, string][] =>
  Object.entries(tags).filter(([key]) => key !== ENVIRONMENT);

export const tagText = (tags: Tags): string =>
  otherTags(tags)
    .map(([key, value]) => `${key}=${value}`)
    .join(", ");
