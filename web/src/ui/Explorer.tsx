// Explorer lists what this page is connected to and the profiles its
// microscope serves.

import { Link } from "@tanstack/react-router";
import { type ReactNode, useCallback, useMemo } from "react";

import { targetName } from "../model";
import { useModel } from "../store";
import { remote, standalone } from "../target";
import { showProfile } from "./panes";
import { fileName } from "./paths";

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
