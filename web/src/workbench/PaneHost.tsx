import { Dynamic } from "@solidjs/web";
import { createMemo, Errored, Show } from "solid-js";

import { classOf } from "./classes";
import { useDock } from "./context";
import type { Pane, PaneId } from "./layout";
import { VisibleContext } from "./visible";

import styles from "./PaneHost.module.css";

const css = classOf(styles);

interface Props {
  readonly id: PaneId;
  readonly pane: Pane;
  readonly visible: boolean;
}

export function PaneHost(props: Props) {
  const { kinds, useMissing } = useDock();
  const kind = createMemo(() => kinds.get(props.pane.kind));
  const missing = useMissing(
    () => kind().needs,
    () => props.pane,
  );
  return (
    <VisibleContext value={() => props.visible}>
      <Show
        when={missing() === null}
        fallback={<div class={["empty", css("pane-missing")]}>{missing()}</div>}
      >
        <Errored
          fallback={(err, reset) => {
            const message = createMemo(() => {
              const failure = err();
              return failure instanceof Error ? failure.message : String(failure);
            });
            return (
              <div class={css("pane-failed")} role="alert" data-part="pane-failed">
                <b>{kind().title(props.pane)} stopped working.</b> {message()}
                <div>
                  <button type="button" class="button small" onClick={reset}>
                    Try again
                  </button>
                </div>
              </div>
            );
          }}
        >
          <Dynamic component={kind().Body} id={props.id} pane={props.pane} />
        </Errored>
      </Show>
    </VisibleContext>
  );
}
