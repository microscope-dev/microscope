import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { type FormEvent, type ReactNode, useEffect, useId, useMemo, useState } from "react";
import { targetName } from "../model";
import type { PathEntry, Process, RecordPreset } from "../protocol";
import { RequestError } from "../protocol";
import { isRecentLaunches, type RecentLaunch, read, rememberLaunch } from "../storage";
import { useConnection, useModel } from "../store";
import { parseEnvironment, splitWords } from "../words";

export type PickerTab = "launch" | "attach" | "core" | "profile";

/** Validates the `?tab=` search parameter. */
export function pickerSearch(search: Record<string, unknown>): { tab?: PickerTab } {
  const tab = search.tab;
  return tab === "attach" || tab === "core" || tab === "launch" || tab === "profile" ? { tab } : {};
}

/** A request to start something, and how to retry it replacing the session. */
interface Pending {
  describe: string;
  send(replace: boolean): Promise<unknown>;
}

export function Picker() {
  const { tab = "launch" } = useSearch({ from: "/pick" });
  const navigate = useNavigate();
  const control = useModel((model) => model.hello?.role === "control");
  const state = useModel((model) => model.state);
  const [busy, setBusy] = useState<Pending | null>(null);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const current = targetName(state);

  const submit = async (pending: Pending, replace = false) => {
    setError(null);
    setWorking(true);
    try {
      await pending.send(replace);
      setBusy(null);
      void navigate({ to: "/" });
    } catch (failure) {
      if (failure instanceof RequestError && failure.kind === "busy") {
        setBusy(pending);
      } else {
        // A replacement that failed has still ended what it replaced.
        setBusy(null);
        setError(failure instanceof Error ? failure.message : String(failure));
      }
    } finally {
      setWorking(false);
    }
  };

  if (!control) {
    return (
      <div className="page">
        <div className="card">
          <div className="card-body">This link can only view the session.</div>
        </div>
      </div>
    );
  }

  return (
    <div className="page">
      <div className="card" data-testid="picker">
        <div className="card-head">
          <h1>Debug something</h1>
          <div className="segmented">
            {(["launch", "attach", "core", "profile"] as const).map((name) => (
              <button
                key={name}
                type="button"
                aria-pressed={tab === name}
                onClick={() => void navigate({ to: "/pick", search: { tab: name }, replace: true })}
              >
                {
                  { launch: "Launch", attach: "Attach", core: "Core dump", profile: "Profile" }[
                    name
                  ]
                }
              </button>
            ))}
          </div>
          {current && (
            <Link to="/" className="button small">
              Back to {current}
            </Link>
          )}
        </div>
        <div className="card-body">
          {busy && current && (
            <div className="banner" role="alert">
              <span>
                <b>End the {current} session?</b> {busy.describe} stops debugging {current}: a
                launched program is killed, an attached one keeps running. Links to it will say the
                session ended.
              </span>
              <div className="actions">
                <button
                  type="button"
                  className="button primary"
                  disabled={working}
                  onClick={() => void submit(busy, true)}
                >
                  End it and continue
                </button>
                <button type="button" className="button" onClick={() => setBusy(null)}>
                  Cancel
                </button>
              </div>
            </div>
          )}
          {error && (
            <div className="banner error" role="alert">
              {error}
            </div>
          )}
          {tab === "launch" && (
            <LaunchForm working={working} submit={(pending) => void submit(pending)} />
          )}
          {tab === "attach" && (
            <AttachForm working={working} submit={(pending) => void submit(pending)} />
          )}
          {tab === "core" && (
            <CoreForm working={working} submit={(pending) => void submit(pending)} />
          )}
          {tab === "profile" && <RecordForm />}
        </div>
      </div>
    </div>
  );
}

interface FormProps {
  working: boolean;
  submit(pending: Pending): void;
}

