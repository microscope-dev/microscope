import {
  DEFAULT_SCOPE,
  RANGE_PRESETS,
  REFRESH_OPTIONS,
  type RefreshInterval,
  type ViewScope,
} from "./types";

export function normalizeScope(raw: Partial<ViewScope> | null | undefined): ViewScope {
  if (!raw) {
    return { ...DEFAULT_SCOPE };
  }
  return {
    org_id: raw.org_id ?? null,
    project_ids: Array.isArray(raw.project_ids) ? raw.project_ids : [],
    filters: Array.isArray(raw.filters) ? raw.filters : [],
    time_range: raw.time_range ?? DEFAULT_SCOPE.time_range,
    refresh: raw.refresh ?? "m1",
    compare: raw.compare ?? null,
    selection: raw.selection ?? null,
  };
}

/**
 * Serializes a `ViewScope` to URL search params, omitting the default `m1`
 * refresh interval and `custom` compare specs (which do not fit compactly in
 * query params).
 */
export function scopeToSearch(scope: ViewScope): string {
  const params = new URLSearchParams();
  if (scope.org_id !== null && scope.org_id !== undefined) {
    params.set("org", String(scope.org_id));
  }
  if (scope.project_ids.length > 0) {
    params.set("projects", scope.project_ids.join(","));
  }
  for (const f of scope.filters) {
    params.append("f", `${f.dimension}:${f.value}`);
  }
  const tr = scope.time_range;
  if (tr.kind === "relative") {
    const preset = RANGE_PRESETS.find((p) => p.secs === tr.duration_secs);
    params.set("range", preset ? preset.slug : `${tr.duration_secs}s`);
  } else {
    params.set("start", String(tr.start_time));
    params.set("end", String(tr.end_time));
  }
  if (scope.refresh !== "m1") {
    params.set("refresh", scope.refresh);
  }
  if (scope.compare) {
    if (scope.compare.kind === "shift") {
      params.set("cmp", `shift:${scope.compare.offset_secs}`);
    } else if (scope.compare.kind !== "custom") {
      params.set("cmp", scope.compare.kind);
    }
  }
  if (scope.selection) {
    if (scope.selection.kind === "function") {
      params.set("fn", scope.selection.name);
    } else if (scope.selection.kind === "zone") {
      params.set("zone", scope.selection.name);
    } else if (scope.selection.name) {
      params.set("syscall", scope.selection.name);
    }
  }
  const text = params.toString();
  return text === "" ? "" : `?${text}`;
}

