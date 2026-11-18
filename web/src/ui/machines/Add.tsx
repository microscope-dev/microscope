import { type ChangeEvent, type FormEvent, useCallback, useEffect, useRef, useState } from "react";

import { copyText } from "../../clipboard";
import {
  ago,
  type Arrival,
  type EnrollmentKey,
  fleetApi,
  type Install,
  INSTALLS,
  type Overview,
  ENVIRONMENT,
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
  const { org, owner, chose } = props;
  const active = props.keys.filter((key) => key.revoked_at === null);
  const [environment, setEnvironment] = useState("");
  const [which, setWhich] = useState<"new" | "existing">("new");
  const [existing, setExisting] = useState<number | null>(active[0]?.id ?? null);
  const [tags, setTags] = useState("");
  const [expires, setExpires] = useState("");
  const { working, error, run } = useAction();
  const [invalid, setInvalid] = useState<string | null>(null);
  const first = useRef<HTMLInputElement>(null);
  useEffect(() => first.current?.focus(), []);

  const typeEnvironment = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setEnvironment(event.target.value),
    [],
  );
  const typeTags = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setTags(event.target.value),
    [],
  );
  const typeExpires = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setExpires(event.target.value),
    [],
  );
  const pickNew = useCallback(() => setWhich("new"), []);
  const pickExisting = useCallback(() => setWhich("existing"), []);
  const pickKey = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => setExisting(Number(event.target.value)),
    [],
  );

  const submit = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      if (!owner) {
        return;
      }
      if (which === "existing") {
        const key = active.find((candidate) => candidate.id === existing);
        if (key) {
          chose({ key, enroll: null });
        }
        return;
      }
      const name = environment.trim();
      const parsed = parseTags(tags);
      if (name === "") {
        setInvalid("Enter an environment, such as prod.");
        return;
      }
      if (typeof parsed === "string") {
        setInvalid(`Tags: ${parsed}.`);
        return;
      }
      setInvalid(null);
      const expiresAt =
        expires === "" ? undefined : Math.floor(Date.parse(`${expires}T23:59:59Z`) / 1000);
      run(async () => {
        const created = await fleetApi.createKey({
          org_id: org,
          name,
          tags: { ...parsed, [ENVIRONMENT]: name },
          ...(expiresAt === undefined ? {} : { expires_at: expiresAt }),
        });
        chose({ key: created, enroll: created.enroll_url });
      });
    },
    [owner, which, active, existing, environment, tags, expires, org, run, chose],
  );

  return (
    <form className="machines-form" aria-label="Enrollment key" onSubmit={submit}>
      {props.install && (
        <>
          <h3>Enrollment key</h3>
          <p>
            The agent on each machine uses an enrollment key to join. One key can enroll many
            machines.
          </p>
        </>
      )}
      {props.install && active.length > 0 && (
        <fieldset className="field">
          <legend className="visually-hidden">Which key</legend>
          <label className="machines-check">
            <input
              type="radio"
              name="which"
              checked={which === "new"}
              disabled={!owner}
              onChange={pickNew}
            />
            <span>Create a new key</span>
          </label>
          <label className="machines-check">
            <input
              type="radio"
              name="which"
              checked={which === "existing"}
              disabled={!owner}
              onChange={pickExisting}
            />
            <span>Use an existing key</span>
          </label>
          {which === "existing" && (
            <select
              className="input"
              aria-label="Existing key"
              value={existing ?? ""}
              onChange={pickKey}
            >
              {active.map((key) => (
                <option key={key.id} value={key.id}>
                  {key.name}
                </option>
              ))}
            </select>
          )}
        </fieldset>
      )}
      {which === "new" && (
        <>
          <div className="field">
            <label htmlFor="machines-environment">Environment</label>
            <input
              id="machines-environment"
              ref={first}
              className="input"
              value={environment}
              placeholder="prod"
              disabled={!owner}
              aria-describedby="machines-environment-note"
              onChange={typeEnvironment}
            />
            <span id="machines-environment-note" className="note">
              Such as prod or staging. The key is named after it.
            </span>
          </div>
          <div className="field">
            <label htmlFor="machines-tags">Tags (optional)</label>
            <input
              id="machines-tags"
              className="input"
              value={tags}
              placeholder="cluster=checkout, team=payments"
              disabled={!owner}
              aria-describedby="machines-tags-note"
              onChange={typeTags}
            />
            <span id="machines-tags-note" className="note">
              Key=value pairs, separated by commas.
            </span>
          </div>
          <div className="field">
            <label htmlFor="machines-expires">Expires (optional)</label>
            <input
              id="machines-expires"
              className="input"
              type="date"
              value={expires}
              disabled={!owner}
              aria-describedby="machines-expires-note"
              onChange={typeExpires}
            />
            <span id="machines-expires-note" className="note">
              After this day (UTC), new machines cannot enroll with the key. Machines already
              enrolled keep running.
            </span>
          </div>
          <p className="machines-note">
            Every machine that enrolls with this key gets these tags. A tag set in the agent config
            wins over the same tag here.
          </p>
        </>
      )}
      {(invalid ?? error) && (
        <p className="error" role="alert">
          {invalid ?? error}
        </p>
      )}
      <div className="actions">
        <button
          type="submit"
          className="button primary"
          aria-disabled={!owner || working}
          title={owner ? undefined : OWNERS_ONLY}
        >
          {which === "new" ? "Create key" : "Use this key"}
        </button>
        {!owner && <span className="muted">{OWNERS_ONLY}</span>}
      </div>
    </form>
  );
}

