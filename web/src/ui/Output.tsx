// The program's output, what logpoints wrote, and the program's input.

import { useState } from "react";

import { controls } from "../model";
import { useConnection, useModel } from "../store";
import { useFollowBottom } from "./scroll";

export function OutputBadge() {
  const bytes = useModel((model) => model.outputBytes);
  return bytes > 0 ? <span className="count"> {bytes.toLocaleString()} bytes</span> : null;
}

/** What the program wrote, and the line it reads next. */
export function Output() {
  return (
    <div className="output-pane">
      <Written />
      <Input />
    </div>
  );
}

function Written() {
  const output = useModel((model) => model.output);
  const body = useFollowBottom<HTMLDivElement>(output);

  return (
    <div className="pane-body" ref={body.ref} onScroll={body.onScroll}>
      {output.length === 0 ? (
        <div className="empty">The program's output appears here.</div>
      ) : (
        <pre className="output" data-testid="output">
          {output.map((piece) => (
            <span key={piece.seq} className={piece.stream}>
              {piece.text}
            </span>
          ))}
        </pre>
      )}
    </div>
  );
}

/** A line typed here is the program's next line of input. */
function Input() {
  const control = useModel(controls);
  const running = useModel(
    (model) =>
      model.state?.inferior.state === "running" || model.state?.inferior.state === "stopped",
  );
  const connection = useConnection();
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  if (!control || !running) {
    return null;
  }
  const send = (eof: boolean) => {
    setError(null);
    connection
      .request("input", { text: eof ? text : `${text}\n`, eof })
      .then(() => setText(""))
      .catch((failure: Error) => setError(failure.message));
  };
  return (
    <form
      className="program-input"
      onSubmit={(event) => {
        event.preventDefault();
        send(false);
      }}
    >
      <span className="prompt" aria-hidden="true">
        ›
      </span>
      <input
        className="input quiet"
        aria-label="Program input"
        placeholder="input for the program; Enter sends a line, Ctrl+D ends its input"
        value={text}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "d" && event.ctrlKey) {
            event.preventDefault();
            send(true);
          }
        }}
      />
      {error && (
        <span className="error" title={error}>
          {error}
        </span>
      )}
    </form>
  );
}
