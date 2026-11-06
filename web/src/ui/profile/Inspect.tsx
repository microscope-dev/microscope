import {
  type KeyboardEvent,
  type MouseEvent,
  type PointerEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import {
  commandLine,
  medianDepth,
  recorderBreakdown,
  recordingSettings,
  shellQuote,
} from "../../profile/inspect";
import {
  formatBytes,
  formatCount,
  formatDuration,
  formatPercent,
  formatWeight,
  largest,
} from "../../profile/paint";
import type { ProfileCommand, ProfileInspection, ProfileSummary } from "../../protocol";
import { useDialogFocus } from "../dialog";
import { useAnswer, useProfile } from "./context";

type Tab = "metadata" | "samples";

const NAMES: Record<Tab, string> = { metadata: "Metadata", samples: "Samples" };
const BOTH: Tab[] = ["metadata", "samples"];
const METADATA_ONLY: Tab[] = ["metadata"];

/** What the profile holds and how it was recorded, over the page until
 * Escape, Close, or a click outside closes it. */
export function Inspect({ revision, close }: { revision: number; close(): void }) {
  const { profile, summary } = useProfile();
  const answer = useAnswer("profileInspect", { profile, revision });
  // A trace of system calls alone has no samples to describe.
  const tabs = summary.samples > 0 ? BOTH : METADATA_ONLY;
  const [tab, setTab] = useState<Tab>("metadata");
  const dialog = useRef<HTMLDivElement>(null);
  const chosen = useRef<HTMLButtonElement>(null);
  useDialogFocus(dialog, chosen);

  useEffect(() => {
    const key = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        close();
      }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [close]);

  const outside = useCallback(
    (event: PointerEvent) => {
      if (event.target === event.currentTarget) {
        close();
      }
    },
    [close],
  );
  const choose = useCallback((event: MouseEvent<HTMLButtonElement>) => {
    setTab(event.currentTarget.dataset.tab as Tab);
  }, []);
  const arrows = useCallback(
    (event: KeyboardEvent) => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
        return;
      }
      event.preventDefault();
      const step = event.key === "ArrowRight" ? 1 : -1;
      setTab((now) => tabs[(tabs.indexOf(now) + step + tabs.length) % tabs.length] ?? now);
      // The tab chosen takes focus once it renders as chosen.
      requestAnimationFrame(() => chosen.current?.focus());
    },
    [tabs],
  );

  const inspection = answer.data;
  return (
    <div className="overlay inspect-overlay" onPointerDown={outside}>
      <div
        className="inspect"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an overlay the page draws, not a <dialog>
        role="dialog"
        aria-modal="true"
        aria-labelledby="inspect-title"
        tabIndex={-1}
        ref={dialog}
      >
        <div className="inspect-head">
          <h1 id="inspect-title">{summary.name}</h1>
          {tabs.length > 1 ? (
            <div className="segmented" role="tablist" aria-label="Inspect">
              {tabs.map((each) => (
                <button
                  key={each}
                  type="button"
                  role="tab"
                  id={`inspect-${each}`}
                  data-tab={each}
                  aria-selected={tab === each}
                  aria-controls="inspect-panel"
                  tabIndex={tab === each ? 0 : -1}
                  ref={tab === each ? chosen : undefined}
                  onClick={choose}
                  onKeyDown={arrows}
                >
                  {NAMES[each]}
                </button>
              ))}
            </div>
          ) : null}
          <button type="button" className="tb ghost" onClick={close}>
            Close
          </button>
        </div>
        <div
          className="inspect-body"
          id="inspect-panel"
          role={tabs.length > 1 ? "tabpanel" : undefined}
          aria-labelledby={tabs.length > 1 ? `inspect-${tab}` : undefined}
        >
          {answer.error ? (
            <span className="error">Could not inspect it: {answer.error.message}</span>
          ) : inspection === undefined ? (
            // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled span, not a form output
            <span className="muted" role="status">
              Reading the profile…
            </span>
          ) : tab === "metadata" ? (
            <Metadata summary={summary} inspection={inspection} />
          ) : (
            <Samples summary={summary} inspection={inspection} />
          )}
        </div>
      </div>
    </div>
  );
}

