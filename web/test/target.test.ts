// Reading the join links a page served on its own is given, and which
// microscope each tab talks to.

import { afterEach, describe, expect, it, vi } from "vitest";

import { parseJoinLink, remote, setRemote } from "../src/target";

describe("a join link", () => {
  it("names the server, its path, the token, and the page it opens", () => {
    expect(parseJoinLink(" http://127.0.0.1:7341/join#c-12ab ")).toEqual({
      server: "http://127.0.0.1:7341/",
      token: "c-12ab",
      to: "/",
    });
    expect(parseJoinLink("https://proxy.example/debug/7/join?to=%2Fs%2Fk7#v-9")).toEqual({
      server: "https://proxy.example/debug/7/",
      token: "v-9",
      to: "/s/k7",
    });
  });

  it("is refused without a token, a join path, or an http scheme", () => {
    const accepted = [
      "http://127.0.0.1:7341/join",
      "http://127.0.0.1:7341/#c-1",
      "http://127.0.0.1:7341/joined#c-1",
      "ftp://127.0.0.1/join#c-1",
      "127.0.0.1:7341",
    ].filter((text) => parseJoinLink(text) !== null);
    expect(accepted).toEqual([]);
  });
});

/** Storage as a browser keeps it, for one area. */
function area(): Storage {
  const items = new Map<string, string>();
  return {
    get length() {
      return items.size;
    },
    clear: () => items.clear(),
    getItem: (key) => items.get(key) ?? null,
    key: (index) => [...items.keys()][index] ?? null,
    removeItem: (key) => items.delete(key),
    setItem: (key, value) => items.set(key, value),
  };
}

/** Makes `tab` the one whose sessionStorage the page reads. Every tab
 * shares the browser's localStorage. */
function inTab(tab: Storage) {
  vi.stubGlobal("sessionStorage", tab);
}

describe("the microscope a tab talks to", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const here = { server: "https://jlinux1.example:8443/", token: "c-1" };
  const local = { server: "http://127.0.0.1:7341/", token: "c-2" };

  it("stays the tab's own when another tab connects elsewhere", () => {
    vi.stubGlobal("localStorage", area());
    const first = area();
    const second = area();

    inTab(first);
    setRemote(here);
    inTab(second);
    // A new tab starts with the latest choice.
    expect(remote()).toEqual(here);
    setRemote(local);

    // The first tab keeps its microscope, as it would across a reload.
    inTab(first);
    expect(remote()).toEqual(here);
    inTab(second);
    expect(remote()).toEqual(local);
    // A third tab starts with the second's, the latest.
    inTab(area());
    expect(remote()).toEqual(local);
  });

  it("keeps the first choice it saw, though another tab chooses later", () => {
    vi.stubGlobal("localStorage", area());
    const first = area();
    inTab(area());
    setRemote(here);

    inTab(first);
    expect(remote()).toEqual(here);
    inTab(area());
    setRemote(local);
    inTab(first);
    expect(remote()).toEqual(here);
  });

  it("takes a choice made later when it had none, and none once it disconnects", () => {
    vi.stubGlobal("localStorage", area());
    const waiting = area();
    inTab(waiting);
    expect(remote()).toBeNull();
    inTab(area());
    setRemote(here);
    inTab(waiting);
    expect(remote()).toEqual(here);

    setRemote(null);
    expect(remote()).toBeNull();
    // A new tab starts with nothing, the latest choice.
    inTab(area());
    expect(remote()).toBeNull();
  });
});
