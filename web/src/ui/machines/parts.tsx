import {
  type ChangeEvent,
  type FormEvent,
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
  typeToConfirm?: string;
  children: ReactNode;
}) {
  const dialog = useRef<HTMLDivElement>(null);
  const keep = useRef<HTMLButtonElement>(null);
  const field = useRef<HTMLInputElement>(null);
  const { typeToConfirm } = props;
  useDialogFocus(dialog, typeToConfirm === undefined ? keep : field);
  const { working, error, run } = useAction();
  const { act, done, cancel } = props;
  const [typed, setTyped] = useState("");
  const type = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setTyped(event.target.value),
    [],
  );
  const ready = typeToConfirm === undefined || typed === typeToConfirm;
  const confirm = useCallback(() => {
    if (!ready) {
      return;
    }
    run(async () => {
      await act();
      done();
    });
  }, [ready, run, act, done]);
  const submit = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      confirm();
    },
    [confirm],
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
        {typeToConfirm !== undefined && (
          <form className="field" onSubmit={submit}>
            <label htmlFor="machines-ask-typed">
              Type <span className="mono">{typeToConfirm}</span> to confirm
            </label>
            <input
              id="machines-ask-typed"
              ref={field}
              className="input mono"
              value={typed}
              autoComplete="off"
              spellCheck={false}
              onChange={type}
            />
          </form>
        )}
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
            disabled={working || !ready}
            onClick={confirm}
          >
            {props.verb}
          </button>
        </div>
      </div>
    </div>
  );
}

export const ANY = "any";

export function Chip(props: {
  label: string;
  value: string;
  options: readonly string[];
  names?: (value: string) => string;
  pick: (value: string) => void;
}) {
  const { pick } = props;
  const change = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => pick(event.target.value),
    [pick],
  );
  const listed = props.options.includes(ANY) ? props.options : [ANY, ...props.options];
  const options = listed.includes(props.value) ? listed : [...listed, props.value];
  return (
    <label className="machines-chip">
      <span>{props.label}</span>
      <select value={props.value} onChange={change}>
        {options.map((option) => (
          <option key={option} value={option}>
            {props.names ? props.names(option) : option}
          </option>
        ))}
      </select>
    </label>
  );
}
