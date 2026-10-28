import { useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { useCallback, useMemo, useState } from "react";
import { base } from "../../base";
import {
  type ProfileLook,
  type ProfileView,
  parseRange,
  parseTransform,
  queryOf,
  rangeOf,
  transformed,
  untransformed,
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
import {
  ProfileContext,
  type ProfileFocus,
  useAnswer,
  useDisconnected,
  useProfile,
} from "./context";
import { FlameView } from "./FlameView";
import { FunctionsView } from "./FunctionsView";
import { Chip } from "./parts";
import { SyscallsView } from "./SyscallsView";
import { Timeline } from "./Timeline";
import { TreeView } from "./TreeView";
import { ZonesView } from "./ZonesView";

/** `/profile/$profile`: one profile, explored. */
export function ProfilePage() {
  const { profile } = useParams({ from: "/profile/$profile" });
  const look = useSearch({ from: "/profile/$profile" });
  const navigate = useNavigate();
  const setLook = useCallback(
    (change: (look: ProfileLook) => ProfileLook, push = false) =>
      void navigate({
        to: "/profile/$profile",
        params: { profile },
        search: change(look),
        replace: !push,
      }),
    [navigate, profile, look],
  );
  return <Profile profile={profile} look={look} setLook={setLook} />;
}

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
        <div className="card" style={{ width: "min(560px, 100%)" }} data-testid="profile-missing">
          <div className="card-head">
            <h1>{missing ? "No such profile" : "Could not read the profile"}</h1>
          </div>
          <div className="card-body">
            <p style={{ margin: 0 }}>{summary.error.message}.</p>
            {missing ? (
              <p className="muted" style={{ margin: 0 }}>
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
          <div className="profile-untimed profile-disconnected" role="status">
            Disconnected. The views keep what they last read until microscope answers again.
          </div>
        ) : null}
        {info ? <Recording info={info} /> : null}
        <ProfileHead />
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
function ProfileHead() {
  const { summary, look, setLook } = useProfile();
  const range = rangeOf(look);
  const snapshots = summary.snapshots > 0;
  // A trace of system calls alone has no frames to invert, fold, or compare.
  const sampled = summary.samples > 0 || summary.syscalls === 0;
  const taskName = (id: string) =>
    id === "none"
      ? "no task known"
      : (summary.tasks.find((task) => String(task.row) === id)?.name ?? `task ${id}`);
  const program = summary.program?.slice(summary.program.lastIndexOf("/") + 1);
  return (
    <div className="profile-head">
      <div className="profile-title">
        <span className="program" title={summary.program ?? undefined}>
          {program ?? summary.name}
        </span>
        <span className="muted">
          {summary.name} · {summary.timed ? `${formatDuration(summary.duration)} · ` : ""}
          {summary.samples > 0 || summary.syscalls === 0
            ? formatCount(summary.samples, "sample")
            : ""}
          {summary.samples > 0 && summary.syscalls > 0 ? " · " : ""}
          {summary.syscalls > 0 ? formatCount(summary.syscalls, "call") : ""}
        </span>
      </div>
      <div className="segmented">
        {VIEWS.filter(({ view }) => offers(summary, view)).map(({ view, label }) => (
          <button
            key={view}
            type="button"
            aria-pressed={(look.view ?? defaultView(summary)) === view}
            onClick={() =>
              setLook(
                (look) => withLook(look, "view", view === defaultView(summary) ? undefined : view),
                true,
              )
            }
          >
            {label}
          </button>
        ))}
      </div>
      {snapshots ? (
        <div className="segmented">
          {(
            [
              [undefined, "CPU", "Where the program ran"],
              ["waits", "Waits", "Where task snapshots found tasks waiting"],
            ] as const
          ).map(([src, label, title]) => (
            <button
              key={label}
              type="button"
              title={title}
              aria-pressed={look.src === src}
              onClick={() => setLook((look) => withLook(look, "src", src), true)}
            >
              {label}
            </button>
          ))}
        </div>
      ) : null}
      <div className="profile-query">
        {range ? (
          <Chip
            label={formatSpan(range)}
            title="Samples in this time range"
            remove={() => setLook((look) => withLook(look, "range", undefined))}
          />
        ) : null}
        {look.th ? (
          <Chip
            label={formatCount(look.th.length, "thread")}
            title="Samples of these threads"
            remove={() => setLook((look) => withLook(look, "th", undefined))}
          />
        ) : null}
        {look.tk ? (
          <Chip
            label={
              look.tk.length === 1
                ? taskName(look.tk[0] ?? "")
                : formatCount(look.tk.length, "task")
            }
            title={look.tk.map(taskName).join("\n")}
            remove={() => setLook((look) => withLook(look, "tk", undefined))}
          />
        ) : null}
        {(look.lb ?? []).map((label) => (
          <Chip
            key={label}
            label={label}
            title="Samples that carry this label"
            remove={() =>
              setLook((look) => {
                const kept = (look.lb ?? []).filter((each) => each !== label);
                return withLook(look, "lb", kept.length > 0 ? kept : undefined);
              })
            }
          />
        ))}
        {(look.tf ?? []).map((entry, index) => {
          const transform = parseTransform(entry);
          return transform ? (
            <Chip
              key={entry}
              label={
                transform.op === "collapseCycles"
                  ? "collapse recursion"
                  : `${transform.op} ${transform.name}`
              }
              title="Remove this transform and those after it"
              remove={() => setLook((look) => untransformed(look, index), true)}
            />
          ) : null;
        })}
        {sampled ? (
          <>
            {(look.tf ?? []).includes("collapseCycles") ? null : (
              <button
                type="button"
                className="tb ghost"
                title="Cut each path back to the first call of anything it calls again, directly or through others"
                onClick={() =>
                  setLook((look) => transformed(look, { op: "collapseCycles", name: "" }), true)
                }
              >
                Collapse recursion
              </button>
            )}
            <label className="check">
              <input
                type="checkbox"
                checked={look.invert === "1"}
                onChange={(event) =>
                  setLook((look) =>
                    withLook(look, "invert", event.target.checked ? "1" : undefined),
                  )
                }
              />
              Inverted
            </label>
            <label className="check" title="Show each async task's frames as a stack of its own">
              <input
                type="checkbox"
                checked={look.native !== "1"}
                onChange={(event) =>
                  setLook((look) =>
                    withLook(look, "native", event.target.checked ? undefined : "1"),
                  )
                }
              />
              Async stacks
            </label>
            {summary.interpreted ? (
              <label
                className="check"
                title="Show each stack as its Python code sees it, without the interpreter's own frames"
              >
                <input
                  type="checkbox"
                  checked={look.py === "1"}
                  onChange={(event) =>
                    setLook((look) => withLook(look, "py", event.target.checked ? "1" : undefined))
                  }
                />
                Python stacks
              </label>
            ) : null}
          </>
        ) : null}
        {snapshots ? (
          <>
            <label className="check" title="Root each path at the task it ran or waited in">
              <input
                type="checkbox"
                checked={look.bt === "1"}
                onChange={(event) =>
                  setLook((look) => withLook(look, "bt", event.target.checked ? "1" : undefined))
                }
              />
              By task
            </label>
            {look.src === "waits" ? null : (
              <label className="check" title="A timeline lane per task, not per thread">
                <input
                  type="checkbox"
                  checked={look.lanes === "tasks"}
                  onChange={(event) =>
                    setLook((look) =>
                      withLook(look, "lanes", event.target.checked ? "tasks" : undefined),
                    )
                  }
                />
                Task lanes
              </label>
            )}
          </>
        ) : null}
        {summary.labelKeys.length > 0 ? (
          <select
            className="input profile-by"
            aria-label="Root by label"
            value={look.bl ?? ""}
            onChange={(event) =>
              setLook((look) => withLook(look, "bl", event.target.value || undefined))
            }
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
              onChange={(event) => {
                const by = event.target.value as ProfileGranularity;
                setLook((look) => withLook(look, "by", by === "function" ? undefined : by));
              }}
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

/** What a diff compares with: another range of this profile, chosen from
 * the selection, or another profile served; a chip once chosen. */
function Compare() {
  const { profile, look, setLook } = useProfile();
  const profiles = useModel((model) => model.profiles);
  const others = profiles.filter((each) => each.id !== profile);
  const baseRange = look.br ? parseRange(look.br) : null;
  if (look.bp || baseRange) {
    const of =
      look.bp && look.bp !== profile
        ? (others.find((each) => each.id === look.bp)?.name ?? look.bp)
        : "this profile";
    const during = baseRange ? ` ${formatSpan(baseRange)}` : "";
    return (
      <Chip
        label={`diff with ${of}${during}`}
        title="Red grew and blue shrank against this base"
        remove={() =>
          setLook((look) => withLook(withLook(look, "bp", undefined), "br", undefined), true)
        }
      />
    );
  }
  if (!look.range && others.length === 0) {
    return null;
  }
  return (
    <select
      className="input profile-by"
      aria-label="Compare with"
      value=""
      onChange={(event) => {
        const chosen = event.target.value;
        setLook(
          (look) =>
            chosen === "range"
              ? withLook(withLook(look, "br", look.range), "range", undefined)
              : withLook(look, "bp", chosen || undefined),
          true,
        );
      }}
    >
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

/** A recording's progress, and saving the profile as a file. */
function Recording({ info }: { info: ProfileInfo }) {
  const connection = useConnection();
  const control = useModel((model) => model.hello?.role === "control");
  const [failure, setFailure] = useState<string | null>(null);
  const save = async () => {
    setFailure(null);
    try {
      const response = await fetch(`${base}api/profile/${info.id}`, {
        method: "POST",
        credentials: "same-origin",
      });
      if (!response.ok) {
        throw new Error(`the server answered ${response.status}`);
      }
      const link = document.createElement("a");
      link.href = URL.createObjectURL(await response.blob());
      link.download = info.name.endsWith(".usprof") ? info.name : `${info.name}.usprof`;
      link.click();
      URL.revokeObjectURL(link.href);
    } catch (error) {
      setFailure(error instanceof Error ? error.message : String(error));
    }
  };
  return (
    <div className="profile-recording" data-testid="recording">
      {info.recording ? (
        <span className="pill running">Recording {info.name}</span>
      ) : info.ended ? (
        <span className="muted">
          {info.name} ended: {info.ended}
        </span>
      ) : null}
      {info.recording && control ? (
        <button
          type="button"
          className="tb ghost"
          onClick={() =>
            void connection
              .request("stopRecording", { profile: info.id })
              .catch((error: Error) => setFailure(error.message))
          }
        >
          Stop
        </button>
      ) : null}
      <span className="spacer" />
      {failure ? <span className="error">{failure}</span> : null}
      <button type="button" className="tb ghost" onClick={() => void save()}>
        Save
      </button>
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
            .map((change) => `${formatDuration(change.time)}: ${change.text}`)
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
