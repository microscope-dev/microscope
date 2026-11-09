// A profile explored in the page: the functions and their callers, a
// transform, the flame graph's zoom, and the timeline's range, each held
// in the address bar, so a reload or a link opens the same view.

import { execFileSync, spawn } from "node:child_process";
import * as path from "node:path";

import { expect, fixture, join, menu, recordCalls, root, showView, test } from "./server";

// The test serves a profile it records, not a program.
test.use({ program: [] });

test("a profile is explored, and its address opens the same view", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("calls.usprof");
  const kept = recordCalls(file, "leaf");
  const served = await microscope.start([file]);
  await join(page, served.link);
  await expect(page).toHaveURL(/\/profile\/[0-9a-f]{16}$/u);
  const accounting = page.getByTestId("accounting");
  await expect(accounting).toContainText(`Sampled${kept} execution`);
  await expect(accounting).toContainText("Samplingeach call of leaf");

  // leaf owns every sample, and its callers are middle, then outer.
  await showView(page, "Functions");
  const functions = page.getByRole("listbox", { name: "Functions" });
  await expect(functions.getByRole("option").first()).toContainText("100.0%");
  await expect(functions.getByRole("option").first()).toContainText("leaf");
  await functions.getByRole("option").filter({ hasText: "leaf" }).click();
  await expect(page).toHaveURL(/fn=leaf/u);
  const callers = page.getByRole("tree", { name: "Callers" }).getByRole("treeitem");
  await expect(callers.nth(0)).toContainText("leaf");
  await expect(callers.nth(1)).toContainText("middle");
  await expect(callers.nth(2)).toContainText("outer");

  // Merged away, middle gives its place to outer, in every view.
  await showView(page, "Call tree");
  await page
    .getByRole("tree", { name: "Call tree" })
    .getByRole("treeitem")
    .filter({ hasText: "middle" })
    .first()
    .click();
  await page.getByTestId("details").getByRole("button", { name: "Merge" }).click();
  await expect(page).toHaveURL(/tf=merge:middle/u);
  await expect(page.getByText("merge middle")).toBeVisible();
  const tree = page.getByRole("tree", { name: "Call tree" });
  await expect(tree.getByRole("treeitem").filter({ hasText: "leaf" }).first()).toBeVisible();
  await expect(tree.getByRole("treeitem").filter({ hasText: "middle" })).toHaveCount(0);

  // The address opens the same view.
  await page.reload();
  await expect(page.getByText("merge middle")).toBeVisible();
  await expect(page.getByRole("tab", { name: /^Call tree/u })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await expect(page.getByRole("tree", { name: "Call tree" })).toBeVisible();

  // A frame of the flame graph zooms, and says what it costs, beside a
  // search as wide as before.
  await showView(page, "Flame graph");
  const flame = page.getByRole("img", { name: "Flame graph" });
  const box = await flame.boundingBox();
  if (!box) {
    throw new Error("the flame graph has no box");
  }
  const search = page.getByRole("searchbox", { name: "Search frames" });
  const searchWidth = (await search.boundingBox())?.width;
  await page.mouse.move(box.x + 4, box.y + 4);
  await expect(page.getByTestId("flame-tip")).toContainText("total");
  await page.mouse.click(box.x + 4, box.y + 4);
  await expect(page).toHaveURL(/z=/u);
  await expect(page.getByRole("button", { name: "Clear" })).toBeVisible();
  await expect(page.getByTestId("details").getByText(/total/u)).toBeVisible();
  expect((await search.boundingBox())?.width).toBe(searchWidth);

  // Dragging across the timeline's ruler selects a range, which a chip names.
  const area = await page.getByTestId("ruler").locator(".ruler-marks").boundingBox();
  if (!area) {
    throw new Error("the timeline has no box");
  }
  await page.mouse.move(area.x + 2, area.y + 4);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width / 2, area.y + 4, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/u);
  await expect(page.getByRole("button", { name: /^Remove .* – /u })).toBeVisible();

  // A frame named longer than the page is cut short in the search and in
  // its transform's chip, rather than widening the flame graph until the
  // page scrolls sideways or wrapping the chip onto more lines.
  const long = `<${"alloc::boxed::Box<dyn core::ops::function::FnMut<(ignore::walk::DirEntry,)>> as ".repeat(8)}call_mut`;
  const address = new URL(page.url());
  address.searchParams.set("fn", long);
  address.searchParams.append("tf", `merge:${long}`);
  await page.goto(address.href);
  await expect(search).toHaveValue(long);
  await expect(search).toHaveAttribute("title", long);
  const chip = page.locator(".profile-chip", { hasText: "merge <alloc::boxed" });
  await expect(chip).toHaveAttribute("title", `merge ${long}\nRemove this transform`);
  const short = await page.locator(".profile-chip", { hasText: "merge middle" }).boundingBox();
  expect((await chip.boundingBox())?.height).toBe(short?.height);
  const widths = await page.evaluate(() => ({
    page: document.documentElement.scrollWidth,
    window: document.documentElement.clientWidth,
  }));
  expect(widths.page).toBe(widths.window);
});