function LaunchForm({ working, submit }: FormProps) {
  const connection = useConnection();
  const [recent, setRecent] = useState(() => read("microscope-recent", [], isRecentLaunches));
  const [form, setForm] = useState<RecentLaunch>(
    () => recent[0] ?? { program: "", arguments: "", cwd: "", environment: "" },
  );
  const [stopAtEntry, setStopAtEntry] = useState(false);
  const words = splitWords(form.arguments);
  const environment = parseEnvironment(form.environment);
  const valid = form.program.trim() !== "" && words.ok && environment.ok;

  const launch = (run: boolean) => (event?: FormEvent) => {
    event?.preventDefault();
    if (!valid || !words.ok || !environment.ok) {
      return;
    }
    const program = form.program.trim();
    submit({
      describe: `Launching ${program}`,
      send: async (replace) => {
        await connection.request("launch", {
          program,
          arguments: words.words,
          cwd: form.cwd.trim() || null,
          environment: environment.pairs,
          stopAtEntry,
          run,
          replace,
        });
        // Only what loaded is worth recalling.
        setRecent(rememberLaunch(form));
      },
    });
  };

  return (
    <form className="card-body" style={{ padding: 0 }} onSubmit={launch(false)}>
      <Field label="Program" note="A path on the machine microscope runs on. Tab completes it.">
        {(id) => (
          <PathInput
            id={id}
            value={form.program}
            onChange={(program) => setForm({ ...form, program })}
            autoFocus
            placeholder="./build/app"
            onlyExecutables
          />
        )}
      </Field>
      <Field label="Arguments" note={words.ok ? undefined : words.error}>
        {(id) => (
          <input
            id={id}
            className="input"
            value={form.arguments}
            aria-invalid={!words.ok}
            onChange={(event) => setForm({ ...form, arguments: event.target.value })}
            placeholder="--port 7000"
            spellCheck={false}
          />
        )}
      </Field>
      <div className="two">
        <Field label="Working directory" note="Empty runs it where microscope runs.">
          {(id) => (
            <PathInput
              id={id}
              value={form.cwd}
              onChange={(cwd) => setForm({ ...form, cwd })}
              placeholder="."
              onlyDirectories
            />
          )}
        </Field>
        <Field
          label="Environment"
          note={environment.ok ? "NAME=VALUE, one per line" : environment.error}
        >
          {(id) => (
            <textarea
              id={id}
              className="input"
              value={form.environment}
              aria-invalid={!environment.ok}
              onChange={(event) => setForm({ ...form, environment: event.target.value })}
              placeholder="RUST_LOG=debug"
              spellCheck={false}
            />
          )}
        </Field>
      </div>
      <label className="check">
        <input
          type="checkbox"
          checked={stopAtEntry}
          onChange={(event) => setStopAtEntry(event.target.checked)}
        />
        Stop at the first instruction when it starts
      </label>
      <div className="actions">
        <button type="submit" className="button primary" disabled={!valid || working}>
          Load
        </button>
        <button
          type="button"
          className="button"
          disabled={!valid || working}
          onClick={launch(true)}
        >
          Load and run
        </button>
        <span className="muted">Load waits for F5 before the program starts.</span>
      </div>
      {recent.length > 0 && (
        <div className="field">
          <span className="label">Recent</span>
          <div className="recent">
            {recent.map((item) => (
              <button
                key={`${item.program} ${item.arguments}`}
                type="button"
                className="chip"
                title={`${item.program} ${item.arguments}`}
                onClick={() => setForm(item)}
              >
                {item.program.slice(item.program.lastIndexOf("/") + 1)}
                {item.arguments && ` ${item.arguments}`}
              </button>
            ))}
          </div>
        </div>
      )}
    </form>
  );
}

const PRESETS: { preset: RecordPreset; label: string }[] = [
  { preset: "development", label: "Development: 999 Hz, deep stacks" },
  { preset: "production", label: "Production: 49 Hz, a fraction of a percent of a CPU" },
  { preset: "maximum", label: "Maximum: 9999 Hz" },
];

