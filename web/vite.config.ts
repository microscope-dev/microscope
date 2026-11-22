import solid from "@solidjs/vite-plugin";
import { playwright } from "@vitest/browser-playwright";
import type { ProxyOptions } from "vite";
import { defineConfig } from "vitest/config";

import { fromServices, toServices } from "./dev-cookies.ts";

const services = process.env.MICROSCOPE_SERVICES_URL ?? "http://127.0.0.1:8080";

const devCookie: NonNullable<ProxyOptions["configure"]> = (proxy) => {
  proxy.on("proxyReq", (request) => {
    const cookie = request.getHeader("cookie");
    if (typeof cookie === "string") {
      request.setHeader("cookie", toServices(cookie));
    }
  });
  proxy.on("proxyRes", (response) => {
    response.headers["set-cookie"] = response.headers["set-cookie"]?.map(fromServices);
  });
};

function budgetScale(): number {
  const scale = Number(process.env.MICROSCOPE_TEST_BUDGET_SCALE ?? "1");
  if (!Number.isFinite(scale) || scale < 1) {
    throw new Error(
      `MICROSCOPE_TEST_BUDGET_SCALE must be a number of at least 1, got ${process.env.MICROSCOPE_TEST_BUDGET_SCALE}`,
    );
  }
  return scale;
}

export default defineConfig({
  plugins: [solid()],

  base: "./",
  build: {
    outDir: "../build/web",
    emptyOutDir: true,
    sourcemap: true,

    assetsInlineLimit: 0,
  },
  server: {
    host: "127.0.0.1",
    port: 3000,
    strictPort: true,

    allowedHosts: true,
    proxy: {
      "/api": { target: services, changeOrigin: true, configure: devCookie },
      "/buildid": { target: services, changeOrigin: true, configure: devCookie },
    },
  },
  test: {
    provide: { proptestCases: process.env.PROPTEST_CASES ?? "", budgetScale: budgetScale() },
    // A case search takes as long as its cases take on this machine, so it has
    // no per-test timeout; the CI job's own timeout still ends a hung run.
    ...((process.env.PROPTEST_CASES ?? "") === "" ? {} : { testTimeout: 0 }),

    expect: { poll: { interval: 2 } },
    projects: [
      {
        extends: true,
        test: {
          name: "unit",
          include: ["test/**/*.test.ts"],
          setupFiles: ["test/budget.ts", "test/props.ts"],
          environment: "node",
        },
      },
      {
        extends: true,

        server: { fs: { allow: [".."] } },

        optimizeDeps: { include: ["fast-check"] },
        test: {
          name: "browser",
          include: ["test/**/*.test.tsx"],
          setupFiles: ["test/budget.ts", "test/props.ts", "test/browser-setup.ts"],
          browser: {
            enabled: true,
            headless: true,
            provider: playwright(),
            instances: [{ browser: "chromium" }],
            screenshotFailures: false,
          },
        },
      },
    ],
  },
});
