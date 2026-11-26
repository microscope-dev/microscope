import { useLocation, useNavigate } from "@solidjs/router";
import type { JSX } from "@solidjs/web";
import {
  createEffect,
  createMemo,
  createSignal,
  createUniqueId,
  For,
  Match,
  onSettled,
  type Setter,
  Show,
  Switch,
} from "solid-js";

import { assert } from "../assert";
import { KIND } from "../documents";
import { toHref } from "../history";
import { targetName } from "../model";
import type { PathEntry, Process, RecordPreset, SyscallsLevel } from "../protocol";
import { RequestError } from "../protocol";
import { isRecentLaunches, type RecentLaunch, read, rememberLaunch } from "../storage";
import { useConnection, useModel } from "../store";
import { parseEnvironment, splitWords } from "../words";
import type { PaneProps } from "../workbench/kinds";
import { close, type KindId, type PaneId } from "../workbench/layout";
import { change } from "../workbench/store";

export type PickerTab = "launch" | "attach" | "core" | "profile";

export const PICKED: Readonly<Record<PickerTab, KindId>> = {
  launch: KIND.launch,
  attach: KIND.attach,
  core: KIND.core,
  profile: KIND.record,
};

export function pickerSearch(search: Record<string, unknown>): { tab?: PickerTab } {
  const tab = search.tab;
  return tab === "attach" || tab === "core" || tab === "launch" || tab === "profile" ? { tab } : {};
}

interface Pending {
  describe: string;
  send(replace: boolean): Promise<unknown>;
}

const done = (pane: PaneId) => change((layout) => close(layout, pane));

export function StartBody(props: PaneProps) {
  const navigate = useNavigate();
  const location = useLocation();
  const state = useModel((model) => model.state);
  const [busy, setBusy] = createSignal<Pending | null>(null);
  const [working, setWorking] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const current = createMemo(() => targetName(state()));
  const conflict = createMemo(() => {
    const pending = busy();
    const name = current();
    return pending && name ? { pending, name } : null;
  });

  const submit = async (pending: Pending, replace = false) => {
    setError(null);
    setWorking(true);
    try {
      await pending.send(replace);
      setBusy(null);
      done(props.id);

      if (!location.pathname.startsWith("/s/")) {
        navigate(toHref("/"));
      }
    } catch (failure) {
      if (failure instanceof RequestError && failure.kind === "busy") {
        setBusy(pending);
      } else {
        setBusy(null);
        setError(failure instanceof Error ? failure.message : String(failure));
      }
    } finally {
      setWorking(false);
    }
  };
  const submitForm = (pending: Pending) => {
    void submit(pending);
  };
  const replace = () => {
    const pending = busy();
    assert(pending !== null, "replacing a session with nothing to start");
    void submit(pending, true);
  };
  const cancel = () => setBusy(null);
  const edited = () => setError(null);
  const recording = () => done(props.id);

  return (
    <div class="pane-body start-pane" onInput={edited}>
      <Show when={conflict()}>
        {(item) => (
          <div class="banner" role="alert">
            <span>
              <b>End the {item().name} session?</b> {item().pending.describe} stops debugging{" "}
              {item().name}: a launched program is killed, an attached one keeps running. Links to
              it will say the session ended.
            </span>
            <div class="actions">
              <button type="button" class="button primary" disabled={working()} onClick={replace}>
                End it and continue
              </button>
              <button type="button" class="button" onClick={cancel}>
                Cancel
              </button>
            </div>
          </div>
        )}
      </Show>
      <Show when={error()}>
        {(message) => (
          <div class="banner error" role="alert">
            {message()}
          </div>
        )}
      </Show>
      <Switch fallback={<RecordForm onRecording={recording} />}>
        <Match when={props.pane.kind === KIND.launch}>
          <LaunchForm working={working()} submit={submitForm} />
        </Match>
        <Match when={props.pane.kind === KIND.attach}>
          <AttachForm working={working()} submit={submitForm} />
        </Match>
        <Match when={props.pane.kind === KIND.core}>
          <CoreForm working={working()} submit={submitForm} />
        </Match>
      </Switch>
    </div>
  );
}

interface FormProps {
  working: boolean;
  submit(pending: Pending): void;
}

