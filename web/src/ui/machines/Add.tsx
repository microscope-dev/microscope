import { createEffect, createMemo, createSignal, For, onSettled, Show } from "solid-js";

import { copyText } from "../../clipboard";
import {
  type EnrollmentKey,
  ENVIRONMENT,
  fleetApi,
  HOME,
  type Install,
  INSTALLS,
  lookHash,
  type Overview,
  parseTags,
  PLACEHOLDER,
  setLook,
  snippet,
  tagText,
} from "../../fleet";
import { useAction } from "../Orgs";
import { OWNERS_ONLY } from "./parts";

const INSTALL_LABEL: Record<Install, string> = {
  kubernetes: "Kubernetes",
  docker: "Docker",
  systemd: "Linux (systemd)",
};

const REMEMBERED = "microscope-machines-install";

function remembered(): Install {
  try {
    const saved = localStorage.getItem(REMEMBERED);
    return INSTALLS.find((install) => install === saved) ?? "kubernetes";
  } catch {
    return "kubernetes";
  }
}

function remember(install: Install) {
  try {
    localStorage.setItem(REMEMBERED, install);
  } catch {}
}

interface Chosen {
  key: EnrollmentKey;
  enroll: string | null;
}

function Form(props: {
  org: number;
  owner: boolean;
  keys: EnrollmentKey[];
  install: boolean;
  chose: (chosen: Chosen) => void;
}) {
  const active = createMemo(() => props.keys.filter((key) => key.revoked_at === null));
  const [environment, setEnvironment] = createSignal("");
  const [which, setWhich] = createSignal<"new" | "existing">("new");
  const [existing, setExisting] = createSignal<number | null>(null);
  const selectedExisting = createMemo(() => existing() ?? active()[0]?.id ?? null);
  const [tags, setTags] = createSignal("");
  const [expires, setExpires] = createSignal("");
  const { working, error, run } = useAction();
  const [invalid, setInvalid] = createSignal<string | null>(null);
  let first: HTMLInputElement | undefined;
  let existingSelect: HTMLSelectElement | undefined;
  onSettled(() => {
    first?.focus();
  });

  createEffect(
    () => [which(), active(), selectedExisting()] as const,
    ([currentWhich, , currentSelected]) => {
      if (currentWhich === "existing" && existingSelect) {
        existingSelect.value = currentSelected === null ? "" : String(currentSelected);
      }
    },
  );

  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    if (!props.owner) {
      return;
    }
    if (which() === "existing") {
      const id = selectedExisting();
      const key = active().find((candidate) => candidate.id === id);
      if (key) {
        props.chose({ key, enroll: null });
      }
      return;
    }
    const name = environment().trim();
    const parsed = parseTags(tags());
    if (name === "") {
      setInvalid("Enter an environment, such as prod.");
      return;
    }
    if (typeof parsed === "string") {
      setInvalid(`Tags: ${parsed}.`);
      return;
    }
    setInvalid(null);
    const rawExpires = expires();
    const expiresAt =
      rawExpires === "" ? undefined : Math.floor(Date.parse(`${rawExpires}T23:59:59Z`) / 1000);
    run(async () => {
      const created = await fleetApi.createKey({
        org_id: props.org,
        name,
        tags: { ...parsed, [ENVIRONMENT]: name },
        ...(expiresAt === undefined ? {} : { expires_at: expiresAt }),
      });
      props.chose({ key: created, enroll: created.enroll_url });
    });
  };

  return (
    <form class="machines-form" aria-label="Enrollment key" onSubmit={submit}>
      <Show when={props.install}>
        <h3>Enrollment key</h3>
        <p>
          The agent on each machine uses an enrollment key to join. One key can enroll many
          machines.
        </p>
      </Show>
      <Show when={props.install && active().length > 0}>
        <fieldset class="field">
          <legend class="visually-hidden">Which key</legend>
          <label class="machines-check">
            <input
              type="radio"
              name="which"
              checked={which() === "new"}
              disabled={!props.owner}
              onChange={() => setWhich("new")}
            />
            <span>Create a new key</span>
          </label>
          <label class="machines-check">
            <input
              type="radio"
              name="which"
              checked={which() === "existing"}
              disabled={!props.owner}
              onChange={() => setWhich("existing")}
            />
            <span>Use an existing key</span>
          </label>
          <Show when={which() === "existing"}>
            <select
              ref={(node) => {
                existingSelect = node;
              }}
              class="input"
              aria-label="Existing key"
              value={selectedExisting() ?? ""}
              onChange={(event) => setExisting(Number(event.currentTarget.value))}
            >
              <For each={active()}>
                {(key) => (
                  <option value={key.id} selected={selectedExisting() === key.id}>
                    {key.name}
                  </option>
                )}
              </For>
            </select>
          </Show>
        </fieldset>
      </Show>
      <Show when={which() === "new"}>
        <div class="field">
          <label for="machines-environment">Environment</label>
          <input
            id="machines-environment"
            ref={(node) => {
              first = node;
            }}
            class="input"
            value={environment()}
            placeholder="prod"
            disabled={!props.owner}
            aria-describedby="machines-environment-note"
            onInput={(event) => setEnvironment(event.currentTarget.value)}
          />
          <span id="machines-environment-note" class="note">
            Such as prod or staging. The key is named after it.
          </span>
        </div>
        <div class="field">
          <label for="machines-tags">Tags (optional)</label>
          <input
            id="machines-tags"
            class="input"
            value={tags()}
            placeholder="cluster=checkout, team=payments"
            disabled={!props.owner}
            aria-describedby="machines-tags-note"
            onInput={(event) => setTags(event.currentTarget.value)}
          />
          <span id="machines-tags-note" class="note">
            Key=value pairs, separated by commas.
          </span>
        </div>
        <div class="field">
          <label for="machines-expires">Expires (optional)</label>
          <input
            id="machines-expires"
            class="input"
            type="date"
            value={expires()}
            disabled={!props.owner}
            aria-describedby="machines-expires-note"
            onInput={(event) => setExpires(event.currentTarget.value)}
          />
          <span id="machines-expires-note" class="note">
            After this day (UTC), new machines cannot enroll with the key. Machines already enrolled
            keep running.
          </span>
        </div>
        <p class="machines-note">
          Every machine that enrolls with this key gets these tags. A tag set in the agent config
          wins over the same tag here.
        </p>
      </Show>
      <Show when={invalid() ?? error()}>
        {(message) => (
          <p class="error" role="alert">
            {message()}
          </p>
        )}
      </Show>
      <div class="actions">
        <button
          type="submit"
          class="button primary"
          aria-disabled={!props.owner || working() ? "true" : "false"}
          title={props.owner ? undefined : OWNERS_ONLY}
        >
          {which() === "new" ? "Create key" : "Use this key"}
        </button>
        <Show when={!props.owner}>
          <span class="muted">{OWNERS_ONLY}</span>
        </Show>
      </div>
    </form>
  );
}