test("each thread's lane says how long it was off its CPU, and the accounting what it all adds up to", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("off-cpu.usprof");
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "-o", file, "--", fixture("profile-offcpu")],
    { stdio: "ignore" },
  );
  const served = await microscope.start([file]);
  await join(page, served.link);
  const lanes = page.getByTestId("timeline");
  // The sleeper naps five times for 20 ms; the spinners share one CPU.
  await expect(lanes.getByRole("button", { name: /^sleeper/u })).toHaveAttribute(
    "title",
    /off its CPU (\d{3} ms|[\d.]+ s) blocked/u,
  );
  const spinners = lanes.getByRole("button", { name: /^spinner/u });
  await expect(spinners).toHaveCount(2);
  for (const spinner of await spinners.all()) {
    await expect(spinner).toHaveAttribute("title", /, (?!0 ns)[\d.]+ \S+ preempted$/u);
  }
  // The sleeper's naps and the spinners' turns add up off their CPUs; what
  // recording cost microscope is File details' to say.
  const accounting = page.getByTestId("accounting");
  await expect(accounting).toContainText("Threads4");
  await expect(accounting).toContainText(/Cores busy≈[\d.]+On CPU[\d.]+ (ns|µs|ms|s)/u);
  await expect(accounting).toContainText(/Off CPU[\d.]+ (ns|µs|ms|s)/u);
  await expect(accounting).not.toContainText("of a CPU");
});

test("File details opens as a pane with the whole command recorded and the samples' shape, and closes", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("argv.usprof");
  const program = fixture("profile-stacks-gcc-o0");
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    // About 150 ms of work, so it holds a hundred samples or so.
    ["record", "-o", file, "--", program, "20000000", "two words", "it's"],
    { stdio: "ignore" },
  );
  const served = await microscope.start([file]);
  await join(page, served.link);
  const inspect = page.getByRole("button", { name: "File details", exact: true });
  await inspect.click();
  const details = page.getByRole("region", { name: "argv.usprof" });
  const tab = page.getByRole("tab", { name: /^File details/u });
  await expect(tab).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/view=details/u);
  await page.getByRole("tab", { name: "Metadata" }).click();

  // Each argument whole on its own line, quoted as a shell reads it back.
  const lines = details.getByTestId("command").locator(":scope > div");
  await expect(lines).toHaveText([
    `${program} \\`,
    "20000000 \\",
    "'two words' \\",
    String.raw`'it'\''s'`,
  ]);
  await expect(details.getByRole("heading", { name: "Command", exact: true })).toBeVisible();
  await expect(details.getByRole("row", { name: /profile-stacks-gcc-o0 .* \d+$/u })).toBeVisible();

  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "Samples" })).toHaveAttribute("aria-selected", "true");
  const bars = details.getByRole("button", { name: /frames?: \d+ samples?$/u });
  await expect(bars.first()).toBeVisible();
  await bars.last().hover();
  await expect(
    details.getByRole("region", { name: /stack depth/u }).getByRole("status"),
  ).toHaveText(/^[\d,]+ frames?: [\d,]+ samples?, /u);

  // The button shows the pane it opened rather than another; closed, it
  // goes, and a reload of its address opens it again.
  await page.getByRole("tab", { name: "argv.usprof", exact: true }).click();
  await inspect.click();
  await expect(tab).toHaveCount(1);
  await expect(tab).toHaveAttribute("aria-selected", "true");
  await page.reload();
  await expect(details).toBeVisible();
  await page.getByRole("button", { name: /^Close File details/u }).click();
  await expect(details).toBeHidden();
  await expect(page).not.toHaveURL(/view=details/u);
});