function LaunchForm(props: FormProps) {
  const connection = useConnection();
  const initialRecent = read("microscope-recent", [], isRecentLaunches);
  const [recent, setRecent] = createSignal(initialRecent);
  const [form, setForm] = createSignal<RecentLaunch>(
    initialRecent[0] ?? { program: "", arguments: "", cwd: "", environment: "" },
  );
  const [stopAtEntry, setStopAtEntry] = createSignal(false);
  const words = createMemo(() => splitWords(form().arguments));
  const environment = createMemo(() => parseEnvironment(form().environment));
  const wordsNote = createMemo(() => {
    const parsed = words();
    return parsed.ok ? undefined : parsed.error;
  });
  const environmentNote = createMemo(() => {
    const parsed = environment();
    return parsed.ok ? "NAME=VALUE, one per line" : parsed.error;
  });
  const valid = createMemo(() => form().program.trim() !== "" && words().ok && environment().ok);
  const fields = launchFields(setForm);

  const launch = (run: boolean, event: Event) => {
    event.preventDefault();
    const currentWords = words();
    const currentEnv = environment();
    const currentForm = form();
    if (!valid() || !currentWords.ok || !currentEnv.ok) {
      return;
    }
    const program = currentForm.program.trim();
    props.submit({
      describe: `Launching ${program}`,
      send: async (replace) => {
        await connection.request("launch", {
          program,
          arguments: currentWords.words,
          cwd: currentForm.cwd.trim() || null,
          environment: currentEnv.pairs,
          stopAtEntry: stopAtEntry(),
          run,
          replace,
        });

        setRecent(rememberLaunch(currentForm));
      },
    });
  };
  const load = (event: SubmitEvent) => launch(false, event);
  const loadAndRun = (event: MouseEvent) => launch(true, event);

  return (
    <form class="card-body" style={NO_PADDING} onSubmit={load}>
      <Field label="Program" note="A path on the machine microscope runs on. Tab completes it.">
        {(id) => (
          <PathInput
            id={id}
            value={form().program}
            onChange={fields.program}
            autoFocus
            placeholder="./build/app"
            onlyExecutables
          />
        )}
      </Field>
      <Field label="Arguments" note={wordsNote()}>
        {(id) => (
          <input
            id={id}
            class="input"
            value={form().arguments}
            aria-invalid={words().ok ? "false" : "true"}
            onInput={fields.arguments}
            placeholder="--port 7000"
            spellcheck={false}
          />
        )}
      </Field>
      <div class="two">
        <Field label="Working directory" note="Leave empty to use microscope's directory.">
          {(id) => (
            <PathInput
              id={id}
              value={form().cwd}
              onChange={fields.cwd}
              placeholder="."
              onlyDirectories
            />
          )}
        </Field>
        <Field label="Environment" note={environmentNote()}>
          {(id) => (
            <textarea
              id={id}
              class="input"
              value={form().environment}
              aria-invalid={environment().ok ? "false" : "true"}
              onInput={fields.environment}
              placeholder="RUST_LOG=debug"
              spellcheck={false}
            />
          )}
        </Field>
      </div>
      <label class="check">
        <input
          type="checkbox"
          checked={stopAtEntry()}
          onChange={(event) => setStopAtEntry(event.currentTarget.checked)}
        />
        Stop at the first instruction when it starts
      </label>
      <div class="actions">
        <button type="submit" class="button primary" disabled={!valid() || props.working}>
          Load
        </button>
        <button
          type="button"
          class="button"
          disabled={!valid() || props.working}
          onClick={loadAndRun}
        >
          Load and run
        </button>
        <span class="muted">Load waits for F5 before the program starts.</span>
      </div>
      <Show when={recent().length > 0}>
        <div class="field">
          <span class="label">Recent</span>
          <div class="recent">
            <For each={recent()} keyed={(item) => `${item.program} ${item.arguments}`}>
              {(item) => <RecentChip item={item()} onPick={setForm} />}
            </For>
          </div>
        </div>
      </Show>
    </form>
  );
}

