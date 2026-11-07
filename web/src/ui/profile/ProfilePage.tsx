import { useLocation, useNavigate } from "@tanstack/react-router";
import {
  type ChangeEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import {
  type ProfileLook,
  parseRange,
  parseTransform,
  queryOf,
  rangeOf,
  untransformed,
  validateProfileLook,
  withLook,
} from "../../profile/look";
import {
  formatCount,
  formatDuration,
  formatPercent,
  formatSpan,
  formatWeight,
} from "../../profile/paint";
import { tracedBy, traceNotes } from "../../profile/syscalls";
import type { ProfileCommand, ProfileInfo, ProfileSummary } from "../../protocol";
import { useConnection, useModel } from "../../store";
import type { PaneProps } from "../../workbench/kinds";
import { groups, type Json, type Pane, setParams } from "../../workbench/layout";
import { change, useLayout, workbench } from "../../workbench/store";
import { useDialogFocus, useDismiss } from "../dialog";
import {
  ProfileContext,
  type ProfileFocus,
  useAnswer,
  useDisconnected,
  useProfile,
} from "./context";
import { CommandBlock, FileDetails } from "./FileDetails";
import { FlameView } from "./FlameView";
import { FunctionsView } from "./FunctionsView";
import { defaultView, Options } from "./Options";
import { Chip, ClearChip, LaneChip, LookButton } from "./parts";
import { SyscallsView } from "./SyscallsView";
import { Timeline } from "./Timeline";
import { TreeView } from "./TreeView";
import { ZonesView } from "./ZonesView";

/** What a profile pane keeps: the profile's name, for its tab, and how it
 * looks at the profile. */
export interface ProfileParams {
  readonly name: string;
  readonly look: ProfileLook;
}

export function profileParams(pane: Pane): ProfileParams {
  const params = (pane.params ?? {}) as { name?: unknown; look?: unknown };
  const look = typeof params.look === "object" && params.look !== null ? params.look : {};
  return {
    name: typeof params.name === "string" ? params.name : "",
    look: validateProfileLook(look as Record<string, unknown>),
  };
}

const sameLook = (a: ProfileLook, b: ProfileLook) => JSON.stringify(a) === JSON.stringify(b);

/** A profile pane. The pane keeps its look; while its tab is the one shown
 * in the focused group and no session's focus is in the address, the
 * address follows it, so a link opens the same view. */
export function ProfileBody({ id, pane }: PaneProps) {
  const profile = pane.key;
  const { look } = profileParams(pane);
  const navigate = useNavigate();
  const location = useLocation();
  const inSession = location.pathname.startsWith("/s/");
  const focused = useLayout(
    (layout) => groups(layout).find((group) => group.id === layout.focused)?.active === id,
  );
  const leads = focused && !inSession;

  const setLook = useCallback(
    (by: (look: ProfileLook) => ProfileLook, push = false) => {
      const now = workbench.getState().layout.panes[id];
      if (!now) {
        return;
      }
      const params = profileParams(now);
      const next = by(params.look);
      change((layout) => setParams(layout, id, { ...params, look: next } as unknown as Json));
      if (leads) {
        void navigate({
          to: "/profile/$profile",
          params: { profile },
          search: next,
          replace: !push,
        });
      }
    },
    [id, leads, navigate, profile],
  );

  // The address names the profile this pane shows, as it changes.
  const search = location.search as Record<string, unknown>;
  const addressed = location.pathname === `/profile/${profile}`;
  useEffect(() => {
    if (leads && (!addressed || !sameLook(validateProfileLook(search), look))) {
      void navigate({ to: "/profile/$profile", params: { profile }, search: look, replace: true });
    }
  }, [leads, addressed, search, look, navigate, profile]);

  return <Profile profile={profile} look={look} setLook={setLook} />;
}

const MISSING_CARD = { width: "min(560px, 100%)" };
const NO_MARGIN = { margin: 0 };

/** A profile as `look` looks at it. */
export function Profile({
  profile,
  look,
  setLook,
}: {
  profile: string;
  look: ProfileLook;
  setLook: ProfileFocus["setLook"];
}) {
  const info = useModel((model) => model.profiles.find((each) => each.id === profile));
  // Each revision of a growing profile is asked for anew; the last one's
  // answers stay on screen until the new ones come.
  const revision = info?.revision ?? 0;
  const summary = useAnswer("profile", { profile, revision });
  const disconnected = useDisconnected();
  const focus = useMemo<ProfileFocus | null>(
    () =>
      summary.data
        ? {
            profile,
            summary: summary.data,
            look,
            query: { ...queryOf(profile, look), revision },
            unit: look.src === "waits" ? "nanoseconds" : summary.data.unit,
            setLook,
          }
        : null,
    [profile, summary.data, look, setLook, revision],
  );

  if (summary.error) {
    // The server lists every profile it serves, and refuses one it does
    // not as invalid; anything else failed reading one it has.
    const missing = !info && summary.error.kind === "invalid";
    return (
      <div className="page">
        <div className="card" style={MISSING_CARD} data-testid="profile-missing">
          <div className="card-head">
            <h1>{missing ? "No such profile" : "Could not read the profile"}</h1>
          </div>
          <div className="card-body">
            <p style={NO_MARGIN}>{summary.error.message}.</p>
            {missing ? (
              <p className="muted" style={NO_MARGIN}>
                Serve it with <code>microscope web PROFILE.usprof</code>.
              </p>
            ) : null}
          </div>
        </div>
      </div>
    );
  }
  if (!focus) {
    return <div className="page muted">Reading the profile…</div>;
  }
  const view = look.view ?? defaultView(focus.summary);
  return (
    <ProfileContext.Provider value={focus}>
      <div className="profile" data-testid="profile">
        <Accounting summary={focus.summary} />
        {disconnected ? (
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result
          <div className="profile-untimed profile-disconnected" role="status">
            Disconnected. The views keep what they last read until microscope answers again.
          </div>
        ) : null}
        {info && (info.recording || info.ended) ? <Recording info={info} /> : null}
        {focus.summary.timed ? (
          <Timeline />
        ) : (
          <div className="profile-untimed muted" data-testid="untimed">
            Read from {focus.summary.source ?? "a file"}, which keeps no times or threads: this
            profile has no timeline.
          </div>
        )}
        <ProfileHead info={info} />
        <div className="profile-view">
          {view === "syscalls" ? (
            <SyscallsView />
          ) : view === "zones" ? (
            <ZonesView />
          ) : view === "functions" ? (
            <FunctionsView />
          ) : view === "tree" ? (
            <TreeView />
          ) : (
            <FlameView />
          )}
        </div>
      </div>
    </ProfileContext.Provider>
  );
}

/** The profile's name, the views, and everything the query holds, each
 * removable where it shows. */
function ProfileHead({ info }: { info: ProfileInfo | undefined }) {
  const { summary, look } = useProfile();
  const [detailing, setDetailing] = useState(false);
  const showDetails = useCallback(() => setDetailing(true), []);
  const hideDetails = useCallback(() => setDetailing(false), []);
  const range = rangeOf(look);
  const snapshots = summary.snapshots > 0;
  // A trace of system calls alone has no frames to compare.
  const sampled = summary.samples > 0 || summary.syscalls === 0;

  return (
    <div className="profile-head">
      <button type="button" className="tb ghost" aria-haspopup="dialog" onClick={showDetails}>
        File details
      </button>
      {detailing ? <FileDetails info={info} close={hideDetails} /> : null}
      {snapshots ? (
        <div className="segmented">
          {SOURCES.map(([src, label, title]) => (
            <LookButton
              key={label}
              field="src"
              value={src}
              title={title}
              pressed={look.src === src}
            >
              {label}
            </LookButton>
          ))}
        </div>
      ) : null}
      <Options />
      <div className="profile-query">
        {range ? (
          <ClearChip field="range" label={formatSpan(range)} title="Samples in this time range" />
        ) : null}
        <LaneChip />
        {(look.lb ?? []).map((label) => (
          <LabelChip key={label} label={label} />
        ))}
        {(look.tf ?? []).map((entry, index) => (
          <TransformChip key={entry} entry={entry} index={index} />
        ))}
        {sampled ? <Compare /> : null}
      </div>
    </div>
  );
}

const SOURCES = [
  [undefined, "CPU", "Where the program ran"],
  ["waits", "Waits", "Where task snapshots found tasks waiting"],
] as const;

const DIFF_MEASURES = [
  [undefined, "time", "Each function's cost, as report --base compares it"],
  ["share", "share", "Each function's share of its own profile"],
] as const;

function LabelChip({ label }: { label: string }) {
  const { setLook } = useProfile();
  const remove = useCallback(
    () =>
      setLook((prev) => {
        const kept = (prev.lb ?? []).filter((each) => each !== label);
        return withLook(prev, "lb", kept.length > 0 ? kept : undefined);
      }),
    [setLook, label],
  );
  return <Chip label={label} title="Samples that carry this label" remove={remove} />;
}

function TransformChip({ entry, index }: { entry: string; index: number }) {
  const { setLook } = useProfile();
  const remove = useCallback(
    () => setLook((prev) => untransformed(prev, index), true),
    [setLook, index],
  );
  const transform = parseTransform(entry);
  // Collapsing recursion is set from Options, not shown as a chip.
  return transform && transform.op !== "collapseCycles" ? (
    <Chip
      label={`${transform.op} ${transform.name}`}
      title="Remove this transform and those after it"
      remove={remove}
    />
  ) : null;
}

/** What a diff compares with: another range of this profile, chosen from
 * the selection, or another profile served; a chip once chosen. */
function Compare() {
  const { profile, look, setLook } = useProfile();
  const profiles = useModel((model) => model.profiles);
  const others = profiles.filter((each) => each.id !== profile);
  const baseRange = look.br ? parseRange(look.br) : null;

  const removeBase = useCallback(
    () =>
      setLook(
        (prev) =>
          // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
          withLook(withLook(withLook(prev, "bp", undefined), "br", undefined), "dm", undefined),
        true,
      ),
    [setLook],
  );
  const chooseBase = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const chosen = event.target.value;
      setLook(
        (prev) =>
          chosen === "range"
            ? // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
              withLook(withLook(prev, "br", prev.range), "range", undefined)
            : withLook(prev, "bp", chosen || undefined),
        true,
      );
    },
    [setLook],
  );

  if (look.bp || baseRange) {
    const of =
      look.bp && look.bp !== profile
        ? (others.find((each) => each.id === look.bp)?.name ?? look.bp)
        : "this profile";
    const during = baseRange ? ` ${formatSpan(baseRange)}` : "";
    return (
      <>
        <Chip
          label={`diff with ${of}${during}`}
          title="Red grew and blue shrank against this base"
          remove={removeBase}
        />
        <div className="segmented" title="What the change column compares">
          {DIFF_MEASURES.map(([dm, label, title]) => (
            <LookButton key={label} field="dm" value={dm} title={title} pressed={look.dm === dm}>
              {label}
            </LookButton>
          ))}
        </div>
      </>
    );
  }
  if (!look.range && others.length === 0) {
    return null;
  }
  return (
    <select className="input profile-by" aria-label="Compare with" value="" onChange={chooseBase}>
      <option value="">no diff</option>
      {look.range ? <option value="range">diff with the selected range</option> : null}
      {others.map((each) => (
        <option key={each.id} value={each.id}>
          diff with {each.name}
        </option>
      ))}
    </select>
  );
}