/** Records a program, opening its profile as it grows. */
function RecordForm() {
  const connection = useConnection();
  const navigate = useNavigate();
  const [form, setForm] = useState<RecentLaunch>({
    program: "",
    arguments: "",
    cwd: "",
    environment: "",
  });
  const [preset, setPreset] = useState<RecordPreset>("development");
  const [function_, setFunction] = useState("");
  const [window_, setWindow] = useState("");
  // System calls are traced with ptrace instead of sampled.
  const [traced, setTraced] = useState(false);
  const [calls, setCalls] = useState("");
  const [stacks, setStacks] = useState(false);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const words = splitWords(form.arguments);
  const environment = parseEnvironment(form.environment);
  const seconds = window_.trim() === "" ? null : Number(window_);
  const validWindow = seconds === null || (Number.isFinite(seconds) && seconds > 0);
  const valid = form.program.trim() !== "" && words.ok && environment.ok && validWindow;

  const record = async (event: FormEvent) => {
    event.preventDefault();
    if (!valid || !words.ok || !environment.ok) {
      return;
    }
    setError(null);
    setWorking(true);
    try {
      const started = await connection.request("recordProfile", {
        program: form.program.trim(),
        arguments: words.words,
        cwd: form.cwd.trim() || null,
        environment: environment.pairs,
        preset,
        function: traced ? null : function_.trim() || null,
        window: traced ? null : seconds,
        syscalls: traced ? { filter: calls.trim(), stacks } : null,
      });
      void navigate({ to: "/profile/$profile", params: { profile: started.profile } });
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setWorking(false);
    }
  };

  return (
    <form className="card-body" style={{ padding: 0 }} onSubmit={(event) => void record(event)}>
      {error && (
        <div className="banner error" role="alert">
          {error}
        </div>
      )}
      <div className="segmented">
        <button type="button" aria-pressed={!traced} onClick={() => setTraced(false)}>
          CPU samples
        </button>
        <button type="button" aria-pressed={traced} onClick={() => setTraced(true)}>
          System calls
        </button>
      </div>
      <Field
        label="Program"
        note={
          traced
            ? "Traced with ptrace, which stops it a few microseconds at each call it traces."
            : "Sampled from outside, so it runs at full speed."
        }
      >
        {(id) => (
          <PathInput
            id={id}
            value={form.program}
            onChange={(program) => setForm({ ...form, program })}
            autoFocus
            placeholder="./build/app"
            onlyExecutables
          />
        )}
      </Field>
      <Field label="Arguments" note={words.ok ? undefined : words.error}>
        {(id) => (
          <input
            id={id}
            className="input"
            value={form.arguments}
            aria-invalid={!words.ok}
            onChange={(event) => setForm({ ...form, arguments: event.target.value })}
            spellCheck={false}
          />
        )}
      </Field>
      <div className="two">
        <Field label="Working directory" note="Empty runs it where microscope runs.">
          {(id) => (
            <PathInput
              id={id}
              value={form.cwd}
              onChange={(cwd) => setForm({ ...form, cwd })}
              placeholder="."
              onlyDirectories
            />
          )}
        </Field>
        <Field
          label="Environment"
          note={environment.ok ? "NAME=VALUE, one per line" : environment.error}
        >
          {(id) => (
            <textarea
              id={id}
              className="input"
              value={form.environment}
              aria-invalid={!environment.ok}
              onChange={(event) => setForm({ ...form, environment: event.target.value })}
              spellCheck={false}
            />
          )}
        </Field>
      </div>
      {traced ? (
        <div className="two">
          <Field label="Calls" note="As strace's -e takes them; empty traces every call.">
            {(id) => (
              <input
                id={id}
                className="input mono"
                value={calls}
                onChange={(event) => setCalls(event.target.value)}
                placeholder="trace=%file,%net"
                spellCheck={false}
              />
            )}
          </Field>
          <label className="check">
            <input
              type="checkbox"
              checked={stacks}
              onChange={(event) => setStacks(event.target.checked)}
            />
            Each call's stack, as -k unwinds it
          </label>
        </div>
      ) : (
        <>
          <div className="two">
            <Field label="Preset">
              {(id) => (
                <select
                  id={id}
                  className="input"
                  value={preset}
                  onChange={(event) => setPreset(event.target.value as RecordPreset)}
                >
                  {PRESETS.map(({ preset, label }) => (
                    <option key={preset} value={preset}>
                      {label}
                    </option>
                  ))}
                </select>
              )}
            </Field>
            <Field label="Calls of" note="A function to sample each call of, instead of CPU time.">
              {(id) => (
                <input
                  id={id}
                  className="input"
                  value={function_}
                  onChange={(event) => setFunction(event.target.value)}
                  placeholder="app::handle"
                  spellCheck={false}
                />
              )}
            </Field>
          </div>
          <div className="two">
            <Field
              label="Keep the last"
              note="Seconds of the recording to keep, which the page shows and saves; empty keeps all of it."
            >
              {(id) => (
                <input
                  id={id}
                  className="input"
                  value={window_}
                  onChange={(event) => setWindow(event.target.value)}
                  placeholder="all"
                  inputMode="decimal"
                  aria-invalid={!validWindow}
                />
              )}
            </Field>
          </div>
        </>
      )}
      <div className="actions">
        <button type="submit" className="button primary" disabled={!valid || working}>
          Record
        </button>
        <span className="muted">The profile opens as it is recorded.</span>
      </div>
    </form>
  );
}

