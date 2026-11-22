import { request } from "./orgs";
import { createState, useSelector } from "./state";

export type DataTab = "services" | "recording" | "privacy" | "symbols" | "storage";

export const DATA_TABS: readonly { id: DataTab; label: string }[] = [
  { id: "services", label: "Services" },
  { id: "recording", label: "Recording" },
  { id: "privacy", label: "Privacy" },
  { id: "symbols", label: "Symbols" },
  { id: "storage", label: "Storage" },
];

export interface DataProjectItem {
  id: number;
  name: string;
  isUnassigned: boolean;
}

export interface ServiceItem {
  id: number;
  name: string;
  projectId: number;
  projectName: string;
  matchedBy: string;
  machines: number;
  processesNow: number;
  versionsRunning: string[];
}

export interface RecordingRuleItem {
  id: number;
  kind: string;
  pattern: string;
  action: string;
  projectId: number | null;
  projectName: string | null;
  serviceName: string | null;
}

export interface UnassignedProgramItem {
  program: string;
  machines: number;
  processesNow: number;
  matchedBy: string;
  buildId: string | null;
}

export interface ServicesResponse {
  orgId: number;
  isOwner: boolean;
  projects: DataProjectItem[];
  services: ServiceItem[];
  rules: RecordingRuleItem[];
  unassigned: UnassignedProgramItem[];
}

export interface PolicyRowItem {
  projectId: number;
  projectName: string;
  environment: string;
  preset: string;
  timeUnit: string;
  machines: number | null;
  totalMachines: number;
  cappedMachines: number;
  estimatedBytesPerDay: number;
}

export interface EnvMachineItem {
  id: number;
  hostname: string;
  environment: string;
  maxPreset: string | null;
  minTimeUnit: string | null;
}

export interface BurstItem {
  id: number;
  projectId: number;
  projectName: string;
  environment: string | null;
  machineCount: number;
  preset: string;
  timeUnit: string;
  durationSecs: number;
  startsAt: number;
  endsAt: number;
  active: boolean;
  sessionId: number | null;
}

export interface RecordingResponse {
  orgId: number;
  isOwner: boolean;
  policies: PolicyRowItem[];
  totalEstimatedBytesPerDay: number;
  limitWarnings: string[];
  machines: EnvMachineItem[];
  bursts: BurstItem[];
}

export interface BuiltinItem {
  name: string;
  action: string;
  fields: string[];
  defaultOn: boolean;
  enabled: boolean;
  locked: boolean;
}

export interface SanitizerRuleItem {
  id: number;
  name: string;
  fields: string[];
  pattern: string;
  action: string;
  projectId: number | null;
  projectName: string | null;
}

export interface RedactionCountItem {
  rule: string;
  field: string;
  fired: number;
}

export interface PrivacyResponse {
  orgId: number;
  isOwner: boolean;
  hours: number;
  fieldClasses: string[];
  builtins: BuiltinItem[];
  rules: SanitizerRuleItem[];
  counts: RedactionCountItem[];
}

export interface PreviewResponse {
  outcome: string;
  output: string;
  matches: { rule: string; count: number }[];
}

export interface PinnedSessionItem {
  id: number;
  name: string | null;
  pinnedBytes: number;
  keptAt: number | null;
}

export interface StorageSettingsResponse {
  orgId: number;
  isOwner: boolean;
  detailDays: number;
  sumsMonths: number;
  sessionDays: number;
  pinCapBytes: number;
  pinnedBytes: number;
  pinnedSessionCount: number;
  pinnedSessions: PinnedSessionItem[];
}

export interface SymbolProjectItem {
  id: number;
  name: string;
  canWrite: boolean;
}

export type SymbolStatus =
  | "missing"
  | "uploaded_ci"
  | "uploaded_agent"
  | "public_cache"
  | "symbolized_on_host";

export interface SymbolBuildItem {
  buildId: string;
  projectId: number;
  projectName: string;
  serviceName: string;
  modulePath: string;
  gitCommit: string | null;
  status: SymbolStatus;
  sharePercent: number;
  unsymbolizedSharePercent: number;
  byteLen: number | null;
  hasDwarf: boolean;
  hasSymtab: boolean;
  symbolCount: number;
  lastSeenAt: number | null;
  uploadedAt: number | null;
  uploadCommand: string;
}

export interface MissingSummary {
  missingBuildCount: number;
  missingTimePercent: number;
}

export interface ProjectTokenItem {
  id: number;
  projectId: number;
  projectName: string;
  name: string;
  tokenPrefix: string;
  createdAt: number;
  lastUsedAt: number | null;
}

export interface SymbolsResponse {
  orgId: number;
  isOwner: boolean;
  hours: number;
  serverUrl: string;
  projects: SymbolProjectItem[];
  missingSummary: MissingSummary;
  builds: SymbolBuildItem[];
  tokens: ProjectTokenItem[];
}

export interface CreatedProjectTokenResponse {
  id: number;
  projectId: number;
  projectName: string;
  name: string;
  token: string;
  tokenPrefix: string;
  createdAt: number;
}

export interface DataWindowState {
  open: boolean;
  tab: DataTab;
  org: number | null;
  waiting: { tab: DataTab; org: number | null } | null;
  pushed: boolean;
}

export const dataWindow = createState<DataWindowState>({
  open: false,
  tab: "services",
  org: null,
  waiting: null,
  pushed: false,
});

export function useDataWindow<T>(select: (s: DataWindowState) => T): () => T {
  return useSelector(dataWindow, select);
}

