import { useCallback, useState } from "react";

import { assert } from "../assert";
import { useRequest } from "../data";
import type { At } from "../focus";
import type { Task, TaskKey } from "../protocol";
import { errorClass } from "./hidden";
import { useGo } from "./navigation";
import { where } from "./Stack";
import { useFocus } from "./Workspace";

/**
 * The tasks of the program's runtimes at the stop shown, alone or grouped
 * by where they are; choosing one shows its stack. Absent for a program
 * whose runtimes have none, but a failure to list them is shown.
 */
export function Tasks() {
  const { at, stale } = useFocus();
  const go = useGo();
  const [grouped, setGrouped] = useState(false);
  // A stop's tasks are read while it lasts.
  const asked = useRequest("tasks", at && stale === null ? { stop: at.stop } : null);
  const list = asked.data;
  // A task opens at the code the program wrote, as its place says.
  const choose = useCallback(
    (task: Task) => {
      assert(at !== null, `task ${task.key.number} chosen with no stop`);
      go({ stop: at.stop, thread: 0, task: task.key, frame: task.frame?.index ?? 0 });
    },
    [at, go],
  );
  const toggleGrouped = useCallback(() => setGrouped((was) => !was), []);
  if (at && asked.error) {
    return (
      <div className="pane-body">
        <div className={errorClass(asked.error)}>{asked.error.message}</div>
      </div>
    );
  }
  if (!at || !list || (list.tasks.length === 0 && list.gaps.length === 0)) {
    return (
      <div className="pane-body">
        <div className="empty">
          {at
            ? list
              ? "This program's runtimes have no tasks at this stop."
              : "Reading the tasks…"
            : "Tasks appear when the program stops."}
        </div>
      </div>
    );
  }
  const noun = list.noun ?? "task";

  return (
    <div className="pane-body">
      <div className="pane-tools">
        <button
          type="button"
          className="link-button"
          aria-pressed={grouped}
          onClick={toggleGrouped}
        >
          {grouped ? "ungroup" : "group"}
        </button>
      </div>
      <ul className="rows" data-testid="tasks">
        {list.gaps.map((gap) => (
          <li key={gap} className="row dim" title={gap}>
            some {noun}s may be missing: {gap}
          </li>
        ))}
        {grouped
          ? groups(list.tasks).map(([place, tasks]) => {
              const first = tasks[0];
              assert(first !== undefined, "an empty group of tasks");
              return (
                <li key={place} className="task-group" data-testid="task-group">
                  <div className="row task-row" title={place}>
                    <span className="dim">{tasks.length} in</span>
                    <span className="fn">{first.frame?.name ?? (place || "no callback")}</span>
                    {first.frame && <span className="end">{where(first.frame)}</span>}
                  </div>
                  <div className="task-numbers">
                    {tasks.map((task) =>
                      // Work that runs no code opens no stack.
                      task.stackless ? (
                        <span
                          key={key(task.key)}
                          className="chip dim"
                          data-testid="stackless-task"
                          title={describe(task)}
                        >
                          {task.key.number}
                        </span>
                      ) : (
                        <TaskButton key={key(task.key)} task={task} at={at} onChoose={choose}>
                          {task.key.number}
                        </TaskButton>
                      ),
                    )}
                  </div>
                </li>
              );
            })
          : list.tasks.map((task) =>
              // Work an event loop waits on has no stack to show: it is
              // what it waits for, then the callback it will run.
              task.stackless ? (
                <li
                  key={key(task.key)}
                  className="row task-row"
                  aria-label={`task ${task.key.number}`}
                  title={describe(task)}
                >
                  <span className="dim task-number">{task.key.number}</span>
                  <span className="fn">{task.detail ?? task.state}</span>
                  {task.place && <span className="dim detail">{task.place}</span>}
                </li>
              ) : (
                <li key={key(task.key)}>
                  <TaskButton task={task} at={at} onChoose={choose} row>
                    <span className="dim task-number">{task.key.number}</span>
                    <span className="fn">{task.frame?.name ?? task.place}</span>
                    <span className="dim detail">{task.detail ?? task.state}</span>
                    {task.frame && <span className="end">{where(task.frame)}</span>}
                  </TaskButton>
                </li>
              ),
            )}
        {list.more && <li className="row dim">and more {noun}s, which this list leaves out</li>}
      </ul>
    </div>
  );
}

/** How many tasks the stop shown has, once they are read. */
export function TasksBadge() {
  const { at, stale } = useFocus();
  const list = useRequest("tasks", at && stale === null ? { stop: at.stop } : null).data;
  return list && list.tasks.length > 0 ? (
    <span className="count">
      {list.tasks.length}
      {list.more ? "+" : ""}
    </span>
  ) : null;
}

function TaskButton({
  task,
  at,
  onChoose,
  row = false,
  children,
}: {
  task: Task;
  at: At;
  onChoose: (task: Task) => void;
  row?: boolean;
  children: React.ReactNode;
}) {
  const shown = at.task?.runtime === task.key.runtime && at.task?.number === task.key.number;
  const onClick = useCallback(() => onChoose(task), [onChoose, task]);
  return (
    <button
      type="button"
      className={`${row ? "row button-row task-row" : "chip"} ${shown ? "selected" : ""}`}
      aria-current={shown ? "true" : undefined}
      aria-label={`task ${task.key.number}`}
      title={describe(task)}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

/** Everything a task's row says, for its tooltip. */
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

/** Tasks by where they are, the place with the most first. */
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
