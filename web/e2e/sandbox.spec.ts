// A hostile renderer tries every way out of the sandbox while listeners on
// the machine record what arrives. Every attempt must fail and nothing may
// arrive (dev/web-ui.md, Visualizer sandbox).

import { createSocket } from "node:dgram";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import * as path from "node:path";
import type { Locator, Page } from "@playwright/test";
import { card, drawAt } from "./drawings";
import { expect, expectErrors, fixture, join, type Microscope, test } from "./server";

/** Runs hostile.js, as a picture or live, with listeners recording what
 * arrives; `outcomes` reads what each probe did from the card. */
async function probe(
  page: Page,
  microscope: Microscope,
  browserName: string,
  mode: "picture" | "live",
  outcomes: (drawn: Locator) => Promise<string[]>,
): Promise<void> {
  // Firefox reports the import() its policy blocked.
  if (browserName === "firefox") {
    expectErrors(/Content-Security-Policy: .*import\.js/);
  }
  const arrived: string[] = [];
  const http = createServer((request, response) => {
    arrived.push(`http ${request.method} ${request.url}`);
    response.setHeader("Access-Control-Allow-Origin", "*");
    response.end("x");
  });
  await new Promise<void>((resolve) => http.listen(0, "127.0.0.1", resolve));
  const udp = createSocket("udp4");
  udp.on("message", (message) => arrived.push(`udp ${message.toString("hex").slice(0, 16)}`));
  await new Promise<void>((resolve) => udp.bind(0, "127.0.0.1", resolve));
  const ports = {
    http: (http.address() as AddressInfo).port,
    udp: udp.address().port,
  };
  try {
    const directory = test.info().outputPath("views");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "hostile.views");
    await writeFile(
      views,
      'microscope-views 1\nextend c life {\n    visualize "hostile" { generation = generation }\n}\n',
    );
    const source = await readFile(path.join(import.meta.dirname, "visualizers/hostile.js"), "utf8");
    await writeFile(
      path.join(directory, "hostile.js"),
      source
        .replaceAll("HTTP_PORT", String(ports.http))
        .replaceAll("UDP_PORT", String(ports.udp))
        .replaceAll("MODE", mode),
    );
    const server = await microscope.start(["--views", views, fixture("life")]);
    await join(page, server.link);
    await drawAt(page, "life.c:59");
    const found = await outcomes(card(page, "life"));
    expect(found.length).toBeGreaterThan(15);
    for (const outcome of found) {
      expect(outcome).toMatch(/: (absent|blocked)$/);
    }

    // What the test sends itself arrives, after anything the renderer sent.
    await fetch(`http://127.0.0.1:${ports.http}/control`);
    udp.send("control", ports.udp, "127.0.0.1");
    await expect.poll(() => arrived.length).toBe(2);
    expect(arrived.sort()).toEqual([
      "http GET /control",
      `udp ${Buffer.from("control").toString("hex")}`,
    ]);
  } finally {
    http.close();
    udp.close();
  }
}

test("a hostile renderer reaches nothing outside its sandbox", async ({
  page,
  microscope,
  browserName,
}) => {
  await probe(page, microscope, browserName, "picture", async (drawn) => {
    await expect(drawn.locator(".drawing-caption")).toHaveText("probes done");
    return drawn.locator("svg text").allTextContents();
  });
});

test("a hostile live renderer reaches nothing, with WebGL in use", async ({
  page,
  microscope,
  browserName,
}) => {
  await probe(page, microscope, browserName, "live", async (drawn) => {
    const caption = drawn.locator(".drawing-caption");
    await expect(caption).toHaveText(/ · probes done$/);
    const [kind, ...found] = (await caption.textContent())?.split(" · ") ?? [];
    // Headless Firefox has no WebGL; Chromium's runs on SwiftShader.
    expect(kind).toBe(browserName === "firefox" ? "2d" : "webgl2");
    return found.slice(0, -1);
  });
});
