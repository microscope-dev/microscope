// What the page for an ended session says about how it ended.

import { describe, expect, it } from "vitest";
import type { Note } from "../src/model";
import { endedNotice } from "../src/ui/pages";

const note = (text: string, at: number): Note => ({ connection: 1, name: "ada", text, at });

describe("an ended session's card", () => {
  const notices = [
    note("ended the kvstore session", 1),
    note("launched /bin/cat", 2),
    note("ended the cat session, but no such file", 3),
    note("ended", 4),
  ];

  it("tells how the session the tab saw ended, not the latest one", () => {
    expect(endedNotice(notices, "kvstore")?.at).toBe(1);
    expect(endedNotice(notices, "cat")?.at).toBe(3);
  });

  it("says nothing of a session the tab never saw", () => {
    expect(endedNotice(notices, null)).toBeUndefined();
    expect(endedNotice(notices, "kv")).toBeUndefined();
  });
});