function Copy({ text }: { text: string }) {
  const [copied, setCopied] = useState<boolean | null>(null);
  const copy = useCallback(() => void copyText(text).then(setCopied), [text]);
  return (
    <span className="actions">
      <button type="button" className="button" onClick={copy}>
        Copy
      </button>
      {copied !== null && (
        <output className={copied ? "muted" : "error"}>
          {copied ? "Copied" : "Could not copy; select the text instead"}
        </output>
      )}
    </span>
  );
}

function Installing({ chosen }: { chosen: Chosen }) {
  const [install, setInstall] = useState<Install>(remembered);
  const pick = useCallback((next: Install) => {
    setInstall(next);
    remember(next);
  }, []);
  const text = snippet(install, chosen.enroll ?? PLACEHOLDER);
  return (
    <section className="machines-step" aria-labelledby="machines-install">
      <h3 id="machines-install">Install the agent</h3>
      <div className="segmented" role="tablist" aria-label="How to install">
        {INSTALLS.map((option) => (
          <InstallTab key={option} option={option} chosen={install === option} pick={pick} />
        ))}
      </div>
      {chosen.enroll ? (
        <p className="machines-warning-text">
          This command contains the new key, and this is the only time you can see it. To add more
          machines later, save it in your secret manager, or create another key.
        </p>
      ) : (
        <p>
          Replace <code>{PLACEHOLDER}</code> with the enroll URL you saved when you created{" "}
          <span className="mono">{chosen.key.name}</span>.
        </p>
      )}
      {install !== "systemd" && (
        <p className="error">
          TODO:{" "}
          {install === "docker" ? "the Docker image is" : "the Docker image and Helm chart are"} not
          published yet, so {install === "docker" ? "this command does" : "these commands do"} not
          work.
        </p>
      )}
      <pre
        className="machines-snippet"
        role="tabpanel"
        tabIndex={0}
        aria-label={INSTALL_LABEL[install]}
      >
        {text}
      </pre>
      <Copy text={text} />
      <p className="note">
        {install === "kubernetes" ? (
          <>
            Replace <code>your-cluster-name</code> with the name of this cluster.
          </>
        ) : (
          <>
            Replace <code>your-cluster-name</code> with the name of the cluster these machines
            belong to, or remove that line.
          </>
        )}{" "}
        Tags set here win over the key tags.
      </p>
    </section>
  );
}

function InstallTab(props: { option: Install; chosen: boolean; pick: (install: Install) => void }) {
  const { option, pick } = props;
  const click = useCallback(() => pick(option), [pick, option]);
  return (
    <button
      type="button"
      role="tab"
      aria-selected={props.chosen}
      tabIndex={props.chosen ? 0 : -1}
      onClick={click}
    >
      {INSTALL_LABEL[option]}
    </button>
  );
}

const WAIT_HINT_SECS = 5 * 60;

