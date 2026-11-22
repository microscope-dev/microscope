import { createEffect, createMemo, createSignal, For, onCleanup, Show } from "solid-js";

import { useAccount } from "../../account";
import {
  activeView,
  addScopeFilter,
  bumpViewRevision,
  type CompareSpec,
  dashboardsApi,
  ensureSessionUrl,
  formatDimensionLabel,
  formatTimeShort,
  openDashboards,
  openDashboardStream,
  openSaveDashboardDialog,
  RANGE_PRESETS,
  REFRESH_MS,
  REFRESH_OPTIONS,
  type RefreshInterval,
  removeScopeFilter,
  setScopeSelection,
  setSingleScopeFilter,
  type StreamPaneSpec,
  updateViewScope,
  useActiveView,
} from "../../dashboards";
import { KIND } from "../../documents";
import { ago } from "../../fleet";
import { flash } from "../../tab";
import type { Layout, Pane } from "../../workbench/layout";
import { useLayout } from "../../workbench/store";

function collectStreamPanes(layout: Layout): StreamPaneSpec[] {
  const out: StreamPaneSpec[] = [];
  for (const id of Object.keys(layout.panes).toSorted()) {
    const pane = layout.panes[id as keyof typeof layout.panes] as Pane | undefined;
    if (!pane) {
      continue;
    }
    const params = pane.params as {
      unlinkedScope?: unknown;
      invert?: boolean;
      groupBy?: string;
      dimension?: string;
    } | null;
    if (params?.unlinkedScope) {
      continue;
    }
    if (pane.kind === KIND.fleetFlame || pane.kind === KIND.fleetTree) {
      out.push({ kind: "flame", id, invert: params?.invert === true });
    } else if (pane.kind === KIND.fleetFunctions) {
      out.push({ kind: "flame", id, invert: false });
    } else if (pane.kind === KIND.fleetOverTime) {
      out.push({ kind: "over_time", id, group_by: params?.groupBy ?? "service.version" });
    } else if (pane.kind === KIND.fleetByMachine) {
      out.push({ kind: "by_machine", id });
    } else if (pane.kind === KIND.fleetBreakdown) {
      out.push({ kind: "breakdown", id, dimension: params?.dimension ?? "service.name" });
    }
  }
  return out;
}

function compareValueOf(compare: CompareSpec | null | undefined): string {
  if (!compare) {
    return "off";
  }
  if (compare.kind === "shift") {
    return `shift:${compare.offset_secs}`;
  }
  return compare.kind;
}

function parseCompareValue(val: string): CompareSpec | null {
  if (val === "off") {
    return null;
  }
  if (val === "previous_version") {
    return { kind: "previous_version" };
  }
  if (val === "rest_of_group") {
    return { kind: "rest_of_group" };
  }
  if (val.startsWith("shift:")) {
    return { kind: "shift", offset_secs: Number(val.slice(6)) };
  }
  return null;
}

function onRangeChange(event: Event & { currentTarget: HTMLSelectElement }) {
  const val = event.currentTarget.value;
  if (val.startsWith("rel:")) {
    const secs = Number(val.slice(4));
    updateViewScope((prev) => ({
      ...prev,
      time_range: { kind: "relative", duration_secs: secs },
    }));
  }
}

function onRefreshChange(event: Event & { currentTarget: HTMLSelectElement }) {
  const refresh = event.currentTarget.value as RefreshInterval;
  updateViewScope((prev) => ({ ...prev, refresh }));
}

function onCompareChange(event: Event & { currentTarget: HTMLSelectElement }) {
  const cmp = parseCompareValue(event.currentTarget.value);
  updateViewScope((prev) => ({ ...prev, compare: cmp }));
}

async function copyCurrentSessionLink() {
  try {
    const url = await ensureSessionUrl();
    if (!url) {
      return;
    }
    await navigator.clipboard.writeText(url);
    flash("Copied link to this session.");
  } catch {
    flash("Could not copy session link.");
  }
}

function onCopyLink() {
  void copyCurrentSessionLink();
}

function setMachineFilter(hostname: string | null) {
  updateViewScope((prev) => {
    const rest = prev.filters.filter(
      (f) => f.dimension !== "host.name" && f.dimension !== "host.id",
    );
    return {
      ...prev,
      filters:
        hostname === null || hostname === ""
          ? rest
          : [...rest, { dimension: "host.name", value: hostname }],
    };
  });
}

