import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, createUniqueId, For, Match, Show, Switch } from "solid-js";

import { captureDownloadUrl, parseCaptureProfileId } from "../../captures";
import { copyText } from "../../clipboard";
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
  formatInstant,
  formatPercent,
  formatWeight,
  largest,
} from "../../profile/paint";
import { timeTicks } from "../../profile/zones";
import type {
  ProfileCommand,
  ProfileInfo,
  ProfileInspection,
  ProfileSummary,
} from "../../protocol";
import { useModel } from "../../store";
import { presented, serverUrl } from "../../target";
import { useAnswer, useProfile } from "./context";

type Tab = "metadata" | "samples";

const CHART_WIDTH = 600;

const NAMES: Record<Tab, string> = { metadata: "Metadata", samples: "Samples" };
const BOTH: Tab[] = ["metadata", "samples"];
const METADATA_ONLY: Tab[] = ["metadata"];

export function FileDetails() {
  const focus = useProfile();
  const info = useModel((model) => model.profiles.find((each) => each.id === focus().profile));
  const captureId = createMemo(() => parseCaptureProfileId(focus().profile));
  const answer = useAnswer("profileInspect", () => ({
    profile: focus().profile,
    revision: info()?.revision ?? 0,
  }));

  const tabs = createMemo(() => (focus().summary.samples > 0 ? BOTH : METADATA_ONLY));
  const [tab, setTab] = createSignal<Tab>("metadata");
  const tabButtons = new Map<Tab, HTMLButtonElement>();
  const choose = (event: MouseEvent & { currentTarget: HTMLButtonElement }) => {
    setTab(event.currentTarget.dataset.tab as Tab);
  };
  const arrows = (event: KeyboardEvent) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
      return;
    }
    event.preventDefault();
    const list = tabs();
    const step = event.key === "ArrowRight" ? 1 : -1;
    const next = list[(list.indexOf(tab()) + step + list.length) % list.length] ?? tab();
    setTab(next);
    requestAnimationFrame(() => tabButtons.get(next)?.focus());
  };

  const inspection = createMemo(() => answer().data);
  return (
    <section class="details" aria-labelledby="details-title">
      <div class="details-head">
        <h1 id="details-title">{focus().summary.name}</h1>
        <Show when={tabs().length > 1}>
          <div class="segmented" role="tablist" aria-label="File details">
            <For each={tabs()}>
              {(each) => (
                <button
                  type="button"
                  role="tab"
                  id={`details-${each}`}
                  data-tab={each}
                  aria-selected={tab() === each ? "true" : "false"}
                  aria-controls="details-panel"
                  tabindex={tab() === each ? 0 : -1}
                  ref={(node) => {
                    tabButtons.set(each, node);
                  }}
                  onClick={choose}
                  onKeyDown={arrows}
                >
                  {NAMES[each]}
                </button>
              )}
            </For>
          </div>
        </Show>
        <Show
          when={info()}
          fallback={
            <Show when={captureId()}>
              {(id) => (
                <a
                  class="tb ghost"
                  href={captureDownloadUrl(id())}
                  download
                  title="Download this capture as .usprof"
                >
                  Download
                </a>
              )}
            </Show>
          }
        >
          {(current) => <Download info={current()} />}
        </Show>
      </div>
      <div
        class="details-body"
        id="details-panel"
        role={tabs().length > 1 ? "tabpanel" : undefined}
        aria-labelledby={tabs().length > 1 ? `details-${tab()}` : undefined}
      >
        <Switch>
          <Match when={answer().error}>
            {(error) => <span class="error">Could not read its details: {error().message}</span>}
          </Match>
          <Match when={inspection() === undefined}>
            {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a styled span, not a form output */}
            <span class="muted" role="status">
              Reading the profile…
            </span>
          </Match>
          <Match when={tab() === "metadata" ? inspection() : undefined}>
            {(data) => <Metadata summary={focus().summary} inspection={data()} />}
          </Match>
          <Match when={inspection()}>
            {(data) => <Samples summary={focus().summary} inspection={data()} />}
          </Match>
        </Switch>
      </div>
    </section>
  );
}