const level = (rows: number[][]) => rows.map((_, index) => [index * 18, 18]);

test("with many threads, each lane's name is as tall as its lane and level with it", async ({
  page,
  microscope,
}, info) => {
  // Sixteen spinners and the thread that answers the pipe.
  const file = info.outputPath("many.usprof");
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "-o", file, "--", fixture("profile-switches"), "10", "100", "16"],
    { stdio: "ignore" },
  );
  const served = await microscope.start([file]);
  await join(page, served.link);
  const timeline = page.getByTestId("timeline");
  const names = timeline.locator(".lane-name");
  await expect.poll(() => names.count()).toBeGreaterThan(8);

  // Each name's top and height, from the top of the lanes' canvas.
  const measured = () =>
    timeline.evaluate((box) => {
      const lanes = box.querySelector("canvas")?.getBoundingClientRect();
      return [...box.querySelectorAll(".lane-name")].map((name) => {
        const at = name.getBoundingClientRect();
        return [Math.round(at.top - (lanes?.top ?? 0)), Math.round(at.height)];
      });
    });
  const before = await measured();
  expect(before).toEqual(level(before));
  await timeline.evaluate((box) => {
    box.scrollTop = 100;
  });
  expect(await timeline.evaluate((box) => box.scrollTop)).toBeGreaterThan(0);
  const after = await measured();
  expect(after).toEqual(level(after));
});

test("a program recorded from the page grows live, stops, and saves", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  // An address from before Record was a pane opens the pane.
  await page.goto(new URL("/pick?tab=profile", microscope.origin).href);
  const record = page.getByRole("tabpanel", { name: "Record" });
  await record.getByLabel("Program").fill(fixture("spin"));
  await record.getByRole("button", { name: "Record" }).click();
  await expect(page).toHaveURL(/\/profile\/[0-9a-f]{16}/u);
  await expect(page.getByTestId("recording")).toContainText("Recording spin");

  // Its samples arrive while it runs.
  await showView(page, "Functions");
  const functions = page.getByRole("listbox", { name: "Functions" });
  const main = functions
    .getByRole("option")
    .filter({ has: page.getByText("main", { exact: true }) });
  await expect(main).toBeVisible({ timeout: 10_000 });

  await page.getByRole("button", { name: "Stop" }).click();
  await expect(page.getByTestId("recording")).toContainText("spin ended: signal: 9");
  await expect(page.getByRole("button", { name: "Stop" })).toHaveCount(0);

  const saving = page.waitForEvent("download");
  await page.getByRole("button", { name: "File details", exact: true }).click();
  await page
    .getByRole("tabpanel", { name: "File details", exact: true })
    .getByRole("button", { name: "Download" })
    .click();
  expect((await saving).suggestedFilename()).toBe("spin.usprof");
});