/** A recording's progress, or how it ended. */
function Recording({ info }: { info: ProfileInfo }) {
  const connection = useConnection();
  const control = useModel((model) => model.hello?.role === "control");
  const [failure, setFailure] = useState<string | null>(null);
  const stop = useCallback(() => {
    void connection
      .request("stopRecording", { profile: info.id })
      .catch((error: Error) => setFailure(error.message));
  }, [connection, info.id]);
  return (
    <div className="profile-recording" data-testid="recording">
      {info.recording ? (
        <span className="pill running">Recording {info.name}</span>
      ) : (
        <span className="muted">
          {info.name} ended: {info.ended}
        </span>
      )}
      {info.recording && control ? (
        <button type="button" className="tb ghost" onClick={stop}>
          Stop
        </button>
      ) : null}
      {failure ? <span className="error">{failure}</span> : null}
    </div>
  );
}

/** A fact of the recording, its label over its value. */
function Stat({
  label,
  title,
  warn = false,
  children,
}: {
  label: string;
  title?: string;
  warn?: boolean;
  children: ReactNode;
}) {
  return (
    <div className="stat" title={title}>
      <span className="stat-label">{label}</span>
      <span className={warn ? "stat-value warn" : "stat-value"}>{children}</span>
    </div>
  );
}

