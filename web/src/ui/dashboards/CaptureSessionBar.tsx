import { createMemo, createSignal, Show } from "solid-js";

import { captureDownloadUrl } from "../../captures";
import { copyText } from "../../clipboard";
import {
  activeView,
  dashboardsApi,
  formatBytes,
  openDashboards,
  type PinEstimate,
  useActiveView,
} from "../../dashboards";
import { day } from "../../fleet";
import { formatCount, formatDuration } from "../../profile/paint";
import { flash } from "../../tab";

export function CaptureSessionBar() {
  const isCapture = useActiveView((v) => v.active && v.captureId !== null);
  const captureId = useActiveView((v) => v.captureId);
  const capture = useActiveView((v) => v.capture);
  const sessionId = useActiveView((v) => v.sessionId);
  const title = useActiveView((v) => v.title);
  const isOwner = useActiveView((v) => v.isOwner);
  const locked = useActiveView((v) => v.locked);
  const pinStatus = useActiveView((v) => v.pinStatus);
  const pinnedBytes = useActiveView((v) => v.pinnedBytes);

  const [estimate, setEstimate] = createSignal<PinEstimate | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);

  const deleted = createMemo(() => capture()?.deleted === true);

  const copyLink = async () => {
    const sid = sessionId();
    if (sid === null) {
      return;
    }
    const url = `${window.location.origin}/s/${sid}`;
    if (await copyText(url)) {
      flash("Copied link to this capture session.");
    } else {
      flash("Could not copy session link.");
    }
  };

  const requestEstimate = async () => {
    const sid = sessionId();
    if (sid === null) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const est = await dashboardsApi.pinEstimate(sid);
      setEstimate(est);
    } catch (failure) {
      setErr(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  const confirmKeep = async (pin: boolean) => {
    const sid = sessionId();
    if (sid === null) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const kept = await dashboardsApi.keepSession(sid, { pin });
      setEstimate(null);
      activeView.setState({
        pinStatus: kept.pinStatus,
        pinnedBytes: kept.pinnedBytes,
        keptAt: kept.keptAt,
        expiresAt: kept.expiresAt,
        capture: kept.capture ?? activeView.getState().capture,
      });
      if (kept.pinStatus === "cap_exceeded") {
        flash("Pin cap exceeded — capture kept without pinning.");
      } else if (kept.pinStatus === "pinned") {
        flash("Capture kept and pinned.");
      } else {
        flash("Capture session kept.");
      }
    } catch (failure) {
      setErr(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  const onUnpin = async () => {
    const sid = sessionId();
    if (sid === null) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await dashboardsApi.unpinSession(sid);
      const refreshed = await dashboardsApi.getSession(sid);
      activeView.setState({
        pinStatus: refreshed.pinStatus,
        pinnedBytes: refreshed.pinnedBytes,
        expiresAt: refreshed.expiresAt,
        capture: refreshed.capture ?? activeView.getState().capture,
      });
      flash("Capture unpinned.");
    } catch (failure) {
      setErr(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Show when={isCapture()}>
      <section
        class="scope-bar capture-session-bar"
        aria-label="Capture session bar"
        data-testid="capture-session-bar"
      >
        <div class="scope-bar-group">
          <button
            type="button"
            class="scope-title-btn"
            onClick={() => openDashboards("sessions")}
            title="View all sessions"
          >
            <span class="scope-title-name">{capture()?.name || title() || "Capture"}</span>
            <span class="scope-badge">capture</span>
            <Show when={pinStatus() === "pinned"}>
              <span class="scope-badge scope-badge-pin">pinned</span>
            </Show>
          </button>

          <Show when={capture()}>
            {(meta) => (
              <>
                <span class="scope-badge">{meta().projectName}</span>
                <span class="muted">
                  {formatBytes(meta().byteLen)}
                  <Show when={meta().samples > 0}> · {formatCount(meta().samples, "sample")}</Show>
                  <Show when={meta().durationNs > 0}> · {formatDuration(meta().durationNs)}</Show>
                </span>
                <Show
                  when={!meta().deleted}
                  fallback={<span class="scope-badge scope-badge-warn">expired</span>}
                >
                  <Show
                    when={pinStatus() === "pinned"}
                    fallback={
                      <Show when={meta().expiresAt}>
                        {(exp) => <span class="muted">Expires {day(exp())}</span>}
                      </Show>
                    }
                  >
                    <span class="muted">Pinned ({formatBytes(pinnedBytes())})</span>
                  </Show>
                </Show>
              </>
            )}
          </Show>
        </div>

        <div class="scope-bar-group scope-bar-end">
          <Show when={isOwner() && !locked() && !deleted()}>
            <Show
              when={pinStatus() === "pinned"}
              fallback={
                <button
                  type="button"
                  class="tb ghost"
                  data-testid="capture-keep-pin"
                  disabled={busy()}
                  onClick={() => void requestEstimate()}
                >
                  Keep &amp; pin…
                </button>
              }
            >
              <button
                type="button"
                class="tb ghost"
                data-testid="capture-unpin"
                disabled={busy()}
                onClick={() => void onUnpin()}
              >
                Unpin
              </button>
            </Show>
          </Show>

          <button type="button" class="tb ghost" onClick={() => void copyLink()}>
            Copy link
          </button>

          <Show when={captureId() !== null && !locked() && !deleted()}>
            <a
              class="tb ghost"
              data-testid="capture-download"
              href={captureDownloadUrl(captureId() as number)}
              download
            >
              Download .usprof
            </a>
          </Show>
        </div>

        <Show when={estimate()}>
          {(est) => (
            <div class="capture-pin-bar" data-testid="capture-pin-confirm">
              <span>
                Pinning keeps this capture ({formatBytes(est().estimatedBytes)}) indefinitely. Org
                pin quota: {formatBytes(est().orgPinnedBytes)} / {formatBytes(est().pinCapBytes)}.
                {est().wouldExceedCap ? " Exceeds pin cap." : ""}
              </span>
              <div class="actions">
                <button
                  type="button"
                  class="button compact"
                  disabled={busy()}
                  onClick={() => void confirmKeep(!est().wouldExceedCap)}
                >
                  {est().wouldExceedCap ? "Keep without pinning" : "Confirm keep & pin"}
                </button>
                <button type="button" class="tb ghost" onClick={() => setEstimate(null)}>
                  Cancel
                </button>
              </div>
            </div>
          )}
        </Show>

        <Show when={err()}>
          {(msg) => (
            <span class="error" role="alert">
              {msg()}
            </span>
          )}
        </Show>
      </section>
    </Show>
  );
}
