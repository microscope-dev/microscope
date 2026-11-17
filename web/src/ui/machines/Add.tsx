import { type ChangeEvent, type FormEvent, useCallback, useEffect, useState } from "react";

import { copyText } from "../../clipboard";
import {
  ago,
  type Arrival,
  type EnrollmentKey,
  fleetApi,
  type Install,
  INSTALLS,
  type Overview,
  labelText,
  parseLabels,
  PLACEHOLDER,
  setLook,
  snippet,
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
  const [labels, setLabels] = useState("");
  const [expires, setExpires] = useState("");
  const { working, error, run } = useAction();
  const [invalid, setInvalid] = useState<string | null>(null);

  const typeEnvironment = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setEnvironment(event.target.value),
    [],
  );
  const typeLabels = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setLabels(event.target.value),
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
      const parsed = parseLabels(labels);
      if (name === "") {
        setInvalid("Name the environment these machines run in.");
        return;
      }
      if (typeof parsed === "string") {
        setInvalid(`Labels: ${parsed}.`);
        return;
      }
      setInvalid(null);
      const expiresAt =
        expires === "" ? undefined : Math.floor(Date.parse(`${expires}T23:59:59Z`) / 1000);
      run(async () => {
        const created = await fleetApi.createKey({
          org_id: org,
          name,
          environment: name,
          labels: parsed,
          ...(expiresAt === undefined ? {} : { expires_at: expiresAt }),
        });
        chose({ key: created, enroll: created.enroll_url });
      });
    },
    [owner, which, active, existing, environment, labels, expires, org, run, chose],
  );

  return (
    <form className="machines-form" onSubmit={submit}>
      <h3>{props.install ? "1. Where they go" : "Where its machines go"}</h3>
      <div className="field">
        <label htmlFor="machines-environment">Environment</label>
        <input
          id="machines-environment"
          className="input"
          data-autofocus
          value={environment}
          placeholder="prod"
          disabled={!owner}
          onChange={typeEnvironment}
        />
        <span className="note">prod, staging, canary, or any name you use</span>
      </div>
      {props.install && active.length > 0 && (
        <fieldset className="field">
          <legend className="label">Enrollment key</legend>
          <label className="machines-check">
            <input
              type="radio"
              name="which"
              checked={which === "new"}
              disabled={!owner}
              onChange={pickNew}
            />
            <span>A new key, named {environment.trim() || "after the environment"}</span>
          </label>
          <label className="machines-check">
            <input
              type="radio"
              name="which"
              checked={which === "existing"}
              disabled={!owner}
              onChange={pickExisting}
            />
            <span>An existing key</span>
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
            <label htmlFor="machines-labels">
              Labels <span className="muted">optional</span>
            </label>
            <input
              id="machines-labels"
              className="input"
              value={labels}
              placeholder="team=payments"
              disabled={!owner}
              onChange={typeLabels}
            />
          </div>
          <div className="field">
            <label htmlFor="machines-expires">
              Expires <span className="muted">optional</span>
            </label>
            <input
              id="machines-expires"
              className="input"
              type="date"
              value={expires}
              disabled={!owner}
              onChange={typeExpires}
            />
          </div>
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
      <p className="note">
        Machines can override the environment and labels in their own config. The key only sets the
        defaults.
      </p>
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
    <section className="machines-step">
      <h3>2. Install</h3>
      <div className="segmented" role="tablist" aria-label="How to install">
        {INSTALLS.map((option) => (
          <InstallTab key={option} option={option} chosen={install === option} pick={pick} />
        ))}
      </div>
      {chosen.enroll ? (
        <p className="machines-warning-text">
          Copy this now. The key is shown only this once. If you lose it, make another; it takes a
          second.
        </p>
      ) : (
        <p className="note">
          Put the enroll URL you saved when you made {chosen.key.name} where it says {PLACEHOLDER}.
        </p>
      )}
      <pre className="machines-snippet" role="tabpanel" aria-label={INSTALL_LABEL[install]}>
        {text}
      </pre>
      <div className="actions">
        <Copy text={text} />
        {install === "kubernetes" && (
          <span className="note">
            Set the cluster name: it is how this window groups your nodes.
          </span>
        )}
      </div>
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
    <section className="machines-step">
      <h3>3. Machines enrolled with this key</h3>
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
                <span className="mono">{arrival.cluster ?? ""}</span>
                <span>{ago(arrival.created_at, now)}</span>
              </li>
            ))}
          </ul>
          <p className="note">
            Updates as machines arrive. Closing this window is safe: they keep enrolling.
          </p>
        </>
      ) : (
        <>
          {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result */}
          <p className="muted" role="status">
            Waiting for the first machine. It usually shows up within a minute of the agent
            starting.
          </p>
          {now - since > WAIT_HINT_SECS && (
            <div className="note">
              <p>Nothing yet. The usual reasons:</p>
              <ul>
                <li>the agent cannot reach this server,</li>
                <li>the enroll URL was mistyped, or</li>
                <li>the agent is not running.</li>
              </ul>
              <p>
                On the machine, <code>microscope agent status</code> shows which.
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
      <output className="machines-made">Key {chosen.key.name} created.</output>
      {chosen.enroll && (
        <>
          <p className="machines-warning-text">
            Copy this enroll URL now. It holds the key, which is shown only this once.
          </p>
          <pre className="machines-snippet">{chosen.enroll}</pre>
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
        This server does not take machines yet. Whoever runs it needs to give it a public URL and
        agent token keys.
      </p>
    );
  }
  return (
    <div className={install ? "machines-add" : "machines-add one"}>
      <div>
        {chosen === null ? (
          <Form
            org={overview.org_id}
            owner={overview.role === "owner"}
            keys={props.keys}
            install={install}
            chose={chose}
          />
        ) : install ? (
          <section className="machines-step">
            <h3>1. Where they go</h3>
            <p>
              Key <span className="mono">{chosen.key.name}</span>
              {chosen.key.environment && <>, environment {chosen.key.environment}</>}
              {Object.keys(chosen.key.labels ?? {}).length > 0 && (
                <>, labels {labelText(chosen.key.labels)}</>
              )}
              .
            </p>
            <div className="actions">
              <button type="button" className="button" onClick={again}>
                Choose another key
              </button>
            </div>
          </section>
        ) : (
          <NewKeyMade chosen={chosen} done={done} />
        )}
      </div>
      {install && (
        <div>
          {chosen ? (
            <>
              <Installing chosen={chosen} />
              <Arrivals chosen={chosen} now={props.now} />
            </>
          ) : (
            <p className="muted">Create or choose a key, and the install steps appear here.</p>
          )}
        </div>
      )}
    </div>
  );
}
