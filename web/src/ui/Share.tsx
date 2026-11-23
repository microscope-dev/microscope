import { useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, onCleanup, Show, untrack } from "solid-js";

import { isCaptureProfile } from "../captures";
import { copyText } from "../clipboard";
import { formatBytes } from "../dashboards";
import type { CloudLoginStarted, CloudStatus, ProfileInfo, Role, SharedProfile } from "../protocol";
import { useConnection, useModel } from "../store";
import { closeShare, openShare, useTab } from "../tab";
import { useLayout } from "../workbench/store";
import { useDialogFocus, useDismiss } from "./dialog";
import { profileOf, shownProfilePane } from "./profile/link";

export function Share() {
  const open = useModel((model) => model.link === "open");
  const shown = useTab((current) => current.share);
  let anchor: HTMLDivElement | undefined;

  useDismiss(
    () => anchor,
    () => open() && shown(),
    closeShare,
  );

  const toggle = () => {
    if (shown()) {
      closeShare();
    } else {
      openShare(null);
    }
  };

  return (
    <Show when={open()}>
      <div
        class="anchor"
        ref={(node) => {
          anchor = node;
        }}
      >
        <button
          type="button"
          class="tb ghost"
          data-testid="toolbar-share"
          aria-expanded={shown() ? "true" : "false"}
          onClick={toggle}
        >
          Share
        </button>
        <Show when={shown()}>
          <Popover />
        </Show>
      </div>
    </Show>
  );
}

const FLUSH = { "margin-left": "0" };

type ShareTab = "upload" | "live";

function Popover() {
  const profiles = useModel((model) => model.profiles);
  const hasTarget = useModel((model) => Boolean(model.state?.target));
  const requestedProfile = useTab((current) => current.shareProfile);
  const activePaneProfile = useLayout((layout) => {
    const paneId = shownProfilePane(layout);
    const pane = paneId ? layout.panes[paneId] : undefined;
    const id = pane ? profileOf(layout, pane) : null;
    return id && !isCaptureProfile(id) ? id : null;
  });

  const targetProfile = createMemo<ProfileInfo | null>(() => {
    const list = profiles();
    if (list.length === 0) {
      return null;
    }
    const requested = requestedProfile();
    if (requested) {
      const found = list.find((p) => p.id === requested);
      if (found) {
        return found;
      }
    }
    const fromPane = activePaneProfile();
    if (fromPane) {
      const found = list.find((p) => p.id === fromPane);
      if (found) {
        return found;
      }
    }
    return list[0] ?? null;
  });

  const canUpload = createMemo(() => targetProfile() !== null);
  const [mode, setMode] = createSignal<ShareTab>(
    untrack(() =>
      canUpload() && (requestedProfile() !== null || !hasTarget()) ? "upload" : "live",
    ),
  );

  createEffect(
    () => [canUpload(), requestedProfile()] as const,
    ([uploadable, explicit]) => {
      if (!uploadable) {
        setMode("live");
      } else if (explicit !== null) {
        setMode("upload");
      }
    },
  );

  let dialog: HTMLDivElement | undefined;
  useDialogFocus(() => dialog);

  return (
    <div
      class="popover share-popover"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a popover, not a modal <dialog>
      role="dialog"
      aria-label={mode() === "upload" ? "Share capture" : "Share this session"}
      data-testid="share-popover"
      ref={(node) => {
        dialog = node;
      }}
    >
      <div class="card-head">
        <h1>{mode() === "upload" ? "Share capture" : "Share this session"}</h1>
      </div>
      <div class="card-body">
        <Show when={canUpload()}>
          <div class="segmented" style={FLUSH} role="tablist" aria-label="Share mode">
            <button
              type="button"
              role="tab"
              aria-selected={mode() === "upload" ? "true" : "false"}
              onClick={() => setMode("upload")}
            >
              Upload capture
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={mode() === "live" ? "true" : "false"}
              onClick={() => setMode("live")}
            >
              Live link
            </button>
          </div>
        </Show>
        <Show when={mode() === "upload" && targetProfile()} fallback={<LiveShareBody />}>
          {(profile) => <UploadCaptureBody profile={profile()} />}
        </Show>
      </div>
    </div>
  );
}

