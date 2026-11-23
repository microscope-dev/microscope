import { useNavigate } from "@solidjs/router";
import { createEffect, createSignal, For, Show } from "solid-js";

import { formatBytes, openDashboards } from "../../dashboards";
import { closeData, dataPolicyApi, type StorageSettingsResponse } from "../../dataPolicy";
import { toHref } from "../../history";
import { flash } from "../../tab";

const GIB = 1024 * 1024 * 1024;

export function StorageView(props: { org: number | null }) {
  const navigate = useNavigate();
  const [data, setData] = createSignal<StorageSettingsResponse | null>(null);
  const [loading, setLoading] = createSignal(true);
  const [err, setErr] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  const [detailDays, setDetailDays] = createSignal("7");
  const [sumsMonths, setSumsMonths] = createSignal("12");
  const [sessionDays, setSessionDays] = createSignal("30");
  const [pinCapGb, setPinCapGb] = createSignal("10");

  let seq = 0;
  let lastOrg: number | null | undefined;
  createEffect(
    () => props.org,
    (org) => {
      const id = ++seq;
      if (lastOrg !== undefined && lastOrg !== org) {
        setData(null);
      }
      lastOrg = org;
      setLoading(true);
      setErr(null);
      void (async () => {
        try {
          const res = await dataPolicyApi.storage(org);
          if (id === seq) {
            setData(res);
            setDetailDays(String(res.detailDays));
            setSumsMonths(String(res.sumsMonths));
            setSessionDays(String(res.sessionDays));
            setPinCapGb(String(Math.round(res.pinCapBytes / GIB)));
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

  const onSave = async (event: SubmitEvent) => {
    event.preventDefault();
    const res = data();
    if (!res || !res.isOwner) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const updated = await dataPolicyApi.updateStorage({
        orgId: res.orgId,
        detailDays: Number(detailDays()),
        sumsMonths: Number(sumsMonths()),
        sessionDays: Number(sessionDays()),
        pinCapBytes: Math.round(Number(pinCapGb()) * GIB),
      });
      setData(updated);
      flash("Storage settings saved.");
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
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
        fallback={<p class="muted">{loading() ? "Loading storage settings…" : ""}</p>}
      >
        {(res) => (
          <div class="data-storage-grid">
            <section class="data-section">
              <h3>Retention & pinned sessions</h3>
              <form class="data-card-form" onSubmit={(e) => void onSave(e)}>
                <label class="dash-field">
                  <span>Detail retention (days)</span>
                  <input
                    type="number"
                    class="input compact"
                    min="1"
                    max="365"
                    disabled={!res().isOwner || busy()}
                    value={detailDays()}
                    onInput={(e) => setDetailDays(e.currentTarget.value)}
                  />
                </label>
                <label class="dash-field">
                  <span>Summaries retention (months)</span>
                  <input
                    type="number"
                    class="input compact"
                    min="1"
                    max="60"
                    disabled={!res().isOwner || busy()}
                    value={sumsMonths()}
                    onInput={(e) => setSumsMonths(e.currentTarget.value)}
                  />
                </label>
                <label class="dash-field">
                  <span>Session link retention (days)</span>
                  <input
                    type="number"
                    class="input compact"
                    min="1"
                    max="365"
                    disabled={!res().isOwner || busy()}
                    value={sessionDays()}
                    onInput={(e) => setSessionDays(e.currentTarget.value)}
                  />
                </label>
                <label class="dash-field">
                  <span>Pinned data cap (GiB)</span>
                  <input
                    type="number"
                    class="input compact"
                    min="0"
                    max="102400"
                    disabled={!res().isOwner || busy()}
                    value={pinCapGb()}
                    onInput={(e) => setPinCapGb(e.currentTarget.value)}
                  />
                </label>
                <Show when={res().isOwner}>
                  <div class="actions">
                    <button type="submit" class="button compact" disabled={busy()}>
                      Save retention
                    </button>
                  </div>
                </Show>
              </form>

              <div class="data-pinned-box">
                <div class="data-section-head">
                  <strong>
                    {formatBytes(res().pinnedBytes)} pinned of {formatBytes(res().pinCapBytes)}
                  </strong>
                  <button
                    type="button"
                    class="tb ghost"
                    onClick={() => {
                      closeData();
                      openDashboards("sessions", props.org);
                    }}
                  >
                    View sessions ({res().pinnedSessionCount})
                  </button>
                </div>
                <Show when={res().pinnedSessions.length > 0}>
                  <ul class="data-pinned-list">
                    <For each={res().pinnedSessions}>
                      {(s) => (
                        <li>
                          <button
                            type="button"
                            class="tb ghost"
                            onClick={() => {
                              closeData();
                              navigate(toHref(`/s/${s.id}`));
                            }}
                          >
                            {s.name ?? `Session #${s.id}`}
                          </button>
                          <span class="mono muted">{formatBytes(s.pinnedBytes)}</span>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </div>
            </section>

            <section class="data-section">
              <h3>Storage destination</h3>
              <div class="data-bucket-card active">
                <div class="data-section-head">
                  <strong>Microscope&apos;s storage</strong>
                  <span class="data-pill ok">Active</span>
                </div>
                <p class="muted">
                  Blocks are encrypted at rest and managed automatically according to the retention
                  policy on the left.
                </p>
              </div>
              <div class="data-bucket-card disabled" aria-disabled="true">
                <div class="data-section-head">
                  <strong>Your own bucket</strong>
                  <span class="data-pill">Phase 10</span>
                </div>
                <p class="muted">
                  Bring your own S3 or GCS bucket for long-term storage will be available in Phase
                  10.
                </p>
              </div>
            </section>
          </div>
        )}
      </Show>
    </div>
  );
}
