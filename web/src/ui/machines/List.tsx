import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import {
  ago,
  clearFilters,
  day,
  type EnrollmentKey,
  filtered,
  fleetApi,
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

const tagValueName = (value: string) => (value === ANY ? "any" : value === NONE ? "none" : value);

function TagChip(props: { look: Look; name: string; value: string; values: string[] | undefined }) {
  const pick = (value: string) => {
    const others = Object.entries(props.look.tags).filter(([other]) => other !== props.name);
    setLook({
      tags: Object.fromEntries(value === ANY ? others : [...others, [props.name, value]]),
      machine: null,
    });
  };
  const options = createMemo(() => [ANY, NONE, ...(props.values ?? [])]);
  const clear = () => pick(ANY);
  return (
    <Chip
      label={props.name}
      value={props.value}
      options={options()}
      names={tagValueName}
      pick={pick}
      clear={clear}
    />
  );
}

function AddTag(props: { look: Look; overview: Overview }) {
  const choices = createMemo(() =>
    Object.entries(props.overview.tags).filter(([name]) => !(name in props.look.tags)),
  );
  const pick = (event: Event & { currentTarget: HTMLSelectElement }) => {
    const choice = event.currentTarget.value;
    event.currentTarget.value = "";
    const at = choice.indexOf("=");
    if (at > 0) {
      setLook({
        tags: { ...props.look.tags, [choice.slice(0, at)]: choice.slice(at + 1) },
        machine: null,
      });
    }
  };
  return (
    <Show when={choices().length > 0}>
      <label class="machines-chip add">
        <span class="visually-hidden">Add tag filter</span>
        <select value="" onChange={pick}>
          <option value="">Add tag filter</option>
          <For each={choices()}>
            {([name, values]) => (
              <optgroup label={name}>
                <For each={values}>
                  {(value) => <option value={`${name}=${value}`}>{value}</option>}
                </For>
                <option value={`${name}=`}>no {name}</option>
              </optgroup>
            )}
          </For>
        </select>
      </label>
    </Show>
  );
}

const pickState = (value: string) => setLook({ state: value as StateChoice, machine: null });
const pickEnvironment = (value: string) =>
  setLook({ environment: value === ANY ? null : value, machine: null });
const pickVersion = (value: string) =>
  setLook({ version: value === ANY ? null : value, machine: null });
const pickKey = (value: string) =>
  setLook({ key: value === ANY ? null : Number(value), machine: null });
const clearState = () => setLook({ state: null, machine: null });
const clearEnvironment = () => pickEnvironment(ANY);
const clearVersion = () => pickVersion(ANY);
const clearKey = () => pickKey(ANY);
const stateName = (value: string) =>
  value === ANY ? "any" : STATE_LABEL[value as keyof typeof STATE_LABEL];

function Chips(props: { look: Look; overview: Overview; keys: EnrollmentKey[] | null }) {
  const keyIds = createMemo(() => (props.keys ?? []).map((each) => String(each.id)));
  const keyName = (value: string) =>
    value === ANY
      ? ANY
      : (props.keys?.find((each) => String(each.id) === value)?.name ?? `enrollment key ${value}`);
  const tagEntries = createMemo(() => Object.entries(props.look.tags));
  return (
    <div class="machines-chips">
      <Chip
        label="State"
        value={shownState(props.look)}
        options={STATE_CHOICES}
        names={stateName}
        pick={pickState}
        clear={props.look.state === null ? undefined : clearState}
      />
      <Chip
        label="Environment"
        value={props.look.environment ?? ANY}
        options={props.overview.environments}
        pick={pickEnvironment}
        clear={props.look.environment === null ? undefined : clearEnvironment}
      />
      <Chip
        label="Version"
        value={props.look.version ?? ANY}
        options={props.overview.versions}
        pick={pickVersion}
        clear={props.look.version === null ? undefined : clearVersion}
      />
      <Show when={props.keys !== null && (props.keys.length > 0 || props.look.key !== null)}>
        <Chip
          label="Enrollment key"
          value={props.look.key === null ? ANY : String(props.look.key)}
          options={keyIds()}
          names={keyName}
          pick={pickKey}
          clear={props.look.key === null ? undefined : clearKey}
        />
      </Show>
      <For each={tagEntries()}>
        {([name, value]) => (
          <TagChip look={props.look} name={name} value={value} values={props.overview.tags[name]} />
        )}
      </For>
      <AddTag look={props.look} overview={props.overview} />
    </div>
  );
}

function Marked(props: { text: string; query: string }) {
  const match = createMemo(() => {
    const q = props.query.trim().toLowerCase();
    const at = q === "" ? -1 : props.text.toLowerCase().indexOf(q);
    return at === -1 ? null : { at, length: q.length };
  });
  return (
    <Show when={match()} fallback={props.text}>
      {(found) => (
        <>
          {props.text.slice(0, found().at)}
          <mark>{props.text.slice(found().at, found().at + found().length)}</mark>
          {props.text.slice(found().at + found().length)}
        </>
      )}
    </Show>
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

function walk(event: KeyboardEvent & { currentTarget: HTMLButtonElement }) {
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

function Row(props: { machine: Machine; look: Look; now: number }) {
  const chosen = createMemo(() => props.look.machine === props.machine.id);
  const open = () => setLook({ machine: chosen() ? null : props.machine.id });
  return (
    <li>
      <button
        type="button"
        class={`machines-row${chosen() ? " chosen" : ""}`}
        aria-current={chosen() ? "true" : "false"}
        aria-label={`${props.machine.hostname}, ${STATE_LABEL[props.machine.state].toLowerCase()}`}
        data-machine={props.machine.id}
        onClick={open}
        onKeyDown={walk}
      >
        <span class="machines-host">
          <Dot state={props.machine.state} />
          <span class="mono">
            <Marked text={props.machine.hostname} query={props.look.q} />
          </span>
          <Show when={props.machine.matched}>
            {(matched) => <span class="machines-matched">matched {matched()}</span>}
          </Show>
        </span>
        <span class="mono">
          <Show when={props.machine.primary_ip} fallback="">
            {(ip) => <Marked text={ip()} query={props.look.q} />}
          </Show>
        </span>
        <span>{props.machine.tags.environment ?? ""}</span>
        <span class="mono">{tagText(props.machine.tags)}</span>
        <span class="mono">{props.machine.agent_version}</span>
        <span>{seen(props.machine, props.now)}</span>
        <span class="machines-warnings">
          <For each={props.machine.warnings}>
            {(warning) => (
              <span class={`machines-warning ${warning}`}>{WARNING_LABEL[warning]}</span>
            )}
          </For>
        </span>
      </button>
    </li>
  );
}

const showEveryState = () => setLook({ state: "any" });

function Empty(props: { look: Look }) {
  const clear = () => setLook({ ...clearFilters(props.look), machine: null });
  const q = createMemo(() => props.look.q.trim());
  const isFiltered = createMemo(() => filtered(props.look));
  return (
    <div class="machines-empty">
      <p>
        {q() === ""
          ? isFiltered()
            ? "No machine matches these filters."
            : "No machines are live right now."
          : `No machine has ${q()} in its hostname, addresses, MACs, or instance id.`}
      </p>
      <Show
        when={isFiltered()}
        fallback={
          <Show when={q() === ""}>
            <button type="button" class="button" onClick={showEveryState}>
              Show machines in every state
            </button>
          </Show>
        }
      >
        <button type="button" class="button" onClick={clear}>
          Clear filters
        </button>
      </Show>
    </div>
  );
}

function useMachines(look: () => Look) {
  const key = createMemo(() => {
    const current = look();
    return JSON.stringify([
      current.org,
      current.q.trim(),
      current.state,
      current.environment,
      current.tags,
      current.version,
      current.key,
    ]);
  });
  const first = useLoaded(() => fleetApi.machines(look(), null), key);
  const [more, setMore] = createSignal<{
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
  const extra = createMemo(() => {
    const current = more();
    return current.key === key() ? current : null;
  });
  const next = createMemo(() => {
    const currentExtra = extra();
    return currentExtra ? currentExtra.next : (first.data()?.next ?? null);
  });
  const showMore = () => {
    const cursor = next();
    if (cursor === null) {
      return;
    }
    const currentLook = look();
    const currentKey = key();
    void fleetApi.machines(currentLook, cursor).then(
      (page) =>
        setMore((before) => ({
          key: currentKey,
          machines: [...(before.key === currentKey ? before.machines : []), ...page.machines],
          next: page.next,
          error: null,
        })),
      (err: Error) => setMore((before) => ({ ...before, key: currentKey, error: err.message })),
    );
  };
  const machines = createMemo(() => {
    const data = first.data();
    return data ? [...data.machines, ...(extra()?.machines ?? [])] : null;
  });
  const error = createMemo(() => first.error() ?? extra()?.error ?? null);
  return {
    machines,
    error,
    next,
    showMore,
    again: first.again,
  };
}

const closeDetail = () => setLook({ machine: null });

export function ListView(props: {
  look: Look;
  overview: Overview;
  keys: EnrollmentKey[] | null;
  now: number;
  changed: () => void;
}) {
  const { machines, error, next, showMore, again } = useMachines(() => props.look);
  const refresh = () => {
    again();
    props.changed();
  };
  return (
    <div class={`machines-split${props.look.machine === null ? "" : " has-detail"}`}>
      <div class="machines-list">
        <Chips look={props.look} overview={props.overview} keys={props.keys} />
        <Switch
          fallback={
            <div class="machines-rows list">
              <div class="machines-row head" aria-hidden="true">
                <span>Hostname</span>
                <span>Primary IP</span>
                <span>Env</span>
                <span>Tags</span>
                <span>Version</span>
                <span>Last seen</span>
                <span />
              </div>
              <ul aria-label="Machines">
                <For each={machines() ?? []}>
                  {(machine) => <Row machine={machine} look={props.look} now={props.now} />}
                </For>
              </ul>
              <Show when={error()}>
                {(message) => (
                  <p class="error" role="alert">
                    Could not load more machines: {message()}.
                  </p>
                )}
              </Show>
              <Show when={next() !== null}>
                <button type="button" class="button" onClick={showMore}>
                  Show more
                </button>
              </Show>
            </div>
          }
        >
          <Match when={machines() === null}>
            <Retry error={error()} again={again} />
          </Match>
          <Match when={machines()?.length === 0}>
            <Empty look={props.look} />
          </Match>
        </Switch>
      </div>
      <Show when={props.look.machine} keyed>
        {(id) => (
          <DetailPanel
            id={id}
            owner={props.overview.role === "owner"}
            now={props.now}
            back={closeDetail}
            changed={refresh}
          />
        )}
      </Show>
    </div>
  );
}

export function Retry(props: { error: string | null; again: () => void; what?: string }) {
  return (
    <div class="machines-loading">
      <Loading error={props.error} what={props.what ?? "machines"} />
      <Show when={props.error}>
        <button type="button" class="button" onClick={() => props.again()}>
          Try again
        </button>
      </Show>
    </div>
  );
}