export function AttachForm({ working, submit }: FormProps) {
  const connection = useConnection();
  const [processes, setProcesses] = useState<Process[] | null>(null);
  const [scope, setScope] = useState<number | null>(null);
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState(0);
  const [error, setError] = useState<string | null>(null);

  const refresh = () => {
    connection
      .request("processes")
      .then((answer) => {
        setError(null);
        setProcesses(answer.processes);
        setScope(answer.ptraceScope);
      })
      .catch((failure: Error) => setError(failure.message));
  };
  // biome-ignore lint/correctness/useExhaustiveDependencies: list once on opening
  useEffect(refresh, []);

  const shown = useMemo(() => {
    const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
    return (processes ?? []).filter((process) =>
      words.every((word) => `${process.pid} ${process.command}`.toLowerCase().includes(word)),
    );
  }, [processes, filter]);
  const choice = shown[Math.min(selected, shown.length - 1)];

  const attach = (process: Process | undefined) => {
    if (!process) {
      return;
    }
    submit({
      describe: `Attaching to process ${process.pid}`,
      send: (replace) => connection.request("attach", { pid: process.pid, replace }),
    });
  };

  return (
    <div className="card-body" style={{ padding: 0 }}>
      {scope !== null && scope > 0 && (
        <div className="banner">
          The kernel's <code>ptrace_scope</code> is {scope}, so attaching to a process that
          microscope did not start may be refused. A program can allow it with{" "}
          <code>prctl(PR_SET_PTRACER, …)</code>, or{" "}
          <code>sudo sysctl kernel.yama.ptrace_scope=0</code> allows it until reboot.
        </div>
      )}
      <Field label="Process" note="Your processes, newest first. Type to filter; Enter attaches.">
        {(id) => (
          <input
            id={id}
            className="input"
            value={filter}
            // biome-ignore lint/a11y/noAutofocus: the tab exists to type a filter
            autoFocus
            placeholder="kvstore"
            onChange={(event) => {
              setFilter(event.target.value);
              setSelected(0);
            }}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown") {
                event.preventDefault();
                setSelected(Math.min(selected + 1, shown.length - 1));
              } else if (event.key === "ArrowUp") {
                event.preventDefault();
                setSelected(Math.max(selected - 1, 0));
              } else if (event.key === "Enter") {
                event.preventDefault();
                attach(choice);
              }
            }}
          />
        )}
      </Field>
      {error && <div className="banner error">{error}</div>}
      <div className="process-list" role="listbox" aria-label="Processes">
        {processes === null ? (
          <div className="empty">{error ? "Could not list processes." : "Listing processes…"}</div>
        ) : shown.length === 0 ? (
          <div className="empty">No process matches.</div>
        ) : (
          shown.map((process) => (
            <div
              key={process.pid}
              role="option"
              tabIndex={-1}
              aria-selected={process === choice}
              className={`row ${process === choice ? "selected" : ""}`}
              onClick={() => setSelected(shown.indexOf(process))}
              onDoubleClick={() => attach(process)}
              onKeyDown={(event) => event.key === "Enter" && attach(process)}
            >
              <span className="pid">{process.pid}</span>
              <span
                title={process.command}
                style={{ overflow: "hidden", textOverflow: "ellipsis" }}
              >
                {process.command}
              </span>
            </div>
          ))
        )}
      </div>
      <div className="actions">
        <button
          type="button"
          className="button primary"
          disabled={!choice || working}
          onClick={() => attach(choice)}
        >
          {choice ? `Attach to ${choice.pid}` : "Attach"}
        </button>
        <button type="button" className="button" onClick={refresh}>
          Refresh
        </button>
      </div>
    </div>
  );
}

function CoreForm({ working, submit }: FormProps) {
  const connection = useConnection();
  const [core, setCore] = useState("");
  const [executable, setExecutable] = useState("");
  const open = (event: FormEvent) => {
    event.preventDefault();
    const path = core.trim();
    if (!path) {
      return;
    }
    submit({
      describe: `Opening ${path}`,
      send: (replace) =>
        connection.request("openCore", {
          core: path,
          executable: executable.trim() || null,
          replace,
        }),
    });
  };
  return (
    <form className="card-body" style={{ padding: 0 }} onSubmit={open}>
      <Field label="Core dump">
        {(id) => (
          <PathInput id={id} value={core} onChange={setCore} placeholder="./core.41870" autoFocus />
        )}
      </Field>
      <Field label="Executable" note="Empty uses the one the dump records.">
        {(id) => (
          <PathInput
            id={id}
            value={executable}
            onChange={setExecutable}
            placeholder="./build/app"
            onlyExecutables
          />
        )}
      </Field>
      <div className="actions">
        <button type="submit" className="button primary" disabled={!core.trim() || working}>
          Open
        </button>
      </div>
    </form>
  );
}