function RecentChip(props: { item: RecentLaunch; onPick(item: RecentLaunch): void }) {
  return (
    <button
      type="button"
      class="chip"
      title={`${props.item.program} ${props.item.arguments}`}
      onClick={() => props.onPick(props.item)}
    >
      {props.item.program.slice(props.item.program.lastIndexOf("/") + 1)}
      {props.item.arguments && ` ${props.item.arguments}`}
    </button>
  );
}

function launchFields(setForm: Setter<RecentLaunch>) {
  return {
    program: (program: string) => setForm((before) => ({ ...before, program })),
    arguments: (event: InputEvent & { currentTarget: HTMLInputElement }) => {
      const value = event.currentTarget.value;
      setForm((before) => ({ ...before, arguments: value }));
    },
    cwd: (cwd: string) => setForm((before) => ({ ...before, cwd })),
    environment: (event: InputEvent & { currentTarget: HTMLTextAreaElement }) => {
      const value = event.currentTarget.value;
      setForm((before) => ({ ...before, environment: value }));
    },
  };
}

const NO_PADDING = { padding: "0" };

const ELLIPSIS: JSX.CSSProperties = { overflow: "hidden", "text-overflow": "ellipsis" };

const PRESETS: { preset: RecordPreset; label: string }[] = [
  { preset: "development", label: "Development: 999 Hz, deep stacks" },
  { preset: "production", label: "Production: 49 Hz, a fraction of a percent of a CPU" },
  { preset: "maximum", label: "Maximum: 9999 Hz" },
];

const LEVELS: { level: SyscallsLevel; label: string; note: string }[] = [
  { level: "off", label: "Off", note: "No system calls." },
  {
    level: "counts",
    label: "Counts",
    note: "Every call counted in the kernel; the failed and slow ones kept whole.",
  },
  { level: "calls", label: "Calls", note: "Every call with its arguments, result, and stack." },
  { level: "full", label: "Full", note: "Calls with whole strings and the bytes behind pointers." },
];

const STOPPING: { allowed: boolean; label: string; note: string }[] = [
  {
    allowed: false,
    label: "Never",
    note: "System calls need CAP_BPF and CAP_PERFMON. Without them, the preset's level records no calls and the profile says why.",
  },
  {
    allowed: true,
    label: "Allowed",
    note: "Without CAP_BPF and CAP_PERFMON, ptrace stops the program at each system call, so each call's time includes the stop.",
  },
];

const PRESET_LEVELS: Readonly<Record<RecordPreset, SyscallsLevel>> = {
  production: "counts",
  development: "calls",
  maximum: "full",
};

