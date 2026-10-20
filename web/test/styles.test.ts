// Dark mode is written twice, once for the system setting and once for the
// chosen theme, so the two copies must not drift apart.

import { readFileSync } from "node:fs";
import { expect, it } from "vitest";

const css = readFileSync("src/styles.css", "utf8");

/** The custom properties declared in the first rule that `selector` opens. */
function declared(selector: string): Map<string, string> {
  const start = css.indexOf(`${selector} {`);
  expect(start, selector).toBeGreaterThanOrEqual(0);
  const body = css.slice(start, css.indexOf("}", start));
  const properties = new Map<string, string>();
  for (const [, name, value] of body.matchAll(/(--[\w-]+):\s*([^;]+);/g)) {
    if (name !== undefined && value !== undefined) {
      properties.set(name, value.trim());
    }
  }
  return properties;
}

const light = declared(":root");
const system = declared(':root:not([data-theme="light"])');
const chosen = declared(':root[data-theme="dark"]');

it("writes both dark blocks the same", () => {
  expect(system.size).toBeGreaterThan(0);
  expect(Object.fromEntries(system)).toEqual(Object.fromEntries(chosen));
});

it("gives every color the dark theme changes a light value", () => {
  const missing = [...chosen.keys()].filter((name) => !light.has(name));
  expect(missing).toEqual([]);
});

it("declares every property the rules use", () => {
  const used = new Set([...css.matchAll(/var\((--[\w-]+)/g)].map((match) => match[1]));
  const missing = [...used].filter((name) => name !== undefined && !light.has(name));
  expect(missing).toEqual([]);
});
