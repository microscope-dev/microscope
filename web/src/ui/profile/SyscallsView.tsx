import {
  createEffect,
  createMemo,
  createSignal,
  For,
  Match,
  Show,
  Switch,
  untrack,
} from "solid-js";

import { type SyscallTab, syscallQueryOf, withLook } from "../../profile/look";
import { formatCount } from "../../profile/paint";
import { lossNotes, unkept } from "../../profile/syscalls";
import { useAnswer, useProfile } from "./context";
import { LookButton } from "./parts";
import { Log } from "./SyscallLog";
import { SamplesDuring, Summary } from "./SyscallSummary";
import { Files, Network, Processes } from "./SyscallTables";

const TABS: { tab: SyscallTab | undefined; label: string }[] = [
  { tab: undefined, label: "Summary" },
  { tab: "log", label: "Log" },
  { tab: "files", label: "Files" },
  { tab: "network", label: "Network" },
  { tab: "processes", label: "Processes" },
];

export function KernelPane() {
  return (
    <div class="kernel" data-testid="kernel">
      <div class="segmented kernel-tabs" role="tablist" aria-label="Kernel data">
        <button type="button" role="tab" id="kernel-syscalls" aria-selected="true">
          Syscalls
        </button>
      </div>
      <div class="kernel-body" role="tabpanel" aria-labelledby="kernel-syscalls">
        <SyscallsView />
      </div>
    </div>
  );
}

function SyscallsView() {
  const focus = useProfile();
  const missing = createMemo(() => unkept(focus().summary.settings));
  return (
    <div class="syscalls" data-testid="syscalls">
      <Show
        when={focus().summary.syscalls === 0 ? missing() : null}
        fallback={<Calls missing={missing()} />}
      >
        {(item) => (
          <>
            <div class="syscalls-bar">
              <Tabs tab={focus().look.st} />
            </div>
            <Unkept text={item().text} flag={item().flag} />
          </>
        )}
      </Show>
    </div>
  );
}

function Tabs(props: { tab: SyscallTab | undefined }) {
  return (
    <div class="segmented">
      <For each={TABS}>
        {(item) => (
          <LookButton field="st" value={item.tab} pressed={props.tab === item.tab}>
            {item.label}
          </LookButton>
        )}
      </For>
    </div>
  );
}

type Missing = NonNullable<ReturnType<typeof unkept>>;

function Unkept(props: Missing) {
  return (
    <p class="profile-untimed muted" data-testid="syscalls-unkept">
      {props.text} Record with <code>{props.flag}</code> to keep each call.
    </p>
  );
}

function Calls(props: { missing: Missing | null }) {
  const focus = useProfile();
  const base = createMemo(() =>
    syscallQueryOf(focus().profile, focus().look, focus().query.revision ?? 0),
  );
  const summary = useAnswer("syscallSummary", () => ({
    ...base(),
    group: focus().look.sg ?? ("syscall" as const),
  }));
  const logKey = createMemo(() => JSON.stringify(base()));
  return (
    <>
      <div class="syscalls-bar">
        <FilterBar error={summary().error?.message} />
        <Tabs tab={focus().look.st} />
        <Show when={summary().data}>
          {(data) => (
            <span class="muted syscalls-counts" data-testid="syscall-counts">
              {formatCount(data().calls)} of {formatCount(data().total, "call")} ·{" "}
              {formatCount(data().errors)} failed
              {data().engine ? ` · ${data().engine}` : ""}
              <For each={lossNotes(data().lost)} keyed={(note) => note.text}>
                {(note) => (
                  <span class={note().warn ? "warn" : undefined} title={note().title}>
                    {" "}
                    · {note().text}
                  </span>
                )}
              </For>
            </span>
          )}
        </Show>
        <SamplesDuring during="all" name="the selected calls">
          Samples during these calls
        </SamplesDuring>
      </div>
      <Show when={props.missing}>
        {(current) => <Unkept text={current().text} flag={current().flag} />}
      </Show>
      <div class="syscalls-body">
        <Switch fallback={<Summary summary={summary()} query={base()} />}>
          <Match when={focus().look.st === "log"}>
            <Show when={logKey()} keyed>
              {(_key) => <Log query={base()} />}
            </Show>
          </Match>
          <Match when={focus().look.st === "files"}>
            <Files query={base()} />
          </Match>
          <Match when={focus().look.st === "network"}>
            <Network query={base()} />
          </Match>
          <Match when={focus().look.st === "processes"}>
            <Processes query={base()} />
          </Match>
        </Switch>
      </div>
    </>
  );
}

function FilterBar(props: { error: string | undefined }) {
  const focus = useProfile();
  const [text, setText] = createSignal(untrack(() => focus().look.sf ?? ""));

  createEffect(
    () => focus().look.sf,
    (sf) => {
      setText(sf ?? "");
    },
  );
  const apply = (event: SubmitEvent) => {
    event.preventDefault();
    focus().setLook((prev) => withLook(prev, "sf", text().trim() || undefined), true);
  };
  const edit = (event: InputEvent & { currentTarget: HTMLInputElement }) =>
    setText(event.currentTarget.value);
  return (
    <form class="syscalls-filter" onSubmit={apply}>
      <input
        class="input mono"
        aria-label="Selection"
        placeholder="trace=%file status=failed"
        value={text()}
        onInput={edit}
        aria-invalid={props.error ? "true" : undefined}
      />
      <Show when={props.error}>
        {(message) => (
          <span class="error-text" role="alert">
            {message()}
          </span>
        )}
      </Show>
    </form>
  );
}
