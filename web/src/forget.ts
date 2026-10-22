// What a tab remembers belongs to one session, and some of it to one stop,
// one connection, or one revision of the program's values. Forgetting
// follows the model, not a page: a tab in the picker while the session
// changes forgets as surely as one watching it.

import type { StoreApi } from "zustand";
import { clearConsole } from "./console";
import { type Cache, cache } from "./data";
import { latestStop } from "./follow";
import type { Model } from "./model";
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
  /** The process of a group the session shows. */
  member: number | null;
}

function marks(model: Model): Marks {
  const state = model.state;
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

/** Forgets what `after` no longer holds of what `before` did. */
export function forget(before: Marks, after: Marks, answers: Cache, values: Recall): boolean {
  if (before.session !== after.session) {
    answers.clear();
    values.clear();
    return true;
  }
  // Every answer is the selected process's, and another process numbers its
  // stops and threads apart, so an answer kept for one could stand for the
  // other's under the same key.
  if (before.member !== after.member) {
    answers.clear();
    return false;
  }
  // The files, modules, and functions a program has change as its libraries
  // load, so each stop asks again.
  if (before.latest !== after.latest) {
    answers.forget("sources ");
    answers.forget("modules ");
    answers.forget("functions ");
  }
  // What a stop holds changes when someone writes a value or memory, and
  // value handles belong to one connection.
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
    // Code is memory too, and the stack is read from registers and memory.
    for (const method of ["disassemble ", "backtrace ", "stepTargets "]) {
      answers.forget(method);
    }
  }
  // Signal policies and the exceptions that stop change without a stop, and
  // reloading views changes how values are presented and drawn.
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
  // Answers a dropped connection lost were not kept; readers ask again.
  if (!before.open && after.open) {
    answers.refresh();
  }
  return false;
}

/** Where a tab keeps a session's open files, which outlive a reload. */
const keptFiles = (session: string) => `microscope-files-${session}`;

/**
 * Forgets as `store`'s model changes, until the returned function is called,
 * and keeps the tab's open files for its session.
 */
export function forgetting(store: StoreApi<Model>): () => void {
  let before = marks(store.getState());
  const forgets = store.subscribe((model) => {
    const after = marks(model);
    if (forget(before, after, cache, recall)) {
      // Another program's files, places, and values are not this one's.
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
