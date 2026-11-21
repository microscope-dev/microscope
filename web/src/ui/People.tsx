import { useNavigate } from "@solidjs/router";
import { createMemo, createSignal, For, Show, untrack } from "solid-js";

import { assert } from "../assert";
import { withinBase } from "../base";
import { isPagePath } from "../focus";
import { toHref } from "../history";
import type { Person } from "../protocol";
import { write } from "../storage";
import { useConnection, useModel } from "../store";

export function hue(name: string): number {
  let hash = 0;
  for (const char of name) {
    const code = char.codePointAt(0);
    assert(code !== undefined, "a character with no code point");
    hash = (hash * 31 + code) >>> 0;
  }
  return hash % 360;
}

const NAME_FIELD = { width: "140px", padding: "2px 6px" };

export function People() {
  const people = useModel((model) => model.people);
  const me = useModel((model) => model.hello?.connection);
  const connection = useConnection();
  const [editing, setEditing] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const mine = createMemo(() => people().find((person) => person.connection === me()));
  const others = createMemo(() => people().length - 1);
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      setEditing(false);
    }
    if (event.key === "Enter") {
      const name = (event.currentTarget as HTMLInputElement).value.trim();
      setEditing(false);
      if (name) {
        setError(null);
        connection
          .request("setName", { name })
          .then(() => write("microscope-name", name))
          .catch((failure: Error) => setError(failure.message));
      }
    }
  };
  const stopEditing = () => setEditing(false);
  const startEditing = () => setEditing(true);

  return (
    <Show when={people().length > 0}>
      <div class="people">
        <ul class="avatars" data-testid="people" aria-label="People in this session">
          <For each={people()} keyed={(person) => person.connection}>
            {(person) => <Avatar person={person()} me={person().connection === me()} />}
          </For>
        </ul>
        <Show
          when={editing() ? mine() : null}
          fallback={
            <button
              type="button"
              class="tb"
              title="Change the name others see"
              onClick={startEditing}
            >
              {mine()?.name ?? "you"}
              <Show when={others() > 0}>
                <span class="muted">+{others()}</span>
              </Show>
            </button>
          }
        >
          {(current) => (
            <input
              class="input"
              style={NAME_FIELD}
              aria-label="Your name"
              value={untrack(() => current().name)}
              autofocus
              ref={(node) => {
                queueMicrotask(() => node.focus());
              }}
              onKeyDown={onKeyDown}
              onBlur={stopEditing}
            />
          )}
        </Show>
        <Show when={error()}>
          {(message) => (
            <span class="pill warn" role="alert" title={message()}>
              {message().length > 60 ? `${message().slice(0, 60)}…` : message()}
            </span>
          )}
        </Show>
      </div>
    </Show>
  );
}

function Avatar(props: { person: Person; me: boolean }) {
  const style = createMemo(() => ({ background: `hsl(${hue(props.person.name)} 55% 30%)` }));
  const who = createMemo(
    () => `${props.person.name}${props.me ? " (you)" : ""} · can ${props.person.role}`,
  );
  const url = createMemo(() => {
    const candidate = props.person.focus?.url;
    return !props.me && isPagePath(candidate) ? candidate : null;
  });

  return (
    <Show
      when={url()}
      fallback={
        <li class={`avatar ${props.person.role}`} style={style()} title={who()}>
          {props.person.name.slice(0, 2)}
        </li>
      }
    >
      {(target) => (
        <li class={`avatar ${props.person.role}`} style={style()}>
          <AvatarLink
            url={target()}
            name={props.person.name}
            title={`${who()}\nat ${props.person.focus?.label}; click to go there`}
            label={`Go to ${props.person.name}'s focus, ${props.person.focus?.label}`}
          />
        </li>
      )}
    </Show>
  );
}

function AvatarLink(props: { url: string; name: string; title: string; label: string }) {
  const navigate = useNavigate();
  return (
    <button
      type="button"
      title={props.title}
      aria-label={props.label}
      onClick={() => navigate(toHref(withinBase(props.url)))}
    >
      {props.name.slice(0, 2)}
    </button>
  );
}
