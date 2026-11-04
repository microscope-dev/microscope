// One pane's body, given what it needs: when a need is missing it says why
// instead, and when the body throws, it shows the error and the rest of the
// workbench keeps running.

import { Component, memo, type ReactNode } from "react";

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

export const PaneHost = memo(function PaneHost({ id, pane, visible }: Props) {
  const { kinds, useMissing } = useDock();
  const kind = kinds.get(pane.kind);
  // oxlint-disable-next-line react/hooks -- the dock hands down one hook for its whole life
  const missing = useMissing(kind.needs);
  const Body = kind.Body;
  return (
    <VisibleContext value={visible}>
      {missing === null ? (
        <Boundary title={kind.title(pane)}>
          <Body id={id} pane={pane} />
        </Boundary>
      ) : (
        <div className={`empty ${css("pane-missing")}`}>{missing}</div>
      )}
    </VisibleContext>
  );
});

class Boundary extends Component<
  { title: string; children: ReactNode },
  { failure: Error | null }
> {
  override state: { failure: Error | null } = { failure: null };

  readonly retry = () => this.setState({ failure: null });

  static getDerivedStateFromError(failure: unknown): { failure: Error } {
    return { failure: failure instanceof Error ? failure : new Error(String(failure)) };
  }

  override render(): ReactNode {
    const { failure } = this.state;
    if (!failure) {
      return this.props.children;
    }
    return (
      <div className={css("pane-failed")} role="alert" data-part="pane-failed">
        <b>{this.props.title} stopped working.</b> {failure.message}
        <div>
          <button type="button" className="button small" onClick={this.retry}>
            Try again
          </button>
        </div>
      </div>
    );
  }
}
