import {
  type KeyboardEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import type { MachineState, Warning } from "../../fleet";
import { useDialogFocus } from "../dialog";
import { useAction } from "../Orgs";

export const OWNERS_ONLY = "Only owners can do this";

export const STATE_LABEL: Record<MachineState, string> = {
  live: "Live",
  quiet: "Went quiet",
  stopped: "Stopped",
  revoked: "Revoked",
};

export const WARNING_LABEL: Record<Warning, string> = {
  went_quiet: "went quiet",
  duplicated_key: "duplicated key",
  old_version: "old version",
  cannot_record_syscalls: "can't record syscalls",
  not_on_host_network: "not on the host's network",
};

export function Dot({ state }: { state: MachineState }) {
  return <span className={`machine-dot ${state}`} aria-hidden="true" />;
}

export function useNow(everyMs: number): number {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now() / 1000), everyMs);
    return () => clearInterval(timer);
  }, [everyMs]);
  return now;
}

export function OwnerButton(props: {
  owner: boolean;
  className?: string;
  onClick: () => void;
  children: ReactNode;
}) {
  const { owner, onClick } = props;
  const click = useCallback(() => owner && onClick(), [owner, onClick]);
  return (
    <button
      type="button"
      className={props.className ?? "button"}
      aria-disabled={!owner}
      title={owner ? undefined : OWNERS_ONLY}
      onClick={click}
    >
      {props.children}
    </button>
  );
}

export function Ask(props: {
  title: string;
  verb: string;
  danger: boolean;
  act: () => Promise<unknown>;
  done: () => void;
  cancel: () => void;
  children: ReactNode;
}) {
  const dialog = useRef<HTMLDivElement>(null);
  const keep = useRef<HTMLButtonElement>(null);
  useDialogFocus(dialog, keep);
  const { working, error, run } = useAction();
  const { act, done, cancel } = props;
  const confirm = useCallback(
    () =>
      run(async () => {
        await act();
        done();
      }),
    [run, act, done],
  );
  const key = useCallback(
    (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        cancel();
      }
    },
    [cancel],
  );
  return (
    <div className="machines-ask-scrim">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- Escape anywhere in the dialog cancels it */}
      <div
        className="machines-ask"
        role="alertdialog"
        aria-modal="true"
        aria-label={props.title}
        ref={dialog}
        onKeyDown={key}
      >
        <h3>{props.title}</h3>
        {props.children}
        {error && (
          <p className="error" role="alert">
            {error}
          </p>
        )}
        <div className="actions end">
          <button type="button" className="button" ref={keep} onClick={cancel}>
            Cancel
          </button>
          <button
            type="button"
            className={props.danger ? "button danger" : "button"}
            disabled={working}
            onClick={confirm}
          >
            {props.verb}
          </button>
        </div>
      </div>
    </div>
  );
}

export function Sparkline({ hours, label }: { hours: readonly number[]; label: string }) {
  const width = 168;
  const height = 24;
  const top = Math.max(1, ...hours);
  const points = hours
    .map(
      (live, at) =>
        `${(at / Math.max(1, hours.length - 1)) * width},${height - 1 - (live / top) * (height - 2)}`,
    )
    .join(" ");
  return (
    <svg
      className="machines-spark"
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an inline drawing, not an image file
      role="img"
      aria-label={label}
    >
      <polyline
        points={points}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
