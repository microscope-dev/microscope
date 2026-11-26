import { spawnSync } from "node:child_process";
import * as path from "node:path";

import { expect, fixture, join, menu, root, showView, test } from "./server";

test.use({ program: [] });

test("a recording's calls are explored from their costs to one call's arguments and stack", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("files.usprof");

  const traced = spawnSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "--allow-stopping", "-o", file, "--", fixture("syscalls"), "files"],
    { cwd: info.outputDir, stdio: "ignore" },
  );
  expect(traced.status).toBe(3);
  const served = await microscope.start([file]);
  await join(page, served.link);

  await page.getByRole("tab", { name: /^Kernel/u }).click();
  const view = page.getByTestId("syscalls");
  await expect(page.getByTestId("syscall-counts")).toContainText("ptrace");
  await expect(page.getByTestId("accounting")).toContainText(/Calls traced[\d,]+ by ptrace/u);
  await expect(page.getByTestId("accounting")).not.toContainText("Calls lost");

  await page.getByLabel("Selection").fill("trace=openat status=failed");
  await page.getByLabel("Selection").press("Enter");
  await expect(page).toHaveURL(/sf=trace%3Dopenat/u);
  await expect(page.getByRole("button", { name: "Remove the system call filter" })).toBeVisible();
  const table = page.getByTestId("syscall-table");
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await table.locator('tr[data-row="openat"] td').nth(1).click();
  await expect(page).toHaveURL(/sc=openat/u);
  const detail = page.getByTestId("syscall-detail");
  await expect(detail).toContainText("ENOENT");
  await expect(page.getByTestId("syscall-callers")).toContainText("files · syscalls.c:24");
  await expect(detail.getByRole("img", { name: "Latency heat map" })).toBeVisible();

  await view.getByRole("button", { name: "Log", exact: true }).click();
  await expect(page).toHaveURL(/st=log/u);
  const line = page
    .getByTestId("syscall-log")
    .getByRole("button")
    .filter({ hasText: '"missing", O_RDONLY) = -1 ENOENT' });
  await line.click();
  await expect(page).toHaveURL(/ev=\d+/u);
  const call = page.getByTestId("syscall-call");
  await expect(call.getByRole("rowheader", { name: "filename" })).toBeVisible();
  await expect(
    call.getByRole("listitem").filter({ has: page.getByText("files", { exact: true }) }),
  ).toBeVisible();

  const site = call.getByTestId("syscall-site");
  await expect(site.locator("figcaption")).toContainText(/^files · .*syscalls\.c:24$/u);
  await expect(site.locator(".made")).toContainText('"missing"');

  await page.reload();
  await expect(page.getByTestId("syscall-call")).toContainText('"missing"');

  await page.getByLabel("Selection").fill("trace=nosuchcall");
  await page.getByLabel("Selection").press("Enter");
  await expect(view.getByRole("alert")).toContainText("nosuchcall");

  await page.getByLabel("Selection").fill("");
  await page.getByLabel("Selection").press("Enter");
  await view.getByRole("button", { name: "Files", exact: true }).click();
  const files = page.getByTestId("syscall-files");
  await expect(files.getByRole("cell", { name: /^\/.*\/present$/u })).toHaveCount(1);
  await expect(files.getByRole("cell", { name: /^\/.*\/missing$/u })).toHaveCount(1);

  await view.getByRole("button", { name: "Summary", exact: true }).click();
  await page.getByLabel("Selection").fill("trace=openat");
  await page.getByLabel("Selection").press("Enter");
  await page.getByLabel("Group", { exact: true }).selectOption("path");
  await expect(page).toHaveURL(/sg=path/u);
  await expect(table.locator("td.name", { hasText: /\/missing$/u })).toHaveCount(1);
  await page.getByLabel("Group", { exact: true }).selectOption("syscall");

  await table.locator('tr[data-row="openat"] td').nth(1).click();
  const heat = detail.getByRole("img", { name: "Latency heat map" });
  await expect(heat).toBeVisible();
  await heat.evaluate((canvas) => {
    const box = canvas.getBoundingClientRect();
    for (let y = box.bottom - 1; y > box.top; y -= 3) {
      for (let x = box.left + 1; x < box.right; x += 2) {
        canvas.dispatchEvent(new MouseEvent("click", { clientX: x, clientY: y, bubbles: true }));
      }
    }
  });
  await expect(page).toHaveURL(/st=log/u);
  await expect(page).toHaveURL(/sl=\d+/u);

  const band = page.getByRole("button", { name: "Remove the latency band" });
  await expect(band).toBeVisible();
  const calls = page.getByTestId("syscall-log").getByRole("button");
  await expect(calls.first()).toContainText("openat(");
  for (const shown of await calls.all()) {
    await expect(shown).toContainText(/openat\(|\+\+\+/u);
  }
  await band.click();
  await expect(page).not.toHaveURL(/sl=/u);
  await page.getByRole("button", { name: /^Remove .* – /u }).click();

  await view.getByRole("button", { name: "Summary", exact: true }).click();
  const lanes = page.getByTestId("timeline").getByRole("img");
  const area = await lanes.boundingBox();
  if (!area) {
    throw new Error("the lanes have no box");
  }
  await page.mouse.move(area.x + 2, area.y + 4);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width / 2, area.y + 4, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/u);
});

