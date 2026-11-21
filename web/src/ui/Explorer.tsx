import type { JSX } from "@solidjs/web";
import { createMemo, For, Show } from "solid-js";

import { toHref } from "../history";
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
  const debugging = createMemo(() => targetName(state()));
  const session = createMemo(() => {
    const id = state()?.session;
    const name = debugging();
    return id && name ? { id, name } : null;
  });

  return (
    <div class="pane-body pane-text explorer">
      <h2>Connection</h2>
      <Show when={server} fallback={<p class="muted">Not connected to a microscope.</p>}>
        {(origin) => (
          <p>
            <code>{origin()}</code>{" "}
            <span class="muted">
              {link() === "open"
                ? `· connected${role() ? `, can ${role()}` : ""}`
                : link() === "unauthorized"
                  ? "· refused this page's link"
                  : link() === "unreachable"
                    ? "· can't be reached, still trying"
                    : `· ${link()}`}
            </span>
          </p>
        )}
      </Show>

      <h2>Session</h2>
      <Show when={session()} fallback={<p class="muted">Nothing is being debugged.</p>}>
        {(current) => (
          <p>
            <SessionLink session={current().id}>{current().name}</SessionLink>
          </p>
        )}
      </Show>

      <h2>Profiles</h2>
      <Show
        when={profiles().length > 0}
        fallback={<p class="muted">This microscope serves no profiles.</p>}
      >
        <ul class="rows">
          <For each={profiles()} keyed={(profile) => profile.id}>
            {(profile) => (
              <li>
                <ProfileButton id={profile().id} name={profile().name} />{" "}
                <Show when={profile().recording}>
                  <span class="muted">recording</span>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}

export function SessionLink(props: { session: string; children: JSX.Element }) {
  return <a href={toHref(`/s/${props.session}`)}>{props.children}</a>;
}

function ProfileButton(props: { id: string; name: string }) {
  return (
    <button type="button" class="link-button" onClick={() => showProfile(props.id, props.name)}>
      {fileName(props.name)}
    </button>
  );
}
