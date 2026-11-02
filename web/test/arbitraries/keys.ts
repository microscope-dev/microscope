// Key presses as a browser reports them, for property tests of anything that
// reads the keyboard.

import fc from "fast-check";
import type { KeyLike } from "../../src/keys";

const LETTERS = "abcdefghijklmnopqrstuvwxyz".split("");
const DIGITS = "0123456789".split("");

/** The names `KeyboardEvent.key` takes for keys that type nothing. */
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

/** What a key types: letters in both cases, digits, punctuation, and what
 * Alt makes keys type on a Mac. */
const TYPED = [
  ...LETTERS,
  ...LETTERS.map((letter) => letter.toUpperCase()),
  ...DIGITS,
  ..."`-=[]\\;',./~!@#$%^&*()_+{}|:\"<>? ".split(""),
  ..."ß∂ƒ©˙∆˚¬µ¡™£¢∞§¶•ªº".split(""),
];

/** The physical key, as `KeyboardEvent.code`. */
export const code = fc.oneof(
  fc.constantFrom(...LETTERS.map((letter) => `Key${letter.toUpperCase()}`)),
  fc.constantFrom(...DIGITS.map((digit) => `Digit${digit}`)),
  fc.constantFrom(...NAMED, "BracketLeft", "BracketRight", "Backslash", "Slash", "Space"),
);

/** Any press, whose `key` and `code` need not agree, as with Alt on a Mac or
 * a layout other than US. */
export const keyPress: fc.Arbitrary<KeyLike> = fc.record({
  key: fc.constantFrom(...NAMED, ...TYPED),
  code,
  shiftKey: fc.boolean(),
  ctrlKey: fc.boolean(),
  altKey: fc.boolean(),
  metaKey: fc.boolean(),
});

/** A press of a letter or digit key with Alt, typing whatever it types. */
export const altLetterOrDigit = fc.record({
  letter: fc.constantFrom(...LETTERS, ...DIGITS),
  typed: fc.constantFrom(...TYPED),
  shiftKey: fc.boolean(),
});
