import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { isPagePath } from "../focus";
import type { Person } from "../protocol";
import { write } from "../storage";
import { useConnection, useModel } from "../store";

/** A steady color for a name, so each person keeps theirs. */
export function hue(name: string): number {
  let hash = 0;
  for (const char of name) {
    hash = (hash * 31 + (char.codePointAt(0) ?? 0)) >>> 0;
  }
  return hash % 360;
}

export function People() {
  const people = useModel((model) => model.people);
  const me = useModel((model) => model.hello?.connection);
  const connection = useConnection();
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mine = people.find((person) => person.connection === me);
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
          style={{ width: 140, padding: "2px 6px" }}
          aria-label="Your name"
          defaultValue={mine.name}
          // biome-ignore lint/a11y/noAutofocus: the field opens because the person asked to type
          autoFocus
          onKeyDown={(event) => {
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
          }}
          onBlur={() => setEditing(false)}
        />
      ) : (
        <button
          type="button"
          className="tb"
          title="Change the name others see"
          onClick={() => setEditing(true)}
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
  const navigate = useNavigate();
  const style = { background: `hsl(${hue(person.name)} 55% 30%)` };
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
      <button
        type="button"
        title={`${who}\nat ${person.focus?.label}; click to go there`}
        aria-label={`Go to ${person.name}'s focus, ${person.focus?.label}`}
        onClick={() => void navigate({ href: url })}
      >
        {person.name.slice(0, 2)}
      </button>
    </li>
  );
}
