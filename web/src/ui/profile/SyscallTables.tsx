import type { JSX } from "@solidjs/web";
import { createMemo, For, Match, Show, Switch } from "solid-js";

import { formatCount } from "../../profile/paint";
import type { RequestError, SyscallQuery } from "../../protocol";
import { useModel } from "../../store";
import { useAnswer } from "./context";
import { Bytes, Duration, Unanswered } from "./parts";
import { Tokens } from "./SyscallLog";
import { SamplesDuring } from "./SyscallSummary";

function Rows(props: {
  head: string[];
  rows: JSX.Element[][] | undefined;
  error: RequestError | undefined;
  reading: string;
  empty: string;
}) {
  return (
    <Switch>
      <Match when={props.error || !props.rows}>
        <Unanswered error={props.error} reading={props.reading} className="pad" />
      </Match>
      <Match when={props.rows?.length === 0}>
        <p class="muted pad">{props.empty}</p>
      </Match>
      <Match when={props.rows}>
        {(rows) => (
          <table class="syscalls-table">
            <thead>
              <tr>
                <For each={props.head}>
                  {(name, index) => <th class={index() > 0 ? "num" : undefined}>{name}</th>}
                </For>
              </tr>
            </thead>
            <tbody>
              <For each={rows()} keyed={false}>
                {(cells) => (
                  <tr>
                    <For each={cells()} keyed={false}>
                      {(cell, index) => <td class={index > 0 ? "num" : "mono"}>{cell()}</td>}
                    </For>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        )}
      </Match>
    </Switch>
  );
}

function Failed(props: { count: number }) {
  return (
    <Show when={props.count > 0} fallback="0">
      <span class="error-text">{formatCount(props.count)}</span>
    </Show>
  );
}

const FILES_HEAD = ["path", "opens", "failed", "read", "written", "fsync", "time", ""];
const NETWORK_HEAD = ["endpoint", "connects", "failed", "connect time", "sent", "received", ""];
const PROCESSES_HEAD = ["process", "threads", "calls", "errors", "lifetime", "end"];
const IN_FLIGHT_HEAD = ["thread", "call", "for"];

export function Files(props: { query: SyscallQuery }) {
  const files = useAnswer("syscallFiles", () => props.query);
  const rows = createMemo(() =>
    files().data?.files.map((file) => [
      file.path,
      formatCount(file.opens),
      <Failed count={file.failed} />,
      <Bytes bytes={file.read} />,
      <Bytes bytes={file.written} />,
      <Duration ns={file.syncNs} />,
      <Duration ns={file.totalNs} />,
      <SamplesDuring during={`path:${file.path}`} name={file.path} />,
    ]),
  );
  return (
    <div class="pane" data-testid="syscall-files">
      <Rows
        head={FILES_HEAD}
        error={files().error}
        reading="Reading files…"
        empty="No file was opened or used."
        rows={rows()}
      />
    </div>
  );
}

export function Network(props: { query: SyscallQuery }) {
  const network = useAnswer("syscallNetwork", () => props.query);
  const rows = createMemo(() =>
    network().data?.endpoints.map((endpoint) => [
      endpoint.endpoint,
      formatCount(endpoint.connects),
      <Failed count={endpoint.failed} />,
      <Duration ns={endpoint.connectNs} />,
      <Bytes bytes={endpoint.sent} />,
      <Bytes bytes={endpoint.received} />,
      <SamplesDuring during={`path:${endpoint.endpoint}`} name={endpoint.endpoint} />,
    ]),
  );
  return (
    <div class="pane" data-testid="syscall-network">
      <Rows
        head={NETWORK_HEAD}
        error={network().error}
        reading="Reading sockets…"
        empty="No socket was used."
        rows={rows()}
      />
    </div>
  );
}

export function Processes(props: { query: SyscallQuery }) {
  const processes = useAnswer("syscallProcesses", () => props.query);
  const live = useModel(
    (model) => model.profiles.find((each) => each.id === props.query.profile)?.recording ?? false,
  );
  const rows = createMemo(() => {
    const all = processes().data?.processes;
    if (!all) {
      return;
    }
    const depth = new Map<number, number>();
    for (const process of all) {
      depth.set(process.pid, process.parent === null ? 0 : (depth.get(process.parent) ?? -1) + 1);
    }
    return all.map((process) => [
      <Indented depth={depth.get(process.pid) ?? 0}>
        {process.pid}{" "}
        <span class="muted">
          {process.images.map((image) => image.slice(image.lastIndexOf("/") + 1)).join(" → ")}
        </span>
      </Indented>,
      formatCount(process.threads),
      formatCount(process.calls),
      <Failed count={process.errors} />,
      process.ended === null ? "running" : <Duration ns={process.ended - process.started} />,
      process.end ?? "",
    ]);
  });
  return (
    <div class="pane" data-testid="syscall-processes">
      <Rows
        head={PROCESSES_HEAD}
        error={processes().error}
        reading="Reading processes…"
        empty="No process was seen."
        rows={rows()}
      />
      <Show when={live()}>
        <InFlight query={props.query} />
      </Show>
    </div>
  );
}

function Indented(props: { depth: number; children: JSX.Element }) {
  return <span style={{ "padding-left": `${props.depth * 16}px` }}>{props.children}</span>;
}

function InFlight(props: { query: SyscallQuery }) {
  const flight = useAnswer("syscallInFlight", () => ({
    profile: props.query.profile,
    revision: props.query.revision ?? 0,
  }));
  const rows = createMemo(() =>
    flight().data?.calls.map((call) => [
      call.tid,
      <span class="mono">
        <Tokens tokens={call.tokens} />
      </span>,
      <Duration ns={call.blockedNs} />,
    ]),
  );
  return (
    <section class="in-flight" data-testid="syscall-in-flight">
      <h3>Blocked now</h3>
      <Rows
        head={IN_FLIGHT_HEAD}
        error={flight().error}
        reading="Reading blocked threads…"
        empty="No thread is blocked in a call."
        rows={rows()}
      />
    </section>
  );
}
