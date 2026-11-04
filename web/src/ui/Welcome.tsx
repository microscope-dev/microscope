// Where a page with nothing open starts: Welcome offers what can be done
// from here, and Explorer lists what this page is connected to and the
// profiles its microscope serves. Each action is a command, so it is offered
// and refused here as it is in the menus.

import { Link } from "@tanstack/react-router";
import { type ReactNode, useCallback, useEffect, useMemo } from "react";

import { checkAccount, useAccount } from "../account";
import { targetName } from "../model";
import { useModel } from "../store";
import { remote, standalone } from "../target";
import type { Availability } from "../workbench/commands";
import { COMMANDS, type CommandId, useCommands } from "./commands";
import { showProfile } from "./panes";
import { fileName } from "./paths";

const ACTIONS: readonly CommandId[] = [
  "launch",
  "attach",
  "core",
  "record",
  "connect",
  "account",
  "help",
];

export function WelcomeBody() {
  const commands = useCommands();
  const now = useAccount();
  // Read whenever anything the actions depend on changes.
  useModel((model) => [model.hello?.role, model.link, model.state?.session].join());
  useEffect(() => {
    void checkAccount();
  }, []);

  return (
    <div className="pane-body pane-text welcome">
      <h1>{now.kind === "in" ? `Welcome, ${now.user.username}` : "Welcome to microscope"}</h1>
      <p className="muted">
        Every view is a pane: drag a tab to arrange them, and the Window menu does the same by
        keyboard.
      </p>
      <ul className="welcome-actions">
        {ACTIONS.map((id) => {
          const availability = commands.availability(id);
          if (availability.state === "absent") {
            return null;
          }
          return (
            <WelcomeAction key={id} id={id} availability={availability} onRun={commands.run} />
          );
        })}
      </ul>
    </div>
  );
}

function WelcomeAction({
  id,
  availability,
  onRun,
}: {
  id: CommandId;
  availability: Availability;
  onRun: (id: CommandId) => unknown;
}) {
  const onClick = useCallback(() => onRun(id), [onRun, id]);
  const key = COMMANDS.describe(id) ?? COMMANDS.describe(id, true);
  return (
    <li>
      <button
        type="button"
        className="button"
        disabled={availability.state !== "ready"}
        title={availability.state === "blocked" ? availability.why : undefined}
        onClick={onClick}
      >
        {COMMANDS.get(id).label}
      </button>
      {key && <kbd>{key}</kbd>}
      {availability.state === "blocked" && <span className="muted">{availability.why}</span>}
    </li>
  );
}

export function ExplorerBody() {
  const link = useModel((model) => model.link);
  const role = useModel((model) => model.hello?.role);
  const state = useModel((model) => model.state);
  const profiles = useModel((model) => model.profiles);
  const server = standalone ? remote()?.server : window.location.origin;
  const debugging = targetName(state);

  return (
    <div className="pane-body pane-text explorer">
      <h2>Connection</h2>
      {server ? (
        <p>
          <code>{server}</code>{" "}
          <span className="muted">
            {link === "open"
              ? `· connected${role ? `, can ${role}` : ""}`
              : link === "unauthorized"
                ? "· refused this page's link"
                : `· ${link}`}
          </span>
        </p>
      ) : (
        <p className="muted">Not connected to a microscope.</p>
      )}

      <h2>Session</h2>
      {state?.session && debugging ? (
        <p>
          <SessionLink session={state.session}>{debugging}</SessionLink>
        </p>
      ) : (
        <p className="muted">Nothing is being debugged.</p>
      )}

      <h2>Profiles</h2>
      {profiles.length === 0 ? (
        <p className="muted">This microscope serves no profiles.</p>
      ) : (
        <ul className="rows">
          {profiles.map((profile) => (
            <li key={profile.id}>
              <ProfileButton id={profile.id} name={profile.name} />{" "}
              {profile.recording && <span className="muted">recording</span>}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function SessionLink({ session, children }: { session: string; children: ReactNode }) {
  const params = useMemo(() => ({ session }), [session]);
  return (
    <Link to="/s/$session" params={params}>
      {children}
    </Link>
  );
}

function ProfileButton({ id, name }: { id: string; name: string }) {
  const onClick = useCallback(() => showProfile(id, name), [id, name]);
  return (
    <button type="button" className="link-button" onClick={onClick}>
      {fileName(name)}
    </button>
  );
}
