import { readdir, stat } from "node:fs/promises";
import { connect } from "node:net";
import * as path from "node:path";

import { mockIdpBinary, servicesBinary, skipServices } from "./services";

const SKIP = "or set MICROSCOPE_E2E_SKIP_SERVICES=1 to skip the tests that run services";

const servicesRoot = path.dirname(path.dirname(path.dirname(servicesBinary)));

const SOURCES = ["src", "migrations", ".sqlx", ".cargo", "Cargo.toml", "Cargo.lock"];

async function newestSource(): Promise<{ file: string; modified: number }> {
  let newest = { file: "", modified: 0 };
  for (const source of SOURCES) {
    const top = path.join(servicesRoot, source);
    const info = await stat(top).catch(() => null);
    if (!info) {
      continue;
    }
    const files = info.isDirectory()
      ? (await readdir(top, { recursive: true, withFileTypes: true }))
          .filter((entry) => entry.isFile())
          .map((entry) => path.join(entry.parentPath, entry.name))
      : [top];
    for (const file of files) {
      const modified = (await stat(file)).mtimeMs;
      if (modified > newest.modified) {
        newest = { file, modified };
      }
    }
  }
  return newest;
}

async function checkBinaries() {
  const built = [];
  for (const binary of [servicesBinary, mockIdpBinary]) {
    const info = await stat(binary).catch(() => null);
    if (!info) {
      throw new Error(
        `services is not built at ${binary}; run \`just build\` in services, ${SKIP}`,
      );
    }
    built.push(info.mtimeMs);
  }

  if (process.env.MICROSCOPE_SERVICES_BINARY) {
    return;
  }
  const newest = await newestSource();
  if (newest.modified > Math.min(...built)) {
    throw new Error(
      `services at ${servicesBinary} is older than ${path.relative(servicesRoot, newest.file)}; run \`just build\` in services, ${SKIP}`,
    );
  }
}

function reachable(host: string, port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = connect({ host, port, timeout: 2_000 });
    const done = (up: boolean) => {
      socket.destroy();
      resolve(up);
    };
    socket.on("connect", () => done(true));
    socket.on("timeout", () => done(false));
    socket.on("error", () => done(false));
  });
}

async function checkDependencies() {
  const database = new URL(
    process.env.DATABASE_URL ?? "postgresql://postgres:postgres@127.0.0.1:5432/services",
  );
  const storage = new URL(process.env.STORAGE_ENDPOINT ?? "http://127.0.0.1:8333");
  for (const [name, url, port] of [
    ["database", database, 5432],
    ["object storage", storage, 80],
  ] as const) {
    if (!(await reachable(url.hostname, Number(url.port) || port))) {
      throw new Error(
        `services' ${name} is not up at ${url.host}; run \`just up\` in services, ${SKIP}`,
      );
    }
  }
}

export default async function setup() {
  if (skipServices) {
    return;
  }
  await checkBinaries();
  await checkDependencies();
}
