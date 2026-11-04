import react from "@vitejs/plugin-react";
import { playwright } from "@vitest/browser-playwright";
import type { ProxyOptions } from "vite";
import { defineConfig } from "vitest/config";

import { fromServices, toServices } from "./dev-cookies";

// `just web` serves the page on its own, which talks to microscope at the
// address its join link names, and to the services backend under its own
// `/api/`, proxied here.
const services = process.env.MICROSCOPE_SERVICES_URL ?? "http://127.0.0.1:8080";

// Services' cookies, renamed for plain HTTP (dev-cookies.ts).
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

export default defineConfig({
  plugins: [react()],
  // Files are named relative to the page, so it can be served under any
  // path; the server roots them at its own (`assets::rooted`).
  base: "./",
  build: {
    outDir: "../build/web",
    emptyOutDir: true,
    sourcemap: true,
    // Inlined assets are data: URLs, which the page's CSP refuses.
    assetsInlineLimit: 0,
  },
  server: {
    host: "127.0.0.1",
    port: 3000,
    strictPort: true,
    // Reached by whatever name the machine has on the network, such as
    // http://jlinux1:3000.
    allowedHosts: true,
    proxy: {
      "/api": { target: services, changeOrigin: true, configure: devCookie },
    },
  },
  test: {
    // How many cases each property test tries, for `just web-props`; tests in
    // a browser can't read the environment, so it is handed to them.
    provide: { proptestCases: process.env.PROPTEST_CASES ?? "" },
    // expect.element and expect.poll check this often; their default of 50ms
    // made most browser tests wait at least one interval.
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
        // Tests read the docs and fixtures they check the page against.
        server: { fs: { allow: [".."] } },
        // Bundled before the first test, so no test reloads the page midway.
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