test("a recording without calls opens the same Kernel pane, saying how to record them", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("off.usprof");
  const recorded = spawnSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "--syscalls", "off", "-o", file, "--", fixture("syscalls"), "files"],
    { cwd: info.outputDir, stdio: "ignore" },
  );
  expect(recorded.status).toBe(3);
  const served = await microscope.start([file]);
  await join(page, served.link);

  await page.getByRole("tab", { name: /^Kernel/u }).click();
  const kernel = page.getByTestId("kernel");
  await expect(kernel.getByRole("tab", { name: "Syscalls" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await expect(page.getByTestId("syscalls-unkept")).toContainText(
    "System calls were not recorded. Record with --syscalls calls",
  );
  for (const tab of ["Summary", "Log", "Files", "Network", "Processes"]) {
    await expect(kernel.getByRole("button", { name: tab, exact: true })).toBeVisible();
  }
  await kernel.getByRole("button", { name: "Log", exact: true }).click();
  await expect(page).toHaveURL(/st=log/u);
  await expect(page.getByTestId("syscalls-unkept")).toBeVisible();

  await expect(page.getByRole("tab", { name: /^Flame graph/u })).toBeVisible();
});

test("calls and samples select each other, and a call's frames lead to the samples' views", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("work.usprof");

  const recorded = spawnSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "--allow-stopping", "-o", file, "--", fixture("syscalls"), "work"],
    { cwd: info.outputDir, stdio: "ignore" },
  );
  expect(recorded.status).toBe(0);
  const served = await microscope.start([file]);
  await join(page, served.link);

  await page.getByRole("tab", { name: /^Kernel/u }).click();
  await page.getByLabel("Selection").fill("trace=clock_nanosleep");
  await page.getByLabel("Selection").press("Enter");
  await page.getByLabel("Group", { exact: true }).selectOption("site");
  await page.getByRole("button", { name: /^Samples during rest · /u }).click();
  await expect(page).toHaveURL(/du=site/u);
  await expect(page).toHaveURL(/sk=1/u);

  await expect(page.getByTestId("syscalls")).toBeVisible();
  await expect(page.getByRole("tab", { name: /^Kernel/u })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  const chip = page.getByRole("button", { name: "Keep samples outside these calls too" });
  await expect(chip).toBeVisible();
  await showView(page, "Functions");
  const functions = page.getByRole("listbox", { name: "Functions" }).getByRole("option");
  await expect(functions.filter({ hasText: "[syscall] clock_nanosleep" }).first()).toBeVisible();
  await expect(functions.filter({ hasText: "rest" }).first()).toBeVisible();
  await expect(functions.filter({ hasText: "burn" })).toHaveCount(0);
  await expect(functions.filter({ hasText: "[syscall] getrandom" })).toHaveCount(0);

  const leaves = async (on: boolean) => {
    await page.getByRole("button", { name: "Options" }).click();
    await page
      .getByRole("dialog", { name: "Options" })
      .getByRole("checkbox", { name: "System calls as leaves" })
      .setChecked(on);
    await page.keyboard.press("Escape");
  };
  await leaves(false);
  await expect(page.getByText(/^No sample falls inside a system call/u)).toBeVisible();
  await leaves(true);
  await expect(functions.filter({ hasText: "rest" }).first()).toBeVisible();

  await page.reload();
  await expect(functions.filter({ hasText: "rest" }).first()).toBeVisible();
  await chip.click();
  await expect(page).not.toHaveURL(/du=/u);
  await expect(functions.filter({ hasText: "burn" }).first()).toBeVisible();
  await expect(functions.filter({ hasText: "[syscall] clock_nanosleep" }).first()).toBeVisible();

  await page.getByRole("tab", { name: /^Kernel/u }).click();
  const view = page.getByTestId("syscalls");
  await view.getByRole("button", { name: "Log", exact: true }).click();
  await page
    .getByTestId("syscall-log")
    .getByRole("button")
    .filter({ hasText: "clock_nanosleep(" })
    .first()
    .click();
  const call = page.getByTestId("syscall-call");
  const frame = call.getByRole("listitem").filter({ has: page.getByText("rest", { exact: true }) });
  await frame.hover();
  await frame.getByRole("button", { name: "rest in the flame graph" }).click();
  await expect(page).toHaveURL(/fn=rest/u);
  await expect(page.getByRole("img", { name: "Flame graph" })).toBeVisible();

  await page.getByRole("tab", { name: /^Kernel/u }).click();
  await frame.hover();
  await frame.getByRole("button", { name: "rest on its source lines" }).click();
  await expect(page).toHaveURL(/ann=lines/u);
  await expect(page.getByRole("button", { name: "Source", exact: true })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  await expect(functions.filter({ hasText: "rest" }).first()).toHaveAttribute(
    "aria-selected",
    "true",
  );
});

