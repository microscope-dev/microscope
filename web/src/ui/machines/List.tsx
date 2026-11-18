import { type ChangeEvent, type KeyboardEvent, useCallback, useMemo, useState } from "react";

import {
  ago,
  clearFilters,
  day,
  type EnrollmentKey,
  fleetApi,
  filtered,
  type Look,
  type Machine,
  type Overview,
  setLook,
  shownState,
  type StateChoice,
  tagText,
} from "../../fleet";
import { Loading, useLoaded } from "../Orgs";
import { DetailPanel } from "./Detail";
import { ANY, Chip, Dot, STATE_LABEL, WARNING_LABEL } from "./parts";

const STATE_CHOICES: readonly StateChoice[] = ["live", "quiet", "stopped", "revoked", "any"];

const NONE = "";

function TagChip(props: { look: Look; name: string; value: string; values: string[] | undefined }) {
  const { look, name } = props;
  const pick = useCallback(
    (value: string) => {
      const others = Object.entries(look.tags).filter(([other]) => other !== name);
      setLook({
        tags: Object.fromEntries(value === ANY ? others : [...others, [name, value]]),
        machine: null,
      });
    },
    [look.tags, name],
  );
  const options = useMemo(() => [ANY, NONE, ...(props.values ?? [])], [props.values]);
  const clear = useCallback(() => pick(ANY), [pick]);
  const names = useCallback(
    (value: string) => (value === ANY ? "any" : value === NONE ? "none" : value),
    [],
  );
  return (
    <Chip
      label={name}
      value={props.value}
      options={options}
      names={names}
      pick={pick}
      clear={clear}
    />
  );
}

function AddTag({ look, overview }: { look: Look; overview: Overview }) {
  const choices = Object.entries(overview.tags).filter(([name]) => !(name in look.tags));
  const pick = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const choice = event.target.value;
      const at = choice.indexOf("=");
      if (at > 0) {
        setLook({
          tags: { ...look.tags, [choice.slice(0, at)]: choice.slice(at + 1) },
          machine: null,
        });
      }
    },
    [look.tags],
  );
  if (choices.length === 0) {
    return null;
  }
  return (
    <label className="machines-chip add">
      <span className="visually-hidden">Add tag filter</span>
      <select value="" onChange={pick}>
        <option value="">Add tag filter</option>
        {choices.map(([name, values]) => (
          <optgroup key={name} label={name}>
            {values.map((value) => (
              <option key={value} value={`${name}=${value}`}>
                {value}
              </option>
            ))}
            <option value={`${name}=`}>no {name}</option>
          </optgroup>
        ))}
      </select>
    </label>
  );
}

function Chips(props: { look: Look; overview: Overview; keys: EnrollmentKey[] | null }) {
  const { look, overview, keys } = props;
  const state = useCallback(
    (value: string) => setLook({ state: value as StateChoice, machine: null }),
    [],
  );
  const environment = useCallback(
    (value: string) => setLook({ environment: value === ANY ? null : value, machine: null }),
    [],
  );
  const version = useCallback(
    (value: string) => setLook({ version: value === ANY ? null : value, machine: null }),
    [],
  );
  const key = useCallback(
    (value: string) => setLook({ key: value === ANY ? null : Number(value), machine: null }),
    [],
  );
  const keyIds = useMemo(() => (keys ?? []).map((each) => String(each.id)), [keys]);
  const keyName = useCallback(
    (value: string) =>
      value === ANY
        ? ANY
        : (keys?.find((each) => String(each.id) === value)?.name ?? `enrollment key ${value}`),
    [keys],
  );
  const clearState = useCallback(() => setLook({ state: null, machine: null }), []);
  const clearEnvironment = useCallback(() => environment(ANY), [environment]);
  const clearVersion = useCallback(() => version(ANY), [version]);
  const clearKey = useCallback(() => key(ANY), [key]);
  const stateName = useCallback(
    (value: string) => (value === ANY ? "any" : STATE_LABEL[value as keyof typeof STATE_LABEL]),
    [],
  );
  return (
    <div className="machines-chips">
      <Chip
        label="State"
        value={shownState(look)}
        options={STATE_CHOICES}
        names={stateName}
        pick={state}
        clear={look.state === null ? undefined : clearState}
      />
      <Chip
        label="Environment"
        value={look.environment ?? ANY}
        options={overview.environments}
        pick={environment}
        clear={look.environment === null ? undefined : clearEnvironment}
      />
      <Chip
        label="Version"
        value={look.version ?? ANY}
        options={overview.versions}
        pick={version}
        clear={look.version === null ? undefined : clearVersion}
      />
      {keys !== null && (keys.length > 0 || look.key !== null) && (
        <Chip
          label="Enrollment key"
          value={look.key === null ? ANY : String(look.key)}
          options={keyIds}
          names={keyName}
          pick={key}
          clear={look.key === null ? undefined : clearKey}
        />
      )}
      {Object.entries(look.tags).map(([name, value]) => (
        <TagChip key={name} look={look} name={name} value={value} values={overview.tags[name]} />
      ))}
      <AddTag look={look} overview={overview} />
    </div>
  );
}

