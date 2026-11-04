import { useNavigate } from "@tanstack/react-router";
import { type KeyboardEvent, useCallback, useMemo, useState } from "react";

import { assert } from "../assert";
import { isPagePath } from "../focus";
import type { Person } from "../protocol";
import { write } from "../storage";
import { useConnection, useModel } from "../store";

/** A steady color for a name, so each person keeps theirs. */
export function hue(name: string): number {
  let hash = 0;
  for (const char of name) {
    const code = char.codePointAt(0);
    assert(code !== undefined, "a character with no code point");
    hash = (hash * 31 + code) >>> 0;
  }
  return hash % 360;
}

const NAME_FIELD = { width: 140, padding: "2px 6px" };

export function People() {
  const people = useModel((model) => model.people);
  const me = useModel((model) => model.hello?.connection);
  const connection = useConnection();
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mine = people.find((person) => person.connection === me);
  const onKeyDown = useCallback(
    (event: KeyboardEvent<HTMLInputElement>) => {
      if (event.key === "Escape") {
        setEditing(false);
      }
      if (event.key === "Enter") {
        const name = event.currentTarget.value.trim();
        setEditing(false);
        if (name) {
          setError(null);
          connection
            .request("setName", { name })
            .then(() => write("microscope-name", name))
            .catch((failure: Error) => setError(failure.message));
        }
      }
    },
    [connection],
  );
  const stopEditing = useCallback(() => setEditing(false), []);
  const startEditing = useCallback(() => setEditing(true), []);
  if (people.length === 0) {
    return null;
  }
  const others = people.length - 1;

  return (
    <div className="people">
      <ul className="avatars" data-testid="people" aria-label="People in this session">
        {people.map((person) => (
          <Avatar key={person.connection} person={person} me={person.connection === me} />
        ))}
      </ul>
      {editing && mine ? (
        <input
          className="input"
          style={NAME_FIELD}
          aria-label="Your name"
          defaultValue={mine.name}
          // oxlint-disable-next-line jsx-a11y/no-autofocus -- the field opens because the person asked to type
          autoFocus
          onKeyDown={onKeyDown}
          onBlur={stopEditing}
        />
      ) : (
        <button
          type="button"
          className="tb"
          title="Change the name others see"
          onClick={startEditing}
        >
          {mine?.name ?? "you"}
          {others > 0 && <span className="muted">+{others}</span>}
        </button>
      )}
      {error && (
        <span className="pill warn" role="alert" title={error}>
          {error.length > 60 ? `${error.slice(0, 60)}…` : error}
        </span>
      )}
    </div>
  );
}

/** One person: hovering says where they look, and clicking goes there. */
function Avatar({ person, me }: { person: Person; me: boolean }) {
  const style = useMemo(() => ({ background: `hsl(${hue(person.name)} 55% 30%)` }), [person.name]);
  const who = `${person.name}${me ? " (you)" : ""} · can ${person.role}`;
  const url = person.focus?.url;
  if (me || !isPagePath(url)) {
    return (
      <li className={`avatar ${person.role}`} style={style} title={who}>
        {person.name.slice(0, 2)}
      </li>
    );
  }
  return (
    <li className={`avatar ${person.role}`} style={style}>
      <AvatarLink
        url={url}
        name={person.name}
        title={`${who}\nat ${person.focus?.label}; click to go there`}
        label={`Go to ${person.name}'s focus, ${person.focus?.label}`}
      />
    </li>
  );
}

function AvatarLink({
  url,
  name,
  title,
  label,
}: {
  url: string;
  name: string;
  title: string;
  label: string;
}) {
  const navigate = useNavigate();
  const onClick = useCallback(() => void navigate({ href: url }), [navigate, url]);
  return (
    <button type="button" title={title} aria-label={label} onClick={onClick}>
      {name.slice(0, 2)}
    </button>
  );
}
