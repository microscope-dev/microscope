import * as fc from "fast-check";
import { describe, expect, it } from "vitest";

import {
  ago,
  clearFilters,
  day,
  filtered,
  HOME,
  type Look,
  lookHash,
  memory,
  parseLook,
  parseTags,
  sharedText,
  shownState,
  snippet,
  tagText,
  type View,
} from "../src/fleet";

const VIEWS: readonly View[] = ["overview", "list", "keys", "add", "newkey"];

const text = fc.string({ unit: "grapheme", maxLength: 24 });
const name = fc.option(
  text.filter((value) => value !== ""),
  { nil: null },
);

const tagKey = fc
  .stringMatching(/^[A-Za-z0-9._\-/]{1,63}$/u)
  .filter((key) => key !== "environment");
const tagValue = fc
  .string({ unit: "grapheme", maxLength: 24 })
  .filter((value) => !value.includes(","));

const looks: fc.Arbitrary<Look> = fc
  .option(fc.integer({ min: 1, max: 2 ** 40 }), { nil: null })
  .chain((machine) =>
    fc.record({
      view: machine === null ? fc.constantFrom(...VIEWS) : fc.constant<View>("list"),
      org: fc.option(fc.integer({ min: 1, max: 2 ** 40 }), { nil: null }),
      q: text,
      state: fc.constantFrom(null, "live", "quiet", "stopped", "revoked", "any"),
      environment: name,
      tags: fc.dictionary(tagKey, tagValue, { maxKeys: 4 }),
      version: name,
      key: fc.option(fc.integer({ min: 1, max: 2 ** 40 }), { nil: null }),
      split: name,
      machine: fc.constant(machine),
    }),
  );

function rank(words: string): number {
  if (words === "just now") {
    return 0;
  }
  if (words === "yesterday") {
    return 86_400;
  }
  const [count = "0", unit = ""] = words.split(" ");
  const scale: Record<string, number> = { s: 1, min: 60, hour: 3600, hours: 3600, days: 86_400 };
  return Number(count) * (scale[unit] ?? 0);
}

describe("the machines window in the URL", () => {
  it("reads back every look it writes", () => {
    fc.assert(
      fc.property(looks, (look) => {
        expect(parseLook(`#${lookHash(look)}`)).toEqual(look);
      }),
    );
  });

  it("writes the links the plan promises", () => {
    expect(lookHash(HOME)).toBe("machines");
    expect(lookHash({ ...HOME, view: "list", q: "10.0.3", state: "any" })).toBe(
      "machines/list?q=10.0.3&state=any",
    );
    expect(lookHash({ ...HOME, view: "list", machine: 81723 })).toBe("machines/m/81723");
    expect(
      lookHash({ ...HOME, view: "list", org: 7, environment: "staging", version: "0.4.1" }),
    ).toBe("machines/list?org=7&environment=staging&version=0.4.1");
    expect(lookHash({ ...HOME, view: "newkey" })).toBe("machines/keys/new");
    expect(
      lookHash({ ...HOME, view: "list", environment: "prod", tags: { cluster: "eu1", team: "" } }),
    ).toBe("machines/list?environment=prod&tags=cluster%3Deu1%2Cteam%3D");
    expect(lookHash({ ...HOME, split: "cluster" })).toBe("machines?split=cluster");
  });

  it("ignores fragments that are not the window's and values it does not know", () => {
    for (const hash of [
      "",
      "#",
      "#machine",
      "#machines/",
      "#machines/m/0",
      "#machines/m/x",
      "#other",
    ]) {
      expect(parseLook(hash)).toBeNull();
    }
    expect(parseLook("#machines/list?state=asleep&org=-3&environment=&split=")).toEqual({
      ...HOME,
      view: "list",
    });
    expect(parseLook("#machines/list?tags=environment%3Dprod%2C%3Dx%2Cbare%2Cteam%3Dweb")).toEqual({
      ...HOME,
      view: "list",
      tags: { team: "web" },
    });
  });

  it("browses live machines and searches every state unless a chip says otherwise", () => {
    expect(shownState(HOME)).toBe("live");
    expect(shownState({ ...HOME, q: "db-03" })).toBe("any");
    expect(shownState({ ...HOME, q: "  " })).toBe("live");
    expect(shownState({ ...HOME, q: "db-03", state: "live" })).toBe("live");
    const narrowed = { ...HOME, q: "db", state: "stopped" as const, tags: { cluster: "eu1" } };
    expect(filtered({ ...HOME, tags: { cluster: "" } })).toBe(true);
    expect(filtered(narrowed)).toBe(true);
    expect(clearFilters(narrowed)).toEqual({ ...HOME, q: "db" });
    expect(filtered(clearFilters(narrowed))).toBe(false);
  });
});