function Marked({ text, query }: { text: string; query: string }) {
  const q = query.trim().toLowerCase();
  const at = q === "" ? -1 : text.toLowerCase().indexOf(q);
  if (at === -1) {
    return text;
  }
  return (
    <>
      {text.slice(0, at)}
      <mark>{text.slice(at, at + q.length)}</mark>
      {text.slice(at + q.length)}
    </>
  );
}

function seen(machine: Machine, now: number): string {
  if (machine.state === "revoked" && machine.revoked_at !== null) {
    return `revoked ${day(machine.revoked_at)}`;
  }
  if (machine.state === "stopped" && machine.stopped_at !== null) {
    return ago(machine.stopped_at, now);
  }
  return ago(machine.last_seen_at, now);
}

function walk(event: KeyboardEvent<HTMLButtonElement>) {
  if (event.key !== "ArrowDown" && event.key !== "ArrowUp") {
    return;
  }
  event.preventDefault();
  const rows = [
    ...(event.currentTarget
      .closest("ul")
      ?.querySelectorAll<HTMLButtonElement>("button.machines-row") ?? []),
  ];
  const at = rows.indexOf(event.currentTarget);
  rows[
    event.key === "ArrowDown" ? Math.min(at + 1, rows.length - 1) : Math.max(at - 1, 0)
  ]?.focus();
}

function Row({ machine, look, now }: { machine: Machine; look: Look; now: number }) {
  const open = useCallback(() => setLook({ machine: machine.id }), [machine.id]);
  const chosen = look.machine === machine.id;
  return (
    <li>
      <button
        type="button"
        className={`machines-row${chosen ? " chosen" : ""}`}
        aria-current={chosen}
        aria-label={`${machine.hostname}, ${STATE_LABEL[machine.state].toLowerCase()}`}
        data-machine={machine.id}
        onClick={open}
        onKeyDown={walk}
      >
        <span className="machines-host">
          <Dot state={machine.state} />
          <span className="mono">
            <Marked text={machine.hostname} query={look.q} />
          </span>
          {machine.matched && <span className="machines-matched">matched {machine.matched}</span>}
        </span>
        <span className="mono">
          {machine.primary_ip ? <Marked text={machine.primary_ip} query={look.q} /> : ""}
        </span>
        <span>{machine.tags.environment ?? ""}</span>
        <span className="mono">{tagText(machine.tags)}</span>
        <span className="mono">{machine.agent_version}</span>
        <span>{seen(machine, now)}</span>
        <span className="machines-warnings">
          {machine.warnings.map((warning) => (
            <span key={warning} className={`machines-warning ${warning}`}>
              {WARNING_LABEL[warning]}
            </span>
          ))}
        </span>
      </button>
    </li>
  );
}

