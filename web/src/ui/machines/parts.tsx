import type { JSX } from "@solidjs/web";
import { createEffect, createMemo, createSignal, For, onSettled, Show } from "solid-js";

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

export function Dot(props: { state: MachineState }) {
  return <span class={`machine-dot ${props.state}`} aria-hidden="true" />;
}

export function useNow(everyMs: number): () => number {
  const [now, setNow] = createSignal(Date.now() / 1000);
  onSettled(() => {
    const timer = setInterval(() => setNow(Date.now() / 1000), everyMs);
    return () => clearInterval(timer);
  });
  return now;
}

export function OwnerButton(props: {
  owner: boolean;
  class?: string;
  onClick: () => void;
  children: JSX.Element;
}) {
  const click = () => {
    if (props.owner) {
      props.onClick();
    }
  };
  return (
    <button
      type="button"
      class={props.class ?? "button"}
      aria-disabled={props.owner ? "false" : "true"}
      title={props.owner ? undefined : OWNERS_ONLY}
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
  children: JSX.Element;
}) {
  let dialog: HTMLDivElement | undefined;
  let keep: HTMLButtonElement | undefined;
  let field: HTMLInputElement | undefined;
  useDialogFocus(
    () => dialog,
    () => (props.typeToConfirm === undefined ? keep : field),
  );
  const { working, error, run } = useAction();
  const [typed, setTyped] = createSignal("");
  const ready = createMemo(
    () => props.typeToConfirm === undefined || typed() === props.typeToConfirm,
  );
  const confirm = () => {
    if (!ready()) {
      return;
    }
    run(async () => {
      await props.act();
      props.done();
    });
  };
  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    confirm();
  };
  const key = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      props.cancel();
    }
  };
  return (
    <div class="machines-ask-scrim">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
      <div
        class="machines-ask"
        role="alertdialog"
        aria-modal="true"
        aria-label={props.title}
        ref={(node) => {
          dialog = node;
        }}
        onKeyDown={key}
      >
        <h3>{props.title}</h3>
        {props.children}
        <Show when={props.typeToConfirm !== undefined}>
          <form class="field" onSubmit={submit}>
            <label for="machines-ask-typed">
              Type <span class="mono">{props.typeToConfirm}</span> to confirm
            </label>
            <input
              id="machines-ask-typed"
              ref={(node) => {
                field = node;
              }}
              class="input mono"
              value={typed()}
              autocomplete="off"
              spellcheck={false}
              onInput={(event) => setTyped(event.currentTarget.value)}
            />
          </form>
        </Show>
        <Show when={error()}>
          {(message) => (
            <p class="error" role="alert">
              {message()}
            </p>
          )}
        </Show>
        <div class="actions end">
          <button
            type="button"
            class="button"
            ref={(node) => {
              keep = node;
            }}
            onClick={() => props.cancel()}
          >
            Cancel
          </button>
          <button
            type="button"
            class={props.danger ? "button danger" : "button"}
            disabled={working() || !ready()}
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
  clear?: (() => void) | undefined;
}) {
  let select: HTMLSelectElement | undefined;
  const options = createMemo(() => {
    const listed = props.options.includes(ANY) ? props.options : [ANY, ...props.options];
    return listed.includes(props.value) ? listed : [...listed, props.value];
  });
  createEffect(
    () => [options(), props.value] as const,
    ([, value]) => {
      if (select) {
        select.value = value;
      }
    },
  );
  return (
    <span class={`machines-chip${props.clear ? " set" : ""}`}>
      <label>
        <span>{props.label}</span>
        <select
          ref={(node) => {
            select = node;
          }}
          value={props.value}
          onChange={(event) => props.pick(event.currentTarget.value)}
        >
          <For each={options()}>
            {(option) => (
              <option value={option} selected={option === props.value}>
                {props.names ? props.names(option) : option}
              </option>
            )}
          </For>
        </select>
      </label>
      <Show when={props.clear}>
        {(clear) => (
          <button
            type="button"
            class="machines-chip-clear"
            aria-label={`Clear ${props.label}`}
            title={`Clear ${props.label}`}
            onClick={() => clear()()}
          >
            ×
          </button>
        )}
      </Show>
    </span>
  );
}
