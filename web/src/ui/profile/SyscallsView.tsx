import { type ChangeEvent, type FormEvent, useCallback, useMemo, useState } from "react";

import { type SyscallTab, syscallQueryOf, withLook } from "../../profile/look";
import { formatCount } from "../../profile/paint";
import { bucketLabel } from "../../profile/syscalls";
import { useAnswer, useProfile } from "./context";
import { ClearChip, LookButton } from "./parts";
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

/** The profile's system calls: what they cost, when, from where, and what
 * they did. Every panel reads the same selection, in strace's `-e`
 * syntax, over the same time range as the rest of the profile. */
export function SyscallsView() {
  const { profile, look, query } = useProfile();
  const base = useMemo(
    () => syscallQueryOf(profile, look, query.revision ?? 0),
    [profile, look, query.revision],
  );
  const summary = useAnswer("syscallSummary", { ...base, group: look.sg ?? "syscall" });
  return (
    <div className="syscalls" data-testid="syscalls">
      <div className="syscalls-bar">
        <FilterBar error={summary.error?.message} />
        <div className="segmented">
          {TABS.map(({ tab, label }) => (
            <LookButton key={label} field="st" value={tab} pressed={look.st === tab}>
              {label}
            </LookButton>
          ))}
        </div>
        {look.sl ? (
          <ClearChip
            field="sl"
            label={bucketLabel(Number(look.sl))}
            title="Calls of this latency"
            removeLabel="Remove the latency band"
          />
        ) : null}
        {summary.data ? (
          <span className="muted syscalls-counts" data-testid="syscall-counts">
            {formatCount(summary.data.calls)} of {formatCount(summary.data.total, "call")} ·{" "}
            {formatCount(summary.data.errors)} failed
            {summary.data.engine ? ` · ${summary.data.engine}` : ""}
            {summary.data.lost > 0 ? (
              <span className="warn"> · lost {formatCount(summary.data.lost)}</span>
            ) : null}
          </span>
        ) : null}
      </div>
      {summary.data?.counted ? (
        <div className="profile-untimed muted">
          The kernel counted these calls without reporting each, so there is only the summary.
        </div>
      ) : (
        <Lanes query={base} />
      )}
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
    </div>
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