export function searchToScope(search: string, base: ViewScope): ViewScope {
  const params = new URLSearchParams(search.startsWith("?") ? search.slice(1) : search);
  if ([...params.keys()].length === 0) {
    return base;
  }
  const next: ViewScope = {
    ...base,
    filters: [...base.filters],
    project_ids: [...base.project_ids],
  };
  const org = params.get("org");
  if (org !== null && /^[1-9]\d*$/u.test(org)) {
    next.org_id = Number(org);
  }
  const projects = params.get("projects");
  if (projects !== null) {
    next.project_ids = projects
      .split(",")
      .map((s) => Number(s.trim()))
      .filter((n) => Number.isInteger(n) && n > 0);
  }
  const fList = params.getAll("f");
  if (fList.length > 0) {
    next.filters = [];
    for (const pair of fList) {
      const idx = pair.indexOf(":");
      if (idx > 0) {
        next.filters.push({ dimension: pair.slice(0, idx), value: pair.slice(idx + 1) });
      }
    }
  }
  const start = params.get("start");
  const end = params.get("end");
  const range = params.get("range");
  if (start !== null && end !== null && /^\d+$/u.test(start) && /^\d+$/u.test(end)) {
    next.time_range = { kind: "absolute", start_time: Number(start), end_time: Number(end) };
  } else if (range !== null) {
    const preset = RANGE_PRESETS.find((p) => p.slug === range);
    if (preset) {
      next.time_range = { kind: "relative", duration_secs: preset.secs };
    } else if (/^\d+s$/u.test(range)) {
      next.time_range = { kind: "relative", duration_secs: Number(range.slice(0, -1)) };
    }
  }
  const refresh = params.get("refresh");
  if (refresh && REFRESH_OPTIONS.some((o) => o.value === refresh)) {
    next.refresh = refresh as RefreshInterval;
  }
  const cmp = params.get("cmp");
  if (cmp === "previous_version") {
    next.compare = { kind: "previous_version" };
  } else if (cmp === "rest_of_group") {
    next.compare = { kind: "rest_of_group" };
  } else if (cmp?.startsWith("shift:")) {
    const secs = Number(cmp.slice(6));
    if (Number.isFinite(secs) && secs >= 60) {
      next.compare = { kind: "shift", offset_secs: secs };
    }
  }
  const fn = params.get("fn");
  const zone = params.get("zone");
  const syscall = params.get("syscall");
  if (fn) {
    next.selection = { kind: "function", name: fn };
  } else if (zone) {
    next.selection = { kind: "zone", name: zone };
  } else if (syscall) {
    next.selection = { kind: "syscall", name: syscall };
  }
  return next;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let idx = 0;
  while (value >= 1024 && idx < units.length - 1) {
    value /= 1024;
    idx += 1;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[idx]}`;
}

export function formatDimensionLabel(dim: string): string {
  switch (dim) {
    case "service.name":
      return "service";
    case "deployment.environment":
      return "env";
    case "service.version":
      return "version";
    case "host.name":
    case "host.id":
      return "machine";
    case "process.name":
      return "process";
    case "thread.name":
      return "thread";
    case "k8s.pod.name":
      return "pod";
    case "container.name":
      return "container";
    default:
      return dim;
  }
}

export function formatTimeShort(ns: number): string {
  const date = new Date(ns / 1_000_000);
  const hh = String(date.getHours()).padStart(2, "0");
  const mm = String(date.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

const DEPLOY_CHAR_WIDTH = 6.1;
export const DEPLOY_LANE_HEIGHT = 11;
const DEPLOY_LANES = 3;
const DEPLOY_GAP = 4;

/**
 * Greedily packs deploy marker labels into `DEPLOY_LANES` horizontal tracks so
 * overlapping labels stack cleanly. Labels that would overflow `right` flip to
 * `anchor: "end"`, and markers that collide in all tracks get `lane: null` (line only).
 */
export function placeDeployLabels<T extends { x: number; display: string }>(
  deploys: readonly T[],
  right: number,
): (T & { lane: number | null; textX: number; anchor: "start" | "end" })[] {
  const laneEnds = Array.from({ length: DEPLOY_LANES }, () => -Infinity);
  return deploys.map((deploy) => {
    const width = deploy.display.length * DEPLOY_CHAR_WIDTH;
    const fits = deploy.x + 3 + width <= right;
    const start = fits ? deploy.x + 3 : deploy.x - 3 - width;
    const lane = laneEnds.findIndex((end) => start >= end + DEPLOY_GAP);
    if (lane !== -1) {
      laneEnds[lane] = start + width;
    }
    return {
      ...deploy,
      lane: lane === -1 ? null : lane,
      textX: fits ? deploy.x + 3 : deploy.x - 3,
      anchor: fits ? "start" : "end",
    };
  });
}

export function scopeKey(scope: ViewScope, sessionId: number | null, revision: number): string {
  return `${revision}:${sessionId ?? "live"}:${JSON.stringify(scope)}`;
}

export type CopyQuerySpec =
  | { kind: "flame"; invert?: boolean; tree?: boolean }
  | { kind: "over-time"; groupBy: string }
  | { kind: "by-machine" }
  | { kind: "breakdown"; dimension: string };

function quoteShell(arg: string): string {
  if (/^[A-Za-z0-9_./:=,@+-]+$/u.test(arg)) {
    return arg;
  }
  return `'${arg.replaceAll("'", "'\"'\"'")}'`;
}

function formatDurationFlag(secs: number): string {
  if (secs > 0 && secs % 86_400 === 0) {
    return `${secs / 86_400}d`;
  }
  if (secs > 0 && secs % 3600 === 0) {
    return `${secs / 3600}h`;
  }
  if (secs > 0 && secs % 60 === 0) {
    return `${secs / 60}m`;
  }
  return `${secs}s`;
}

