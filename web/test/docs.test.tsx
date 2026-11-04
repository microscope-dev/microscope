// Every renderer example in docs/visualizers.md runs in the real worker and
// returns a picture the page accepts, and the tutorial's files are the
// fixture's, so the reference cannot drift from what microscope does.

import { expect, it } from "vitest";
import { commands } from "vitest/browser";

import { validate } from "../src/visualize/picture";
import { run as runRenderer } from "./renderer";
import { sandbox, Watched } from "./sandboxes";

const docs = await commands.readFile("../docs/visualizers.md");

/** What `source` returns for `input`, which is also its previous input, as
 * if each input were the value's member of the same name. */
function run(source: string, input: unknown): Promise<Record<string, unknown>> {
  return runRenderer(source, input, {
    previous: input,
    paths: Object.fromEntries(Object.keys(input as object).map((name) => [name, name])),
    width: 600,
  });
}

const examples = [
  ...docs.matchAll(/```microscope-renderer-example\n([\s\S]*?)\n---\n([\s\S]*?)\n```/gu),
].map(([, source, input]) => ({ source: source as string, input: input as string }));

it("has renderer examples", () => {
  expect(examples.length).toBeGreaterThanOrEqual(3);
});

for (const [index, example] of examples.entries()) {
  it(`runs example ${index + 1} to a picture the page accepts`, async () => {
    const input = new Function(`return ${example.input};`)();
    const answer = await run(example.source, input);
    expect(answer).toMatchObject({ type: "picture" });
    expect(() => validate(answer.picture)).not.toThrow();
  });
}

const liveExamples = [
  ...docs.matchAll(/```microscope-live-example\n([\s\S]*?)\n---\n([\s\S]*?)\n```/gu),
].map(([, source, input]) => ({ source: source as string, input: input as string }));

for (const [index, example] of liveExamples.entries()) {
  it(`runs live example ${index + 1} to frames that follow the pointer`, async () => {
    const input = new Function(`return ${example.input};`)();
    const live = new Watched(sandbox(), "card", example.source, input);
    await live.expect("started");
    live.session.frame(0);
    expect((await live.expect("frame")).bitmap).toBeInstanceOf(ImageBitmap);
    live.session.pointer({
      type: "move",
      x: 1,
      y: 1,
      dx: 0,
      dy: 0,
      buttons: 0,
      wheel: 0,
      shift: false,
      ctrl: false,
      alt: false,
    });
    expect((await live.expect("hint")).text).not.toBeNull();
    await live.expect("redraw");
  });
}

it("has a live example", () => {
  expect(liveExamples).toHaveLength(1);
});

it("shows the life fixture's own files", async () => {
  const blocks = [...docs.matchAll(/```(?:js|text)\n([\s\S]*?)```/gu)].map(([, body]) => body);
  expect(blocks).toContain(await commands.readFile("../tests/fixtures/c/life/life.js"));
  expect(blocks).toContain(await commands.readFile("../tests/fixtures/c/life/life.views"));
});
