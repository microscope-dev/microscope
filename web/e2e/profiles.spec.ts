// A profile explored in the page: the functions and their callers, a
// transform, the flame graph's zoom, and the timeline's range, each held
// in the address bar, so a reload or a link opens the same view.

import { execFileSync, spawn } from "node:child_process";
import * as path from "node:path";

import type { Page } from "@playwright/test";

import { expect, fixture, join, menu, root, test } from "./server";

/** Shows a view from Options, opening it if it is closed, and puts it away. */
async function showView(page: Page, label: string): Promise<void> {
  const options = page.getByRole("dialog", { name: "Options" });
  if (!(await options.isVisible())) {
    await page.getByRole("button", { name: "Options" }).click();
  }
  await options.getByRole("combobox", { name: "View" }).selectOption({ label });
  await page.keyboard.press("Escape");
  await expect(options).toBeHidden();
}

// The test serves a profile it records, not a program.
test.use({ program: [] });

test("a profile is explored, and its address opens the same view", async ({
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
      // A small copy, as the Rust tests' recordings of leaf take: six deep
      // ones overfill the rings a recording gets while others hold perf
      // memory, and a starved reader then loses calls made back to back.
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
  await expect(page).toHaveURL(/\/profile\/[0-9a-f]{16}$/u);
  const accounting = page.getByTestId("accounting");
  await expect(accounting).toContainText("Sampled6 executions");
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
  await page.getByRole("button", { name: "Options" }).click();
  await expect(page.getByRole("combobox", { name: "View" })).toHaveValue("tree");
  await page.keyboard.press("Escape");

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

  // Dragging across the timeline selects a range, which a chip names.
  const lanes = page.getByTestId("timeline").getByRole("img");
  const area = await lanes.boundingBox();
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
  await expect(chip).toHaveAttribute(
    "title",
    `merge ${long}\nRemove this transform and those after it`,
  );
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

test("File details shows the whole command recorded, the samples' shape, and closes", async ({
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
  await page.getByRole("button", { name: "File details" }).click();
  const dialog = page.getByRole("dialog", { name: "argv.usprof" });
  await expect(page.getByRole("tab", { name: "Metadata" })).toBeFocused();

  // Each argument whole on its own line, quoted as a shell reads it back.
  const lines = dialog.getByTestId("command").locator(":scope > div");
  await expect(lines).toHaveText([
    `${program} \\`,
    "20000000 \\",
    "'two words' \\",
    String.raw`'it'\''s'`,
  ]);
  await expect(dialog.getByRole("heading", { name: "Command", exact: true })).toBeVisible();
  await expect(dialog.getByRole("row", { name: /profile-stacks-gcc-o0 .* \d+$/u })).toBeVisible();

  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "Samples" })).toHaveAttribute("aria-selected", "true");
  const bars = dialog.getByRole("button", { name: /frames?: \d+ samples?$/u });
  await expect(bars.first()).toBeVisible();
  await bars.last().hover();
  await expect(dialog.getByRole("region", { name: /stack depth/u }).getByRole("status")).toHaveText(
    /^[\d,]+ frames?: [\d,]+ samples?, /u,
  );

  // A click on the page around it closes it; so does Escape, which gives
  // focus back to the button that opened it.
  const inspect = page.getByRole("button", { name: "File details" });
  await page.mouse.click(4, 4);
  await expect(dialog).toBeHidden();
  await inspect.click();
  await expect(dialog).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(inspect).toBeFocused();
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
  await page.getByRole("button", { name: "File details" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Download" }).click();
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
  const options = page.getByRole("dialog", { name: "Options" });
  await page.getByRole("button", { name: "Options" }).click();
  await options.getByRole("checkbox", { name: "By task" }).check();
  await showView(page, "Call tree");
  const tree = page.getByRole("tree", { name: "Call tree" }).getByRole("treeitem");
  await expect(tree.first()).toContainText("goroutine");

  // The CPU's lanes by task: the spinner's, which selects its samples.
  await page.getByRole("button", { name: "CPU" }).click();
  await page.getByRole("button", { name: "Options" }).click();
  await options.getByRole("checkbox", { name: "By task" }).uncheck();
  await options.getByRole("checkbox", { name: "Task lanes" }).check();
  await page.keyboard.press("Escape");
  const spinner = page.getByTestId("timeline").getByRole("button", { name: /phase=spin/u });
  await spinner.click();
  await expect(page).toHaveURL(/tk=\d+/u);
  await expect(
    page.getByRole("button", { name: /Remove goroutine \d+ \{phase=spin\}/u }),
  ).toBeVisible();
  await expect(tree.filter({ hasText: "main.spin" }).first()).toBeVisible();
});

test("a program's zones show in tracks, and Find Zone measures one and filters by a label", async ({
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
  await showView(page, "Zones");
  await expect(page).toHaveURL(/view=zones/u);
  await expect(page.getByRole("img", { name: /^Zones over/u })).toBeVisible();
  await expect(page.getByRole("img", { name: "Plot round, 3 values from 0 – 2" })).toBeVisible();

  // A site's zones, measured; its longest selects its span.
  const find = page.getByTestId("find-zone");
  await find.getByRole("button", { name: "outer", exact: true }).click();
  await expect(page).toHaveURL(/zs=outer/u);
  const stats = page.getByTestId("zone-stats");
  await expect(stats.locator("dd").first()).toHaveText("3");
  await stats.getByRole("listitem").first().getByRole("button").click();
  await expect(page).toHaveURL(/range=\d+-\d+/u);

  // Ctrl and the wheel zoom about the pointer, dragging pans, and the
  // ruler names the times in view.
  const tracks = page.getByRole("img", { name: /^Zones over/u });
  const range = () => {
    const [start = 0, end = 0] = (new URL(page.url()).searchParams.get("range") ?? "")
      .split("-")
      .map(Number);
    return { start, end };
  };
  const selected = range();
  const bounds = (await tracks.boundingBox()) ?? { x: 0, y: 0, width: 0, height: 0 };
  await page.mouse.move(bounds.x + bounds.width / 2, bounds.y + 4);
  await page.keyboard.down("Control");
  await page.mouse.wheel(0, 400);
  await page.keyboard.up("Control");
  await expect
    .poll(() => range().end - range().start)
    .toBeGreaterThan(selected.end - selected.start);
  await expect(
    page.getByTestId("zones").getByRole("img", { name: "Time", exact: true }),
  ).toContainText(/\d s|ms|µs/u);
  const zoomed = range();
  await page.mouse.down();
  await page.mouse.move(bounds.x + bounds.width / 2 + 80, bounds.y + 4, { steps: 4 });
  await page.mouse.up();
  await expect.poll(() => range().start).toBeLessThan(zoomed.start);
  expect(range().end - range().start).toBe(zoomed.end - zoomed.start);

  // Another theme repaints the tracks in its colors.
  await page.emulateMedia({ colorScheme: "light" });
  const painted = () => tracks.evaluate((canvas) => (canvas as HTMLCanvasElement).toDataURL());
  const before = await painted();
  await menu(page, "View", "Theme: dark");
  await expect.poll(painted).not.toBe(before);
  await stats.getByRole("listitem").first().getByRole("button").click();

  // A label's values are filters of every view: in that span, the round
  // around it.
  await find.getByRole("button", { name: "round", exact: true }).click();
  const value = stats.getByRole("button", { name: /^round=\d$/u });
  await expect(value).toHaveCount(1);
  const round = (await value.textContent()) ?? "";
  await value.click();
  await expect(page).toHaveURL(new RegExp(`lb=${encodeURIComponent(round)}`, "u"));
  await expect(page.getByRole("button", { name: `Remove ${round}` })).toBeVisible();
  await stats.getByRole("button", { name: "By round" }).click();
  await expect(page).toHaveURL(/bl=round/u);
  await page.getByRole("button", { name: "Options" }).click();
  await expect(page.getByRole("combobox", { name: "Root by label" })).toHaveValue("round");
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
  const recorded = (symbol: string, name: string) => {
    const file = info.outputPath(name);
    execFileSync(
      path.join(root, "target", "debug", "microscope"),
      [
        "record",
        "-o",
        file,
        "--function",
        symbol,
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
    return file;
  };
  const served = await microscope.start([
    recorded("leaf", "calls.usprof"),
    recorded("middle", "middle.usprof"),
  ]);
  await join(page, served.link);

  // The selected range becomes the base, and nothing changed against it.
  const lanes = page.getByTestId("timeline").getByRole("img");
  const area = await lanes.boundingBox();
  if (!area) {
    throw new Error("the timeline has no box");
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
  await expect(row("leaf")).toHaveAttribute("aria-label", /, \+6 executions new change$/u);
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
  await page.getByRole("button", { name: "Assembly", exact: true }).click();
  await expect(page).toHaveURL(/ann=code/u);
  const code = page.getByTestId("code");
  await expect(code.locator(".annotated-head").first()).toContainText("leaf");
  await expect(code.locator(".annotated-row").first()).toContainText("100.0%");
  await expect(code.locator(".hot")).toHaveCount(1);
});