function Field({
  label,
  note,
  children,
}: {
  label: string;
  note?: string | undefined;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      {children(id)}
      {note && <span className="note">{note}</span>}
    </div>
  );
}

/** A path input that completes against the server's disk. */
export function PathInput({
  id,
  value,
  onChange,
  placeholder,
  autoFocus = false,
  onlyExecutables = false,
  onlyDirectories = false,
}: {
  id: string;
  value: string;
  onChange(value: string): void;
  placeholder?: string;
  autoFocus?: boolean;
  onlyExecutables?: boolean;
  onlyDirectories?: boolean;
}) {
  const connection = useConnection();
  // Suggestions belong to the text they complete; any others are stale.
  const [completion, setCompletion] = useState<{ text: string; entries: PathEntry[] } | null>(null);
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState(0);
  // Tab pressed before the current text's suggestions arrived.
  const [tabbed, setTabbed] = useState(false);
  // Whether an arrow key chose the selected suggestion.
  const [picked, setPicked] = useState(false);
  const listId = useId();

  useEffect(() => {
    if (!open) {
      return;
    }
    let current = true;
    const timer = setTimeout(() => {
      connection
        .request("completePath", { text: value })
        .then((answer) => {
          if (current) {
            setCompletion({
              text: value,
              entries: answer.entries.filter(
                (entry) =>
                  entry.kind === "directory" ||
                  (!onlyDirectories && (!onlyExecutables || entry.kind === "executable")),
              ),
            });
            setSelected(0);
            setPicked(false);
          }
        })
        .catch(() => {
          // No suggestions, rather than none yet, so Tab is not held back.
          if (current) {
            setCompletion({ text: value, entries: [] });
          }
        });
    }, 60);
    return () => {
      current = false;
      clearTimeout(timer);
    };
  }, [value, open, connection, onlyExecutables, onlyDirectories]);

  const accept = (entry: PathEntry | undefined) => {
    if (!entry) {
      return;
    }
    onChange(entry.text);
    setPicked(false);
    // A directory opens onto its contents; a file is chosen.
    setOpen(entry.kind === "directory");
  };
  const current = completion?.text === value ? completion.entries : null;
  const shown = open && current ? current.slice(0, 50) : [];
  // Tab completes a partial name, or takes what the arrow keys chose; in an
  // empty field or after a directory it moves on, as it would anywhere else.
  const completes = picked || (value !== "" && !value.endsWith("/"));

  useEffect(() => {
    if (tabbed && current) {
      setTabbed(false);
      accept(current[0]);
    }
  });

  return (
    <div className="combo">
      <input
        id={id}
        className="input"
        value={value}
        placeholder={placeholder}
        // biome-ignore lint/a11y/noAutofocus: the picker opens to type a path
        autoFocus={autoFocus}
        spellCheck={false}
        autoComplete="off"
        role="combobox"
        aria-expanded={shown.length > 0}
        aria-controls={listId}
        onChange={(event) => {
          onChange(event.target.value);
          setOpen(true);
          setTabbed(false);
          setPicked(false);
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onKeyDown={(event) => {
          const tab = event.key === "Tab" && !event.shiftKey && completes;
          if (tab && open && current === null) {
            event.preventDefault();
            setTabbed(true);
          } else if (tab && shown.length > 0) {
            event.preventDefault();
            accept(shown[selected]);
          } else if (event.key === "ArrowDown" && shown.length > 0) {
            event.preventDefault();
            setSelected(Math.min(selected + 1, shown.length - 1));
            setPicked(true);
          } else if (event.key === "ArrowUp" && shown.length > 0) {
            event.preventDefault();
            setSelected(Math.max(selected - 1, 0));
            setPicked(true);
          } else if (event.key === "Escape") {
            setOpen(false);
          } else if (event.key === "Enter" && shown.length > 0 && shown[selected]?.text !== value) {
            event.preventDefault();
            accept(shown[selected]);
          }
        }}
      />
      {shown.length > 0 && (
        <div className="suggestions" id={listId} role="listbox">
          {shown.map((entry, index) => (
            <div
              key={entry.text}
              role="option"
              tabIndex={-1}
              aria-selected={index === selected}
              onMouseDown={(event) => {
                event.preventDefault();
                accept(entry);
              }}
            >
              <span>
                {entry.text.slice(entry.text.lastIndexOf("/", entry.text.length - 2) + 1)}
              </span>
              <span className="kind">{entry.kind}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
