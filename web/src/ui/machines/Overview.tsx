import { createMemo, For, Show } from "solid-js";

import { type Group, type Overview, setLook } from "../../fleet";
import { ANY, Chip } from "./parts";

function GroupRow(props: { group: Group; split: string | null }) {
  const show = (state: "quiet" | null) =>
    setLook({
      view: "list",
      q: "",
      state,
      environment: props.group.environment,
      tags: props.split === null ? {} : { [props.split]: props.group.value ?? "" },
      version: null,
      machine: null,
    });
  const range = createMemo(() =>
    props.group.live_min_7d === props.group.live_max_7d
      ? String(props.group.live_max_7d)
      : `${props.group.live_min_7d} to ${props.group.live_max_7d}`,
  );
  const value = createMemo(() => props.group.value ?? `no ${props.split}`);
  return (
    <li class="machines-group">
      <button
        type="button"
        class="machines-row"
        aria-label={`${props.group.environment}${props.split === null ? "" : `, ${value()}`}: ${props.group.live} live now, ${range()} live over the last 7 days, ${props.group.enrolled_24h} enrolled and ${props.group.stopped_24h} stopped in 24 hours`}
        onClick={() => show(null)}
      >
        <span>{props.group.environment}</span>
        <Show when={props.split !== null}>
          <span class={props.group.value ? "mono" : "muted"}>{value()}</span>
        </Show>
        <span class="n">{props.group.live}</span>
        <span class="n">{range()}</span>
        <span class="n">{props.group.enrolled_24h}</span>
        <span class="n">{props.group.stopped_24h}</span>
      </button>
      <Show when={props.group.went_quiet_24h > 0}>
        <button type="button" class="machines-quiet" onClick={() => show("quiet")}>
          {props.group.went_quiet_24h} went quiet
        </button>
      </Show>
    </li>
  );
}

const splitName = (value: string) => (value === ANY ? "environment only" : value);
const pickSplit = (value: string) =>
  setLook({ split: value === ANY ? null : value, machine: null });
const clearSplit = () => pickSplit(ANY);

function SplitPicker(props: { overview: Overview }) {
  const keys = createMemo(() => Object.keys(props.overview.tags));
  return (
    <Show when={keys().length > 0 || props.overview.split !== null}>
      <div class="machines-chips">
        <Chip
          label="Split by"
          value={props.overview.split ?? ANY}
          options={keys()}
          names={splitName}
          pick={pickSplit}
          clear={props.overview.split === null ? undefined : clearSplit}
        />
      </div>
    </Show>
  );
}

export function OverviewView(props: { overview: Overview }) {
  return (
    <Show
      when={props.overview.groups.length > 0}
      fallback={
        <p class="machines-empty">
          No machines are live right now. Search above to find ones that stopped.
        </p>
      }
    >
      <SplitPicker overview={props.overview} />
      <div class={`machines-rows overview${props.overview.split === null ? " whole" : ""}`}>
        <div class="machines-row head" aria-hidden="true">
          <span>Environment</span>
          <Show when={props.overview.split !== null}>
            <span>{props.overview.split}</span>
          </Show>
          <span class="n">Live now</span>
          <span class="n">Live range, 7 days</span>
          <span class="n">Enrolled, 24 h</span>
          <span class="n">Stopped, 24 h</span>
        </div>
        <ul aria-label="Groups">
          <For each={props.overview.groups}>
            {(group) => <GroupRow group={group} split={props.overview.split} />}
          </For>
        </ul>
      </div>
    </Show>
  );
}
