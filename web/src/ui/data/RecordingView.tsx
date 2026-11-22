import { useNavigate } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import {
  closeData,
  dataPolicyApi,
  formatBytesPerDay,
  type PolicyRowItem,
  type RecordingResponse,
} from "../../dataPolicy";
import { toHref } from "../../history";
import { flash } from "../../tab";

const PRESETS = [
  { id: "prod", label: "prod" },
  { id: "dev", label: "dev" },
  { id: "max", label: "max" },
] as const;

const TIME_UNITS = [
  { id: "1ms", label: "1 ms" },
  { id: "1us", label: "1 µs" },
  { id: "1ns", label: "1 ns" },
] as const;

export function RecordingView(props: { org: number | null }) {
  const navigate = useNavigate();
  const [data, setData] = createSignal<RecordingResponse | null>(null);
  const [loading, setLoading] = createSignal(true);
  const [err, setErr] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [rev, setRev] = createSignal(0);

  const [burstOpen, setBurstOpen] = createSignal(false);
  const [burstProjectId, setBurstProjectId] = createSignal("");
  const [burstEnv, setBurstEnv] = createSignal("");
  const [burstCount, setBurstCount] = createSignal("");
  const [burstPreset, setBurstPreset] = createSignal("dev");
  const [burstUnit, setBurstUnit] = createSignal("1ns");
  const [burstDuration, setBurstDuration] = createSignal("300");

  let seq = 0;
  createEffect(
    () => [props.org, rev()] as const,
    ([org]) => {
      const id = ++seq;
      setLoading(true);
      setErr(null);
      void (async () => {
        try {
          const res = await dataPolicyApi.recording(org);
          if (id === seq) {
            setData(res);
            if (!burstProjectId() && res.policies[0]) {
              setBurstProjectId(String(res.policies[0].projectId));
            }
          }
        } catch (error: unknown) {
          if (id === seq) {
            setErr(error instanceof Error ? error.message : String(error));
          }
        } finally {
          if (id === seq) {
            setLoading(false);
          }
        }
      })();
    },
  );

  const projects = createMemo(() => {
    const seen = new Map<number, string>();
    for (const p of data()?.policies ?? []) {
      seen.set(p.projectId, p.projectName);
    }
    return [...seen.entries()].map(([id, name]) => ({ id, name }));
  });

  const environments = createMemo(() => {
    const set = new Set<string>();
    for (const p of data()?.policies ?? []) {
      set.add(p.environment);
    }
    return [...set];
  });

  const updateRow = async (
    row: PolicyRowItem,
    patch: { preset?: string; timeUnit?: string; machines?: number | null },
  ) => {
    const res = data();
    if (!res || !res.isOwner) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.upsertPolicy({
        orgId: res.orgId,
        projectId: row.projectId,
        environment: row.environment,
        preset: patch.preset ?? row.preset,
        timeUnit: patch.timeUnit ?? row.timeUnit,
        machines: patch.machines === undefined ? row.machines : patch.machines,
      });
      setRev((r) => r + 1);
    } catch (e: unknown) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const onCreateBurst = async (event: SubmitEvent) => {
    event.preventDefault();
    const res = data();
    if (!res || !burstProjectId()) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const created = await dataPolicyApi.createBurst({
        orgId: res.orgId,
        projectId: Number(burstProjectId()),
        environment: burstEnv() || null,
        machineCount: burstCount().trim() ? Number(burstCount()) : null,
        preset: burstPreset(),
        timeUnit: burstUnit(),
        durationSecs: Number(burstDuration()),
      });
      setBurstOpen(false);
      flash(`Started ${created.preset} burst on ${created.machineCount} machine(s).`);
      setRev((r) => r + 1);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const openSession = (sessionId: number) => {
    closeData();
    navigate(toHref(`/s/${sessionId}`));
  };

  return (
    <div class="data-panel">
      <Show when={err()}>
        {(msg) => (
          <p class="error" role="alert">
            {msg()}
          </p>
        )}
      </Show>

      <Show
        when={data()}
        fallback={<p class="muted">{loading() ? "Loading recording policy…" : ""}</p>}
      >
        {(res) => (
          <>
            <div class="data-toolbar">
              <div class="data-summary-line">
                <strong>Estimated volume:</strong>{" "}
                <span class="mono">{formatBytesPerDay(res().totalEstimatedBytesPerDay)}</span>
              </div>
              <Show when={projects().length > 0}>
                <button
                  type="button"
                  class="button compact"
                  onClick={() => setBurstOpen((v) => !v)}
                >
                  {burstOpen() ? "Close burst form" : "Record a burst…"}
                </button>
              </Show>
            </div>

            <Show when={res().limitWarnings.length > 0}>
              <output class="data-banner-warn">
                <For each={res().limitWarnings}>{(w) => <div>{w}</div>}</For>
              </output>
            </Show>

            <Show when={burstOpen()}>
              <form class="data-card-form" onSubmit={(e) => void onCreateBurst(e)}>
                <h4>Start a timed burst</h4>
                <div class="data-form-grid">
                  <label class="dash-field">
                    <span>Project</span>
                    <select
                      class="input compact"
                      value={burstProjectId()}
                      onChange={(e) => setBurstProjectId(e.currentTarget.value)}
                      required
                    >
                      <For each={projects()}>{(p) => <option value={p.id}>{p.name}</option>}</For>
                    </select>
                  </label>
                  <label class="dash-field">
                    <span>Environment</span>
                    <select
                      class="input compact"
                      value={burstEnv()}
                      onChange={(e) => setBurstEnv(e.currentTarget.value)}
                    >
                      <option value="">All environments</option>
                      <For each={environments()}>{(env) => <option value={env}>{env}</option>}</For>
                    </select>
                  </label>
                  <label class="dash-field">
                    <span>Preset</span>
                    <select
                      class="input compact"
                      value={burstPreset()}
                      onChange={(e) => setBurstPreset(e.currentTarget.value)}
                    >
                      <option value="dev">dev</option>
                      <option value="max">max</option>
                    </select>
                  </label>
                  <label class="dash-field">
                    <span>Time resolution</span>
                    <select
                      class="input compact"
                      value={burstUnit()}
                      onChange={(e) => setBurstUnit(e.currentTarget.value)}
                    >
                      <option value="1ms">1 ms</option>
                      <option value="1us">1 µs</option>
                      <option value="1ns">1 ns</option>
                    </select>
                  </label>
                  <label class="dash-field">
                    <span>Duration (seconds)</span>
                    <input
                      type="number"
                      class="input compact"
                      min="10"
                      max="3600"
                      value={burstDuration()}
                      onInput={(e) => setBurstDuration(e.currentTarget.value)}
                      required
                    />
                  </label>
                  <label class="dash-field">
                    <span>Machines (blank = all eligible)</span>
                    <input
                      type="number"
                      class="input compact"
                      min="1"
                      placeholder="all"
                      value={burstCount()}
                      onInput={(e) => setBurstCount(e.currentTarget.value)}
                    />
                  </label>
                </div>
                <div class="actions">
                  <button type="submit" class="button compact" disabled={busy()}>
                    Start burst
                  </button>
                  <button type="button" class="tb ghost" onClick={() => setBurstOpen(false)}>
                    Cancel
                  </button>
                </div>
              </form>
            </Show>

            <div class="data-table-wrap">
              <table class="data-table">
                <thead>
                  <tr>
                    <th>Project</th>
                    <th>Environment</th>
                    <th>Preset</th>
                    <th>Times</th>
                    <th>Machines</th>
                    <th class="num">Est. volume</th>
                  </tr>
                </thead>
                <tbody>
                  <Show
                    when={res().policies.length > 0}
                    fallback={
                      <tr>
                        <td colspan="6" class="muted">
                          Create a project to configure recording presets.
                        </td>
                      </tr>
                    }
                  >
                    <For each={res().policies}>
                      {(row) => (
                        <tr>
                          <td>
                            <strong>{row.projectName}</strong>
                          </td>
                          <td>{row.environment}</td>
                          <td aria-label="Preset">
                            <div class="data-seg">
                              <For each={PRESETS}>
                                {(p) => (
                                  <button
                                    type="button"
                                    class={`data-seg-btn ${row.preset === p.id ? "active" : ""}`}
                                    aria-label={`Preset ${p.label} for ${row.projectName} ${row.environment}`}
                                    disabled={!res().isOwner || busy()}
                                    onClick={() => void updateRow(row, { preset: p.id })}
                                  >
                                    {p.label}
                                  </button>
                                )}
                              </For>
                            </div>
                          </td>
                          <td aria-label="Times">
                            <div class="data-seg">
                              <For each={TIME_UNITS}>
                                {(u) => (
                                  <button
                                    type="button"
                                    class={`data-seg-btn ${row.timeUnit === u.id ? "active" : ""}`}
                                    aria-label={`Times ${u.label} for ${row.projectName} ${row.environment}`}
                                    disabled={!res().isOwner || busy()}
                                    onClick={() => void updateRow(row, { timeUnit: u.id })}
                                  >
                                    {u.label}
                                  </button>
                                )}
                              </For>
                            </div>
                          </td>
                          <td aria-label="Machines">
                            <div class="data-machine-pick">
                              <Show
                                when={res().isOwner}
                                fallback={
                                  <span>
                                    {row.machines === null
                                      ? `all ${row.totalMachines}`
                                      : `${row.machines} of ${row.totalMachines}`}
                                  </span>
                                }
                              >
                                <input
                                  type="number"
                                  class="input compact data-num-input"
                                  min="0"
                                  placeholder="all"
                                  aria-label={`Machine count for ${row.projectName} ${row.environment}`}
                                  value={row.machines ?? ""}
                                  disabled={busy()}
                                  onChange={(e) => {
                                    const raw = e.currentTarget.value.trim();
                                    void updateRow(row, {
                                      machines: raw === "" ? null : Number(raw),
                                    });
                                  }}
                                />
                                <span class="muted">of {row.totalMachines}</span>
                              </Show>
                            </div>
                          </td>
                          <td class="num mono">{formatBytesPerDay(row.estimatedBytesPerDay)}</td>
                        </tr>
                      )}
                    </For>
                  </Show>
                </tbody>
              </table>
            </div>

            <section class="data-section">
              <div class="data-section-head">
                <h3>Bursts</h3>
                <span class="muted">Timed high-detail captures linked to a session</span>
              </div>
              <Show
                when={res().bursts.length > 0}
                fallback={<p class="muted">No bursts recorded yet.</p>}
              >
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Project</th>
                      <th>Preset / Times</th>
                      <th>Machines</th>
                      <th>Duration</th>
                      <th>Status</th>
                      <th>Session</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={res().bursts}>
                      {(b) => (
                        <tr>
                          <td>
                            <strong>{b.projectName}</strong>
                            <Show when={b.environment}>
                              {(env) => <span class="muted"> ({env()})</span>}
                            </Show>
                          </td>
                          <td class="mono">
                            {b.preset} · {b.timeUnit}
                          </td>
                          <td>{b.machineCount}</td>
                          <td>{b.durationSecs}s</td>
                          <td>
                            <span class={`data-pill ${b.active ? "ok" : ""}`}>
                              {b.active ? "Active" : "Finished"}
                            </span>
                          </td>
                          <td>
                            <Show when={b.sessionId}>
                              {(sid) => (
                                <button type="button" class="tb" onClick={() => openSession(sid())}>
                                  Open session
                                </button>
                              )}
                            </Show>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </Show>
            </section>
          </>
        )}
      </Show>
    </div>
  );
}
