// Reading the join links a page served on its own is given.

import { describe, expect, it } from "vitest";
import { parseJoinLink } from "../src/target";

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
    for (const text of [
      "http://127.0.0.1:7341/join",
      "http://127.0.0.1:7341/#c-1",
      "http://127.0.0.1:7341/joined#c-1",
      "ftp://127.0.0.1/join#c-1",
      "127.0.0.1:7341",
    ]) {
      expect(parseJoinLink(text), text).toBeNull();
    }
  });
});
