// Which modules may import which. The page is layered: state and logic below,
// views above, entry points on top, and the tests outside it all. An import
// that crosses a layer the wrong way fails here, by file and line, before it
// can grow into a tangle.

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";

import { describe, expect, it } from "vitest";

import { FILES, OWN } from "./budget";

const web = join(import.meta.dirname, "..");

interface Rule {
  /** Files the rule applies to, as paths from web/. */
  from: RegExp;
  /** Paths from web/ those files may not import. */
  to: RegExp;
  /** Files the rule leaves out, as paths from web/. */
  except?: RegExp;
  why: string;
}

const RULES: readonly Rule[] = [
  {
    from: /^src\//u,
    to: /^(test|e2e)\//u,
    why: "the page never depends on its tests",
  },
  {
    from: /^src\//u,
    to: /^src\/ui\//u,
    except: /^src\/(ui\/|router\.tsx$|main\.tsx$)/u,
    why: "state and logic sit below the views, which only the entry points assemble",
  },
  {
    from: /^src\/workbench\//u,
    to: /^src\/(?!workbench\/|assert$)/u,
    why: "the workbench is the frame every pane builds on, so it knows nothing of what panes show",
  },
];

function files(directory: string): string[] {
  return readdirSync(join(web, directory), { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      return files(path);
    }
    return /\.(ts|tsx)$/u.test(entry.name) && !entry.name.endsWith(".gen.ts") ? [path] : [];
  });
}

/** Every relative import in `file`, as a path from web/, with its line. */
function imports(file: string): { target: string; line: number }[] {
  const found: { target: string; line: number }[] = [];
  const pattern = /(?:\bfrom\s*|\bimport\s*\(?\s*)["'](\.{1,2}\/[^"']+)["']/gu;
  for (const [index, text] of readFileSync(join(web, file), "utf8").split("\n").entries()) {
    for (const match of text.matchAll(pattern)) {
      const specifier = match[1] as string;
      found.push({
        target: relative(web, resolve(web, dirname(file), specifier)),
        line: index + 1,
      });
    }
  }
  return found;
}

describe("module boundaries", () => {
  const all = [...files("src"), ...files("test"), ...files("e2e")];

  it("finds the modules it checks", () => {
    expect(all).toContain("src/store.ts");
    expect(imports("src/router.tsx").map((found) => found.target)).toContain("src/ui/Frame");
  });

  it.each(RULES)("$why", (rule) => {
    const crossings = all
      .filter((file) => rule.from.test(file) && !rule.except?.test(file))
      .flatMap((file) =>
        imports(file)
          .filter((found) => rule.to.test(found.target))
          .map((found) => `${file}:${found.line} imports ${found.target}`),
      );
    expect(crossings).toEqual([]);
  });
});

describe("the speed budget", () => {
  it("names files and title segments that exist", () => {
    const unnamed = Object.keys(OWN).flatMap((name) => {
      const [file, title] = name.split(" :: ") as [string, string];
      const source = readFileSync(join(web, file), "utf8");
      return title
        .split(" > ")
        .filter((segment) => !source.includes(JSON.stringify(segment)))
        .map((segment) => `${name}: ${segment}`);
    });
    expect(unnamed).toEqual([]);
    const missing = Object.keys(FILES).filter(
      (file) => !readdirSync(join(web, dirname(file))).includes(file.split("/").at(-1) ?? ""),
    );
    expect(missing).toEqual([]);
  });
});
