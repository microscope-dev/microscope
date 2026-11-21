import { createSignal, For, Match, Show, Switch } from "solid-js";

import { useRequest } from "../data";
import { controls } from "../model";
import type { SignalPolicy } from "../protocol";
import { useConnection, useModel } from "../store";

type Field = "stop" | "print" | "pass";

const FIELDS: readonly Field[] = ["stop", "print", "pass"];

export function Signals() {
  const session = useModel((model) => model.state?.session);
  const signals = useRequest("signals", () => (session() ? undefined : null));
  const control = useModel(controls);
  const connection = useConnection();
  const [error, setError] = createSignal<string | null>(null);
  const change = (policy: SignalPolicy, field: Field, on: boolean) => {
    setError(null);
    connection
      .request("setSignal", { ...policy, [field]: on })
      .catch((failure: Error) => setError(failure.message));
  };
  return (
    <Switch fallback={<div class="empty">Reading signal policies…</div>}>
      <Match when={!session()}>
        <div class="empty">Signals apply once something is being debugged.</div>
      </Match>
      <Match when={signals().error}>
        {(err) => <div class="empty error">{err().message}</div>}
      </Match>
      <Match when={signals().data}>
        {(data) => (
          <div class="pane-body table-body">
            <Show when={error()}>{(message) => <div class="error">{message()}</div>}</Show>
            <table class="table" data-testid="signals">
              <thead>
                <tr>
                  <th>Signal</th>
                  <th>Number</th>
                  <th>Stop</th>
                  <th>Print</th>
                  <th>Pass</th>
                </tr>
              </thead>
              <tbody>
                <For each={data().signals} keyed={(policy) => policy.signal}>
                  {(policy) => (
                    <tr>
                      <td>{policy().name}</td>
                      <td class="dim">{policy().signal}</td>
                      <For each={FIELDS}>
                        {(field) => (
                          <td>
                            <PolicyBox
                              policy={policy()}
                              field={field}
                              control={control()}
                              onChange={change}
                            />
                          </td>
                        )}
                      </For>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
        )}
      </Match>
    </Switch>
  );
}

export function Modules() {
  const session = useModel((model) => model.state?.session);
  const modules = useRequest("modules", () => (session() ? undefined : null));
  return (
    <Switch
      fallback={<div class="empty">{session() ? "Reading modules…" : "Nothing is loaded."}</div>}
    >
      <Match when={modules().error}>
        {(err) => <div class="empty error">{err().message}</div>}
      </Match>
      <Match when={modules().data}>
        {(data) => (
          <Show
            when={data().modules.length > 0}
            fallback={<div class="empty">Modules appear once the program runs.</div>}
          >
            <div class="pane-body table-body">
              <table class="table" data-testid="modules">
                <thead>
                  <tr>
                    <th>Module</th>
                    <th>Start</th>
                    <th>End</th>
                    <th>Symbols</th>
                    <th>Path</th>
                    <th>Debug file</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={data().modules} keyed={(module) => module.id}>
                    {(module) => (
                      <tr>
                        <td>{module().name}</td>
                        <Bound value={module().start} />
                        <Bound value={module().end} />
                        <td
                          class={
                            module().debugInformationProblem
                              ? "error"
                              : module().symbols === "none"
                                ? "dim"
                                : ""
                          }
                          title={module().debugInformationProblem ?? undefined}
                        >
                          {module().symbols}
                        </td>
                        <td class="dim" title={module().path}>
                          {module().path}
                        </td>
                        <td
                          class={module().debugFileProblem ? "error" : "dim"}
                          title={
                            module().debugFileProblem
                              ? `cannot use ${module().debugFile}: ${module().debugFileProblem}`
                              : (module().debugFile ?? undefined)
                          }
                        >
                          {module().debugFile ?? "none"}
                        </td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </div>
          </Show>
        )}
      </Match>
    </Switch>
  );
}

function Bound(props: { value: string | null }) {
  return (
    <Show
      when={props.value !== null}
      fallback={
        <td class="dim" title="microscope could not read this module's file">
          unknown
        </td>
      }
    >
      <td class="mono">{props.value}</td>
    </Show>
  );
}

function PolicyBox(props: {
  policy: SignalPolicy;
  field: Field;
  control: boolean;
  onChange: (policy: SignalPolicy, field: Field, on: boolean) => void;
}) {
  return (
    <input
      type="checkbox"
      aria-label={`${props.field[0]?.toUpperCase()}${props.field.slice(1)} on ${props.policy.name}`}
      checked={props.policy[props.field]}
      disabled={!props.control || (props.field === "print" && props.policy.stop)}
      onChange={(event) => props.onChange(props.policy, props.field, event.currentTarget.checked)}
    />
  );
}
