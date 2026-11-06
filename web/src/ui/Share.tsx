import { useLocation } from "@tanstack/react-router";
import { useCallback, useEffect, useRef, useState } from "react";

import type { Role } from "../protocol";
import { useConnection, useModel } from "../store";
import { tab, useTab } from "../tab";
import { useDialogFocus, useDismiss } from "./dialog";

export function Share() {
  const open = useModel((model) => model.link === "open");
  // The File menu opens it too.
  const shown = useTab((current) => current.share);
  const anchor = useRef<HTMLDivElement>(null);

  // Escape or a click elsewhere puts it away.
  const away = useCallback(() => tab.setState({ share: false }), []);
  useDismiss(anchor, shown, away);

  const toggle = useCallback(() => tab.setState({ share: !shown }), [shown]);

  if (!open) {
    return null;
  }
  return (
    <div className="anchor" ref={anchor}>
      <button type="button" className="tb ghost" aria-expanded={shown} onClick={toggle}>
        Share
      </button>
      {shown && <Popover />}
    </div>
  );
}

const FLUSH = { marginLeft: 0 };

/** The link for the access chosen, made when it opens and on each change. */
function Popover() {
  const role = useModel((model) => model.hello?.role);
  const connection = useConnection();
  const location = useLocation();
  const [access, setAccess] = useState<Role>("view");
  const [url, setUrl] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const field = useRef<HTMLInputElement>(null);
  const dialog = useRef<HTMLDivElement>(null);
  useDialogFocus(dialog);
  // The whole look travels: the path and its query, never the hash.
  const to = location.pathname + location.searchStr;
  const port = window.location.port || "80";

  useEffect(() => {
    // Only the link for the access chosen last is shown.
    let live = true;
    // oxlint-disable-next-line react/set-state-in-effect -- the last link goes at once, before the new one is asked for
    setUrl(null);
    setCopied(false);
    setError(null);
    connection
      .request("share", { role: access, to })
      .then((link) => live && setUrl(link.url))
      .catch((failure: Error) => live && setError(failure.message));
    return () => {
      live = false;
    };
  }, [access, to, connection]);

  const copy = useCallback(async () => {
    if (!url) {
      return;
    }
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
    } catch {
      field.current?.select();
    }
  }, [url]);
  const canView = useCallback(() => setAccess("view"), []);
  const canControl = useCallback(() => setAccess("control"), []);

  return (
    // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a popover, not a modal <dialog>
    <div className="popover" role="dialog" aria-label="Share this session" ref={dialog}>
      <div className="card-head">
        <h1>Share this session</h1>
      </div>
      <div className="card-body">
        <div className="segmented" style={FLUSH}>
          <button type="button" aria-pressed={access === "view"} onClick={canView}>
            Can view
          </button>
          <button
            type="button"
            aria-pressed={access === "control"}
            disabled={role !== "control"}
            onClick={canControl}
          >
            Can control
          </button>
        </div>
        <span className="muted">
          {access === "view"
            ? "They see every pane and move their own focus."
            : "They can also run, stop, and choose what to debug."}
        </span>
        <div className="link-field">
          <input ref={field} readOnly value={url ?? ""} aria-label="Join link" />
          <button type="button" onClick={copy} disabled={!url}>
            {copied ? "Copied" : "Copy"}
          </button>
        </div>
        {error === null && url === null && (
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled span, not a form output
          <span className="muted" role="status">
            Making a link…
          </span>
        )}
        {error && <span className="error">{error}</span>}
        <span className="muted">
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