test("a profile shows where tasks waited, by task, and a lane per task", async ({
  page,
  microscope,
}, info) => {
  // The program waits on its standard input the whole time it is recorded.
  const file = info.outputPath("waits.usprof");
  const program = spawn(fixture("profile-go-waits"), [], { stdio: ["pipe", "ignore", "ignore"] });
  const exited = new Promise((done) => {
    program.once("exit", done);
  });
  try {
    execFileSync(
      path.join(root, "target", "debug", "microscope"),
      ["record", "-o", file, "--pid", String(program.pid), "--task-snapshots", "0.05"].concat([
        "--duration",
        "2",
      ]),
      { stdio: "ignore" },
    );
  } finally {
    program.stdin?.end("x");
    await exited;
  }
  const served = await microscope.start([file]);
  await join(page, served.link);
  await expect(page.getByTestId("accounting")).toContainText("Task snapshots");

  // The waits' functions are the receivers', and by task each is its own.
  await page.getByRole("button", { name: "Waits", exact: true }).click();
  await expect(page).toHaveURL(/src=waits/u);
  await showView(page, "Functions");
  const functions = page.getByRole("listbox", { name: "Functions" });
  await expect(functions.getByRole("option").filter({ hasText: "main.receive" })).toHaveCount(1);
  // By task is the call tree's own: the functions stay as they were.
  await showView(page, "Call tree");
  const options = page.getByRole("dialog", { name: "Options" });
  await page.getByRole("button", { name: "Options" }).click();
  await options.getByRole("checkbox", { name: "By task" }).check();
  await page.keyboard.press("Escape");
  const tree = page.getByRole("tree", { name: "Call tree" }).getByRole("treeitem");
  await expect(tree.first()).toContainText("goroutine");
  await expect(page).toHaveURL(/bt=1/u);
  await showView(page, "Functions");
  await expect(page).not.toHaveURL(/bt=/u);
  await showView(page, "Call tree");

  // The CPU's lanes by task: the spinner's, which selects its samples.
  await page.getByRole("button", { name: "CPU" }).click();
  await page.getByRole("button", { name: "Options" }).click();
  await options.getByRole("checkbox", { name: "By task" }).uncheck();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Task lanes", exact: true }).click();
  await expect(page).toHaveURL(/lanes=tasks/u);
  const spinner = page.getByTestId("timeline").getByRole("button", { name: /phase=spin/u });
  await spinner.click();
  await expect(page).toHaveURL(/tk=\d+/u);
  await expect(
    page.getByRole("button", { name: /Remove goroutine \d+ \{phase=spin\}/u }),
  ).toBeVisible();
  await expect(tree.filter({ hasText: "main.spin" }).first()).toBeVisible();
});