export function buildQueryCommand(
  spec: CopyQuerySpec,
  scope: ViewScope,
  sessionId: number | null,
  serverOrigin?: string,
): string {
  const parts: string[] = ["microscope", "query", spec.kind];

  if (spec.kind === "flame") {
    if (spec.invert) {
      parts.push("--invert");
    }
    if (spec.tree) {
      parts.push("--tree");
    }
  } else if (spec.kind === "over-time") {
    parts.push("--group-by", spec.groupBy);
  } else if (spec.kind === "breakdown") {
    parts.push("--dimension", spec.dimension);
  }

  if (serverOrigin && serverOrigin !== "https://microscope.dev") {
    parts.push("--server", serverOrigin);
  }
  if (sessionId !== null) {
    parts.push("--session", String(sessionId), "--exact-scope");
  }
  if (scope.org_id !== null && scope.org_id !== undefined) {
    parts.push("--org", String(scope.org_id));
  }
  for (const pid of scope.project_ids) {
    parts.push("--project", String(pid));
  }
  for (const f of scope.filters) {
    parts.push("--filter", `${f.dimension}=${f.value}`);
  }
  if (scope.time_range.kind === "relative") {
    parts.push("--last", formatDurationFlag(scope.time_range.duration_secs));
  } else {
    parts.push(
      "--from",
      String(scope.time_range.start_time),
      "--to",
      String(scope.time_range.end_time),
    );
  }
  if (scope.compare) {
    if (scope.compare.kind === "previous_version") {
      parts.push("--compare", "previous-version");
    } else if (scope.compare.kind === "rest_of_group") {
      parts.push("--compare", "rest-of-group");
    } else if (scope.compare.kind === "shift") {
      parts.push("--compare", `shift:${formatDurationFlag(scope.compare.offset_secs)}`);
    } else if (scope.compare.kind === "custom") {
      parts.push(
        "--compare",
        `custom:${JSON.stringify({
          filters: scope.compare.filters,
          time_range: scope.compare.time_range ?? null,
        })}`,
      );
    }
  }
  if (scope.selection) {
    if (scope.selection.kind === "function") {
      parts.push("--select-function", scope.selection.name);
    } else if (scope.selection.kind === "zone") {
      parts.push("--select-zone", scope.selection.name);
    } else if (scope.selection.kind === "syscall") {
      const sel = scope.selection;
      const syscallSpec =
        sel.abi !== null && sel.abi !== undefined && sel.number !== null && sel.number !== undefined
          ? sel.name
            ? `${sel.abi}:${sel.number}:${sel.name}`
            : `${sel.abi}:${sel.number}`
          : (sel.name ?? "");
      if (syscallSpec) {
        parts.push("--select-syscall", syscallSpec);
      }
    }
  }

  return parts.map((arg) => quoteShell(arg)).join(" ");
}

export function buildCurlCommand(
  spec: CopyQuerySpec,
  scope: ViewScope,
  sessionId: number | null,
  serverOrigin: string,
): string {
  const origin = serverOrigin.replace(/\/+$/u, "");
  const path =
    spec.kind === "flame"
      ? "/api/query/flame"
      : spec.kind === "over-time"
        ? "/api/query/over-time"
        : spec.kind === "by-machine"
          ? "/api/query/by-machine"
          : "/api/query/breakdown";

  const body: Record<string, unknown> = { scope };
  if (sessionId !== null) {
    body.session_id = sessionId;
  }
  if (spec.kind === "flame" && spec.invert) {
    body.invert = true;
  } else if (spec.kind === "over-time") {
    body.group_by = spec.groupBy;
  } else if (spec.kind === "breakdown") {
    body.dimension = spec.dimension;
  }

  return [
    "curl",
    "-sS",
    "-X",
    "POST",
    quoteShell(`${origin}${path}`),
    "-H",
    '"Authorization: Bearer $MICROSCOPE_TOKEN"',
    "-H",
    "'Content-Type: application/json'",
    "-d",
    quoteShell(JSON.stringify(body)),
  ].join(" ");
}
