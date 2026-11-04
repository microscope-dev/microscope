// Splits a line of program arguments as a shell would, without expanding
// anything: quotes group, backslashes escape.

export type Split = { ok: true; words: string[] } | { ok: false; error: string };

export function splitWords(line: string): Split {
  const words: string[] = [];
  let word = "";
  let inWord = false;
  let quote: '"' | "'" | null = null;
  for (let index = 0; index < line.length; index += 1) {
    const char = line[index] as string;
    if (quote === "'") {
      if (char === "'") {
        quote = null;
      } else {
        word += char;
      }
      continue;
    }
    if (char === "\\") {
      const next = line[index + 1];
      if (next === undefined) {
        return { ok: false, error: "a backslash ends the line" };
      }
      // Inside double quotes a backslash escapes only what is special there.
      if (quote === '"' && !'"\\$`'.includes(next)) {
        word += char;
      }
      word += next;
      inWord = true;
      index += 1;
      continue;
    }
    if (quote === '"') {
      if (char === '"') {
        quote = null;
      } else {
        word += char;
      }
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      inWord = true;
    } else if (/\s/u.test(char)) {
      if (inWord) {
        words.push(word);
        word = "";
        inWord = false;
      }
    } else {
      word += char;
      inWord = true;
    }
  }
  if (quote) {
    return { ok: false, error: `a ${quote} quote is not closed` };
  }
  if (inWord) {
    words.push(word);
  }
  return { ok: true, words };
}

/** Writes words back as a line that splits into them again. */
export function joinWords(words: readonly string[]): string {
  return words
    .map((word) =>
      word !== "" && /^[\w@%+=:,./-]+$/u.test(word) ? word : `'${word.replaceAll("'", `'\\''`)}'`,
    )
    .join(" ");
}

/** Parses `NAME=VALUE` lines, skipping blank ones. */
export function parseEnvironment(
  text: string,
): { ok: true; pairs: [string, string][] } | { ok: false; error: string } {
  const pairs: [string, string][] = [];
  for (const [index, raw] of text.split("\n").entries()) {
    const line = raw.trim();
    if (line === "") {
      continue;
    }
    const equals = line.indexOf("=");
    if (equals <= 0) {
      return { ok: false, error: `line ${index + 1} is not NAME=VALUE` };
    }
    pairs.push([line.slice(0, equals), line.slice(equals + 1)]);
  }
  return { ok: true, pairs };
}