test("a program's zones show under its threads, and the Zones pane picks which samples the views read", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("zones.usprof");
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "-o", file, "--", fixture("probes"), "zones"],
    { stdio: "ignore" },
  );
  const served = await microscope.start([file]);
  await join(page, served.link);
  await expect(page.getByTestId("accounting")).toContainText(/Probe events[\d,]+, lost 0/u);
  const lanes = page.getByRole("img", { name: /^CPU and zones over/u });
  await expect(lanes).toBeVisible();
  await expect(page.getByRole("img", { name: "Plot round, 3 values from 0 – 2" })).toBeVisible();
  const pane = page.getByTestId("zones-pane");
  await expect(pane).toBeVisible();
  // The flame graph shows beside the zones, each a pane of its own.
  await expect(page.getByRole("img", { name: /flame graph/iu }).first()).toBeVisible();
  await expect(page.getByRole("tab", { name: /^Zones/u })).toHaveAttribute("aria-selected", "true");
  // A link to the Zones view shows its pane.
  const zonesLink = new URL(page.url());
  zonesLink.searchParams.set("view", "zones");
  await page.goto(zonesLink.href);
  await expect(pane).toBeVisible();
  await expect(page).toHaveURL(/view=zones/u);

  const look = () => new URL(page.url()).searchParams;

  // A site's zones, measured, and its samples alone.
  await pane.getByRole("button", { name: "step", exact: true }).click();
  await expect(page).toHaveURL(/zs=step/u);
  const stats = page.getByTestId("zone-stats");
  await expect(stats.locator("dd").first()).toHaveText("2");
  await pane.getByRole("button", { name: "Inside every one" }).click();
  await expect(page).toHaveURL(/zf=all/u);
  await expect(page.getByRole("button", { name: "Remove zone: step", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Remove inside every one" })).toBeVisible();

  // A band from the histogram; then only its zones' samples, then a diff
  // of them against the site's others.
  await stats.getByRole("img", { name: /^Lengths of step/u }).click({ position: { x: 2, y: 30 } });
  await expect(page).toHaveURL(/zb=\d+-\d+/u);
  await expect(page.getByTestId("zone-band")).toContainText(/of 2/u);
  await pane.getByRole("button", { name: "Inside the band" }).click();
  await expect(page).toHaveURL(/zf=band/u);
  await expect(page.getByRole("button", { name: /^Remove zone: step .* – /u })).toBeVisible();
  await expect(page.getByRole("button", { name: "Remove inside the band" })).toBeVisible();
  await pane.getByRole("button", { name: "Compare the band with the rest" }).click();
  await expect(page).toHaveURL(/bz=rest/u);
  await expect(page.getByRole("button", { name: "Remove the band vs the rest" })).toBeVisible();
  // The band and the rest hold different numbers of zones, so the diff
  // can compare each side's share instead.
  await page.getByRole("button", { name: "share", exact: true }).click();
  await expect(page).toHaveURL(/dm=share/u);

  // One of the longest: the views read its span, and the threads are as
  // they were. Escape lets go of it, then the band, then the site.
  await stats.getByRole("listitem").first().getByRole("button").first().click();
  await expect(page).toHaveURL(/zi=\d+(%3A|:)\d+-\d+/u);
  expect(look().get("th")).toBeNull();
  await expect(page.getByTestId("zone-rank")).toContainText("of 2");
  // Somewhere down the middle of it, its tip says how much of it ran on
  // its CPU. The lanes show the whole recording, which their label says
  // the length of.
  const [, from = 0, to = 0] = (look().get("zi") ?? "").split(/[:-]/u).map(Number);
  const named = (await lanes.getAttribute("aria-label")) ?? "";
  const [, length = "0", unit] = / ([\d.]+) (ms|s)$/u.exec(named) ?? [];
  const total = Number(length) * (unit === "s" ? 1e9 : 1e6);
  const tip = page.getByTestId("zone-tip");
  await expect
    .poll(async () => {
      const box = (await lanes.boundingBox()) ?? { x: 0, y: 0, width: 0, height: 0 };
      for (let y = 20; y < box.height; y += 8) {
        await page.mouse.move(box.x + (box.width * (from + to)) / 2 / total, box.y + y);
        const text = (await tip.count()) > 0 ? await tip.textContent() : null;
        if (text?.startsWith("step")) {
          return text;
        }
      }
      return "";
    })
    .toMatch(/^step: 20\.\d ms\d+% on CPU, [\d.]+ \S+ blocked, [\d.]+ \S+ preempted/u);
  await page.mouse.move(0, 0);
  // From anywhere on the page, even what takes no focus.
  await page.getByTestId("accounting").click();
  await page.keyboard.press("Escape");
  await expect(page).not.toHaveURL(/zi=/u);
  await page.keyboard.press("Escape");
  await expect(page).not.toHaveURL(/zb=|bz=|zf=band/u);
  await expect(page).toHaveURL(/zs=step/u);
  await page.keyboard.press("Escape");
  await expect(page).not.toHaveURL(/zs=/u);

  // The chips let go of the same parts: the zone's keeps its site, and
  // the site's takes the zone with it.
  await pane.getByRole("button", { name: "step", exact: true }).click();
  const longest = stats.getByRole("listitem").first().getByRole("button").first();
  const zoneChip = page.getByRole("button", { name: /^Remove [\d.]+ m?s, /u });
  await longest.click();
  await zoneChip.click();
  await expect(page).not.toHaveURL(/zi=/u);
  await expect(page).toHaveURL(/zs=step/u);
  await longest.click();
  await expect(zoneChip).toBeVisible();
  await page.getByRole("button", { name: "Remove zone: step", exact: true }).click();
  await expect(page).not.toHaveURL(/zs=|zi=/u);
  await expect(zoneChip).toHaveCount(0);

  // Ctrl and the wheel zoom anywhere in the lanes, their names too, and
  // dragging with ctrl held pans; neither changes the range the views read.
  const ruler = page.getByTestId("ruler");
  const end = () => ruler.locator(".ruler-marks > span.last").textContent();
  const whole = await end();
  const names = (await page.locator(".lane-names").boundingBox()) ?? { x: 0, y: 0, width: 0 };
  const drawn = (await lanes.boundingBox()) ?? { x: 0, y: 0, width: 0, height: 0 };
  await page.mouse.move(names.x + names.width / 2, names.y + 6);
  await page.keyboard.down("Control");
  await page.mouse.wheel(0, -400);
  await page.keyboard.up("Control");
  await expect.poll(end).not.toBe(whole);
  const zoomed = await end();
  await page.mouse.move(drawn.x + drawn.width / 2, drawn.y + 4);
  await page.keyboard.down("Control");
  await page.mouse.down();
  // Zoomed in at the start, so a drag to the left shows later times.
  await page.mouse.move(drawn.x + drawn.width / 2 - 120, drawn.y + 4, { steps: 4 });
  await page.mouse.up();
  await page.keyboard.up("Control");
  await expect.poll(end).not.toBe(zoomed);
  expect(look().get("range")).toBeNull();
  expect(look().get("zi")).toBeNull();

  // Dragging across the lanes selects a range, and a click off the zones
  // lets go of it; so does dragging on the ruler, and a click there.
  const marks = (await ruler.boundingBox()) ?? drawn;
  const below = drawn.y + drawn.height - 2;
  await page.mouse.move(drawn.x + 10, below);
  await page.mouse.down();
  await page.mouse.move(drawn.x + drawn.width / 2, below, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/u);
  await expect(page.getByTestId("range-length")).toBeVisible();
  await page.mouse.click(drawn.x + 10, below);
  await expect(page).not.toHaveURL(/range=/u);
  await page.mouse.move(drawn.x + 10, marks.y + 8);
  await page.mouse.down();
  await page.mouse.move(drawn.x + drawn.width / 2, marks.y + 8, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/u);

  // Another theme repaints the lanes in its colors.
  await page.emulateMedia({ colorScheme: "light" });
  const painted = () => lanes.evaluate((canvas) => (canvas as HTMLCanvasElement).toDataURL());
  const before = await painted();
  await menu(page, "View", "Theme: dark");
  await expect.poll(painted).not.toBe(before);
  await page.getByRole("button", { name: /^Remove .* – /u }).click();

  // A label's values are filters of every view.
  await pane.getByRole("button", { name: "round", exact: true }).click();
  const value = stats.getByRole("button", { name: /^round=\d$/u });
  await expect(value).toHaveCount(3);
  const round = (await value.first().textContent()) ?? "";
  await value.first().click();
  await expect(page).toHaveURL(new RegExp(`lb=${encodeURIComponent(round)}`, "u"));
  await expect(page.getByRole("button", { name: `Remove ${round}` })).toBeVisible();
  await stats.getByRole("button", { name: "By round" }).click();
  await expect(page).toHaveURL(/bl=round/u);

  // The pane closes, and opens again from the overview, as it was.
  await page.getByRole("button", { name: "Close Zones", exact: true }).click();
  await expect(pane).toBeHidden();
  await page.getByRole("button", { name: "Zones", exact: true }).click();
  await expect(pane).toBeVisible();
  await expect(page).toHaveURL(/view=zones/u);
  await expect(pane.getByRole("button", { name: "round", exact: true })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
});

test("a recording without zones has no Zones pane, and its lanes ask for none", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("plain.usprof");
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "-o", file, "--", fixture("profile-stacks-gcc-o0"), "20000000"],
    { stdio: "ignore" },
  );
  const served = await microscope.start([file]);
  await join(page, served.link);
  await expect(page.getByRole("img", { name: /^CPU over/u })).toBeVisible();
  await expect(page.getByTestId("zones-pane")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Zones", exact: true })).toHaveCount(0);
  // A link naming a site this recording lacks keeps nothing it cannot read.
  const address = new URL(page.url());
  address.searchParams.set("zs", "nowhere");
  address.searchParams.set("zf", "all");
  await page.goto(address.href);
  await page.goto(address.href);
  await expect(page.getByText("no zones are named nowhere")).toBeVisible();
  await page.getByRole("button", { name: "Remove inside every one" }).click();
  await expect(page.getByText("no zones are named nowhere")).toBeHidden();
});