function RecordForm(props: { onRecording(): void }) {
  const connection = useConnection();
  const navigate = useNavigate();
  const [form, setForm] = createSignal<RecentLaunch>({
    program: "",
    arguments: "",
    cwd: "",
    environment: "",
  });
  const [preset, setPreset] = createSignal<RecordPreset>("development");
  const [function_, setFunction] = createSignal("");
  const [window_, setWindow] = createSignal("");

  const [level, setLevel] = createSignal<SyscallsLevel | null>(null);
  const [stopping, setStopping] = createSignal(false);
  const shownLevel = createMemo(() => level() ?? PRESET_LEVELS[preset()]);
  const allowStopping = createMemo(() => stopping() && preset() !== "production");
  const [working, setWorking] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const words = createMemo(() => splitWords(form().arguments));
  const environment = createMemo(() => parseEnvironment(form().environment));
  const wordsNote = createMemo(() => {
    const parsed = words();
    return parsed.ok ? undefined : parsed.error;
  });
  const environmentNote = createMemo(() => {
    const parsed = environment();
    return parsed.ok ? "NAME=VALUE, one per line" : parsed.error;
  });
  const seconds = createMemo(() => (window_().trim() === "" ? null : Number(window_())));
  const validWindow = createMemo(() => {
    const value = seconds();
    return value === null || (Number.isFinite(value) && value > 0);
  });
  const valid = createMemo(
    () => form().program.trim() !== "" && words().ok && environment().ok && validWindow(),
  );
  const fields = launchFields(setForm);

  const record = async (event: SubmitEvent) => {
    event.preventDefault();
    const currentWords = words();
    const currentEnv = environment();
    const currentForm = form();
    if (!valid() || !currentWords.ok || !currentEnv.ok) {
      return;
    }
    setError(null);
    setWorking(true);
    try {
      const started = await connection.request("recordProfile", {
        program: currentForm.program.trim(),
        arguments: currentWords.words,
        cwd: currentForm.cwd.trim() || null,
        environment: currentEnv.pairs,
        preset: preset(),
        function: function_().trim() || null,
        window: seconds(),
        syscalls: level(),
        allowStopping: allowStopping(),
      });
      props.onRecording();
      navigate(toHref(`/profile/${started.profile}`));
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setWorking(false);
    }
  };
  const submit = (event: SubmitEvent) => {
    void record(event);
  };

  return (
    <form class="card-body" style={NO_PADDING} onSubmit={submit}>
      <Show when={error()}>
        {(message) => (
          <div class="banner error" role="alert">
            {message()}
          </div>
        )}
      </Show>
      <Field label="Program">
        {(id) => (
          <PathInput
            id={id}
            value={form().program}
            onChange={fields.program}
            autoFocus
            placeholder="./build/app"
            onlyExecutables
          />
        )}
      </Field>
      <Field label="Arguments" note={wordsNote()}>
        {(id) => (
          <input
            id={id}
            class="input"
            value={form().arguments}
            aria-invalid={words().ok ? "false" : "true"}
            onInput={fields.arguments}
            spellcheck={false}
          />
        )}
      </Field>
      <div class="two">
        <Field label="Working directory" note="Leave empty to use microscope's directory.">
          {(id) => (
            <PathInput
              id={id}
              value={form().cwd}
              onChange={fields.cwd}
              placeholder="."
              onlyDirectories
            />
          )}
        </Field>
        <Field label="Environment" note={environmentNote()}>
          {(id) => (
            <textarea
              id={id}
              class="input"
              value={form().environment}
              aria-invalid={environment().ok ? "false" : "true"}
              onInput={fields.environment}
              spellcheck={false}
            />
          )}
        </Field>
      </div>
      <div class="two">
        <Field label="Preset">
          {(id) => (
            <select
              id={id}
              class="input"
              value={preset()}
              onChange={(event) => setPreset(event.currentTarget.value as RecordPreset)}
            >
              <For each={PRESETS}>
                {(each) => (
                  <option value={each.preset} selected={preset() === each.preset}>
                    {each.label}
                  </option>
                )}
              </For>
            </select>
          )}
        </Field>
        <Field label="Keep the last" note="Seconds to keep. Leave empty to keep everything.">
          {(id) => (
            <input
              id={id}
              class="input"
              value={window_()}
              onInput={(event) => setWindow(event.currentTarget.value)}
              placeholder="all"
              inputmode="decimal"
              aria-invalid={validWindow() ? "false" : "true"}
            />
          )}
        </Field>
      </div>
      <div class="two">
        <Field label="System calls" note={LEVELS.find((each) => each.level === shownLevel())?.note}>
          {(id) => (
            <select
              id={id}
              class="input"
              value={shownLevel()}
              onChange={(event) => setLevel(event.currentTarget.value as SyscallsLevel)}
            >
              <For each={LEVELS}>
                {(each) => (
                  <option value={each.level} selected={shownLevel() === each.level}>
                    {each.label}
                  </option>
                )}
              </For>
            </select>
          )}
        </Field>
        <Field
          label="Stopping"
          note={
            preset() === "production"
              ? "The production preset never stops the program."
              : STOPPING.find((each) => each.allowed === allowStopping())?.note
          }
        >
          {(id) => (
            <select
              id={id}
              class="input"
              value={allowStopping() ? "allowed" : "never"}
              disabled={preset() === "production"}
              onChange={(event) => setStopping(event.currentTarget.value === "allowed")}
            >
              <For each={STOPPING}>
                {(each) => (
                  <option
                    value={each.allowed ? "allowed" : "never"}
                    selected={allowStopping() === each.allowed}
                  >
                    {each.label}
                  </option>
                )}
              </For>
            </select>
          )}
        </Field>
      </div>
      <Field label="Function" note="Sample every call of this function instead of CPU time.">
        {(id) => (
          <input
            id={id}
            class="input"
            value={function_()}
            onInput={(event) => setFunction(event.currentTarget.value)}
            placeholder="app::handle"
            spellcheck={false}
          />
        )}
      </Field>
      <div class="actions">
        <button type="submit" class="button primary" disabled={!valid() || working()}>
          Record
        </button>
        <span class="muted">The profile opens as it is recorded.</span>
      </div>
    </form>
  );
}

