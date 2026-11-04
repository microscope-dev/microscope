// What an authenticator app does with the key services shows: RFC 6238
// codes, computed here so tests can sign in with them.

import { createHmac } from "node:crypto";

export function base32(text: string): Buffer {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  const bytes: number[] = [];
  let buffer = 0;
  let bits = 0;
  for (const char of text.replaceAll(/\s/gu, "")) {
    buffer = (buffer << 5) | alphabet.indexOf(char);
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      bytes.push((buffer >> bits) & 0xff);
    }
    buffer &= (1 << bits) - 1;
  }
  return Buffer.from(bytes);
}

/** RFC 6238 with SHA-1, six digits, and 30 second steps, `ahead` steps from now. */
export function totp(secret: Buffer, ahead = 0): string {
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30_000) + ahead));
  const hash = createHmac("sha1", secret).update(counter).digest();
  const offset = (hash[19] ?? 0) & 0x0f;
  const value = hash.readUInt32BE(offset) & 0x7fffffff;
  return String(value % 1_000_000).padStart(6, "0");
}

/** A six digit code that no step near now produces. */
export function wrong(secret: Buffer): string {
  const valid = new Set([-1, 0, 1, 2].map((ahead) => totp(secret, ahead)));
  for (let n = 0; ; n++) {
    const code = String(n).padStart(6, "0");
    if (!valid.has(code)) {
      return code;
    }
  }
}
