import { createMemo, createSignal, For, Show } from "solid-js";

import { type OwnLook, type ProfileView, splitLook, withLook } from "../../profile/look";
import { formatCount, formatDuration, formatInstant } from "../../profile/paint";
import type { SyscallLine, SyscallQuery, SyscallSite, SyscallToken } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Unanswered } from "./parts";

export function Tokens(props: { tokens: SyscallToken[] }) {
  return (
    <For each={props.tokens} keyed={false}>
      {(token) => <span class={`tk-${token().kind}`}>{token().text}</span>}
    </For>
  );
}

const PAGE = 500;

export function Log(props: { query: SyscallQuery }) {
  const focus = useProfile();
  const [pages, setPages] = createSignal(1);
  const log = useAnswer("syscallLog", () => ({
    ...props.query,
    offset: 0,
    limit: PAGE * pages(),
  }));
  const showMore = () => setPages((n) => n + 1);
  return (
    <div class="pane syscalls-log" data-testid="syscall-log">
      <Show
        when={log().data}
        fallback={<Unanswered error={log().error} reading="Reading calls…" className="pad" />}
      >
        {(data) => {
          const left = createMemo(() => data().total - data().lines.length);
          return (
            <>
              <div class="log-lines mono">
                <For each={data().lines} keyed={(line) => line.event}>
                  {(line) => (
                    <LogLine line={line()} open={focus().look.ev === String(line().event)} />
                  )}
                </For>
              </div>
              <Show when={left() > 0}>
                <button type="button" class="button" onClick={showMore}>
                  Show more ({formatCount(left())} left)
                </button>
              </Show>
              <Show when={data().total === 0}>
                <p class="muted">No call matches.</p>
              </Show>
            </>
          );
        }}
      </Show>
    </div>
  );
}

function LogLine(props: { line: SyscallLine; open: boolean }) {
  const focus = useProfile();
  const toggle = () =>
    focus().setLook((look) =>
      withLook(look, "ev", props.open ? undefined : String(props.line.event)),
    );
  return (
    <div class={`log-line ${props.line.failed ? "failed" : ""}`}>
      <button
        type="button"
        class="log-head"
        disabled={!props.line.call}
        aria-expanded={props.line.call ? (props.open ? "true" : "false") : undefined}
        onClick={toggle}
      >
        <span class="muted">{formatInstant(props.line.time, 1000, focus().summary.duration)}</span>{" "}
        <span class="tk-pid">{props.line.tid}</span> <Tokens tokens={props.line.tokens} />
        <Show when={props.line.durationNs === null ? null : { ns: props.line.durationNs }}>
          {(duration) => <span class="tk-time"> &lt;{formatDuration(duration().ns)}&gt;</span>}
        </Show>
      </button>
      <Show when={props.open}>
        <Call event={props.line.event} />
      </Show>
    </div>
  );
}

function Call(props: { event: number }) {
  const focus = useProfile();
  const call = useAnswer("syscallCall", () => ({
    profile: focus().profile,
    revision: focus().query.revision ?? 0,
    event: props.event,
  }));
  return (
    <Show
      when={call().data}
      fallback={
        <Unanswered error={call().error} reading="Reading the call…" className="log-call" />
      }
    >
      {(data) => (
        <div class="log-call" data-testid="syscall-call">
          <table>
            <tbody>
              <For each={data().arguments} keyed={(argument) => argument.name}>
                {(argument) => (
                  <tr>
                    <th>{argument().name}</th>
                    <td>
                      <Tokens tokens={argument().tokens} />
                    </td>
                  </tr>
                )}
              </For>
              <tr>
                <th>result</th>
                <td>
                  <Tokens tokens={data().result} />
                </td>
              </tr>
            </tbody>
          </table>
          <Show when={data().dumps.length > 0}>
            <pre class="dump">{data().dumps.join("\n")}</pre>
          </Show>
          <Show when={data().site}>{(site) => <Site site={site()} />}</Show>
          <Show when={data().stack.length > 0}>
            <ol class="stack">
              <For each={data().stack} keyed={false}>
                {(name) => (
                  <li>
                    <Frame name={name()} />
                  </li>
                )}
              </For>
              <Show when={data().stackEnds}>
                {(ends) => <li class="muted">stack ends: {ends()}</li>}
              </Show>
            </ol>
          </Show>
        </div>
      )}
    </Show>
  );
}

const FRAME_LINKS: {
  label: string;
  title: string;
  view: ProfileView;
  own?: OwnLook;
}[] = [
  { label: "flame", title: "in the flame graph", view: "flame" },
  { label: "tree", title: "in the call tree", view: "tree" },
  { label: "source", title: "on its source lines", view: "functions", own: { ann: "lines" } },
];

function Frame(props: { name: string }) {
  return (
    <span class="frame-links">
      <span class="mono">{props.name}</span>
      <For each={FRAME_LINKS}>
        {(link) => (
          <FrameLink
            name={props.name}
            label={link.label}
            title={link.title}
            view={link.view}
            own={link.own}
          />
        )}
      </For>
    </span>
  );
}

function FrameLink(props: {
  name: string;
  label: string;
  title: string;
  view: ProfileView;
  own?: OwnLook | undefined;
}) {
  const focus = useProfile();

  const show = () =>
    focus().setLook(
      (prev) =>
        props.own
          ? { ...splitLook(prev).shared, ...props.own, view: props.view, fn: props.name }
          : { ...prev, view: props.view, fn: props.name },
      true,
      props.own !== undefined,
    );
  return (
    <button
      type="button"
      class="link-button"
      title={`${props.name} ${props.title}`}
      aria-label={`${props.name} ${props.title}`}
      onClick={show}
    >
      {props.label}
    </button>
  );
}

function Site(props: { site: SyscallSite }) {
  return (
    <figure class="call-site" data-testid="syscall-site">
      <figcaption class="mono">
        {props.site.function} · {props.site.path}:{props.site.line}
      </figcaption>
      <Show when={props.site.text} fallback={<p class="muted">{props.site.unread}</p>}>
        {(lines) => (
          <pre class="source-lines">
            <For each={lines()} keyed={false}>
              {(text, index) => {
                const number = createMemo(() => props.site.first + index);
                return (
                  <div class={number() === props.site.line ? "made" : undefined}>
                    <span class="line-number">{number()}</span>
                    {text()}
                  </div>
                );
              }}
            </For>
          </pre>
        )}
      </Show>
    </figure>
  );
}