test("a Go program's own pprof profile opens in its unit, without a timeline", async ({
  page,
  microscope,
}, info) => {
  const allocs = info.outputPath("allocs.pb.gz");
  execFileSync(fixture("pprof-files"), [allocs, info.outputPath("goroutine.pb.gz")], {
    stdio: "ignore",
  });
  const served = await microscope.start([allocs]);
  await join(page, served.link);
  await expect(page.getByTestId("untimed")).toContainText("pprof's alloc_space (bytes)");
  await showView(page, "Functions");
  const allocate = page
    .getByRole("listbox", { name: "Functions" })
    .getByRole("option")
    .filter({ hasText: "main.allocate" });
  await expect(allocate).toHaveAttribute("title", "62.5 KiB");
});

test("a profile is diffed with a range of its own, and with another profile", async ({
  page,
  microscope,
}, info) => {
  const leaf = info.outputPath("calls.usprof");
  const kept = recordCalls(leaf, "leaf");
  const middle = info.outputPath("middle.usprof");
  recordCalls(middle, "middle");
  const served = await microscope.start([leaf, middle]);
  await join(page, served.link);

  // The selected range becomes the base, and nothing changed against it.
  const area = await page.getByTestId("ruler").locator(".ruler-marks").boundingBox();
  if (!area) {
    throw new Error("the timeline has no ruler");
  }
  await page.mouse.move(area.x + 1, area.y + 4);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width - 1, area.y + 4, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/u);
  const compare = page.getByRole("combobox", { name: "Compare with" });
  await compare.selectOption("range");
  await expect(page).toHaveURL(/br=\d+-\d+/u);
  await expect(page).not.toHaveURL(/[?&]range=/u);
  await showView(page, "Functions");
  const functions = page.getByRole("listbox", { name: "Functions" });
  const row = (name: string) =>
    functions.getByRole("option").filter({ has: page.getByText(name, { exact: true }) });
  await expect(row("leaf")).toContainText("±0");

  // Against the calls of middle, leaf is all new and middle all gone.
  await page.getByRole("button", { name: /^Remove diff with this profile/u }).click();
  await expect(page).not.toHaveURL(/br=/u);
  await compare.selectOption({ label: "diff with middle.usprof" });
  await expect(page).toHaveURL(/bp=[0-9a-f]{16}/u);
  // In time by default, as report --base compares them.
  await expect(row("leaf")).toHaveAttribute(
    "aria-label",
    new RegExp(`, \\+${kept} executions? new change$`, "u"),
  );
  await expect(row("middle")).toHaveAttribute("aria-label", /, −\d+ executions? gone change$/u);
  await expect(page.getByRole("button", { name: "by change" })).toBeVisible();

  // In share, each side's part of its own profile; the choice is in the address.
  await page.getByRole("button", { name: "share", exact: true }).click();
  await expect(page).toHaveURL(/dm=share/u);
  await expect(row("leaf")).toHaveAttribute("aria-label", /, \+100\.0 pp change$/u);
  await expect(row("middle")).toContainText("−100.0 pp");
  await page.getByRole("button", { name: "time", exact: true }).click();
  await expect(page).not.toHaveURL(/dm=/u);
  await expect(row("leaf")).toContainText("new");
});

