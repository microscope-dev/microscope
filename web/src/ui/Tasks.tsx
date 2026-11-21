import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { assert } from "../assert";
import { useRequest } from "../data";
import type { At } from "../focus";
import type { Task, TaskKey } from "../protocol";
import { errorClass } from "./hidden";
import { useGo } from "./navigation";
import { where } from "./Stack";
import { useFocus } from "./Workspace";

export function Tasks() {
  const focus = useFocus();
  const go = useGo();
  const [grouped, setGrouped] = createSignal(false);

  const asked = useRequest("tasks", () => {
    const { at, stale } = focus();
    return at && stale === null ? { stop: at.stop } : null;
  });
  const list = createMemo(() => asked().data);
  const ready = createMemo(() => {
    const currentAt = focus().at;
    const currentList = list();
    return currentAt && currentList && (currentList.tasks.length > 0 || currentList.gaps.length > 0)
      ? { at: currentAt, list: currentList }
      : null;
  });
  const noun = createMemo(() => list()?.noun ?? "task");
  const groupedTasks = createMemo(() => {
    const currentList = list();
    return currentList ? groups(currentList.tasks) : [];
  });

  const choose = (task: Task) => {
    const currentAt = focus().at;
    assert(currentAt !== null, `task ${task.key.number} chosen with no stop`);
    go({ stop: currentAt.stop, thread: 0, task: task.key, frame: task.frame?.index ?? 0 });
  };
  const toggleGrouped = () => setGrouped((was) => !was);

  return (
    <Switch
      fallback={
        <div class="pane-body">
          <div class="empty">
            {focus().at
              ? list()
                ? "This program's runtimes have no tasks at this stop."
                : "Reading the tasks…"
              : "Tasks appear when the program stops."}
          </div>
        </div>
      }
    >
      <Match when={focus().at ? asked().error : undefined}>
        {(err) => (
          <div class="pane-body">
            <div class={errorClass(err())}>{err().message}</div>
          </div>
        )}
      </Match>
      <Match when={ready()}>
        {(data) => (
          <div class="pane-body">
            <div class="pane-tools">
              <button
                type="button"
                class="link-button"
                aria-pressed={grouped() ? "true" : "false"}
                onClick={toggleGrouped}
              >
                {grouped() ? "ungroup" : "group"}
              </button>
            </div>
            <ul class="rows" data-testid="tasks">
              <For each={data().list.gaps}>
                {(gap) => (
                  <li class="row dim" title={gap}>
                    some {noun()}s may be missing: {gap}
                  </li>
                )}
              </For>
              <Show
                when={grouped()}
                fallback={
                  <For each={data().list.tasks} keyed={(task) => key(task.key)}>
                    {(task) => (
                      <Show
                        when={task().stackless}
                        fallback={
                          <li>
                            <TaskButton task={task()} at={data().at} onChoose={choose} row>
                              <span class="dim task-number">{task().key.number}</span>
                              <span class="fn">{task().frame?.name ?? task().place}</span>
                              <span class="dim detail">{task().detail ?? task().state}</span>
                              <Show when={task().frame}>
                                {(frame) => <span class="end">{where(frame())}</span>}
                              </Show>
                            </TaskButton>
                          </li>
                        }
                      >
                        <li
                          class="row task-row"
                          aria-label={`task ${task().key.number}`}
                          title={describe(task())}
                        >
                          <span class="dim task-number">{task().key.number}</span>
                          <span class="fn">{task().detail ?? task().state}</span>
                          <Show when={task().place}>
                            {(place) => <span class="dim detail">{place()}</span>}
                          </Show>
                        </li>
                      </Show>
                    )}
                  </For>
                }
              >
                <For each={groupedTasks()} keyed={([place]) => place}>
                  {(group) => {
                    const place = createMemo(() => group()[0]);
                    const tasks = createMemo(() => group()[1]);
                    const first = createMemo(() => {
                      const head = tasks()[0];
                      assert(head !== undefined, "an empty group of tasks");
                      return head;
                    });
                    return (
                      <li class="task-group" data-testid="task-group">
                        <div class="row task-row" title={place()}>
                          <span class="dim">{tasks().length} in</span>
                          <span class="fn">
                            {first().frame?.name ?? (place() || "no callback")}
                          </span>
                          <Show when={first().frame}>
                            {(frame) => <span class="end">{where(frame())}</span>}
                          </Show>
                        </div>
                        <div class="task-numbers">
                          <For each={tasks()} keyed={(task) => key(task.key)}>
                            {(task) => (
                              <Show
                                when={task().stackless}
                                fallback={
                                  <TaskButton task={task()} at={data().at} onChoose={choose}>
                                    {task().key.number}
                                  </TaskButton>
                                }
                              >
                                <span
                                  class="chip dim"
                                  data-testid="stackless-task"
                                  title={describe(task())}
                                >
                                  {task().key.number}
                                </span>
                              </Show>
                            )}
                          </For>
                        </div>
                      </li>
                    );
                  }}
                </For>
              </Show>
              <Show when={data().list.more}>
                <li class="row dim">and more {noun()}s, which this list leaves out</li>
              </Show>
            </ul>
          </div>
        )}
      </Match>
    </Switch>
  );
}

export function TasksBadge() {
  const focus = useFocus();
  const asked = useRequest("tasks", () => {
    const { at, stale } = focus();
    return at && stale === null ? { stop: at.stop } : null;
  });
  const list = createMemo(() => {
    const data = asked().data;
    return data && data.tasks.length > 0 ? data : null;
  });
  return (
    <Show when={list()}>
      {(data) => (
        <span class="count">
          {data().tasks.length}
          {data().more ? "+" : ""}
        </span>
      )}
    </Show>
  );
}

function TaskButton(props: {
  task: Task;
  at: At;
  onChoose: (task: Task) => void;
  row?: boolean;
  children: JSX.Element;
}) {
  const shown = createMemo(
    () =>
      props.at.task?.runtime === props.task.key.runtime &&
      props.at.task?.number === props.task.key.number,
  );
  return (
    <button
      type="button"
      class={[props.row ? "row button-row task-row" : "chip", { selected: shown() }]}
      aria-current={shown() ? "true" : undefined}
      aria-label={`task ${props.task.key.number}`}
      title={describe(props.task)}
      onClick={() => props.onChoose(props.task)}
    >
      {props.children}
    </button>
  );
}

function describe(task: Task): string {
  return [
    task.stackless
      ? `task ${task.key.number}${task.place ? `, ${task.place}` : ""}`
      : `task ${task.key.number} in ${task.place}`,
    task.detail ?? task.state,
    task.labels,
    task.thread === null ? null : `on thread ${task.thread}`,
  ]
    .filter((part) => part !== null)
    .join(" · ");
}

function key(task: TaskKey): string {
  return `${task.runtime}.${task.number}`;
}

function groups(tasks: readonly Task[]): [string, Task[]][] {
  const byPlace = new Map<string, Task[]>();
  for (const task of tasks) {
    const same = byPlace.get(task.place);
    if (same) {
      same.push(task);
    } else {
      byPlace.set(task.place, [task]);
    }
  }
  const sorted = [...byPlace].toSorted(([, a], [, b]) => b.length - a.length);
  assert(
    sorted.reduce((sum, [, group]) => sum + group.length, 0) === tasks.length,
    `${tasks.length} tasks grouped into ${sorted.length} places lost some`,
  );
  return sorted;
}
