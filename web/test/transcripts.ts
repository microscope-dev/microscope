// Recorded traffic between real `microscope web` servers and the Rust test
// clients (tests/web): `just web-transcripts` refreshes it.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import type { Envelope, ServerMessage } from "../src/protocol";

export type Line =
  | { to: "server"; message: Envelope }
  | { to: "page"; message: ServerMessage }
  /** The bytes sent before the answer to request `id`. */
  | { to: "page"; binary: { id: number; hex: string } };

const directory = join(import.meta.dirname, "transcripts");

export function transcriptNames(): string[] {
  return readdirSync(directory)
    .filter((name) => name.endsWith(".jsonl"))
    .map((name) => name.slice(0, -".jsonl".length))
    .toSorted();
}

export function transcript(name: string): Line[] {
  return readFileSync(join(directory, `${name}.jsonl`), "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line) as Line);
}

/** The messages the page received, in order. */
export function received(name: string): ServerMessage[] {
  return transcript(name).flatMap((line) =>
    line.to === "page" && "message" in line ? [line.message] : [],
  );
}

/** The bytes the page received before each answer, by request. */
export function binaries(name: string): Map<number, Uint8Array> {
  const frames = new Map<number, Uint8Array>();
  for (const line of transcript(name)) {
    if (line.to === "page" && "binary" in line) {
      const hex = line.binary.hex;
      const bytes = new Uint8Array(hex.length / 2);
      for (let at = 0; at < bytes.length; at += 1) {
        bytes[at] = Number.parseInt(hex.slice(at * 2, at * 2 + 2), 16);
      }
      frames.set(line.binary.id, bytes);
    }
  }
  return frames;
}