function Metadata({
  summary,
  inspection,
}: {
  summary: ProfileSummary;
  inspection: ProfileInspection;
}) {
  const recorder = summary.settings.find((each) => each.name === "recorder")?.value;
  const file = inspection.file;
  const tableBytes = inspection.tables.reduce((sum, table) => sum + table.bytes, 0);
  const mostTable = largest(
    inspection.tables.map((table) => table.bytes),
    1,
  );
  const cpu = recorderBreakdown(
    summary.recorderCpu,
    summary.recorderLoading,
    inspection.recorderSetup,
    inspection.recorderFinishing,
  );
  return (
    <>
      {inspection.command ? <Command command={inspection.command} /> : null}
      {inspection.processes.length > 1 ? (
        <Section title={`Processes, ${inspection.processes.length}`}>
          <table className="inspect-table">
            <thead>
              <tr>
                <th className="n">PID</th>
                <th>Name</th>
                <th>Command</th>
              </tr>
            </thead>
            <tbody>
              {inspection.processes.map((process, row) => (
                // oxlint-disable-next-line react/no-array-index-key -- a pid may be reused
                <tr key={row}>
                  <td className="n">{process.pid}</td>
                  <td>{process.name}</td>
                  <td className="mono wrap">
                    {process.command ? commandLine(process.command) : ""}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </Section>
      ) : null}
      <div className="inspect-columns">
        <Section title="Recording">
          <Facts>
            {summary.timed ? (
              <Fact name="Recorded" value={formatDuration(summary.duration)} />
            ) : null}
            <Fact name="Threads" value={formatCount(summary.threads.length)} />
            {summary.syscalls > 0 ? (
              <Fact name="System calls" value={formatCount(summary.syscalls)} />
            ) : null}
            {recordingSettings(summary.settings).map(({ name, value }) => (
              <Fact key={name} name={name.charAt(0).toUpperCase() + name.slice(1)} value={value} />
            ))}
          </Facts>
        </Section>
        <div className="inspect-stack">
          <Section title="File">
            <Facts>
              {file === null ? <Fact name="Source" value="a live recording" /> : null}
              {file !== null && file.bytes !== null ? (
                <Fact name="Size" value={formatBytes(file.bytes)} />
              ) : null}
              {file === null ? null : <Fact name="Chunks" value={formatCount(file.chunks)} />}
              {file?.truncated ? (
                <Fact name="Truncated" value="yes, only its complete chunks were read" warn />
              ) : null}
              {recorder ? <Fact name="Recorder" value={recorder} /> : null}
            </Facts>
          </Section>
          {summary.recorderCpu > 0 ? (
            <Section title="microscope's CPU">
              <table className="inspect-table">
                <tbody>
                  {cpu.map(([what, nanoseconds]) => (
                    <tr key={what}>
                      <th scope="row">{what}</th>
                      <td className="n">{formatDuration(nanoseconds)}</td>
                      <Bar value={nanoseconds} of={summary.recorderCpu} kind="context" />
                    </tr>
                  ))}
                  <tr>
                    <th scope="row">Total</th>
                    <td className="n">{formatDuration(summary.recorderCpu)}</td>
                  </tr>
                </tbody>
              </table>
            </Section>
          ) : null}
        </div>
      </div>
      {inspection.modules.length > 0 ? (
        <Section title={`Modules, ${inspection.modules.length}`}>
          <table className="inspect-table">
            <thead>
              <tr>
                <th>Path</th>
                <th className="n">Size</th>
                <th>Build ID</th>
                <th className="n">Frames</th>
              </tr>
            </thead>
            <tbody>
              {inspection.modules.map((module, row) => (
                // oxlint-disable-next-line react/no-array-index-key -- two builds may share a path
                <tr key={row}>
                  <td className="mono wrap">{module.path}</td>
                  <td className="n">{formatBytes(module.size)}</td>
                  <td className={module.buildId ? "mono wrap" : "muted"}>
                    {module.buildId ?? "none"}
                  </td>
                  <td className="n">{formatCount(module.frames)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Section>
      ) : null}
      {inspection.tables.length > 0 ? (
        <Section title="Bytes by table">
          <table className="inspect-table">
            <tbody>
              {inspection.tables.map((table) => (
                <tr key={table.name}>
                  <th scope="row">{table.name}</th>
                  <td className="n">{formatBytes(table.bytes)}</td>
                  <td className="n">{formatPercent(table.bytes, tableBytes)}</td>
                  <Bar value={table.bytes} of={mostTable} kind="context" />
                </tr>
              ))}
            </tbody>
          </table>
        </Section>
      ) : null}
    </>
  );
}

/** The whole command, one argument to a line, as a shell reads it. */
function Command({ command }: { command: ProfileCommand }) {
  const [copied, setCopied] = useState<"copied" | "failed" | null>(null);
  const block = useRef<HTMLDivElement>(null);
  const copy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(commandLine(command));
      setCopied("copied");
    } catch {
      // Selected, it can still be copied by hand.
      if (block.current) {
        window.getSelection()?.selectAllChildren(block.current);
      }
      setCopied("failed");
    }
  }, [command]);

  const lines = command.arguments?.map(shellQuote) ?? [command.text];
  const title = command.arguments
    ? `Command, ${formatCount(command.arguments.length - 1, "argument")}`
    : "Command";
  return (
    <Section title={title}>
      <div className="inspect-command">
        <div className="inspect-lines" ref={block} data-testid="command">
          {lines.map((line, index) => (
            // oxlint-disable-next-line react/no-array-index-key -- a command's arguments may repeat
            <div key={index} className={index > 0 ? "argument" : undefined}>
              {line}
              {index < lines.length - 1 ? <span className="muted"> \</span> : null}
            </div>
          ))}
        </div>
        <button type="button" className="tb ghost" onClick={copy}>
          {copied === "copied" ? "Copied" : copied === "failed" ? "Selected, copy it" : "Copy"}
        </button>
      </div>
      {command.arguments ? null : (
        <span className="muted">
          This recording kept the command as one line, so where each argument ends is unknown.
        </span>
      )}
    </Section>
  );
}

function Samples({
  summary,
  inspection,
}: {
  summary: ProfileSummary;
  inspection: ProfileInspection;
}) {
  const cut = summary.truncations.reduce((sum, each) => sum + each.count, 0);
  const nanoseconds = summary.unit === "nanoseconds";
  const busiest = largest(
    summary.threads.map((thread) => thread.weight + thread.blocked + thread.preempted),
    1,
  );
  const bin = summary.duration / Math.max(inspection.overTime.length, 1);
  const busiestBin = largest(inspection.overTime, 1);
  return (
    <>
      <div className="inspect-columns">
        <Section title="Totals">
          <Facts>
            <Fact name="Samples" value={formatCount(summary.samples)} />
            {inspection.counted > 0 ? (
              <Fact
                name={nanoseconds ? "CPU sampled" : "Sampled"}
                value={`${formatWeight(summary.weight, summary.unit)} of ${formatWeight(inspection.counted, summary.unit)}, ${formatPercent(summary.weight, inspection.counted)}`}
              />
            ) : null}
            <Fact name="Lost" value={formatCount(summary.lost)} warn={summary.lost > 0} />
            <Fact
              name="Throttled"
              value={formatCount(summary.throttles)}
              warn={summary.throttles > 0}
            />
            <Fact
              name="Stacks cut short"
              value={`${formatCount(cut)}, ${formatPercent(cut, summary.samples)}`}
              warn={cut > 0}
            />
            {inspection.shortened > 0 ? (
              <Fact name="Copies shortened" value={formatCount(inspection.shortened)} warn />
            ) : null}
            <Fact name="Distinct stacks" value={formatCount(inspection.distinctStacks)} />
            <Fact
              name="Frames"
              value={`${formatCount(inspection.frames)}, ${formatCount(inspection.framesWithLines)} with lines`}
            />
            {inspection.stackBytes > 0 ? (
              <Fact name="Stack copied" value={formatBytes(inspection.stackBytes)} />
            ) : null}
          </Facts>
        </Section>
        {inspection.overTime.length > 0 ? (
          <Section title={`Samples over time, ${formatDuration(bin)} bins`}>
            <div className="inspect-histogram">
              {inspection.overTime.map((count, index) => (
                <Column
                  // oxlint-disable-next-line react/no-array-index-key -- a bin's place is its identity
                  key={index}
                  count={count}
                  most={busiestBin}
                  label={`${formatDuration(index * bin)} on`}
                />
              ))}
            </div>
            <div className="inspect-axis">
              <span>0</span>
              <span>{formatDuration(summary.duration)}</span>
            </div>
          </Section>
        ) : null}
        <Section title="How stacks ended">
          <table className="inspect-table">
            <tbody>
              <Ending
                what="Reached the outermost frame"
                count={Math.max(summary.samples - cut, 0)}
                samples={summary.samples}
                kind="main"
              />
              {summary.truncations.map(({ what, count }) => (
                <Ending
                  key={what}
                  what={what.charAt(0).toUpperCase() + what.slice(1)}
                  count={count}
                  samples={summary.samples}
                  kind="warn"
                />
              ))}
            </tbody>
          </table>
        </Section>
        <Depths depths={inspection.depths} samples={summary.samples} />
      </div>
      {summary.threads.length > 0 ? (
        <Section title={`Threads, ${summary.threads.length}`}>
          <table className="inspect-table">
            <thead>
              <tr>
                <th className="n">TID</th>
                <th>Name</th>
                <th className="n">{nanoseconds ? "On CPU" : "Samples"}</th>
                <th className="n">Off CPU</th>
                <th aria-label="On and off CPU" />
              </tr>
            </thead>
            <tbody>
              {summary.threads.map((thread) => (
                <tr key={thread.row}>
                  <td className="n">{thread.tid}</td>
                  <td>{thread.name ?? ""}</td>
                  <td className="n">
                    {nanoseconds ? formatDuration(thread.weight) : formatCount(thread.samples)}
                  </td>
                  <td className="n">{formatDuration(thread.blocked + thread.preempted)}</td>
                  <Bar
                    value={nanoseconds ? thread.weight : 0}
                    rest={thread.blocked + thread.preempted}
                    of={busiest}
                    kind="main"
                  />
                </tr>
              ))}
            </tbody>
          </table>
        </Section>
      ) : null}
    </>
  );
}

function Ending({
  what,
  count,
  samples,
  kind,
}: {
  what: string;
  count: number;
  samples: number;
  kind: BarKind;
}) {
  return (
    <tr>
      <th scope="row">{what}</th>
      <td className="n">{formatCount(count)}</td>
      <td className="n">{formatPercent(count, samples)}</td>
      <Bar value={count} of={samples} kind={kind} />
    </tr>
  );
}

/** Samples by stack depth, a bar to a depth, the median's marked, each
 * read out when pointed at or focused. */
function Depths({ depths, samples }: { depths: number[]; samples: number }) {
  const median = medianDepth(depths);
  const [shown, setShown] = useState<number | null>(null);
  const show = useCallback(
    (event: { currentTarget: HTMLElement }) => setShown(Number(event.currentTarget.dataset.depth)),
    [],
  );
  const hide = useCallback(() => setShown(null), []);
  if (median === null) {
    return null;
  }
  // A stack is at least one frame deep, unless some samples have none.
  const first = (depths[0] ?? 0) > 0 ? 0 : 1;
  const most = largest(depths, 1);
  const count = shown === null ? 0 : (depths[shown] ?? 0);
  return (
    <Section title="Samples by stack depth, in frames">
      <div className="inspect-histogram" onPointerLeave={hide}>
        {depths.slice(first).map((each, index) => (
          <Column
            // oxlint-disable-next-line react/no-array-index-key -- a depth is its bar's identity
            key={index + first}
            count={each}
            most={most}
            label={formatCount(index + first, "frame")}
            depth={index + first}
            marked={index + first === median}
            onShow={show}
            onHide={hide}
          />
        ))}
      </div>
      <div className="inspect-axis">
        <span>{first}</span>
        <span>median {median}</span>
        <span>{depths.length - 1}</span>
      </div>
      {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a readout, not a form's result */}
      <span className="muted" role="status" data-testid="depth">
        {shown === null
          ? "Point at a bar for its count."
          : `${formatCount(shown, "frame")}: ${formatCount(count, "sample")}, ${formatPercent(count, samples)}`}
      </span>
    </Section>
  );
}

/** A histogram's bar: a button when it reads out a depth, else a span
 * that says its count on hover. */
function Column({
  count,
  most,
  label,
  depth,
  marked,
  onShow,
  onHide,
}: {
  count: number;
  most: number;
  label: string;
  depth?: number;
  marked?: boolean;
  onShow?(event: { currentTarget: HTMLElement }): void;
  onHide?(): void;
}) {
  const style = useMemo(() => ({ height: share(count, most) }), [count, most]);
  const said = `${label}: ${formatCount(count, "sample")}`;
  return onShow ? (
    <button
      type="button"
      className={marked ? "marked" : undefined}
      aria-label={said}
      data-depth={depth}
      onPointerEnter={onShow}
      onFocus={onShow}
      onBlur={onHide}
    >
      <i style={style} />
    </button>
  ) : (
    <span title={said}>
      <i style={style} />
    </span>
  );
}

/** A label, a value, and whether the value is a warning. */
function Fact({ name, value, warn = false }: { name: string; value: string; warn?: boolean }) {
  return (
    <tr>
      <th scope="row">{name}</th>
      <td className={warn ? "warn wrap" : "wrap"}>{value}</td>
    </tr>
  );
}

function Facts({ children }: { children: ReactNode }) {
  return (
    <table className="inspect-table">
      <tbody>{children}</tbody>
    </table>
  );
}

type BarKind = "main" | "context" | "warn";

/** A cell holding `value`'s share of `of`, and after it `rest`'s. */
function Bar({
  value,
  rest = 0,
  of,
  kind,
}: {
  value: number;
  rest?: number;
  of: number;
  kind: BarKind;
}) {
  const first = useMemo(() => ({ width: share(value, of) }), [value, of]);
  const second = useMemo(() => ({ width: share(rest, of) }), [rest, of]);
  return (
    <td className="inspect-bar" aria-hidden="true">
      <div>
        <i className={kind} style={first} />
        {rest > 0 ? <i className="context" style={second} /> : null}
      </div>
    </td>
  );
}

function share(part: number, of: number): string {
  return `${(Math.max(part, 0) / Math.max(of, 1)) * 100}%`;
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="inspect-section">
      <h2>{title}</h2>
      {children}
    </section>
  );
}