export function ScopeBar() {
  const nowAccount = useAccount();
  const isActive = useActiveView((v) => v.active);
  const title = useActiveView((v) => v.title);
  const mode = useActiveView((v) => v.mode);
  const scope = useActiveView((v) => v.scope);
  const dims = useActiveView((v) => v.dimensions);
  const locked = useActiveView((v) => v.locked);
  const pinStatus = useActiveView((v) => v.pinStatus);
  const lastFetchedAt = useActiveView((v) => v.lastFetchedAt);
  const sessionId = useActiveView((v) => v.sessionId);
  const revision = useActiveView((v) => v.revision);
  const streamActive = useActiveView((v) => v.streamActive);
  const watching = useActiveView((v) => v.watching);
  const watchedMachines = useActiveView((v) => v.watchedMachines);
  const effectiveIntervalMs = useActiveView((v) => v.effectiveIntervalMs);
  const streamPanes = useLayout(
    (layout) => collectStreamPanes(layout),
    (a, b) => JSON.stringify(a) === JSON.stringify(b),
  );

  const [addOpen, setAddOpen] = createSignal(false);
  const [addDim, setAddDim] = createSignal("k8s.pod.name");
  const [addVal, setAddVal] = createSignal("");
  const [nowSec, setNowSec] = createSignal(Math.floor(Date.now() / 1000));
  const [tabVisible, setTabVisible] = createSignal(
    typeof document === "undefined" || document.visibilityState !== "hidden",
  );
  const [flushing, setFlushing] = createSignal(false);
  let dimsReqSeq = 0;

  if (typeof document !== "undefined") {
    const onVisibility = () => {
      setTabVisible(document.visibilityState !== "hidden");
    };
    document.addEventListener("visibilitychange", onVisibility);
    onCleanup(() => {
      document.removeEventListener("visibilitychange", onVisibility);
    });
  }

  const streamKey = createMemo(() => {
    if (
      !isActive() ||
      nowAccount().kind !== "in" ||
      locked() ||
      !tabVisible() ||
      scope().time_range.kind !== "relative" ||
      scope().refresh === "off"
    ) {
      return null;
    }
    return JSON.stringify({
      scope: scope(),
      session_id: sessionId(),
      panes: streamPanes(),
    });
  });

  createEffect(streamKey, (key) => {
    if (key === null) {
      activeView.setState({
        streamActive: false,
        streamConnected: false,
        streamReady: false,
        streamKey: null,
        watching: false,
        watchedMachines: 0,
        effectiveIntervalMs: null,
        newestDataNs: null,
        serverTimeNs: null,
        streamUpdatedAt: null,
        streamedPanes: {},
      });
      return;
    }
    const reqScope = activeView.getState().scope;
    const reqSession = activeView.getState().sessionId;
    const panes = streamPanes();
    activeView.setState({
      streamActive: true,
      streamConnected: false,
      streamReady: false,
      streamKey: key,
      newestDataNs: null,
      streamedPanes: {},
    });
    let alive = true;
    const handle = openDashboardStream(
      {
        scope: reqScope,
        ...(reqSession === null ? {} : { session_id: reqSession }),
        panes,
      },
      {
        onOpen: () => {
          if (alive) {
            activeView.setState({
              streamConnected: true,
              streamKey: key,
            });
          }
        },
        onStatus: (ev) => {
          if (!alive) {
            return;
          }
          const nowMs = Date.now();
          activeView.setState({
            streamActive: true,
            streamConnected: true,
            streamReady: true,
            streamKey: key,
            watching: ev.watching,
            watchedMachines: ev.watchedMachines,
            effectiveIntervalMs: ev.effectiveIntervalMs,
            locked: ev.locked,
            newestDataNs: ev.newestDataNs,
            serverTimeNs: ev.serverTimeNs,
            streamUpdatedAt: nowMs,
            clockMs: nowMs,
          });
        },
        onPane: (ev) => {
          if (!alive) {
            return;
          }
          const nowMs = Date.now();
          activeView.setState((prev) => ({
            lastFetchedAt: nowMs,
            newestDataNs:
              ev.newestDataNs === null
                ? prev.streamKey === key
                  ? prev.newestDataNs
                  : null
                : ev.newestDataNs,
            serverTimeNs: ev.serverTimeNs,
            streamUpdatedAt: nowMs,
            clockMs: nowMs,
            streamedPanes: {
              ...(prev.streamKey === key ? prev.streamedPanes : {}),
              [ev.id]: {
                kind: ev.kind,
                data: ev.data,
                serverTimeNs: ev.serverTimeNs,
                newestDataNs: ev.newestDataNs,
                receivedAt: nowMs,
                streamKey: key,
              },
            },
          }));
        },
        onDimensions: (ev) => {
          if (!alive) {
            return;
          }
          activeView.setState({
            dimensions: ev.data,
            lastFetchedAt: Date.now(),
          });
        },
        onHeartbeat: (ev) => {
          if (!alive) {
            return;
          }
          const nowMs = Date.now();
          activeView.setState({
            watchedMachines: ev.watchedMachines,
            effectiveIntervalMs: ev.effectiveIntervalMs,
            newestDataNs: ev.newestDataNs,
            serverTimeNs: ev.serverTimeNs,
            streamUpdatedAt: nowMs,
            clockMs: nowMs,
          });
        },
        onDisconnect: () => {
          if (alive) {
            activeView.setState({
              streamActive: false,
              streamConnected: false,
              streamReady: false,
              watching: false,
              watchedMachines: 0,
              effectiveIntervalMs: null,
              streamedPanes: {},
            });
          }
        },
        onFatal: () => {
          if (alive) {
            activeView.setState({
              streamActive: false,
              streamConnected: false,
              streamReady: false,
              streamKey: null,
              watching: false,
              watchedMachines: 0,
              effectiveIntervalMs: null,
              streamedPanes: {},
            });
          }
        },
      },
    );
    return () => {
      alive = false;
      handle.close();
      activeView.setState({
        streamActive: false,
        streamConnected: false,
        streamReady: false,
        streamKey: null,
        watching: false,
        watchedMachines: 0,
        effectiveIntervalMs: null,
        streamedPanes: {},
      });
    };
  });

  createEffect(
    () => [isActive(), scope().refresh] as const,
    ([active, refresh]) => {
      if (!active) {
        return;
      }
      const tick = setInterval(() => {
        const nowMs = Date.now();
        setNowSec(Math.floor(nowMs / 1000));
        activeView.setState({ clockMs: nowMs });
      }, 2_000);
      const ms = REFRESH_MS[refresh];
      const timer = ms === null ? null : setInterval(() => bumpViewRevision(), ms);
      return () => {
        clearInterval(tick);
        if (timer !== null) {
          clearInterval(timer);
        }
      };
    },
  );

  createEffect(
    () =>
      [
        isActive(),
        streamActive(),
        scope().org_id,
        scope().project_ids.join(","),
        scope().time_range,
        sessionId(),
        revision(),
      ] as const,
    ([active, streaming, orgId]) => {
      const reqSeq = ++dimsReqSeq;
      if (!active || streaming) {
        return;
      }
      let cancelled = false;
      const cur = activeView.getState();
      if (cur.dimensions && cur.dimensions.orgId !== orgId) {
        activeView.setState({ dimensions: null });
      }
      const reqScope = cur.scope;
      const reqSession = cur.sessionId;
      const loadDims = async () => {
        try {
          const res = await dashboardsApi.queryDimensions(reqScope, reqSession);
          const latest = activeView.getState();
          if (
            !cancelled &&
            reqSeq === dimsReqSeq &&
            latest.active &&
            latest.scope.org_id === orgId
          ) {
            activeView.setState({ dimensions: res });
          }
        } catch {
          const latest = activeView.getState();
          if (
            !cancelled &&
            reqSeq === dimsReqSeq &&
            latest.active &&
            latest.scope.org_id === orgId
          ) {
            activeView.setState({ dimensions: null });
          }
        }
      };
      void loadDims();
      return () => {
        cancelled = true;
      };
    },
  );

  const onManualRefresh = () => {
    if (flushing()) {
      return;
    }
    const cur = activeView.getState();
    const shouldFlush = cur.scope.time_range.kind === "relative" && nowAccount().kind === "in";
    if (!shouldFlush) {
      bumpViewRevision();
      return;
    }
    setFlushing(true);
    const runFlush = async () => {
      try {
        const report = await dashboardsApi.flush(cur.scope, cur.sessionId);
        if (report.timedOut.length > 0) {
          const names = report.timedOut.slice(0, 3).map((m) => m.hostname);
          const extra = report.timedOut.length > 3 ? ` (+${report.timedOut.length - 3} more)` : "";
          flash(`Timed out waiting for ${names.join(", ")}${extra}.`);
        }
      } catch {
      } finally {
        setFlushing(false);
        bumpViewRevision();
      }
    };
    void runFlush();
  };

  const watchLabel = createMemo(() => {
    if (!watching()) {
      return null;
    }
    const count = watchedMachines();
    const base = count === 1 ? "1 watched" : `${count} watched`;
    const eff = effectiveIntervalMs();
    const req = REFRESH_MS[scope().refresh];
    if (eff !== null && req !== null && eff > req + 500) {
      return `${base} · ${(eff / 1000).toFixed(0)}s`;
    }
    return base;
  });

  const services = createMemo(() => dims()?.indexed["service.name"] ?? []);
  const envs = createMemo(() => dims()?.indexed["deployment.environment"] ?? []);
  const versions = createMemo(() => dims()?.indexed["service.version"] ?? []);
  const machines = createMemo(() => dims()?.indexed["host.name"] ?? []);

  const allExtraDims = createMemo(() => {
    const d = dims();
    const set = new Set<string>(["k8s.pod.name", "container.name", "process.name", "thread.name"]);
    if (d) {
      for (const key of Object.keys(d.adhoc)) {
        set.add(key);
      }
      for (const key of Object.keys(d.indexed)) {
        if (key.startsWith("tag.")) {
          set.add(key);
        }
      }
    }
    return [...set].toSorted();
  });

  const extraSuggestions = createMemo(() => {
    const d = dims();
    const key = addDim();
    return d?.adhoc[key] ?? d?.indexed[key] ?? [];
  });

  const currentVal = (dimension: string) =>
    scope().filters.find((f) => f.dimension === dimension)?.value ?? "";

  const currentMachineName = createMemo(() => {
    const byName = currentVal("host.name");
    if (byName !== "") {
      return byName;
    }
    const byId = currentVal("host.id");
    if (byId !== "") {
      return dims()?.machines.find((item) => String(item.id) === byId)?.hostname ?? "";
    }
    return "";
  });

  const filterDisplayValue = (dimension: string, value: string): string => {
    if (dimension === "host.id") {
      const m = dims()?.machines.find((item) => String(item.id) === value);
      return m ? `${m.hostname} (#${value})` : `#${value}`;
    }
    return value;
  };

  const rangeValue = createMemo(() => {
    const tr = scope().time_range;
    if (tr.kind === "relative") {
      return `rel:${tr.duration_secs}`;
    }
    return "abs";
  });

  const absLabel = createMemo(() => {
    const tr = scope().time_range;
    if (tr.kind !== "absolute") {
      return "";
    }
    return `${formatTimeShort(tr.start_time)} – ${formatTimeShort(tr.end_time)}`;
  });

  const onAddFilterSubmit = (event: SubmitEvent) => {
    event.preventDefault();
    const dim = addDim().trim();
    const val = addVal().trim();
    if (dim !== "" && val !== "") {
      addScopeFilter(dim, val);
      setAddVal("");
      setAddOpen(false);
    }
  };

  const freshness = createMemo(() => {
    const ts = lastFetchedAt();
    if (ts === null) {
      return null;
    }
    return `updated ${ago(Math.floor(ts / 1000), nowSec())}`;
  });

  const selectionLabel = createMemo(() => {
    const sel = scope().selection;
    if (!sel) {
      return null;
    }
    if (sel.kind === "function") {
      return `fn: ${sel.name}`;
    }
    if (sel.kind === "zone") {
      return `zone: ${sel.name}`;
    }
    return `syscall: ${sel.name ?? sel.number ?? ""}`;
  });

  return (
    <Show when={isActive()}>
      <section class="scope-bar" aria-label="Scope bar" data-testid="scope-bar">
        <div class="scope-bar-group">
          <button
            type="button"
            class="scope-title-btn"
            onClick={() => openDashboards(mode() === "session" ? "sessions" : "dashboards")}
            title="Switch dashboard or session"
          >
            <span class="scope-title-name">{title() || "Dashboard"}</span>
            <Show when={mode() === "session"}>
              <span class="scope-badge">session</span>
            </Show>
            <Show when={pinStatus() === "pinned"}>
              <span class="scope-badge scope-badge-pin">pinned</span>
            </Show>
          </button>

          <select
            class="scope-select"
            aria-label="Service filter"
            value={currentVal("service.name")}
            disabled={locked()}
            onChange={(e) => setSingleScopeFilter("service.name", e.currentTarget.value || null)}
          >
            <option value="">service: all</option>
            <For each={services()}>{(v) => <option value={v}>{v}</option>}</For>
          </select>

          <select
            class="scope-select"
            aria-label="Environment filter"
            value={currentVal("deployment.environment")}
            disabled={locked()}
            onChange={(e) =>
              setSingleScopeFilter("deployment.environment", e.currentTarget.value || null)
            }
          >
            <option value="">env: all</option>
            <For each={envs()}>{(v) => <option value={v}>{v}</option>}</For>
          </select>

          <select
            class="scope-select"
            aria-label="Version filter"
            value={currentVal("service.version")}
            disabled={locked()}
            onChange={(e) => setSingleScopeFilter("service.version", e.currentTarget.value || null)}
          >
            <option value="">version: all</option>
            <For each={versions()}>{(v) => <option value={v}>{v}</option>}</For>
          </select>

          <select
            class="scope-select"
            aria-label="Machine filter"
            value={currentMachineName()}
            disabled={locked()}
            onChange={(e) => setMachineFilter(e.currentTarget.value || null)}
          >
            <option value="">machine: all</option>
            <For each={machines()}>{(v) => <option value={v}>{v}</option>}</For>
          </select>

          <For each={scope().filters}>
            {(filter) => (
              <span class="scope-chip" data-testid="scope-filter-chip">
                <span>
                  {formatDimensionLabel(filter.dimension)}=
                  {filterDisplayValue(filter.dimension, filter.value)}
                </span>
                <button
                  type="button"
                  class="scope-chip-x"
                  aria-label={`Remove ${filter.dimension} filter`}
                  onClick={() => removeScopeFilter(filter.dimension, filter.value)}
                >
                  ×
                </button>
              </span>
            )}
          </For>

          <Show when={selectionLabel()}>
            {(label) => (
              <span class="scope-chip scope-chip-sel" data-testid="scope-selection-chip">
                <span>{label()}</span>
                <button
                  type="button"
                  class="scope-chip-x"
                  aria-label="Clear selection"
                  onClick={() => setScopeSelection(null)}
                >
                  ×
                </button>
              </span>
            )}
          </Show>

          <div class="scope-add-wrap">
            <button
              type="button"
              class="tb ghost scope-add-btn"
              disabled={locked()}
              onClick={() => setAddOpen((o) => !o)}
            >
              + filter
            </button>
            <Show when={addOpen()}>
              <form class="scope-add-popover" onSubmit={onAddFilterSubmit}>
                <select
                  class="input compact"
                  aria-label="Filter dimension"
                  value={addDim()}
                  onChange={(e) => setAddDim(e.currentTarget.value)}
                >
                  <For each={allExtraDims()}>{(d) => <option value={d}>{d}</option>}</For>
                </select>
                <input
                  type="text"
                  class="input compact"
                  list="scope-extra-values"
                  aria-label="Filter value"
                  placeholder="Value…"
                  value={addVal()}
                  onInput={(e) => setAddVal(e.currentTarget.value)}
                />
                <datalist id="scope-extra-values">
                  <For each={extraSuggestions()}>{(v) => <option value={v}>{v}</option>}</For>
                </datalist>
                <button type="submit" class="button compact">
                  Add
                </button>
              </form>
            </Show>
          </div>
        </div>

        <div class="scope-bar-group scope-bar-end">
          <select
            class="scope-select"
            aria-label="Compare"
            value={compareValueOf(scope().compare)}
            disabled={locked()}
            onChange={onCompareChange}
          >
            <option value="off">Compare: off</option>
            <option value="previous_version">Compare: previous version</option>
            <option value="rest_of_group">Compare: rest of group</option>
            <option value="shift:3600">Compare: 1h earlier</option>
            <option value="shift:604800">Compare: 1w earlier</option>
          </select>

          <select
            class="scope-select"
            aria-label="Time range"
            value={rangeValue()}
            disabled={locked()}
            onChange={onRangeChange}
          >
            <Show when={rangeValue() === "abs"}>
              <option value="abs">{absLabel()}</option>
            </Show>
            <For each={RANGE_PRESETS}>
              {(preset) => <option value={`rel:${preset.secs}`}>{preset.label}</option>}
            </For>
          </select>

          <select
            class="scope-select"
            aria-label="Refresh interval"
            value={scope().refresh}
            onChange={onRefreshChange}
          >
            <For each={REFRESH_OPTIONS}>
              {(opt) => <option value={opt.value}>Refresh: {opt.label}</option>}
            </For>
          </select>

          <Show when={watchLabel()}>
            {(label) => (
              <span
                class="scope-watching"
                data-testid="scope-watching"
                title="Live machines in scope posting at this refresh interval"
              >
                <span class="scope-live-dot" aria-hidden="true" />
                <span>{label()}</span>
              </span>
            )}
          </Show>

          <button type="button" class="tb ghost" disabled={flushing()} onClick={onManualRefresh}>
            {flushing() ? "Refreshing…" : "Refresh"}
          </button>

          <Show when={freshness()}>
            {(text) => <span class="muted scope-freshness">{text()}</span>}
          </Show>

          <button type="button" class="tb ghost" onClick={onCopyLink}>
            Copy link
          </button>
          <button type="button" class="tb ghost" onClick={openSaveDashboardDialog}>
            Save…
          </button>
        </div>
      </section>
    </Show>
  );
}
