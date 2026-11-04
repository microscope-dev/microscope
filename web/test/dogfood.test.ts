// The built-in renderers use only the API every renderer has: the page
// never names one, so it cannot treat one specially.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { expect, it } from "vitest";

const builtIns = readdirSync("../views/visualizers")
  .filter((file) => file.endsWith(".js"))
  .map((file) => file.slice(0, -3));

function files(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory() ? files(join(directory, entry.name)) : [join(directory, entry.name)],
  );
}

it("knows the eleven built-ins", () => {
  expect(builtIns.toSorted()).toEqual([
    "bar-chart",
    "bitmap",
    "bits",
    "box-plot",
    "donut-chart",
    "flame-graph",
    "heatmap",
    "histogram",
    "line-plot",
    "mesh",
    "scatter-plot",
  ]);
});

it("names no built-in anywhere in the page", () => {
  const named = files("src").flatMap((file) => {
    const text = readFileSync(file, "utf8");
    return builtIns
      .filter((name) => new RegExp(`["'\`]${name}["'\`]`, "u").test(text))
      .map((name) => `${file} names ${name}`);
  });
  expect(named).toEqual([]);
});
