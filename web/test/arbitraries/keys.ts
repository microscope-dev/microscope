import * as fc from "fast-check";

import type { KeyLike } from "../../src/workbench/keys";

const LETTERS = "abcdefghijklmnopqrstuvwxyz".split("");
const DIGITS = "0123456789".split("");

const NAMED = [
  ...Array.from({ length: 12 }, (_, index) => `F${index + 1}`),
  "Escape",
  "Enter",
  "Tab",
  "Backspace",
  "ArrowLeft",
  "ArrowRight",
  "ArrowUp",
  "ArrowDown",
  "Home",
  "End",
  "PageUp",
  "PageDown",
];

const TYPED = [
  ...LETTERS,
  ...LETTERS.map((letter) => letter.toUpperCase()),
  ...DIGITS,
  ..."`-=[]\\;',./~!@#$%^&*()_+{}|:\"<>? ".split(""),
  ..."ß∂ƒ©˙∆˚¬µ¡™£¢∞§¶•ªº".split(""),
];

export const code = fc.oneof(
  fc.constantFrom(...LETTERS.map((letter) => `Key${letter.toUpperCase()}`)),
  fc.constantFrom(...DIGITS.map((digit) => `Digit${digit}`)),
  fc.constantFrom(...NAMED, "BracketLeft", "BracketRight", "Backslash", "Slash", "Space"),
);

export const keyPress: fc.Arbitrary<KeyLike> = fc.record({
  key: fc.constantFrom(...NAMED, ...TYPED),
  code,
  shiftKey: fc.boolean(),
  ctrlKey: fc.boolean(),
  altKey: fc.boolean(),
  metaKey: fc.boolean(),
});

export const altLetterOrDigit = fc.record({
  letter: fc.constantFrom(...LETTERS, ...DIGITS),
  typed: fc.constantFrom(...TYPED),
  shiftKey: fc.boolean(),
});
