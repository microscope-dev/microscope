import { useState } from "react";
import { withLook } from "../../profile/look";
import { formatCount, formatDuration, formatInstant } from "../../profile/paint";
import type { SyscallQuery, SyscallSite, SyscallToken } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Unanswered } from "./parts";

/** Tokens colored as the CLI colors them. */
export function Tokens({ tokens }: { tokens: SyscallToken[] }) {
  return (
    <>
      {tokens.map((token, index) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: tokens never move
        <span key={index} className={`tk-${token.kind}`}>
          {token.text}
        </span>
      ))}
    </>
  );
}

const PAGE = 500;

/** The trace as strace prints it, a page at a time; a call's line opens
 * its arguments, bytes, and stack. */
export function Log({ query }: { query: SyscallQuery }) {
  const { look, setLook, summary } = useProfile();
  const [pages, setPages] = useState(1);
  const log = useAnswer("syscallLog", { ...query, offset: 0, limit: PAGE * pages });
  if (!log.data) {
    return (
      <div className="pane syscalls-log" data-testid="syscall-log">
        <Unanswered error={log.error} reading="Reading calls…" className="pad" />
      </div>
    );
  }
  const lines = log.data.lines;
  const left = log.data.total - lines.length;
  return (
    <div className="pane syscalls-log" data-testid="syscall-log">
      <div className="log-lines mono">
        {lines.map((line) => {
          const open = look.ev === String(line.event);
          return (
            <div key={line.event} className={`log-line ${line.failed ? "failed" : ""}`}>
              <button
                type="button"
                className="log-head"
                disabled={!line.call}
                aria-expanded={line.call ? open : undefined}
                onClick={() =>
                  setLook((look) => withLook(look, "ev", open ? undefined : String(line.event)))
                }
              >
                <span className="muted">{formatInstant(line.time, 1000, summary.duration)}</span>{" "}
                <span className="tk-pid">{line.tid}</span> <Tokens tokens={line.tokens} />
                {line.durationNs !== null ? (
                  <span className="tk-time"> &lt;{formatDuration(line.durationNs)}&gt;</span>
                ) : null}
              </button>
              {open ? <Call event={line.event} /> : null}
            </div>
          );
        })}
      </div>
      {left > 0 ? (
        <button type="button" className="button" onClick={() => setPages(pages + 1)}>
          Show more ({formatCount(left)} left)
        </button>
      ) : null}
      {log.data.total === 0 ? <p className="muted">No call matches.</p> : null}
    </div>
  );
}

function Call({ event }: { event: number }) {
  const { profile, query } = useProfile();
  const call = useAnswer("syscallCall", { profile, revision: query.revision ?? 0, event });
  const data = call.data;
  if (!data) {
    return <Unanswered error={call.error} reading="Reading the call…" className="log-call" />;
  }
  return (
    <div className="log-call" data-testid="syscall-call">
      <table>
        <tbody>
          {data.arguments.map((argument) => (
            <tr key={argument.name}>
              <th>{argument.name}</th>
              <td>
                <Tokens tokens={argument.tokens} />
              </td>
            </tr>
          ))}
          <tr>
            <th>result</th>
            <td>
              <Tokens tokens={data.result} />
            </td>
          </tr>
        </tbody>
      </table>
      {data.dumps.length > 0 ? <pre className="dump">{data.dumps.join("\n")}</pre> : null}
      {data.site ? <Site site={data.site} /> : null}
      {data.stack.length > 0 ? (
        <ol className="stack">
          {data.stack.map((name, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: a stack's frames never move
            <li key={index}>{name}</li>
          ))}
          {data.stackEnds ? <li className="muted">stack ends: {data.stackEnds}</li> : null}
        </ol>
      ) : null}
    </div>
  );
}

/** Where a call was made, with the lines around it. */
function Site({ site }: { site: SyscallSite }) {
  return (
    <figure className="call-site" data-testid="syscall-site">
      <figcaption className="mono">
        {site.function} · {site.path}:{site.line}
      </figcaption>
      {site.text ? (
        <pre className="source-lines">
          {site.text.map((text, index) => {
            const number = site.first + index;
            return (
              <div key={number} className={number === site.line ? "made" : undefined}>
                <span className="line-number">{number}</span>
                {text}
              </div>
            );
          })}
        </pre>
      ) : (
        <p className="muted">{site.unread}</p>
      )}
    </figure>
  );
}
