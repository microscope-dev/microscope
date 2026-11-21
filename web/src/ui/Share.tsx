import { useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, Show } from "solid-js";

import type { Role } from "../protocol";
import { useConnection, useModel } from "../store";
import { tab, useTab } from "../tab";
import { useDialogFocus, useDismiss } from "./dialog";

export function Share() {
  const open = useModel((model) => model.link === "open");
  const shown = useTab((current) => current.share);
  let anchor: HTMLDivElement | undefined;

  useDismiss(
    () => anchor,
    () => open() && shown(),
    () => tab.setState({ share: false }),
  );

  const toggle = () => tab.setState({ share: !shown() });

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

function Popover() {
  const role = useModel((model) => model.hello?.role);
  const connection = useConnection();
  const location = useLocation();
  const [access, setAccess] = createSignal<Role>("view");
  const [url, setUrl] = createSignal<string | null>(null);
  const [copied, setCopied] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  let field: HTMLInputElement | undefined;
  let dialog: HTMLDivElement | undefined;
  useDialogFocus(() => dialog);

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
    <div
      class="popover"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a popover, not a modal <dialog>
      role="dialog"
      aria-label="Share this session"
      ref={(node) => {
        dialog = node;
      }}
    >
      <div class="card-head">
        <h1>Share this session</h1>
      </div>
      <div class="card-body">
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
      </div>
    </div>
  );
}