describe("words for times and sizes", () => {
  const now = 1_795_000_000;

  it("says how long ago in the units a person would", () => {
    expect(ago(now, now)).toBe("just now");
    expect(ago(now + 30, now)).toBe("just now");
    expect(ago(now - 40, now)).toBe("40 s ago");
    expect(ago(now - 125, now)).toBe("2 min ago");
    expect(ago(now - 3_600, now)).toBe("1 hour ago");
    expect(ago(now - 5 * 3_600, now)).toBe("5 hours ago");
    expect(ago(now - 86_400, now)).toBe("yesterday");
    expect(ago(now - 3 * 86_400, now)).toBe("3 days ago");
    expect(ago(1_762_041_600, now)).toBe("2 Nov 2025");
  });

  it("never goes backwards as time passes", () => {
    fc.assert(
      fc.property(
        fc.integer({ min: 0, max: 58 * 86_400 }),
        fc.integer({ min: 0, max: 86_400 }),
        (age, more) => {
          expect(rank(ago(now - age - more, now))).toBeGreaterThanOrEqual(
            rank(ago(now - age, now)),
          );
        },
      ),
    );
  });

  it("writes dates in UTC and memory in GB from a gigabyte up", () => {
    expect(day(0)).toBe("1 Jan 1970");
    expect(day(1_802_649_599)).toBe("14 Feb 2027");
    expect(memory(512)).toBe("512 MB");
    expect(memory(32_768)).toBe("32 GB");
  });

  it("tells one machine with several agents from copies of one image", () => {
    expect(sharedText({ kind: "same_machine", source: "smbios-uuid", live_machines: 1 })).toBe(
      "One other live agent reports the same smbios-uuid, so it runs on this machine too.",
    );
    expect(sharedText({ kind: "same_machine", source: "smbios-uuid", live_machines: 2 })).toBe(
      "2 other live agents report the same smbios-uuid, so they run on this machine too.",
    );
    expect(sharedText({ kind: "same_image", source: "machine-id", live_machines: 1 })).toBe(
      "One other live machine has the same machine-id. They may be copies of one disk image that was not emptied before capture.",
    );
    expect(sharedText({ kind: "same_image", source: "machine-id", live_machines: 40 })).toBe(
      "40 other live machines have the same machine-id. They may be copies of one disk image that was not emptied before capture.",
    );
  });
});

describe("tags typed into Add machines", () => {
  it("reads key=value pairs split by commas", () => {
    expect(parseTags("")).toEqual({});
    expect(parseTags(" team = payments ,cluster=eu1,")).toEqual({
      team: "payments",
      cluster: "eu1",
    });
    expect(parseTags("url=a=b")).toEqual({ url: "a=b" });
    expect(parseTags("k8s.io/zone=b")).toEqual({ "k8s.io/zone": "b" });
  });

  it("names the pair that is wrong", () => {
    expect(parseTags("team")).toBe("write team as key=value");
    expect(parseTags("=x")).toBe("write =x as key=value");
    expect(parseTags("team=")).toBe("write team= as key=value");
    expect(parseTags("my team=x")).toBe("my team can only use letters, digits, and . _ - /");
    expect(parseTags("environment=prod")).toBe("set the environment in its own field");
    expect(parseTags("a=1, a=2")).toBe("a is set twice");
  });

  it("shows every tag but the environment", () => {
    expect(tagText({ environment: "prod", cluster: "eu1", team: "web" })).toBe(
      "cluster=eu1, team=web",
    );
    expect(tagText({ environment: "prod" })).toBe("");
  });
});

describe("install snippets", () => {
  const url = "https://mse_4gq7aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaasnb47oni@microscope.dev";

  it("carry the enroll URL the agent reads", () => {
    expect(snippet("systemd", url)).toContain(`enroll = "${url}"`);
    expect(snippet("docker", url)).toContain(`-e MICROSCOPE_ENROLL='${url}'`);
    expect(snippet("kubernetes", url)).toContain(`--from-literal=enroll='${url}'`);
  });

  it("show where the cluster tag goes", () => {
    expect(snippet("systemd", url)).toContain('[tags]\ncluster = "your-cluster-name"');
    expect(snippet("docker", url)).toContain("-e MICROSCOPE_TAGS=cluster=your-cluster-name");
    expect(snippet("kubernetes", url)).toContain("--set tags.cluster=your-cluster-name");
  });

  it("quote anything a shell or TOML would read differently", () => {
    const odd = `https://it's "odd"@host`;
    expect(snippet("docker", odd)).toContain(`MICROSCOPE_ENROLL='https://it'\\''s "odd"@host'`);
    expect(snippet("systemd", odd)).toContain(`enroll = "https://it's \\"odd\\"@host"`);
  });
});
