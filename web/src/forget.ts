import { assert } from "./assert";
import { clearConsole } from "./console";
import { type Cache, cache } from "./data";
import { latestStop } from "./follow";
import type { Model } from "./model";
import type { Store } from "./state";
import { read, write } from "./storage";
import { initialTab, isKeptFiles, type KeptFiles, tab } from "./tab";
import { type Recall, recall } from "./tree";

export interface Marks {
  session: string | null;
  latest: number | undefined;
  writes: number | undefined;
  settings: number | undefined;
  connection: number | undefined;
  open: boolean;

  member: number | null;
}

function marks(model: Model): Marks {
  const state = model.state;
  assert(model.link !== "open" || model.hello !== null, "the link is open with no hello");
  return {
    session: state?.session ?? null,
    latest: state ? latestStop(state) : undefined,
    writes: state?.writes,
    settings: state?.settings,
    connection: model.hello?.connection,
    open: model.link === "open",
    member: state?.group?.selected ?? null,
  };
}

/**
 * Selectively invalidates cached RPC answers and remembered value strings
 * (used for change highlighting) based on what changed between two debugger
 * state snapshots (`before` -> `after`).
 *
 * Returns `true` only when the session itself changed so callers can reset
 * per-session tab state and console output.
 */
export function forget(before: Marks, after: Marks, answers: Cache, values: Recall): boolean {
  if (before.session !== after.session) {
    answers.clear();
    values.clear();
    return true;
  }

  if (before.member !== after.member) {
    answers.clear();
    return false;
  }

  if (before.latest !== after.latest) {
    answers.forget("sources ");
    answers.forget("modules ");
    answers.forget("functions ");
  }

  if (before.writes !== after.writes || before.connection !== after.connection) {
    for (const method of [
      "scopes ",
      "children ",
      "evaluate ",
      "readMemory ",
      "registers ",
      "tasks ",
      "draw ",
    ]) {
      answers.forget(method);
    }
  }
  if (before.writes !== after.writes) {
    for (const method of ["disassemble ", "backtrace ", "stepTargets "]) {
      answers.forget(method);
    }
  }

  if (before.settings !== after.settings) {
    for (const method of [
      "signals ",
      "exceptions ",
      "scopes ",
      "children ",
      "evaluate ",
      "draw ",
      "renderers ",
    ]) {
      answers.forget(method);
    }
  }

  if (!before.open && after.open) {
    answers.refresh();
  }
  return false;
}

const keptFiles = (session: string) => `microscope-files-${session}`;

export function forgetting(store: Store<Model>): () => void {
  let before = marks(store.getState());
  const forgets = store.subscribe((model) => {
    const after = marks(model);
    if (forget(before, after, cache, recall)) {
      const { cursor, shown, editing, pinned, frames } = initialTab;
      const empty: KeptFiles = { files: initialTab.files, places: initialTab.places };
      const { files, places } = after.session
        ? read(keptFiles(after.session), empty, isKeptFiles, "session")
        : empty;
      tab.setState({ files, places, cursor, shown, editing, pinned, frames });
      clearConsole();
    }
    before = after;
  });
  const keeps = tab.subscribe(({ files, places }, previous) => {
    const session = store.getState().state?.session;
    if (session && (files !== previous.files || places !== previous.places)) {
      write(keptFiles(session), { files, places }, "session");
    }
  });
  return () => {
    forgets();
    keeps();
  };
}