/** A small bar `share` full, up to all of it. */
function ShareBar({ share }: { share: number }) {
  const style = useMemo(() => ({ width: `${Math.min(Math.max(share, 0), 1) * 100}%` }), [share]);
  return (
    <span className="stat-bar" aria-hidden="true">
      <i style={style} />
    </span>
  );
}

/** The program recorded, by its name, which shows its whole command. */
function Program({ summary }: { summary: ProfileSummary }) {
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLDivElement>(null);
  const toggle = useCallback(() => setOpen((was) => !was), []);
  const away = useCallback(() => setOpen(false), []);
  useDismiss(anchor, open, away);
  const { command } = summary;
  const program = command?.arguments?.[0] ?? command?.text.split(" ")[0] ?? summary.program;
  if (!program) {
    return null;
  }
  // By its name alone: a path, such as a Nix store's, says little at a glance.
  const name = program.slice(program.lastIndexOf("/") + 1);
  return (
    <div className="stat anchor" ref={anchor}>
      <span className="stat-label">Program</span>
      {command ? (
        <button
          type="button"
          className="stat-value stat-program"
          aria-haspopup="dialog"
          aria-expanded={open}
          title="Show the whole command"
          onClick={toggle}
        >
          {name}
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path
              d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8s-2.4 4.5-6.5 4.5S1.5 8 1.5 8Z"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.3"
              strokeLinejoin="round"
            />
            <circle cx="8" cy="8" r="2" fill="currentColor" />
          </svg>
        </button>
      ) : (
        <span className="stat-value">{name}</span>
      )}
      {open && command ? <CommandPopover command={command} /> : null}
    </div>
  );
}

