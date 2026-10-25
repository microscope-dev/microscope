import { useState } from "react";
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
  if (at && asked.error) {
    return (
      <section className="pane" style={{ flex: "0 1 auto" }} aria-label="Tasks" tabIndex={-1}>
        <div className="pane-head">Tasks</div>
        <div className="pane-body">
          <div className={errorClass(asked.error)}>{asked.error.message}</div>
        </div>
      </section>
    );
  }
  if (!at || !list || (list.tasks.length === 0 && list.gaps.length === 0)) {
    return null;
  }
  const noun = list.noun ?? "task";
  // A task opens at the code the program wrote, as its place says.
  const choose = (task: Task) =>
    go({ stop: at.stop, thread: 0, task: task.key, frame: task.frame?.index ?? 0 });

  return (
    <section
      className="pane"
      style={{ flex: "0 1 auto", maxHeight: "35%" }}
      aria-label="Tasks"
      tabIndex={-1}
    >
      <div className="pane-head">
        Tasks{" "}
        <span className="count">
          {list.tasks.length}
          {list.more ? "+" : ""}
        </span>
        <button
          type="button"
          className="link-button"
          aria-pressed={grouped}
          onClick={() => setGrouped(!grouped)}
        >
          {grouped ? "ungroup" : "group"}
        </button>
      </div>
      <div className="pane-body">
        <ul className="rows" data-testid="tasks">
          {list.gaps.map((gap) => (
            <li key={gap} className="row dim" title={gap}>
              some {noun}s may be missing: {gap}
            </li>
          ))}
          {grouped
            ? groups(list.tasks).map(([place, tasks]) => (
                <li key={place} className="task-group" data-testid="task-group">
                  <div className="row task-row" title={place}>
                    <span className="dim">{tasks.length} in</span>
                    <span className="fn">{tasks[0]?.frame?.name ?? (place || "no callback")}</span>
                    {tasks[0]?.frame && <span className="end">{where(tasks[0].frame)}</span>}
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
              ))
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
    </section>
  );
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
  return (
    <button
      type="button"
      className={`${row ? "row button-row task-row" : "chip"} ${shown ? "selected" : ""}`}
      aria-current={shown ? "true" : undefined}
      aria-label={`task ${task.key.number}`}
      title={describe(task)}
      onClick={() => onChoose(task)}
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
  return [...byPlace].sort(([, a], [, b]) => b.length - a.length);
}