export function AttachForm(props: FormProps) {
  const connection = useConnection();
  const [processes, setProcesses] = createSignal<Process[] | null>(null);
  const [scope, setScope] = createSignal<number | null>(null);
  const [filter, setFilter] = createSignal("");
  const [selected, setSelected] = createSignal(0);
  const [error, setError] = createSignal<string | null>(null);

  const refresh = async () => {
    try {
      const answer = await connection.request("processes");
      setError(null);
      setProcesses(answer.processes);
      setScope(answer.ptraceScope);
    } catch (failure) {
      setError((failure as Error).message);
    }
  };
  onSettled(() => {
    void refresh();
  });

  const shown = createMemo(() => {
    const words = filter().toLowerCase().split(/\s+/u).filter(Boolean);
    const matching = (processes() ?? []).filter((process) =>
      words.every((word) => `${process.pid} ${process.command}`.toLowerCase().includes(word)),
    );

    const named = (process: Process) => (words.includes(String(process.pid)) ? 0 : 1);
    return matching.toSorted((a, b) => named(a) - named(b));
  });
  const choice = createMemo(() => {
    const list = shown();
    return list[Math.min(selected(), list.length - 1)];
  });
  const restrictedScope = createMemo(() => {
    const current = scope();
    return current !== null && current > 0 ? current : null;
  });

  const attach = (process: Process | undefined) => {
    if (!process) {
      return;
    }
    props.submit({
      describe: `Attaching to process ${process.pid}`,
      send: (replace) => connection.request("attach", { pid: process.pid, replace }),
    });
  };
  const attachChoice = () => attach(choice());
  const select = (process: Process) => {
    const list = shown();
    const index = list.indexOf(process);
    assert(index >= 0, `process ${process.pid} is not in the ${list.length} shown`);
    setSelected(index);
  };
  const type = (event: InputEvent & { currentTarget: HTMLInputElement }) => {
    setFilter(event.currentTarget.value);
    setSelected(0);
  };
  const key = (event: KeyboardEvent) => {
    const list = shown();
    if (event.key === "ArrowDown" && list.length > 0) {
      event.preventDefault();
      setSelected(Math.min(selected() + 1, list.length - 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setSelected(Math.max(selected() - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      attach(choice());
    }
  };

  return (
    <div class="card-body" style={NO_PADDING}>
      <Show when={restrictedScope()}>
        {(currentScope) => (
          <div class="banner">
            The kernel&apos;s <code>ptrace_scope</code> is {currentScope()}, so attaching to a
            process that microscope did not start may be refused. A program can allow it with{" "}
            <code>prctl(PR_SET_PTRACER, …)</code>, or{" "}
            <code>sudo sysctl kernel.yama.ptrace_scope=0</code> allows it until reboot.
          </div>
        )}
      </Show>
      <Field label="Process" note="Your processes, newest first. Type to filter; Enter attaches.">
        {(id) => (
          <input
            id={id}
            class="input"
            value={filter()}
            data-autofocus
            placeholder="kvstore"
            onInput={type}
            onKeyDown={key}
          />
        )}
      </Field>
      <Show when={error()}>{(message) => <div class="banner error">{message()}</div>}</Show>
      {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a list to filter and pick from, not a <select> */}
      <div class="process-list" role="listbox" aria-label="Processes">
        <Switch
          fallback={
            <For each={shown()} keyed={(process) => process.pid}>
              {(process) => (
                <ProcessOption
                  process={process()}
                  chosen={process() === choice()}
                  onSelect={select}
                  onAttach={attach}
                />
              )}
            </For>
          }
        >
          <Match when={processes() === null}>
            <div class="empty">{error() ? "Could not list processes." : "Listing processes…"}</div>
          </Match>
          <Match when={shown().length === 0}>
            <div class="empty">No process matches.</div>
          </Match>
        </Switch>
      </div>
      <div class="actions">
        <button
          type="button"
          class="button primary"
          disabled={!choice() || props.working}
          onClick={attachChoice}
        >
          {choice() ? `Attach to ${choice()?.pid}` : "Attach"}
        </button>
        <button type="button" class="button" onClick={() => void refresh()}>
          Refresh
        </button>
      </div>
    </div>
  );
}

function ProcessOption(props: {
  process: Process;
  chosen: boolean;
  onSelect(process: Process): void;
  onAttach(process: Process): void;
}) {
  return (
    <div
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an option of the list above
      role="option"
      tabindex={-1}
      aria-selected={props.chosen ? "true" : "false"}
      class={["row", { selected: props.chosen }]}
      onClick={() => props.onSelect(props.process)}
      onDblClick={() => props.onAttach(props.process)}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          props.onAttach(props.process);
        }
      }}
    >
      <span class="pid">{props.process.pid}</span>
      <span title={props.process.command} style={ELLIPSIS}>
        {props.process.command}
      </span>
    </div>
  );
}

function CoreForm(props: FormProps) {
  const connection = useConnection();
  const [core, setCore] = createSignal("");
  const [executable, setExecutable] = createSignal("");
  const open = (event: SubmitEvent) => {
    event.preventDefault();
    const path = core().trim();
    if (!path) {
      return;
    }
    props.submit({
      describe: `Opening ${path}`,
      send: (replace) =>
        connection.request("openCore", {
          core: path,
          executable: executable().trim() || null,
          replace,
        }),
    });
  };
  return (
    <form class="card-body" style={NO_PADDING} onSubmit={open}>
      <Field label="Core dump">
        {(id) => (
          <PathInput
            id={id}
            value={core()}
            onChange={setCore}
            placeholder="./core.41870"
            autoFocus
          />
        )}
      </Field>
      <Field label="Executable" note="Leave empty to use the program recorded in the dump.">
        {(id) => (
          <PathInput
            id={id}
            value={executable()}
            onChange={setExecutable}
            placeholder="./build/app"
            onlyExecutables
          />
        )}
      </Field>
      <div class="actions">
        <button type="submit" class="button primary" disabled={!core().trim() || props.working}>
          Open
        </button>
      </div>
    </form>
  );
}

function Field(props: {
  label: string;
  note?: string | undefined;
  children: (id: string) => JSX.Element;
}) {
  const id = createUniqueId();
  return (
    <div class="field">
      <label for={id}>{props.label}</label>
      {props.children(id)}
      <Show when={props.note}>{(note) => <span class="note">{note()}</span>}</Show>
    </div>
  );
}

export function PathInput(props: {
  id: string;
  value: string;
  onChange(value: string): void;
  placeholder?: string;
  autoFocus?: boolean;
  onlyExecutables?: boolean;
  onlyDirectories?: boolean;
}) {
  const connection = useConnection();

  const [completion, setCompletion] = createSignal<{ text: string; entries: PathEntry[] } | null>(
    null,
  );
  const [open, setOpen] = createSignal(false);
  const [selected, setSelected] = createSignal(0);
  const [tabbed, setTabbed] = createSignal(false);
  const [picked, setPicked] = createSignal(false);
  const listId = createUniqueId();
  let input: HTMLInputElement | undefined;

  createEffect(
    () =>
      [
        props.value,
        open(),
        props.onlyExecutables ?? false,
        props.onlyDirectories ?? false,
      ] as const,
    ([value, isOpen, onlyExecutables, onlyDirectories]) => {
      if (!isOpen) {
        return;
      }
      let current = true;
      const complete = async () => {
        try {
          const answer = await connection.request("completePath", { text: value });
          if (current) {
            const entries = answer.entries.filter(
              (entry) =>
                entry.kind === "directory" ||
                (!onlyDirectories && (!onlyExecutables || entry.kind === "executable")),
            );
            setCompletion({ text: value, entries });
            setSelected((index) => Math.min(index, Math.max(entries.length - 1, 0)));
          }
        } catch {
          if (current) {
            setCompletion({ text: value, entries: [] });
          }
        }
      };
      const timer = setTimeout(() => void complete(), 60);
      return () => {
        current = false;
        clearTimeout(timer);
      };
    },
  );

  const accept = (entry: PathEntry | undefined) => {
    if (!entry) {
      return;
    }
    props.onChange(entry.text);
    setSelected(0);
    setPicked(false);
    setOpen(entry.kind === "directory");
  };

  const current = createMemo(() => {
    const completed = completion();
    return completed?.text === props.value ? completed.entries : null;
  });
  const shown = createMemo(() => {
    const entries = current();
    return open() && entries ? entries.slice(0, 50) : [];
  });
  const completes = createMemo(
    () => picked() || (props.value !== "" && !props.value.endsWith("/")),
  );

  createEffect(
    () => selected(),
    (index) => {
      assert(index >= 0, `suggestion ${index} selected`);
    },
  );

  createEffect(
    () => [tabbed(), current()] as const,
    ([wasTabbed, entries]) => {
      if (wasTabbed && entries) {
        setTabbed(false);
        if (entries.length > 0) {
          accept(entries[0]);
        } else if (input) {
          focusAfter(input);
        }
      }
    },
  );

  const type = (event: InputEvent & { currentTarget: HTMLInputElement }) => {
    props.onChange(event.currentTarget.value);
    setOpen(true);
    setTabbed(false);
    setSelected(0);
    setPicked(false);
  };
  const focus = () => setOpen(true);
  const dismiss = () => {
    setOpen(false);
    setTabbed(false);
  };
  const key = (event: KeyboardEvent) => {
    const tab = event.key === "Tab" && !event.shiftKey && completes();
    const list = shown();
    const currentIndex = selected();
    if (tab && open() && current() === null) {
      event.preventDefault();
      setTabbed(true);
    } else if (tab && list.length > 0) {
      event.preventDefault();
      accept(list[currentIndex]);
    } else if (event.key === "ArrowDown" && list.length > 0) {
      event.preventDefault();
      setSelected(Math.min(currentIndex + 1, list.length - 1));
      setPicked(true);
    } else if (event.key === "ArrowUp" && list.length > 0) {
      event.preventDefault();
      setSelected(Math.max(currentIndex - 1, 0));
      setPicked(true);
    } else if (event.key === "Escape") {
      dismiss();
    } else if (
      event.key === "Enter" &&
      list.length > 0 &&
      list[currentIndex]?.text !== props.value
    ) {
      event.preventDefault();
      accept(list[currentIndex]);
    }
  };

  return (
    <div class="combo">
      <input
        ref={(node) => {
          input = node;
        }}
        id={props.id}
        class="input"
        value={props.value}
        placeholder={props.placeholder}
        data-autofocus={props.autoFocus || undefined}
        spellcheck={false}
        autocomplete="off"
        role="combobox"
        aria-expanded={shown().length > 0 ? "true" : "false"}
        aria-controls={listId}
        onInput={type}
        onFocus={focus}
        onBlur={dismiss}
        onKeyDown={key}
      />
      <Show when={shown().length > 0}>
        {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a combobox's suggestions, not a <select> */}
        <div class="suggestions" id={listId} role="listbox">
          <For each={shown()} keyed={(entry) => entry.text}>
            {(entry, index) => (
              <Suggestion entry={entry()} selected={index() === selected()} onAccept={accept} />
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function focusAfter(from: HTMLElement) {
  assert(from.tabIndex === 0, `moving on from tabindex ${from.tabIndex}`);
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_ELEMENT);
  walker.currentNode = from;
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if ((node instanceof HTMLElement || node instanceof SVGElement) && node.tabIndex === 0) {
      node.focus();
      if (document.activeElement === node) {
        return;
      }
    }
  }
}

function Suggestion(props: {
  entry: PathEntry;
  selected: boolean;
  onAccept(entry: PathEntry): void;
}) {
  const choose = (event: MouseEvent) => {
    event.preventDefault();
    props.onAccept(props.entry);
  };
  return (
    <div
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a combobox's suggestion, not an <option>
      role="option"
      tabindex={-1}
      aria-selected={props.selected ? "true" : "false"}
      onMouseDown={choose}
    >
      <span>
        {props.entry.text.slice(props.entry.text.lastIndexOf("/", props.entry.text.length - 2) + 1)}
      </span>
      <span class="kind">{props.entry.kind}</span>
    </div>
  );
}
