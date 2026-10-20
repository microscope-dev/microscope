import type { ReactNode } from "react";
import { formatCount } from "../../profile/paint";
import type { RequestError, SyscallQuery } from "../../protocol";
import { useModel } from "../../store";
import { useAnswer } from "./context";
import { Bytes, Duration, Unanswered } from "./parts";
import { Tokens } from "./SyscallLog";

/** A plain table of rows, or what it is reading, why it could not, or
 * that there are none. */
function Rows({
  head,
  rows,
  error,
  reading,
  empty,
}: {
  head: string[];
  /** Absent until the first answer comes. */
  rows: ReactNode[][] | undefined;
  error: RequestError | undefined;
  reading: string;
  empty: string;
}) {
  if (error || !rows) {
    return <Unanswered error={error} reading={reading} className="pad" />;
  }
  if (rows.length === 0) {
    return <p className="muted pad">{empty}</p>;
  }
  return (
    <table className="syscalls-table">
      <thead>
        <tr>
          {head.map((name, index) => (
            <th key={name} className={index > 0 ? "num" : undefined}>
              {name}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((cells, row) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: rows are answered in order
          <tr key={row}>
            {cells.map((cell, index) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: columns are fixed
              <td key={index} className={index > 0 ? "num" : "mono"}>
                {cell}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** A count of failures, red when there are any. */
function Failed({ count }: { count: number }) {
  return count > 0 ? <span className="error-text">{formatCount(count)}</span> : "0";
}

export function Files({ query }: { query: SyscallQuery }) {
  const files = useAnswer("syscallFiles", query);
  return (
    <div className="pane" data-testid="syscall-files">
      <Rows
        head={["path", "opens", "failed", "read", "written", "fsync", "time"]}
        error={files.error}
        reading="Reading files…"
        empty="No file was opened or used."
        rows={files.data?.files.map((file) => [
          file.path,
          formatCount(file.opens),
          <Failed key="failed" count={file.failed} />,
          <Bytes key="read" bytes={file.read} />,
          <Bytes key="written" bytes={file.written} />,
          <Duration key="fsync" ns={file.syncNs} />,
          <Duration key="time" ns={file.totalNs} />,
        ])}
      />
    </div>
  );
}

export function Network({ query }: { query: SyscallQuery }) {
  const network = useAnswer("syscallNetwork", query);
  return (
    <div className="pane" data-testid="syscall-network">
      <Rows
        head={["endpoint", "connects", "failed", "connect time", "sent", "received"]}
        error={network.error}
        reading="Reading sockets…"
        empty="No socket was used."
        rows={network.data?.endpoints.map((endpoint) => [
          endpoint.endpoint,
          formatCount(endpoint.connects),
          <Failed key="failed" count={endpoint.failed} />,
          <Duration key="connect" ns={endpoint.connectNs} />,
          <Bytes key="sent" bytes={endpoint.sent} />,
          <Bytes key="received" bytes={endpoint.received} />,
        ])}
      />
    </div>
  );
}

export function Processes({ query }: { query: SyscallQuery }) {
  const processes = useAnswer("syscallProcesses", query);
  const live = useModel(
    (model) => model.profiles.find((each) => each.id === query.profile)?.recording ?? false,
  );
  const all = processes.data?.processes;
  const depth = new Map<number, number>();
  for (const process of all ?? []) {
    depth.set(process.pid, process.parent === null ? 0 : (depth.get(process.parent) ?? -1) + 1);
  }
  return (
    <div className="pane" data-testid="syscall-processes">
      <Rows
        head={["process", "threads", "calls", "errors", "lifetime", "end"]}
        error={processes.error}
        reading="Reading processes…"
        empty="No process was seen."
        rows={all?.map((process) => [
          <span key="name" style={{ paddingLeft: `${(depth.get(process.pid) ?? 0) * 16}px` }}>
            {process.pid}{" "}
            <span className="muted">
              {process.images.map((image) => image.slice(image.lastIndexOf("/") + 1)).join(" → ")}
            </span>
          </span>,
          formatCount(process.threads),
          formatCount(process.calls),
          <Failed key="errors" count={process.errors} />,
          process.ended !== null ? (
            <Duration key="lifetime" ns={process.ended - process.started} />
          ) : (
            "running"
          ),
          process.end ?? "",
        ])}
      />
      {live ? <InFlight query={query} /> : null}
    </div>
  );
}

/** What each thread of a live trace is blocked in, as of its last
 * publication. */
function InFlight({ query }: { query: SyscallQuery }) {
  const flight = useAnswer("syscallInFlight", {
    profile: query.profile,
    revision: query.revision ?? 0,
  });
  return (
    <section className="in-flight" data-testid="syscall-in-flight">
      <h3>Blocked now</h3>
      <Rows
        head={["thread", "call", "for"]}
        error={flight.error}
        reading="Reading blocked threads…"
        empty="No thread is blocked in a call."
        rows={flight.data?.calls.map((call) => [
          call.tid,
          <span key="call" className="mono">
            <Tokens tokens={call.tokens} />
          </span>,
          <Duration key="for" ns={call.blockedNs} />,
        ])}
      />
    </section>
  );
}
