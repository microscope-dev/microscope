import { useLocation, useNavigate } from "@tanstack/react-router";
import { type ChangeEvent, type ReactNode, useCallback, useEffect, useMemo, useState } from "react";

import {
  type ProfileLook,
  type ProfileView,
  parseRange,
  parseTransform,
  queryOf,
  rangeOf,
  transformed,
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
import type { ProfileGranularity, ProfileInfo, ProfileSummary } from "../../protocol";
import { useConnection, useModel } from "../../store";
import { presented, serverUrl } from "../../target";
import type { PaneProps } from "../../workbench/kinds";
import { groups, type Json, type Pane, setParams } from "../../workbench/layout";
import { change, useLayout, workbench } from "../../workbench/store";
import {
  ProfileContext,
  type ProfileFocus,
  useAnswer,
  useDisconnected,
  useProfile,
} from "./context";
import { FlameView } from "./FlameView";
import { FunctionsView } from "./FunctionsView";
import { Chip, ClearChip, LookButton } from "./parts";
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
        {disconnected ? (
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result
          <div className="profile-untimed profile-disconnected" role="status">
            Disconnected. The views keep what they last read until microscope answers again.
          </div>
        ) : null}
        {info && (info.recording || info.ended) ? <Recording info={info} /> : null}
        <ProfileHead info={info} />
        {focus.summary.timed ? (
          <Timeline />
        ) : (
          <div className="profile-untimed muted" data-testid="untimed">
            Read from {focus.summary.source ?? "a file"}, which keeps no times or threads: this
            profile has no timeline.
          </div>
        )}
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
        <ProfileFoot summary={focus.summary} />
      </div>
    </ProfileContext.Provider>
  );
}

const VIEWS: { view: ProfileView; label: string }[] = [
  { view: "flame", label: "Flame graph" },
  { view: "tree", label: "Call tree" },
  { view: "functions", label: "Functions" },
  { view: "zones", label: "Zones" },
  { view: "syscalls", label: "Syscalls" },
];

/** A trace with no samples opens on its calls; anything else on its
 * flame graph. */
function defaultView(summary: ProfileSummary): ProfileView {
  return summary.samples === 0 && summary.syscalls > 0 ? "syscalls" : "flame";
}

/** Whether the head offers a view: sample views when there are samples or
 * nothing else, zones when there are some, calls when traced. */
function offers(summary: ProfileSummary, view: ProfileView): boolean {
  switch (view) {
    case "zones":
      return summary.zoneSites.length > 0 || summary.probes.plotted > 0;
    case "syscalls":
      return summary.syscalls > 0;
    default:
      return summary.samples > 0 || summary.syscalls === 0;
  }
}

const GRANULARITIES: { by: ProfileGranularity; label: string }[] = [
  { by: "function", label: "functions" },
  { by: "physical", label: "functions, inlines folded" },
  { by: "line", label: "lines" },
  { by: "address", label: "instructions" },
];

/** The profile's name, the views, and everything the query holds, each
 * removable where it shows. */