export function openData(tab: DataTab = "services", org?: number | null) {
  const cur = dataWindow.getState();
  dataWindow.setState({
    open: true,
    tab,
    org: org === undefined ? cur.org : org,
    waiting: null,
  });
}

export function closeData() {
  dataWindow.setState({ open: false, waiting: null });
}

export function dataHash(tab: DataTab, org: number | null): string {
  const base = `data/${tab}`;
  return org === null ? base : `${base}?org=${org}`;
}

export function parseDataHash(hash: string): { tab: DataTab; org: number | null } | null {
  const raw = hash.startsWith("#") ? hash.slice(1) : hash;
  if (!raw.startsWith("data")) {
    return null;
  }
  const [pathPart, queryPart] = raw.split("?");
  const seg = (pathPart ?? "").split("/")[1] ?? "services";
  const tab: DataTab =
    seg === "services" ||
    seg === "recording" ||
    seg === "privacy" ||
    seg === "symbols" ||
    seg === "storage"
      ? seg
      : "services";
  let org: number | null = null;
  if (queryPart) {
    const params = new URLSearchParams(queryPart);
    const o = params.get("org");
    if (o !== null && /^\d+$/u.test(o)) {
      org = Number(o);
    }
  }
  return { tab, org };
}

function orgQuery(org: number | null, extra?: Record<string, string | number>): string {
  const params = new URLSearchParams();
  if (org !== null) {
    params.set("org", String(org));
  }
  if (extra) {
    for (const [k, v] of Object.entries(extra)) {
      params.set(k, String(v));
    }
  }
  const q = params.toString();
  return q ? `?${q}` : "";
}

export const dataPolicyApi = {
  services: (org: number | null) =>
    request<ServicesResponse>("GET", `/api/data/services${orgQuery(org)}`),
  createRule: (body: {
    orgId?: number | null;
    kind: string;
    pattern: string;
    action: string;
    projectId?: number | null;
    serviceName?: string | null;
  }) => request<RecordingRuleItem>("POST", "/api/data/rules", body),
  deleteRule: (id: number) => request<void>("DELETE", `/api/data/rules/${id}`),
  assignUnassigned: (body: {
    orgId?: number | null;
    program: string;
    projectId: number;
    serviceName: string;
    buildId?: string | null;
  }) => request<void>("POST", "/api/data/unassigned/assign", body),
  stopUnassigned: (body: { orgId?: number | null; program: string }) =>
    request<void>("POST", "/api/data/unassigned/stop", body),
  recording: (org: number | null) =>
    request<RecordingResponse>("GET", `/api/data/recording${orgQuery(org)}`),
  upsertPolicy: (body: {
    orgId?: number | null;
    projectId: number;
    environment: string;
    preset: string;
    timeUnit?: string;
    machines?: number | null;
  }) => request<void>("PUT", "/api/data/recording/policy", body),
  createBurst: (body: {
    orgId?: number | null;
    projectId: number;
    environment?: string | null;
    machineCount?: number | null;
    preset: string;
    timeUnit?: string;
    durationSecs: number;
  }) => request<BurstItem>("POST", "/api/data/recording/bursts", body),
  privacy: (org: number | null, hours = 24) =>
    request<PrivacyResponse>("GET", `/api/data/privacy${orgQuery(org, { hours })}`),
  setBuiltins: (body: { orgId?: number | null; enabledBuiltins: string[] }) =>
    request<void>("PUT", "/api/data/privacy/builtins", body),
  createSanitizerRule: (body: {
    orgId?: number | null;
    projectId?: number | null;
    name: string;
    fields: string[];
    pattern: string;
    action: string;
  }) => request<SanitizerRuleItem>("POST", "/api/data/privacy/rules", body),
  deleteSanitizerRule: (id: number) => request<void>("DELETE", `/api/data/privacy/rules/${id}`),
  previewPrivacy: (body: {
    orgId?: number | null;
    field: string;
    text: string;
    draftRule?: { name: string; fields: string[]; pattern: string; action: string } | null;
  }) => request<PreviewResponse>("POST", "/api/data/privacy/preview", body),
  storage: (org: number | null) =>
    request<StorageSettingsResponse>("GET", `/api/data/storage${orgQuery(org)}`),
  updateStorage: (body: {
    orgId?: number | null;
    detailDays?: number;
    sumsMonths?: number;
    sessionDays?: number;
    pinCapBytes?: number;
  }) => request<StorageSettingsResponse>("PATCH", "/api/data/storage", body),
  symbols: (org: number | null, hours = 24) =>
    request<SymbolsResponse>("GET", `/api/data/symbols${orgQuery(org, { hours })}`),
  createSymbolToken: (body: { orgId?: number | null; projectId: number; name: string }) =>
    request<CreatedProjectTokenResponse>("POST", "/api/data/symbols/tokens", body),
  revokeSymbolToken: (id: number) => request<void>("DELETE", `/api/data/symbols/tokens/${id}`),
};

export function formatByteSize(bytes: number | null): string {
  if (bytes === null || bytes <= 0) {
    return "—";
  }
  if (bytes >= 1_048_576) {
    return `${(bytes / 1_048_576).toFixed(1)} MiB`;
  }
  if (bytes >= 1024) {
    return `${(bytes / 1024).toFixed(1)} KiB`;
  }
  return `${bytes} B`;
}

export function formatBytesPerDay(bytes: number): string {
  if (bytes <= 0) {
    return "0 MB/day";
  }
  const gb = bytes / 1_000_000_000;
  if (gb >= 1) {
    return `${gb.toFixed(gb >= 10 ? 1 : 2)} GB/day`;
  }
  const mb = bytes / 1_000_000;
  return `${Math.round(mb)} MB/day`;
}