test("a function's samples show on its source lines and its instructions", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("calls.usprof");
  execFileSync(
    path.join(root, "target", "debug", "microscope"),
    [
      "record",
      "-o",
      file,
      "--function",
      "leaf",
      "--function-period",
      "1",
      "--stack-bytes",
      "512",
      "--",
      fixture("profile-stacks-gcc-o0"),
      "1",
    ],
    { stdio: "ignore" },
  );
  const served = await microscope.start([file]);
  await join(page, served.link);
  await showView(page, "Functions");
  // The window to itself, for room to move the panel's bar in.
  await page.keyboard.press("Alt+Enter");
  await page
    .getByRole("listbox", { name: "Functions" })
    .getByRole("option")
    .filter({ hasText: "leaf" })
    .click();

  // Every call of leaf is on one line of it, and on its first instruction.
  await page.getByRole("button", { name: "Source", exact: true }).click();
  await expect(page).toHaveURL(/ann=lines/u);
  const lines = page.getByTestId("source-lines");
  await expect(lines.locator(".hot")).toHaveCount(1);
  await expect(lines.locator(".hot")).toContainText("100.0%");
  await page.getByRole("button", { name: "Source and assembly", exact: true }).click();
  await expect(page).toHaveURL(/ann=code/u);
  // Source sits beside the instructions: it opens on leaf's sampled line,
  // whose instructions, the first among them, are chosen and tinted.
  const code = page.getByTestId("code");
  await expect(code).toContainText("profile-stacks.c");
  const source = code.getByRole("listbox", { name: "Source" });
  const assembly = code.getByRole("listbox", { name: "Assembly" });
  const line = source.getByRole("option", { selected: true });
  await expect(line).toHaveCount(1);
  await expect(line).toContainText("100.0%");
  const tint = await line.getAttribute("data-tint");
  expect(tint).not.toBeNull();
  const first = assembly.getByRole("option").first();
  await expect(first).toContainText("100.0%");
  await expect(first).toHaveAttribute("aria-selected", "true");
  await expect(first).toHaveAttribute("data-tint", tint ?? "");
  await expect(code.getByTestId("run")).toContainText(/^run 1 of \d+$/u);

  // Each pane names its columns over its rows, however far it scrolls.
  for (const pane of [source, assembly]) {
    const head = pane.locator(".code-head");
    await expect(head).toHaveText("owntotal");
    await pane.evaluate((element) => {
      element.scrollTop = element.scrollHeight;
    });
    const [edge, shown] = await Promise.all([pane.boundingBox(), head.boundingBox()]);
    expect(shown?.y).toBeCloseTo(edge?.y ?? -1, 0);
  }

  // The bar over the panel moves it: a drag, then the arrow keys.
  const divider = page.getByRole("separator", { name: /^Resize the functions/u });
  const listed = page.getByRole("listbox", { name: "Functions" });
  const tall = async () => (await listed.boundingBox())?.height ?? 0;
  const opened = await tall();
  const bar = await divider.boundingBox();
  if (!bar) {
    throw new Error("the divider is not shown");
  }
  await page.mouse.move(bar.x + bar.width / 2, bar.y + bar.height / 2);
  await page.mouse.down();
  await page.mouse.move(bar.x + bar.width / 2, bar.y + bar.height / 2 + 100, { steps: 4 });
  await page.mouse.up();
  await expect.poll(tall).toBeCloseTo(opened + 100, -1);
  await expect(divider).toBeFocused();
  const dragged = await tall();
  await page.keyboard.press("ArrowUp");
  await expect.poll(tall).toBeCloseTo(dragged - 16, 0);
  await divider.dblclick();
  await expect.poll(tall).toBeCloseTo(opened, 0);

  // In a narrow window a long line scrolls sideways under its costs,
  // which stay at the pane's edge. Both panes pin by the same rule; leaf's
  // instructions are too short to need it.
  await page.setViewportSize({ width: 400, height: 800 });
  await expect
    .poll(() => source.evaluate((element) => element.scrollWidth - element.clientWidth))
    .toBeGreaterThan(40);
  const row = source.getByRole("option", { selected: true }).first();
  const before = {
    pin: await row.locator(".pin").boundingBox(),
    text: await row.locator(".text").boundingBox(),
  };
  await source.evaluate((element) => {
    element.scrollLeft = element.scrollWidth;
  });
  const [edge, pin, text] = await Promise.all([
    source.boundingBox(),
    row.locator(".pin").boundingBox(),
    row.locator(".text").boundingBox(),
  ]);
  expect(pin?.x).toBe(before.pin?.x);
  expect(pin?.x).toBeCloseTo(edge?.x ?? -1, 0);
  expect(text?.x).toBeLessThan((before.text?.x ?? 0) - 40);
  await expect(row.locator(".percent").first()).toBeVisible();
  // So do the costs of rows shorter than the scroll.
  const strays = await source.evaluate((element) => {
    const { left } = element.getBoundingClientRect();
    return [...element.querySelectorAll(".pin")]
      .map((pinned) => pinned.getBoundingClientRect().left - left)
      .filter((offset) => Math.abs(offset) > 1);
  });
  expect(strays).toEqual([]);
});
