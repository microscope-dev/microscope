import { describe, expect, it } from "vitest";

import { initialTab, openFile, shownAt, tab } from "../src/tab";

describe("open files", () => {
  it("make room by closing the file shown longest ago, keeping the tabs' order", () => {
    tab.setState(initialTab);
    const names = ["a", "b", "c", "d", "e", "f", "g", "h"];
    for (const name of names) {
      openFile(name);
      shownAt(name, `${name}:1`);
    }
    shownAt("a", "a:3");
    openFile("i");
    expect(tab.getState().files).toEqual(["a", "c", "d", "e", "f", "g", "h", "i"]);
    expect(tab.getState().places.a).toBe("a:3");
  });
});