test("a family's processes are listed parents first, each with how it ended", async ({
  page,
  microscope,
}, info) => {
  const file = info.outputPath("family.usprof");
  spawnSync(
    path.join(root, "target", "debug", "microscope"),
    ["record", "--allow-stopping", "-o", file, "--", fixture("syscalls"), "family"],
    { cwd: info.outputDir, stdio: "ignore" },
  );
  const served = await microscope.start([file]);
  await join(page, served.link);
  await page.getByRole("tab", { name: /^Kernel/u }).click();
  await page.getByTestId("syscalls").getByRole("button", { name: "Processes" }).click();
  const rows = page.getByTestId("syscall-processes").locator("tbody tr");
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(1)).toContainText("exited with 4");

  const timeline = page.getByTestId("timeline").getByRole("img");
  const box = await timeline.boundingBox();
  if (!box) {
    throw new Error("the timeline has no box");
  }
  const tip = page.getByTestId("zone-tip");
  const found = async () => {
    for (let top = 0; top + 30 <= box.height; top += 30) {
      for (let x = 1; x < box.width; x += 4) {
        await page.mouse.move(box.x + x, box.y + top + 24);
        if ((await tip.count()) > 0 && /waited in wait4/u.test((await tip.textContent()) ?? "")) {
          return true;
        }
      }
    }
    return false;
  };
  expect(await found()).toBe(true);
  await page.mouse.down();
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/u);
});

test("a recording made from the page shows what its program is blocked in, until stopped", async ({
  page,
  microscope,
}) => {
  await join(page, microscope.link);
  await menu(page, "File", "Record a profile…");
  const record = page.getByRole("tabpanel", { name: "Record" });
  await record.getByLabel("Program").fill(fixture("syscalls"));
  await record.getByLabel("Arguments").fill("pause");

  await record.getByLabel("Preset").selectOption("production");
  await expect(record.getByLabel("Stopping")).toBeDisabled();
  await expect(record.getByLabel("Stopping")).toHaveValue("never");
  await record.getByLabel("Preset").selectOption("development");
  await record.getByLabel("Stopping").selectOption("allowed");
  await expect(record.getByLabel("System calls")).toHaveValue("calls");
  await record.getByRole("button", { name: "Record" }).click();
  await expect(page).toHaveURL(/\/profile\/[0-9a-f]{16}/u);
  await expect(page.getByTestId("recording")).toContainText("Recording syscalls");

  await page.getByRole("tab", { name: /^Kernel/u }).click();
  const view = page.getByTestId("syscalls");
  await view.getByRole("button", { name: "Processes", exact: true }).click();
  const blocked = page.getByTestId("syscall-in-flight");
  await expect(blocked.getByRole("row").filter({ hasText: "pause(" })).toBeVisible({
    timeout: 10_000,
  });

  await page.getByRole("button", { name: "Stop" }).click();
  await expect(page.getByTestId("recording")).toContainText("killed by SIGKILL");
  await expect(blocked).toHaveCount(0);
  await expect(page.getByTestId("syscall-processes")).toContainText("killed by SIGKILL");
});