function UploadCaptureBody(props: { profile: ProfileInfo }) {
  const role = useModel((model) => model.hello?.role);
  const connection = useConnection();
  const [status, setStatus] = createSignal<CloudStatus | null>(null);
  const [loadingStatus, setLoadingStatus] = createSignal(true);
  const [statusError, setStatusError] = createSignal<string | null>(null);

  const [serverInput, setServerInput] = createSignal("");
  const [login, setLogin] = createSignal<CloudLoginStarted | null>(null);
  const [loginBusy, setLoginBusy] = createSignal(false);
  const [loginError, setLoginError] = createSignal<string | null>(null);

  const [projectId, setProjectId] = createSignal<number | null>(null);
  const [captureName, setCaptureName] = createSignal(untrack(() => props.profile.name));
  const [uploading, setUploading] = createSignal(false);
  const [uploadError, setUploadError] = createSignal<string | null>(null);
  const [shared, setShared] = createSignal<SharedProfile | null>(null);
  const [copied, setCopied] = createSignal(false);
  let linkField: HTMLInputElement | undefined;
  let pollTimer: ReturnType<typeof setTimeout> | null = null;
  let alive = true;

  onCleanup(() => {
    alive = false;
    if (pollTimer !== null) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
  });

  createEffect(
    () => props.profile.id,
    () => {
      setCaptureName(props.profile.name);
      setShared(null);
      setUploadError(null);
      setCopied(false);
    },
  );

  const refreshStatus = async () => {
    setLoadingStatus(true);
    setStatusError(null);
    try {
      const next = await connection.request("cloudStatus");
      if (!alive) {
        return;
      }
      setStatus(next);
      if (serverInput() === "") {
        setServerInput(next.server ?? next.defaultServer);
      }
      if (next.projects.length > 0 && !next.projects.some((p) => p.id === projectId())) {
        setProjectId(next.projects[0]?.id ?? null);
      }
    } catch (failure) {
      if (!alive) {
        return;
      }
      setStatusError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      if (alive) {
        setLoadingStatus(false);
      }
    }
  };

  createEffect(
    () => role(),
    (currentRole) => {
      if (currentRole === "control") {
        void refreshStatus();
      } else {
        setLoadingStatus(false);
      }
    },
  );

  const LOGIN_POLL_DEADLINE_MS = 10 * 60 * 1000;
  const LOGIN_POLL_MAX_RETRIES = 3;

  const schedulePoll = (started: CloudLoginStarted, deadline: number, consecutiveFailures = 0) => {
    if (pollTimer !== null) {
      clearTimeout(pollTimer);
    }
    const interval = Math.max(started.pollIntervalMs || 1500, 500);
    pollTimer = setTimeout(() => {
      pollTimer = null;
      void pollOnce(started, deadline, consecutiveFailures);
    }, interval);
  };

  const pollOnce = async (
    started: CloudLoginStarted,
    deadline: number,
    consecutiveFailures: number,
  ) => {
    if (Date.now() > deadline) {
      if (alive) {
        setLogin(null);
        setLoginBusy(false);
        setLoginError("Sign-in timed out. Try again.");
      }
      return;
    }
    try {
      const res = await connection.request("cloudLoginPoll", {
        server: started.server,
        pollToken: started.pollToken,
      });
      if (!alive) {
        return;
      }
      if (res.status === "approved") {
        setLogin(null);
        setLoginBusy(false);
        await refreshStatus();
      } else if (res.status === "pending") {
        schedulePoll(started, deadline, 0);
      } else {
        setLogin(null);
        setLoginBusy(false);
        setLoginError(`Sign-in ${res.status}. Try again.`);
      }
    } catch (failure) {
      if (!alive) {
        return;
      }
      const nextFailures = consecutiveFailures + 1;
      if (nextFailures < LOGIN_POLL_MAX_RETRIES && Date.now() < deadline) {
        schedulePoll(started, deadline, nextFailures);
        return;
      }
      setLogin(null);
      setLoginBusy(false);
      setLoginError(failure instanceof Error ? failure.message : String(failure));
    }
  };

  const startSignIn = async () => {
    setLoginBusy(true);
    setLoginError(null);
    try {
      const trimmed = serverInput().trim();
      const started = await connection.request("cloudLoginStart", {
        server: trimmed === "" ? null : trimmed,
      });
      if (!alive) {
        return;
      }
      setLogin(started);
      window.open(started.verifyUrl, "_blank", "noopener,noreferrer");
      schedulePoll(started, Date.now() + LOGIN_POLL_DEADLINE_MS, 0);
    } catch (failure) {
      if (!alive) {
        return;
      }
      setLoginBusy(false);
      setLoginError(failure instanceof Error ? failure.message : String(failure));
    }
  };

  const upload = async () => {
    const pid = projectId();
    if (pid === null) {
      setUploadError("Select a project to upload into.");
      return;
    }
    setUploading(true);
    setUploadError(null);
    setCopied(false);
    try {
      const trimmedName = captureName().trim();
      const res = await connection.request("shareProfile", {
        profile: props.profile.id,
        projectId: pid,
        name: trimmedName === "" ? null : trimmedName,
      });
      if (!alive) {
        return;
      }
      setShared(res);
    } catch (failure) {
      if (!alive) {
        return;
      }
      setUploadError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      if (alive) {
        setUploading(false);
      }
    }
  };

  const copyShared = async () => {
    const url = shared()?.url;
    if (!url) {
      return;
    }
    if (await copyText(url)) {
      setCopied(true);
    } else {
      linkField?.select();
    }
  };

  return (
    <Show
      when={role() === "control"}
      fallback={
        <span class="muted">
          This link can only view the session. Uploading captures requires control access.
        </span>
      }
    >
      <Show
        when={!loadingStatus()}
        fallback={
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- styled status text
          <span class="muted" role="status">
            Checking Microscope sign-in…
          </span>
        }
      >
        <Show when={statusError()}>
          {(err) => (
            <span class="error" role="alert">
              {err()}
            </span>
          )}
        </Show>

        <Show when={status() && !status()?.loggedIn}>
          <label class="dash-field">
            <span>Server</span>
            <input
              type="url"
              class="input"
              aria-label="Microscope server URL"
              value={serverInput()}
              onInput={(e) => setServerInput(e.currentTarget.value)}
              disabled={loginBusy()}
            />
          </label>
          <Show
            when={login()}
            fallback={
              <button
                type="button"
                class="button"
                data-testid="share-sign-in"
                disabled={loginBusy()}
                onClick={() => void startSignIn()}
              >
                {loginBusy() ? "Starting sign-in…" : "Sign in…"}
              </button>
            }
          >
            {(flow) => (
              <div class="share-login-flow" data-testid="share-login-flow">
                <span class="muted">
                  Approve code <code>{flow().userCode}</code> in your browser:
                </span>
                <a
                  href={flow().verifyUrl}
                  target="_blank"
                  rel="noopener noreferrer"
                  class="link-button"
                >
                  Open verification page
                </a>
                {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- status line */}
                <span class="muted" role="status">
                  Waiting for approval…
                </span>
              </div>
            )}
          </Show>
          <Show when={loginError()}>
            {(err) => (
              <span class="error" role="alert">
                {err()}
              </span>
            )}
          </Show>
          <Show when={status()?.error}>{(err) => <span class="muted">{err()}</span>}</Show>
          <span class="muted">
            Or run <code>microscope login</code> in a terminal.
          </span>
        </Show>

        <Show when={status()?.loggedIn}>
          <span class="muted">
            Signed in as <strong>{status()?.username}</strong>
            <Show when={status()?.server}>{(srv) => <> on {srv()}</>}</Show>
          </span>
          <Show when={status()?.error}>
            {(err) => (
              <span class="error" role="alert">
                {err()}
              </span>
            )}
          </Show>
          <Show
            when={(status()?.projects.length ?? 0) > 0}
            fallback={
              <span class="error" role="alert">
                No writable projects found for your account.
              </span>
            }
          >
            <label class="dash-field">
              <span>Project</span>
              <select
                class="input"
                aria-label="Target project"
                data-testid="share-project-select"
                value={projectId() ?? ""}
                disabled={uploading()}
                onChange={(e) => setProjectId(Number(e.currentTarget.value))}
              >
                <For each={status()?.projects ?? []}>
                  {(proj) => (
                    <option value={proj.id}>
                      {proj.orgName ? `${proj.orgName} / ${proj.name}` : proj.name}
                    </option>
                  )}
                </For>
              </select>
            </label>
            <label class="dash-field">
              <span>Capture name</span>
              <input
                type="text"
                class="input"
                aria-label="Capture name"
                value={captureName()}
                disabled={uploading()}
                onInput={(e) => setCaptureName(e.currentTarget.value)}
              />
            </label>
            <button
              type="button"
              class="button"
              data-testid="share-upload-submit"
              disabled={uploading() || projectId() === null}
              onClick={() => void upload()}
            >
              {uploading() ? "Sanitizing & uploading…" : "Upload & get link"}
            </button>
          </Show>

          <Show when={uploadError()}>
            {(err) => (
              <span class="error" role="alert">
                {err()}
              </span>
            )}
          </Show>

          <Show when={shared()}>
            {(result) => (
              <div class="share-uploaded" data-testid="share-uploaded-result">
                <div class="link-field">
                  <input
                    ref={(node) => {
                      linkField = node;
                    }}
                    readonly
                    value={result().url}
                    aria-label="Shared capture link"
                  />
                  <button type="button" onClick={() => void copyShared()}>
                    {copied() ? "Copied" : "Copy"}
                  </button>
                </div>
                <div class="actions">
                  <a class="tb ghost" href={result().url} target="_blank" rel="noopener noreferrer">
                    Open
                  </a>
                  <span class="muted">
                    {result().projectName} · {formatBytes(result().byteLen)}
                    {result().redactedCount > 0 ? ` · ${result().redactedCount} redacted` : ""}
                  </span>
                </div>
              </div>
            )}
          </Show>

          <span class="muted">
            Sanitized on your workstation using project rules before upload. Only members with
            project read access can open the link.
          </span>
        </Show>
      </Show>
    </Show>
  );
}