function Copy(props: { text: string }) {
  const [copied, setCopied] = createSignal<boolean | null>(null);
  const copy = () => {
    void copyText(props.text).then(setCopied);
  };
  return (
    <span class="actions">
      <button type="button" class="button" onClick={copy}>
        Copy
      </button>
      <Show when={copied() !== null}>
        <output class={copied() ? "muted" : "error"}>
          {copied() ? "Copied" : "Could not copy; select the text instead"}
        </output>
      </Show>
    </span>
  );
}

function Installing(props: { chosen: Chosen }) {
  const [install, setInstall] = createSignal<Install>(remembered());
  const pick = (next: Install) => {
    setInstall(next);
    remember(next);
  };
  const text = createMemo(() => snippet(install(), props.chosen.enroll ?? PLACEHOLDER));
  return (
    <section class="machines-step" aria-labelledby="machines-install">
      <h3 id="machines-install">Install the agent</h3>
      <div class="segmented" role="tablist" aria-label="How to install">
        <For each={INSTALLS}>
          {(option) => <InstallTab option={option} chosen={install() === option} pick={pick} />}
        </For>
      </div>
      <Show
        when={props.chosen.enroll}
        fallback={
          <p>
            Replace <code>{PLACEHOLDER}</code> with the enroll URL you saved when you created{" "}
            <span class="mono">{props.chosen.key.name}</span>.
          </p>
        }
      >
        <p class="machines-warning-text">
          This command contains the new key, and this is the only time you can see it. To add more
          machines later, save it in your secret manager, or create another key.
        </p>
      </Show>
      <Show when={install() !== "systemd"}>
        <p class="error">
          TODO:{" "}
          {install() === "docker" ? "the Docker image is" : "the Docker image and Helm chart are"}{" "}
          not published yet, so {install() === "docker" ? "this command does" : "these commands do"}{" "}
          not work.
        </p>
      </Show>
      <pre
        class="machines-snippet"
        role="tabpanel"
        tabindex={0}
        aria-label={INSTALL_LABEL[install()]}
      >
        {text()}
      </pre>
      <Copy text={text()} />
      <p class="note">
        <Show
          when={install() === "kubernetes"}
          fallback={
            <>
              Replace <code>your-cluster-name</code> with the name of the cluster these machines
              belong to, or remove that line.
            </>
          }
        >
          Replace <code>your-cluster-name</code> with the name of this cluster.
        </Show>{" "}
        Tags set here win over the key tags.
      </p>
    </section>
  );
}