/** Its focus goes back to the program's button when it closes. */
function CommandPopover({ command }: { command: ProfileCommand }) {
  const dialog = useRef<HTMLDivElement>(null);
  useDialogFocus(dialog);
  return (
    <div
      className="popover command-popover"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a popover, not a modal <dialog>
      role="dialog"
      aria-label="Command"
      tabIndex={-1}
      ref={dialog}
    >
      <CommandBlock command={command} />
    </div>
  );
}

/** What was recorded and how far to trust it, always in view: the program,
 * how long it ran and on how many threads, where its time went, how it was
 * sampled, and any loss, which alone shows in warning colors. */
function Accounting({ summary }: { summary: ProfileSummary }) {
  const truncated = summary.truncations.reduce((sum, each) => sum + each.count, 0);
  const setting = (name: string) => summary.settings.find((each) => each.name === name)?.value;
  const source = setting("source");
  const rate = source?.match(/^CPU time at (\d+ Hz)$/u)?.[1];
  const kernel = setting("kernel") === "on";
  const paused = summary.gaps.reduce((sum, [from, to]) => sum + (to - from), 0);
  const behind = summary.behind.reduce((sum, [from, to]) => sum + (to - from), 0);
  const span = Math.max(summary.duration, 1);
  const offCpu = summary.threads.reduce((sum, each) => sum + each.blocked + each.preempted, 0);
  const cpu = summary.unit === "nanoseconds" && summary.timed;
  const busy = summary.weight / span;
  const by = tracedBy(summary.settings);
  const vdso =
    "Calls through the vDSO, such as clock_gettime, never enter the kernel and are not traced";
  const probesLost =
    summary.probes.overwritten +
    summary.probes.noRing +
    summary.probes.interrupted +
    summary.probes.malformed;
  const sampled = summary.samples > 0 || summary.syscalls === 0;

  return (
    <div className="profile-accounting" data-testid="accounting">
      <Program summary={summary} />
      {sampled && source ? (
        <Stat label="Sampling" title={kernel ? `${source}, kernel too` : source}>
          {rate ?? source}
          {kernel ? " + kernel" : ""}
        </Stat>
      ) : null}
      {summary.timed ? (
        <Stat label="Ran for" title="How long the recording ran">
          {formatDuration(summary.duration)}
        </Stat>
      ) : null}
      {summary.threads.length > 0 ? (
        <Stat label="Threads">{formatCount(summary.threads.length)}</Stat>
      ) : null}
      {sampled && cpu ? (
        <>
          <Stat
            label="Cores busy"
            title={`On average ${busy.toFixed(1)} CPUs ran the program at once${summary.threads.length > 0 ? `, of ${summary.threads.length} threads` : ""}`}
          >
            ≈{busy.toFixed(1)}
            {summary.threads.length > 0 ? <ShareBar share={busy / summary.threads.length} /> : null}
          </Stat>
          <Stat label="On CPU" title="CPU time sampled, every thread's added up">
            {formatDuration(summary.weight)}
          </Stat>
        </>
      ) : sampled ? (
        <Stat label="Sampled">{formatWeight(summary.weight, summary.unit)}</Stat>
      ) : null}
      {offCpu > 0 ? (
        <Stat label="Off CPU" title="Time the threads spent blocked or preempted, added up">
          {formatDuration(offCpu)}
        </Stat>
      ) : null}
      {summary.syscalls > 0 ? (
        <Stat label="Calls traced" title={by.title ? `${by.title}\n${vdso}` : vdso}>
          {formatCount(summary.syscalls)}
          {by.text}
        </Stat>
      ) : null}
      {summary.syscallsLost > 0 ? (
        <Stat label="Calls lost" warn>
          {formatCount(summary.syscallsLost)}
        </Stat>
      ) : null}
      {summary.throttles > 0 ? (
        <Stat label="Throttled" warn>
          {formatCount(summary.throttles)}
        </Stat>
      ) : null}
      {truncated > 0 ? (
        <Stat
          label="Cut short"
          title={summary.truncations.map((each) => `${each.what}: ${each.count}`).join("\n")}
          warn
        >
          {formatPercent(truncated, summary.samples)}
        </Stat>
      ) : null}
      {summary.gaps.length > 0 ? (
        <Stat
          label="Paused for budget"
          title={summary.budgetChanges
            .map((each) => `${formatDuration(each.time)}: ${each.text}`)
            .join("\n")}
          warn
        >
          {formatPercent(paused, span)}
        </Stat>
      ) : null}
      {summary.behind.length > 0 ? (
        <Stat
          label="Paused to catch up"
          title={`Sampling was off ${formatCount(summary.behind.length, "time")} while microscope caught up on the samples it had read; a lower --rate or --stack-bytes keeps more`}
          warn
        >
          {formatPercent(behind, span)}
        </Stat>
      ) : null}
      {summary.probes.events > 0 || probesLost > 0 ? (
        <Stat
          label="Probe events"
          title={[
            `${formatCount(summary.probes.overwritten, "record")} overwritten before microscope read them`,
            `${formatCount(summary.probes.noRing, "event")} found no ring`,
            `${formatCount(summary.probes.interrupted, "event")} dropped by a signal handler`,
            `${formatCount(summary.probes.malformed, "record")} broke the probes ABI`,
            ...summary.probeProblems.map((problem) => problem.text),
          ].join("\n")}
          warn={probesLost > 0 || summary.probeProblems.length > 0}
        >
          {formatCount(summary.probes.events)}, lost {formatCount(probesLost)}
        </Stat>
      ) : null}
      {summary.snapshots > 0 ? (
        <Stat
          label="Task snapshots"
          title={[
            `${formatPercent(summary.taskSamples, summary.samples)} of samples know their task`,
            ...summary.snapshotProblems.map(
              (problem) => `${formatDuration(problem.time)}: ${problem.text}`,
            ),
          ].join("\n")}
          warn={summary.snapshotProblems.length > 0}
        >
          {formatCount(summary.snapshots)}, stopped {formatPercent(summary.snapshotPause, span)}
        </Stat>
      ) : null}
      {traceNotes(summary.settings).map((note) => (
        <span
          key={note.text}
          className={note.warn ? "stat-note warn" : "stat-note"}
          title={note.title}
        >
          {note.text}
        </span>
      ))}
    </div>
  );
}
