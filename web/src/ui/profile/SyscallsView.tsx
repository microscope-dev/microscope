import { type ChangeEvent, type FormEvent, useCallback, useMemo, useState } from "react";

import { type SyscallTab, syscallQueryOf, withLook } from "../../profile/look";
import { formatCount } from "../../profile/paint";
import { lossNotes, unkept } from "../../profile/syscalls";
import { useAnswer, useProfile } from "./context";
import { LookButton } from "./parts";
import { Lanes } from "./SyscallLanes";
import { Log } from "./SyscallLog";
import { Summary } from "./SyscallSummary";
import { Files, Network, Processes } from "./SyscallTables";

const TABS: { tab: SyscallTab | undefined; label: string }[] = [
  { tab: undefined, label: "Summary" },
  { tab: "log", label: "Log" },
  { tab: "files", label: "Files" },
  { tab: "network", label: "Network" },
  { tab: "processes", label: "Processes" },
];

/** What the kernel did for the program. System calls are its first kind,
 * and the one it has so far. */
export function KernelPane() {
  return (
    <div className="kernel" data-testid="kernel">
      <div className="segmented kernel-tabs" role="tablist" aria-label="Kernel data">
        <button type="button" role="tab" id="kernel-syscalls" aria-selected="true">
          Syscalls
        </button>
      </div>
      <div className="kernel-body" role="tabpanel" aria-labelledby="kernel-syscalls">
        <SyscallsView />
      </div>
    </div>
  );
}

/** The profile's system calls: what they cost, when, from where, and what
 * they did. Every panel reads the same selection, in strace's `-e`
 * syntax, over the same time range as the rest of the profile. A
 * recording that kept none, or only the exceptions, says so in their
 * place, with the flag that would have kept them. */
function SyscallsView() {
  const { summary, look } = useProfile();
  const missing = unkept(summary.settings);
  return (
    <div className="syscalls" data-testid="syscalls">
      {missing && summary.syscalls === 0 ? (
        <>
          <div className="syscalls-bar">
            <Tabs tab={look.st} />
          </div>
          <Unkept {...missing} />
        </>
      ) : (
        <Calls missing={missing} />
      )}
    </div>
  );
}

function Tabs({ tab: shown }: { tab: SyscallTab | undefined }) {
  return (
    <div className="segmented">
      {TABS.map(({ tab, label }) => (
        <LookButton key={label} field="st" value={tab} pressed={shown === tab}>
          {label}
        </LookButton>
      ))}
    </div>
  );
}

type Missing = NonNullable<ReturnType<typeof unkept>>;

function Unkept({ text, flag }: Missing) {
  return (
    <p className="profile-untimed muted" data-testid="syscalls-unkept">
      {text} Record with <code>{flag}</code> to keep each call.
    </p>
  );
}

/** The calls the recording kept, under what it left out when it kept
 * only some. */
function Calls({ missing }: { missing: Missing | null }) {
  const { profile, look, query } = useProfile();
  const base = useMemo(
    () => syscallQueryOf(profile, look, query.revision ?? 0),
    [profile, look, query.revision],
  );
  const summary = useAnswer("syscallSummary", { ...base, group: look.sg ?? "syscall" });
  return (
    <>
      <div className="syscalls-bar">
        <FilterBar error={summary.error?.message} />
        <Tabs tab={look.st} />
        {summary.data ? (
          <span className="muted syscalls-counts" data-testid="syscall-counts">
            {formatCount(summary.data.calls)} of {formatCount(summary.data.total, "call")} ·{" "}
            {formatCount(summary.data.errors)} failed
            {summary.data.engine ? ` · ${summary.data.engine}` : ""}
            {lossNotes(summary.data.lost).map((note) => (
              <span key={note.text} className={note.warn ? "warn" : undefined} title={note.title}>
                {" "}
                · {note.text}
              </span>
            ))}
          </span>
        ) : null}
      </div>
      {missing ? <Unkept {...missing} /> : <Lanes query={base} />}
      <div className="syscalls-body">
        {look.st === "log" ? (
          // A new selection starts at its first page.
          <Log key={JSON.stringify(base)} query={base} />
        ) : look.st === "files" ? (
          <Files query={base} />
        ) : look.st === "network" ? (
          <Network query={base} />
        ) : look.st === "processes" ? (
          <Processes query={base} />
        ) : (
          <Summary summary={summary} query={base} />
        )}
      </div>
    </>
  );
}

/** The selection, as `-e` takes it; Enter applies it. */
function FilterBar({ error }: { error: string | undefined }) {
  const { look, setLook } = useProfile();
  const [text, setText] = useState(look.sf ?? "");
  // A selection applied elsewhere replaces what is typed.
  const [applied, setApplied] = useState(look.sf);
  if (applied !== look.sf) {
    setApplied(look.sf);
    setText(look.sf ?? "");
  }
  const apply = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      setLook((prev) => withLook(prev, "sf", text.trim() || undefined), true);
    },
    [setLook, text],
  );
  const edit = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setText(event.target.value),
    [],
  );
  return (
    <form className="syscalls-filter" onSubmit={apply}>
      <input
        className="input mono"
        aria-label="Selection"
        placeholder="trace=%file status=failed"
        value={text}
        onChange={edit}
        aria-invalid={error ? true : undefined}
      />
      {error ? (
        <span className="error-text" role="alert">
          {error}
        </span>
      ) : null}
    </form>
  );
}