function Empty({ look }: { look: Look }) {
  const clear = useCallback(() => setLook({ ...clearFilters(look), machine: null }), [look]);
  const everything = useCallback(() => setLook({ state: "any" }), []);
  const q = look.q.trim();
  return (
    <div className="machines-empty">
      <p>
        {q === ""
          ? filtered(look)
            ? "No machine matches these filters."
            : "No machines are live right now."
          : `No machine has ${q} in its hostname, addresses, MACs, or instance id.`}
      </p>
      {filtered(look) ? (
        <button type="button" className="button" onClick={clear}>
          Clear filters
        </button>
      ) : (
        q === "" && (
          <button type="button" className="button" onClick={everything}>
            Show machines in every state
          </button>
        )
      )}
    </div>
  );
}

function useMachines(look: Look) {
  const key = JSON.stringify([
    look.org,
    look.q.trim(),
    look.state,
    look.environment,
    look.tags,
    look.version,
    look.key,
  ]);
  const first = useLoaded(() => fleetApi.machines(look, null), key);
  const [more, setMore] = useState<{
    key: string;
    machines: Machine[];
    next: string | null;
    error: string | null;
  }>({
    key: "",
    machines: [],
    next: null,
    error: null,
  });
  const extra = more.key === key ? more : null;
  const next = extra ? extra.next : (first.data?.next ?? null);
  const showMore = useCallback(() => {
    if (next === null) {
      return;
    }
    fleetApi.machines(look, next).then(
      (page) =>
        setMore((before) => ({
          key,
          machines: [...(before.key === key ? before.machines : []), ...page.machines],
          next: page.next,
          error: null,
        })),
      (error: Error) => setMore((before) => ({ ...before, key, error: error.message })),
    );
  }, [look, key, next]);
  return {
    machines: first.data ? [...first.data.machines, ...(extra?.machines ?? [])] : null,
    error: first.error ?? extra?.error ?? null,
    next,
    showMore,
    again: first.again,
  };
}

export function ListView(props: {
  look: Look;
  overview: Overview;
  keys: EnrollmentKey[] | null;
  now: number;
  changed: () => void;
}) {
  const { look, overview, now, changed } = props;
  const { machines, error, next, showMore, again } = useMachines(look);
  const back = useCallback(() => setLook({ machine: null }), []);
  const refresh = useCallback(() => {
    again();
    changed();
  }, [again, changed]);
  return (
    <div className={`machines-split${look.machine === null ? "" : " has-detail"}`}>
      <div className="machines-list">
        <Chips look={look} overview={overview} keys={props.keys} />
        {machines === null ? (
          <Retry error={error} again={again} />
        ) : machines.length === 0 ? (
          <Empty look={look} />
        ) : (
          <div className="machines-rows list">
            <div className="machines-row head" aria-hidden="true">
              <span>Hostname</span>
              <span>Primary IP</span>
              <span>Env</span>
              <span>Tags</span>
              <span>Version</span>
              <span>Last seen</span>
              <span />
            </div>
            <ul aria-label="Machines">
              {machines.map((machine) => (
                <Row key={machine.id} machine={machine} look={look} now={now} />
              ))}
            </ul>
            {error && (
              <p className="error" role="alert">
                Could not load more machines: {error}.
              </p>
            )}
            {next !== null && (
              <button type="button" className="button" onClick={showMore}>
                Show more
              </button>
            )}
          </div>
        )}
      </div>
      {look.machine !== null && (
        <DetailPanel
          key={look.machine}
          id={look.machine}
          owner={overview.role === "owner"}
          now={now}
          back={back}
          changed={refresh}
        />
      )}
    </div>
  );
}

export function Retry(props: { error: string | null; again: () => void; what?: string }) {
  const { error, again } = props;
  return (
    <div className="machines-loading">
      <Loading error={error} what={props.what ?? "machines"} />
      {error && (
        <button type="button" className="button" onClick={again}>
          Try again
        </button>
      )}
    </div>
  );
}