function Metadata(props: { summary: ProfileSummary; inspection: ProfileInspection }) {
  const recorder = createMemo(
    () => props.summary.settings.find((each) => each.name === "recorder")?.value,
  );
  const file = createMemo(() => props.inspection.file);
  const fileBytes = createMemo(() => {
    const current = file();
    return current && current.bytes !== null ? { bytes: current.bytes } : null;
  });
  const tableBytes = createMemo(() =>
    props.inspection.tables.reduce((sum, table) => sum + table.bytes, 0),
  );
  const mostTable = createMemo(() =>
    largest(
      props.inspection.tables.map((table) => table.bytes),
      1,
    ),
  );
  const cpu = createMemo(() =>
    recorderBreakdown(
      props.summary.recorderCpu,
      props.summary.recorderLoading,
      props.inspection.recorderSetup,
      props.inspection.recorderFinishing,
    ),
  );
  return (
    <>
      <Show when={props.summary.command}>{(command) => <Command command={command()} />}</Show>
      <Show when={props.inspection.processes.length > 1}>
        <Section title={`Processes, ${props.inspection.processes.length}`}>
          <table class="details-table">
            <thead>
              <tr>
                <th class="n">PID</th>
                <th>Name</th>
                <th>Command</th>
              </tr>
            </thead>
            <tbody>
              <For each={props.inspection.processes} keyed={false}>
                {(process) => (
                  <tr>
                    <td class="n">{process().pid}</td>
                    <td>{process().name}</td>
                    <td class="mono wrap">
                      <Show when={process().command}>{(cmd) => <>{commandLine(cmd())}</>}</Show>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Section>
      </Show>
      <div class="details-columns">
        <Section title="Recording">
          <Facts>
            <Fact name="Recorded" value={formatDuration(props.summary.duration)} />
            <Fact name="Threads" value={formatCount(props.summary.threads.length)} />
            <Show when={props.summary.syscalls > 0}>
              <Fact name="System calls" value={formatCount(props.summary.syscalls)} />
            </Show>
            <For each={recordingSettings(props.summary.settings)} keyed={(each) => each.name}>
              {(each) => (
                <Fact
                  name={each().name.charAt(0).toUpperCase() + each().name.slice(1)}
                  value={each().value}
                />
              )}
            </For>
          </Facts>
        </Section>
        <div class="details-stack">
          <Section title="File">
            <Facts>
              <Show when={file() === null}>
                <Fact name="Source" value="a live recording" />
              </Show>
              <Show when={fileBytes()}>
                {(size) => <Fact name="Size" value={formatBytes(size().bytes)} />}
              </Show>
              <Show when={file()}>
                {(current) => <Fact name="Chunks" value={formatCount(current().chunks)} />}
              </Show>
              <Show when={file()?.truncated}>
                <Fact name="Truncated" value="yes, only complete chunks were read" warn />
              </Show>
              <Show when={recorder()}>{(name) => <Fact name="Recorder" value={name()} />}</Show>
            </Facts>
          </Section>
          <Show when={props.summary.recorderCpu > 0}>
            <Section title="microscope's CPU">
              <table class="details-table">
                <tbody>
                  <For each={cpu()} keyed={([what]) => what}>
                    {(entry) => (
                      <tr>
                        <th scope="row">{entry()[0]}</th>
                        <td class="n">{formatDuration(entry()[1])}</td>
                        <Bar value={entry()[1]} of={props.summary.recorderCpu} kind="context" />
                      </tr>
                    )}
                  </For>
                  <tr>
                    <th scope="row">Total</th>
                    <td class="n">{formatDuration(props.summary.recorderCpu)}</td>
                  </tr>
                </tbody>
              </table>
            </Section>
          </Show>
        </div>
      </div>
      <Show when={props.inspection.modules.length > 0}>
        <Section title={`Modules, ${props.inspection.modules.length}`}>
          <table class="details-table">
            <thead>
              <tr>
                <th>Path</th>
                <th class="n">Size</th>
                <th>Build ID</th>
                <th class="n">Frames</th>
              </tr>
            </thead>
            <tbody>
              <For each={props.inspection.modules} keyed={false}>
                {(module) => (
                  <tr>
                    <td class="mono wrap">{module().path}</td>
                    <td class="n">{formatBytes(module().size)}</td>
                    <td class={module().buildId ? "mono wrap" : "muted"}>
                      {module().buildId ?? "none"}
                    </td>
                    <td class="n">{formatCount(module().frames)}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Section>
      </Show>
      <Show when={props.inspection.tables.length > 0}>
        <Section title="Bytes by table">
          <table class="details-table">
            <tbody>
              <For each={props.inspection.tables} keyed={(table) => table.name}>
                {(table) => (
                  <tr>
                    <th scope="row">{table().name}</th>
                    <td class="n">{formatBytes(table().bytes)}</td>
                    <td class="n">{formatPercent(table().bytes, tableBytes())}</td>
                    <Bar value={table().bytes} of={mostTable()} kind="context" />
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Section>
      </Show>
    </>
  );
}

function Command(props: { command: ProfileCommand }) {
  return (
    <Section title="Command">
      <CommandBlock command={props.command} />
    </Section>
  );
}

export function CommandBlock(props: { command: ProfileCommand }) {
  const [copied, setCopied] = createSignal<"copied" | "failed" | null>(null);
  let block: HTMLDivElement | undefined;
  const copy = async () => {
    if (await copyText(commandLine(props.command))) {
      setCopied("copied");
      return;
    }

    if (block) {
      window.getSelection()?.selectAllChildren(block);
    }
    setCopied("failed");
  };

  const lines = createMemo(() => props.command.arguments?.map(shellQuote) ?? [props.command.text]);
  return (
    <>
      <div class="details-command">
        <div
          class="details-lines"
          ref={(node) => {
            block = node;
          }}
          data-testid="command"
        >
          <For each={lines()} keyed={false}>
            {(line, index) => (
              <div class={index > 0 ? "argument" : undefined}>
                {line()}
                <Show when={index < lines().length - 1}>
                  <span class="muted"> \</span>
                </Show>
              </div>
            )}
          </For>
        </div>
        <button type="button" class="tb ghost" onClick={() => void copy()}>
          {copied() === "copied" ? "Copied" : copied() === "failed" ? "Selected, copy it" : "Copy"}
        </button>
      </div>
      <Show when={!props.command.arguments}>
        <span class="muted">
          This recording kept the command as one line, so where each argument ends is unknown.
        </span>
      </Show>
    </>
  );
}

function Samples(props: { summary: ProfileSummary; inspection: ProfileInspection }) {
  const cut = createMemo(() =>
    props.summary.truncations.reduce((sum, each) => sum + each.count, 0),
  );
  const nanoseconds = createMemo(() => props.summary.unit === "nanoseconds");
  const busiest = createMemo(() =>
    largest(
      props.summary.threads.map((thread) => thread.weight + thread.blocked + thread.preempted),
      1,
    ),
  );
  const bin = createMemo(
    () => props.summary.duration / Math.max(props.inspection.overTime.length, 1),
  );
  const binLabels = createMemo(() => {
    const step = bin();
    return props.inspection.overTime.map(
      (_, index) =>
        `${formatInstant(index * step, step / 10, props.summary.duration)} – ${formatInstant((index + 1) * step, step / 10, props.summary.duration)}`,
    );
  });
  return (
    <>
      <div class="details-columns">
        <Section title="Totals">
          <Facts>
            <Fact name="Samples" value={formatCount(props.summary.samples)} />
            <Show when={props.inspection.counted > 0}>
              <Fact
                name={nanoseconds() ? "CPU sampled" : "Sampled"}
                value={`${formatWeight(props.summary.weight, props.summary.unit)} of ${formatWeight(props.inspection.counted, props.summary.unit)}, ${formatPercent(props.summary.weight, props.inspection.counted)}`}
              />
            </Show>
            <Fact
              name="Lost"
              value={formatCount(props.summary.lost)}
              warn={props.summary.lost > 0}
            />
            <Fact
              name="Throttled"
              value={formatCount(props.summary.throttles)}
              warn={props.summary.throttles > 0}
            />
            <Fact
              name="Stacks cut short"
              value={`${formatCount(cut())}, ${formatPercent(cut(), props.summary.samples)}`}
              warn={cut() > 0}
            />
            <Show when={props.inspection.shortened > 0}>
              <Fact name="Copies shortened" value={formatCount(props.inspection.shortened)} warn />
            </Show>
            <Fact name="Distinct stacks" value={formatCount(props.inspection.distinctStacks)} />
            <Fact
              name="Frames"
              value={`${formatCount(props.inspection.frames)}, ${formatCount(props.inspection.framesWithLines)} with lines`}
            />
            <Show when={props.inspection.stackBytes > 0}>
              <Fact name="Stack copied" value={formatBytes(props.inspection.stackBytes)} />
            </Show>
          </Facts>
        </Section>
        <Section title="How stacks ended">
          <table class="details-table">
            <tbody>
              <Ending
                what="Reached the outermost frame"
                count={Math.max(props.summary.samples - cut(), 0)}
                samples={props.summary.samples}
                kind="main"
              />
              <For each={props.summary.truncations} keyed={(each) => each.what}>
                {(each) => (
                  <Ending
                    what={each().what.charAt(0).toUpperCase() + each().what.slice(1)}
                    count={each().count}
                    samples={props.summary.samples}
                    kind="warn"
                  />
                )}
              </For>
            </tbody>
          </table>
        </Section>
      </div>
      <Show when={props.inspection.overTime.length > 0}>
        <Section title={`Samples over time, ${formatDuration(bin())} bins`}>
          <Histogram
            counts={props.inspection.overTime}
            labels={binLabels()}
            samples={props.summary.samples}
            filled
          >
            <div class="details-ticks">
              <For
                each={timeTicks([0, props.summary.duration], CHART_WIDTH)}
                keyed={(t) => t.label}
              >
                {(tick) => <Tick at={tick().at} label={tick().label} />}
              </For>
            </div>
          </Histogram>
        </Section>
      </Show>
      <Depths depths={props.inspection.depths} samples={props.summary.samples} />
      <Show when={props.summary.threads.length > 0}>
        <Section title={`Threads, ${props.summary.threads.length}`}>
          <table class="details-table">
            <thead>
              <tr>
                <th class="n">TID</th>
                <th>Name</th>
                <th class="n">{nanoseconds() ? "On CPU" : "Samples"}</th>
                <th class="n">Off CPU</th>
                <th aria-label="On and off CPU" />
              </tr>
            </thead>
            <tbody>
              <For each={props.summary.threads} keyed={(thread) => thread.row}>
                {(thread) => (
                  <tr>
                    <td class="n">{thread().tid}</td>
                    <td>{thread().name ?? ""}</td>
                    <td class="n">
                      {nanoseconds()
                        ? formatDuration(thread().weight)
                        : formatCount(thread().samples)}
                    </td>
                    <td class="n">{formatDuration(thread().blocked + thread().preempted)}</td>
                    <Bar
                      value={nanoseconds() ? thread().weight : 0}
                      rest={thread().blocked + thread().preempted}
                      of={busiest()}
                      kind="main"
                    />
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Section>
      </Show>
    </>
  );
}

function Ending(props: { what: string; count: number; samples: number; kind: BarKind }) {
  return (
    <tr>
      <th scope="row">{props.what}</th>
      <td class="n">{formatCount(props.count)}</td>
      <td class="n">{formatPercent(props.count, props.samples)}</td>
      <Bar value={props.count} of={props.samples} kind={props.kind} />
    </tr>
  );
}

function Depths(props: { depths: number[]; samples: number }) {
  const median = createMemo(() => medianDepth(props.depths));
  const first = createMemo(() => ((props.depths[0] ?? 0) > 0 ? 0 : 1));
  const counts = createMemo(() => props.depths.slice(first()));
  const labels = createMemo(() => {
    const start = first();
    return counts().map((_, index) => formatCount(index + start, "frame"));
  });
  const marked = createMemo(() => {
    const current = median();
    return current === null ? null : { median: current, at: current - first() };
  });
  return (
    <Show when={marked()}>
      {(current) => (
        <Section title="Samples by stack depth, in frames">
          <Histogram
            counts={counts()}
            labels={labels()}
            marked={current().at}
            samples={props.samples}
          >
            <div class="details-axis">
              <span>{first()}</span>
              <span>median {current().median}</span>
              <span>{props.depths.length - 1}</span>
            </div>
          </Histogram>
        </Section>
      )}
    </Show>
  );
}

function Histogram(props: {
  counts: number[];
  labels: string[];
  samples: number;
  marked?: number | undefined;
  filled?: boolean | undefined;
  children: JSX.Element;
}) {
  const [shown, setShown] = createSignal<number | null>(null);
  const show = (event: { currentTarget: HTMLElement }) =>
    setShown(Number(event.currentTarget.dataset.bar));
  const hide = () => setShown(null);
  const most = createMemo(() => largest(props.counts, 1));
  const count = createMemo(() => {
    const at = shown();
    return at === null ? 0 : (props.counts[at] ?? 0);
  });
  const readout = createMemo(() => {
    const at = shown();
    if (at === null) {
      return null;
    }
    return `${props.labels[at] ?? ""}: ${formatCount(count(), "sample")}, ${formatPercent(count(), props.samples)}`;
  });
  return (
    <>
      <div
        class={props.filled ? "details-histogram filled" : "details-histogram"}
        onPointerOut={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
            hide();
          }
        }}
      >
        <For each={props.counts} keyed={false}>
          {(each, index) => (
            <Column
              index={index}
              count={each()}
              most={most()}
              label={props.labels[index] ?? ""}
              marked={index === props.marked}
              onShow={show}
              onHide={hide}
            />
          )}
        </For>
      </div>
      {props.children}
      {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a readout, not a form's result */}
      <span class="muted details-readout" role="status">
        <Show when={readout()}>{(text) => <>{text()}</>}</Show>
      </span>
    </>
  );
}

function Column(props: {
  index: number;
  count: number;
  most: number;
  label: string;
  marked: boolean;
  onShow(event: { currentTarget: HTMLElement }): void;
  onHide(): void;
}) {
  return (
    <button
      type="button"
      class={props.marked ? "marked" : undefined}
      aria-label={`${props.label}: ${formatCount(props.count, "sample")}`}
      data-bar={props.index}
      onPointerEnter={(event) => props.onShow(event)}
      onFocus={(event) => props.onShow(event)}
      onBlur={() => props.onHide()}
    >
      <i style={{ height: share(props.count, props.most) }} />
    </button>
  );
}

function Tick(props: { at: number; label: string }) {
  return <span style={{ left: `${props.at * 100}%` }}>{props.label}</span>;
}

function Fact(props: { name: string; value: string; warn?: boolean | undefined }) {
  return (
    <tr>
      <th scope="row">{props.name}</th>
      <td class={props.warn ? "warn wrap" : "wrap"}>{props.value}</td>
    </tr>
  );
}

function Facts(props: { children: JSX.Element }) {
  return (
    <table class="details-table">
      <tbody>{props.children}</tbody>
    </table>
  );
}

type BarKind = "main" | "context" | "warn";

function Bar(props: { value: number; rest?: number | undefined; of: number; kind: BarKind }) {
  return (
    <td class="details-bar" aria-hidden="true">
      <div>
        <i class={props.kind} style={{ width: share(props.value, props.of) }} />
        <Show when={(props.rest ?? 0) > 0}>
          <i class="context" style={{ width: share(props.rest ?? 0, props.of) }} />
        </Show>
      </div>
    </td>
  );
}

function share(part: number, of: number): string {
  return `${(Math.max(part, 0) / Math.max(of, 1)) * 100}%`;
}

function Section(props: { title: string; children: JSX.Element }) {
  const heading = createUniqueId();
  return (
    <section class="details-section" aria-labelledby={heading}>
      <h2 id={heading}>{props.title}</h2>
      {props.children}
    </section>
  );
}

function fileName(info: ProfileInfo): string {
  return info.name.endsWith(".usprof") ? info.name : `${info.name}.usprof`;
}

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

function Download(props: { info: ProfileInfo }) {
  const [failure, setFailure] = createSignal<string | null>(null);
  const download = () => {
    setFailure(null);
    saveProfile(props.info).catch((error: unknown) => {
      setFailure(error instanceof Error ? error.message : String(error));
    });
  };
  return (
    <>
      <button
        type="button"
        class="tb ghost"
        title={`Download ${fileName(props.info)} to open later with microscope web`}
        onClick={download}
      >
        Download
      </button>
      <Show when={failure()}>
        {(message) => <span class="error">Could not download it: {message()}</span>}
      </Show>
    </>
  );
}