function InstallTab(props: { option: Install; chosen: boolean; pick: (install: Install) => void }) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={props.chosen ? "true" : "false"}
      tabindex={props.chosen ? 0 : -1}
      onClick={() => props.pick(props.option)}
    >
      {INSTALL_LABEL[props.option]}
    </button>
  );
}

function EnrolledLink(props: { chosen: Chosen; org: number }) {
  const hash = createMemo(() =>
    lookHash({ ...HOME, view: "list", org: props.org, state: "any", key: props.chosen.key.id }),
  );
  return (
    <section class="machines-step">
      <a
        class="button"
        href={`${window.location.pathname}${window.location.search}#${hash()}`}
        target="_blank"
        rel="noopener noreferrer"
      >
        Show machines enrolled with this key
      </a>
    </section>
  );
}

function NewKeyMade(props: { chosen: Chosen; done: () => void }) {
  return (
    <section class="machines-step">
      <output class="machines-made">Created the key {props.chosen.key.name}.</output>
      <Show when={props.chosen.enroll}>
        {(enroll) => (
          <>
            <p class="machines-warning-text">
              Copy the enroll URL now. It contains the key, and this is the only time you can see
              it.
            </p>
            <pre class="machines-snippet wrap">{enroll()}</pre>
            <Copy text={enroll()} />
          </>
        )}
      </Show>
      <div class="actions">
        <button type="button" class="button" onClick={() => props.done()}>
          Done
        </button>
      </div>
    </section>
  );
}

const doneKeys = () => setLook({ view: "keys" });

export function AddView(props: {
  overview: Overview;
  keys: EnrollmentKey[];
  install: boolean;
  changed: () => void;
}) {
  const [chosen, setChosen] = createSignal<Chosen | null>(null);
  const chose = (next: Chosen) => {
    setChosen(next);
    props.changed();
  };
  const again = () => setChosen(null);
  return (
    <Show
      when={props.overview.agents_on}
      fallback={
        <p class="machines-empty">
          This server cannot enroll machines yet. Whoever runs it needs to set{" "}
          <code>--public-url</code> and <code>--agent-token-keys</code>.
        </p>
      }
    >
      <Show
        when={props.install}
        fallback={
          <div class="machines-add one">
            <Show
              when={chosen()}
              fallback={
                <Form
                  org={props.overview.org_id}
                  owner={props.overview.role === "owner"}
                  keys={props.keys}
                  install={false}
                  chose={chose}
                />
              }
            >
              {(picked) => <NewKeyMade chosen={picked()} done={doneKeys} />}
            </Show>
          </div>
        }
      >
        <div class="machines-add">
          <Show
            when={chosen()}
            fallback={
              <Form
                org={props.overview.org_id}
                owner={props.overview.role === "owner"}
                keys={props.keys}
                install
                chose={chose}
              />
            }
          >
            {(picked) => (
              <>
                <section class="machines-step" aria-labelledby="machines-key">
                  <h3 id="machines-key">Enrollment key</h3>
                  <p>
                    Using <span class="mono">{picked().key.name}</span>
                    <Show when={picked().key.tags[ENVIRONMENT]}>
                      {(env) => <>, environment {env()}</>}
                    </Show>
                    <Show when={tagText(picked().key.tags)}>{(tags) => <>, tags {tags()}</>}</Show>.{" "}
                    <button type="button" class="link-button" onClick={again}>
                      Use a different key
                    </button>
                  </p>
                </section>
                <Installing chosen={picked()} />
                <EnrolledLink chosen={picked()} org={props.overview.org_id} />
              </>
            )}
          </Show>
        </div>
      </Show>
    </Show>
  );
}