function Arrivals({ chosen, now }: { chosen: Chosen; now: number }) {
  const [arrivals, setArrivals] = useState<Arrival[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [since] = useState(() => Date.now() / 1000);
  const id = chosen.key.id;
  useEffect(() => {
    let live = true;
    const ask = async () => {
      try {
        const found = await fleetApi.arrivals(id);
        if (live) {
          setArrivals(found);
          setError(null);
        }
      } catch (failure) {
        if (live) {
          setError((failure as Error).message);
        }
      }
    };
    void ask();
    const timer = setInterval(ask, 3000);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [id]);
  return (
    <section className="machines-step" aria-labelledby="machines-arrived">
      <h3 id="machines-arrived">Machines enrolled with this key</h3>
      {error && (
        <p className="error" role="alert">
          Could not check for machines: {error}.
        </p>
      )}
      {arrivals && arrivals.length > 0 ? (
        <>
          <ul className="machines-arrivals" aria-label="Arrived">
            {arrivals.map((arrival) => (
              <li key={arrival.id}>
                <span className="mono">{arrival.hostname}</span>
                <span className="mono">{arrival.primary_ip ?? ""}</span>
                <span className="mono">{tagText(arrival.tags)}</span>
                <span>{ago(arrival.created_at, now)}</span>
              </li>
            ))}
          </ul>
          <p className="note">
            New machines appear here as they enroll. You can close this window; they keep enrolling
            without it.
          </p>
        </>
      ) : (
        <>
          {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result */}
          <p className="muted" role="status">
            Waiting for the first machine. Machines usually appear within a minute of the agent
            starting.
          </p>
          {now - since > WAIT_HINT_SECS && (
            <div className="note">
              <p>Still nothing? The usual causes:</p>
              <ul>
                <li>The agent is not running.</li>
                <li>The agent cannot reach this server.</li>
                <li>The enroll URL was copied incompletely.</li>
              </ul>
              <p>
                Run <code>microscope agent status</code> on the machine to see which.
              </p>
            </div>
          )}
        </>
      )}
    </section>
  );
}

function NewKeyMade({ chosen, done }: { chosen: Chosen; done: () => void }) {
  return (
    <section className="machines-step">
      <output className="machines-made">Created the key {chosen.key.name}.</output>
      {chosen.enroll && (
        <>
          <p className="machines-warning-text">
            Copy the enroll URL now. It contains the key, and this is the only time you can see it.
          </p>
          <pre className="machines-snippet wrap">{chosen.enroll}</pre>
          <Copy text={chosen.enroll} />
        </>
      )}
      <div className="actions">
        <button type="button" className="button" onClick={done}>
          Done
        </button>
      </div>
    </section>
  );
}

export function AddView(props: {
  overview: Overview;
  keys: EnrollmentKey[];
  install: boolean;
  now: number;
  changed: () => void;
}) {
  const { overview, install, changed } = props;
  const [chosen, setChosen] = useState<Chosen | null>(null);
  const chose = useCallback(
    (next: Chosen) => {
      setChosen(next);
      changed();
    },
    [changed],
  );
  const done = useCallback(() => setLook({ view: "keys" }), []);
  const again = useCallback(() => setChosen(null), []);
  if (!overview.agents_on) {
    return (
      <p className="machines-empty">
        This server cannot enroll machines yet. Whoever runs it needs to set{" "}
        <code>--public-url</code> and <code>--agent-token-keys</code>.
      </p>
    );
  }
  if (!install) {
    return (
      <div className="machines-add one">
        {chosen === null ? (
          <Form
            org={overview.org_id}
            owner={overview.role === "owner"}
            keys={props.keys}
            install={false}
            chose={chose}
          />
        ) : (
          <NewKeyMade chosen={chosen} done={done} />
        )}
      </div>
    );
  }
  return (
    <div className="machines-add">
      {chosen === null ? (
        <Form
          org={overview.org_id}
          owner={overview.role === "owner"}
          keys={props.keys}
          install
          chose={chose}
        />
      ) : (
        <>
          <section className="machines-step" aria-labelledby="machines-key">
            <h3 id="machines-key">Enrollment key</h3>
            <p>
              Using <span className="mono">{chosen.key.name}</span>
              {chosen.key.tags[ENVIRONMENT] && <>, environment {chosen.key.tags[ENVIRONMENT]}</>}
              {tagText(chosen.key.tags) && <>, tags {tagText(chosen.key.tags)}</>}.{" "}
              <button type="button" className="link-button" onClick={again}>
                Use a different key
              </button>
            </p>
          </section>
          <Installing chosen={chosen} />
          <Arrivals chosen={chosen} now={props.now} />
        </>
      )}
    </div>
  );
}
