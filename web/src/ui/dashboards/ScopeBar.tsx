import { createEffect, createMemo, createSignal, For, onCleanup, Show } from "solid-js";

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
  openSaveDashboardDialog,
  RANGE_PRESETS,
  REFRESH_MS,
  REFRESH_OPTIONS,
  type RefreshInterval,
  removeScopeFilter,
  setScopeSelection,
  setSingleScopeFilter,
  updateViewScope,
  useActiveView,
} from "../../dashboards";
import { ago } from "../../fleet";
import { flash } from "../../tab";

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

export function ScopeBar() {
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

  const [addOpen, setAddOpen] = createSignal(false);
  const [addDim, setAddDim] = createSignal("k8s.pod.name");
  const [addVal, setAddVal] = createSignal("");
  const [nowSec, setNowSec] = createSignal(Math.floor(Date.now() / 1000));

  createEffect(
    () => [isActive(), scope().refresh] as const,
    ([active, refresh]) => {
      if (!active) {
        return;
      }
      const tick = setInterval(() => setNowSec(Math.floor(Date.now() / 1000)), 5_000);
      const ms = REFRESH_MS[refresh];
      const timer = ms === null ? null : setInterval(() => bumpViewRevision(), ms);
      onCleanup(() => {
        clearInterval(tick);
        if (timer !== null) {
          clearInterval(timer);
        }
      });
    },
  );

  createEffect(
    () =>
      [
        isActive(),
        scope().org_id,
        scope().project_ids.join(","),
        scope().time_range,
        sessionId(),
        revision(),
      ] as const,
    ([active]) => {
      if (!active) {
        return;
      }
      const cur = activeView.getState();
      const loadDims = async () => {
        try {
          const res = await dashboardsApi.queryDimensions(cur.scope, cur.sessionId);
          activeView.setState({ dimensions: res });
        } catch {
          activeView.setState({ dimensions: null });
        }
      };
      void loadDims();
    },
  );

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
            value={currentVal("host.name")}
            disabled={locked()}
            onChange={(e) => setSingleScopeFilter("host.name", e.currentTarget.value || null)}
          >
            <option value="">machine: all</option>
            <For each={machines()}>{(v) => <option value={v}>{v}</option>}</For>
          </select>

          <For each={scope().filters}>
            {(filter) => (
              <span class="scope-chip" data-testid="scope-filter-chip">
                <span>
                  {formatDimensionLabel(filter.dimension)}={filter.value}
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

          <button type="button" class="tb ghost" onClick={bumpViewRevision}>
            Refresh
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