function ProfileHead({ info }: { info: ProfileInfo | undefined }) {
  const { summary, look, setLook } = useProfile();
  const range = rangeOf(look);
  const snapshots = summary.snapshots > 0;
  // A trace of system calls alone has no frames to invert, fold, or compare.
  const sampled = summary.samples > 0 || summary.syscalls === 0;
  const taskName = (id: string) =>
    id === "none"
      ? "no task known"
      : (summary.tasks.find((task) => String(task.row) === id)?.name ?? `task ${id}`);

  const collapseCycles = useCallback(
    () => setLook((prev) => transformed(prev, { op: "collapseCycles", name: "" }), true),
    [setLook],
  );
  const rootByLabel = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const key = event.target.value || undefined;
      setLook((prev) => withLook(prev, "bl", key));
    },
    [setLook],
  );
  const framesBy = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const by = event.target.value as ProfileGranularity;
      setLook((prev) => withLook(prev, "by", by === "function" ? undefined : by));
    },
    [setLook],
  );

  return (
    <div className="profile-head">
      <div className="profile-title">
        {/* Its tab names the profile, so this says only how much it holds. */}
        <span className="muted" title={summary.program ?? undefined}>
          {[
            summary.timed ? formatDuration(summary.duration) : null,
            summary.samples > 0 || summary.syscalls === 0
              ? formatCount(summary.samples, "sample")
              : null,
            summary.syscalls > 0 ? formatCount(summary.syscalls, "call") : null,
          ]
            .filter((fact) => fact !== null)
            .join(" · ")}
        </span>
        {info ? <Download info={info} /> : null}
      </div>
      <div className="segmented">
        {VIEWS.filter(({ view }) => offers(summary, view)).map(({ view, label }) => (
          <LookButton
            key={view}
            field="view"
            value={view === defaultView(summary) ? undefined : view}
            pressed={(look.view ?? defaultView(summary)) === view}
          >
            {label}
          </LookButton>
        ))}
      </div>
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
      <div className="profile-query">
        {range ? (
          <ClearChip field="range" label={formatSpan(range)} title="Samples in this time range" />
        ) : null}
        {look.th ? (
          <ClearChip
            field="th"
            label={formatCount(look.th.length, "thread")}
            title="Samples of these threads"
          />
        ) : null}
        {look.tk ? (
          <ClearChip
            field="tk"
            label={
              look.tk.length === 1
                ? taskName(look.tk[0] ?? "")
                : formatCount(look.tk.length, "task")
            }
            title={look.tk.map(taskName).join("\n")}
          />
        ) : null}
        {(look.lb ?? []).map((label) => (
          <LabelChip key={label} label={label} />
        ))}
        {(look.tf ?? []).map((entry, index) => (
          <TransformChip key={entry} entry={entry} index={index} />
        ))}
        {sampled ? (
          <>
            {(look.tf ?? []).includes("collapseCycles") ? null : (
              <button
                type="button"
                className="tb ghost"
                title="Cut each path back to the first call of anything it calls again, directly or through others"
                onClick={collapseCycles}
              >
                Collapse recursion
              </button>
            )}
            <LookCheck field="invert" checked={look.invert === "1"} on="1" off={undefined}>
              Inverted
            </LookCheck>
            <LookCheck
              field="native"
              title="Show each async task's frames as a stack of its own"
              checked={look.native !== "1"}
              on={undefined}
              off="1"
            >
              Async stacks
            </LookCheck>
            {summary.interpreted ? (
              <LookCheck
                field="py"
                title="Show each stack as its Python code sees it, without the interpreter's own frames"
                checked={look.py === "1"}
                on="1"
                off={undefined}
              >
                Python stacks
              </LookCheck>
            ) : null}
          </>
        ) : null}
        {snapshots ? (
          <>
            <LookCheck
              field="bt"
              title="Root each path at the task it ran or waited in"
              checked={look.bt === "1"}
              on="1"
              off={undefined}
            >
              By task
            </LookCheck>
            {look.src === "waits" ? null : (
              <LookCheck
                field="lanes"
                title="A timeline lane per task, not per thread"
                checked={look.lanes === "tasks"}
                on="tasks"
                off={undefined}
              >
                Task lanes
              </LookCheck>
            )}
          </>
        ) : null}
        {summary.labelKeys.length > 0 ? (
          <select
            className="input profile-by"
            aria-label="Root by label"
            value={look.bl ?? ""}
            onChange={rootByLabel}
          >
            <option value="">no label roots</option>
            {summary.labelKeys.map((key) => (
              <option key={key} value={key}>
                rooted by {key}
              </option>
            ))}
          </select>
        ) : null}
        {sampled ? (
          <>
            <select
              className="input profile-by"
              aria-label="Frames by"
              value={look.by ?? "function"}
              onChange={framesBy}
            >
              {GRANULARITIES.map(({ by, label }) => (
                <option key={by} value={by}>
                  by {label}
                </option>
              ))}
            </select>
            <Compare />
          </>
        ) : null}
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

/** A checkbox that sets one part of the look to `on` or `off`. */
function LookCheck<K extends keyof ProfileLook>({
  field,
  title,
  checked,
  on,
  off,
  children,
}: {
  field: K;
  title?: string;
  checked: boolean;
  on: ProfileLook[K] | undefined;
  off: ProfileLook[K] | undefined;
  children: ReactNode;
}) {
  const { setLook } = useProfile();
  const toggle = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => {
      const value = event.target.checked ? on : off;
      setLook((prev) => withLook(prev, field, value));
    },
    [setLook, field, on, off],
  );
  return (
    <label className="check" title={title}>
      <input type="checkbox" checked={checked} onChange={toggle} />
      {children}
    </label>
  );
}

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
  return transform ? (
    <Chip
      label={
        transform.op === "collapseCycles"
          ? "collapse recursion"
          : `${transform.op} ${transform.name}`
      }
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

function fileName(info: ProfileInfo): string {
  return info.name.endsWith(".usprof") ? info.name : `${info.name}.usprof`;
}

/** Downloads the profile as a file. */
async function saveProfile(info: ProfileInfo) {
  const response = await fetch(serverUrl(`api/profile/${info.id}`), {
    method: "POST",
    credentials: "same-origin",
    body: presented(),
  });
  if (!response.ok) {
    throw new Error(`the server answered ${response.status}`);
  }
  const link = document.createElement("a");
  link.href = URL.createObjectURL(await response.blob());
  link.download = fileName(info);
  link.click();
  URL.revokeObjectURL(link.href);
}

/** Downloads the profile as a file, which `microscope web` opens again. */
function Download({ info }: { info: ProfileInfo }) {
  const [failure, setFailure] = useState<string | null>(null);
  const download = useCallback(() => {
    setFailure(null);
    saveProfile(info).catch((error: unknown) => {
      setFailure(error instanceof Error ? error.message : String(error));
    });
  }, [info]);
  return (
    <>
      <button
        type="button"
        className="tb ghost"
        title={`Download ${fileName(info)} to open later with microscope web`}
        onClick={download}
      >
        Download
      </button>
      {failure ? <span className="error">Could not download it: {failure}</span> : null}
    </>
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

/** What the profile cannot show, always in view. */
function ProfileFoot({ summary }: { summary: ProfileSummary }) {
  const truncated = summary.truncations.reduce((sum, each) => sum + each.count, 0);
  const setting = (name: string) => summary.settings.find((each) => each.name === name)?.value;
  const source = setting("source");
  const stack = setting("stack bytes");
  const paused = summary.gaps.reduce((sum, [from, to]) => sum + (to - from), 0);
  const behind = summary.behind.reduce((sum, [from, to]) => sum + (to - from), 0);
  const span = Math.max(summary.duration, 1);
  const by = tracedBy(summary.settings);
  const vdso =
    "Calls through the vDSO, such as clock_gettime, never enter the kernel and are not traced";
  const traced =
    summary.syscalls > 0 ? (
      <>
        <span title={by.title ? `${by.title}\n${vdso}` : vdso}>
          {formatCount(summary.syscalls, "call")} traced{by.text}
        </span>
        <span className={summary.syscallsLost > 0 ? "warn" : undefined}>
          calls lost {formatCount(summary.syscallsLost)}
        </span>
        {traceNotes(summary.settings).map((note) => (
          <span key={note.text} className={note.warn ? "warn" : undefined} title={note.title}>
            {note.text}
          </span>
        ))}
      </>
    ) : null;
  if (summary.samples === 0 && summary.syscalls > 0) {
    return (
      <div className="profile-foot" data-testid="accounting">
        {traced}
      </div>
    );
  }
  const probesLost =
    summary.probes.overwritten +
    summary.probes.noRing +
    summary.probes.interrupted +
    summary.probes.malformed;
  return (
    <div className="profile-foot" data-testid="accounting">
      <span>{formatWeight(summary.weight, summary.unit)} sampled</span>
      {traced}
      {source ? <span>{source}</span> : null}
      {stack ? <span>{stack} B of stack copied</span> : null}
      <span className={summary.lost > 0 ? "warn" : undefined}>
        lost {formatCount(summary.lost)}
      </span>
      <span className={summary.throttles > 0 ? "warn" : undefined}>
        throttled {formatCount(summary.throttles)}
      </span>
      <span
        className={truncated > 0 ? "warn" : undefined}
        title={summary.truncations.map((each) => `${each.what}: ${each.count}`).join("\n")}
      >
        truncated {formatPercent(truncated, summary.samples)}
      </span>
      {summary.recorderCpu > 0 ? (
        <span
          title={`${formatDuration(summary.recorderCpu)} of CPU time recording, ${formatDuration(summary.recorderLoading)} of it reading debug information`}
        >
          microscope {formatPercent(summary.recorderCpu, span)} of a CPU
        </span>
      ) : null}
      {summary.gaps.length > 0 ? (
        <span
          className="warn"
          title={summary.budgetChanges
            .map((each) => `${formatDuration(each.time)}: ${each.text}`)
            .join("\n")}
        >
          paused {formatPercent(paused, span)} for the budget
        </span>
      ) : null}
      {summary.behind.length > 0 ? (
        <span
          className="warn"
          title={`Sampling was off ${formatCount(summary.behind.length, "time")} while microscope caught up on the samples it had read; a lower --rate or --stack-bytes keeps more`}
        >
          paused {formatPercent(behind, span)} to catch up
        </span>
      ) : null}
      {summary.probes.events > 0 || probesLost > 0 ? (
        <span
          className={probesLost > 0 || summary.probeProblems.length > 0 ? "warn" : undefined}
          title={[
            `${formatCount(summary.probes.overwritten, "record")} overwritten before microscope read them`,
            `${formatCount(summary.probes.noRing, "event")} found no ring`,
            `${formatCount(summary.probes.interrupted, "event")} dropped by a signal handler`,
            `${formatCount(summary.probes.malformed, "record")} broke the probes ABI`,
            ...summary.probeProblems.map((problem) => problem.text),
          ].join("\n")}
        >
          {formatCount(summary.probes.events, "probe event")}, lost {formatCount(probesLost)}
        </span>
      ) : null}
      {summary.snapshots > 0 ? (
        <span
          className={summary.snapshotProblems.length > 0 ? "warn" : undefined}
          title={[
            `${formatPercent(summary.taskSamples, summary.samples)} of samples know their task`,
            ...summary.snapshotProblems.map(
              (problem) => `${formatDuration(problem.time)}: ${problem.text}`,
            ),
          ].join("\n")}
        >
          {formatCount(summary.snapshots, "task snapshot")}, stopped{" "}
          {formatPercent(summary.snapshotPause, span)}
        </span>
      ) : null}
    </div>
  );
}