function LiveShareBody() {
  const role = useModel((model) => model.hello?.role);
  const connection = useConnection();
  const location = useLocation();
  const [access, setAccess] = createSignal<Role>("view");
  const [url, setUrl] = createSignal<string | null>(null);
  const [copied, setCopied] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  let field: HTMLInputElement | undefined;

  const to = createMemo(() => location.pathname + location.search);
  const port = window.location.port || "80";

  createEffect(
    () => [access(), to()] as const,
    ([selectedAccess, target]) => {
      let live = true;
      setUrl(null);
      setCopied(false);
      setError(null);
      connection
        .request("share", { role: selectedAccess, to: target })
        .then((link) => live && setUrl(link.url))
        .catch((failure: Error) => live && setError(failure.message));
      return () => {
        live = false;
      };
    },
  );

  const copy = async () => {
    const current = url();
    if (!current) {
      return;
    }
    try {
      await navigator.clipboard.writeText(current);
      setCopied(true);
    } catch {
      field?.select();
    }
  };

  return (
    <>
      <div class="segmented" style={FLUSH}>
        <button
          type="button"
          aria-pressed={access() === "view" ? "true" : "false"}
          onClick={() => setAccess("view")}
        >
          Can view
        </button>
        <button
          type="button"
          aria-pressed={access() === "control" ? "true" : "false"}
          disabled={role() !== "control"}
          onClick={() => setAccess("control")}
        >
          Can control
        </button>
      </div>
      <span class="muted">
        {access() === "view"
          ? "They see every pane and move their own focus."
          : "They can also run, stop, and choose what to debug."}
      </span>
      <div class="link-field">
        <input
          ref={(node) => {
            field = node;
          }}
          readonly
          value={url() ?? ""}
          aria-label="Join link"
        />
        <button type="button" onClick={() => void copy()} disabled={!url()}>
          {copied() ? "Copied" : "Copy"}
        </button>
      </div>
      <Show when={error() === null && url() === null}>
        {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled span, not a form output */}
        <span class="muted" role="status">
          Making a link…
        </span>
      </Show>
      <Show when={error()}>{(message) => <span class="error">{message()}</span>}</Show>
      <span class="muted">
        Works until microscope exits. Someone on another machine needs a tunnel, such as{" "}
        <code>
          ssh -L {port}:127.0.0.1:{port} host
        </code>
        .
      </span>
    </>
  );
}
