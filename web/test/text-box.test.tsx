// Which focused elements take letter keys instead of the debugger.

import { afterEach, describe, expect, it } from "vitest";
import { isTextBox, keepsEscape } from "../src/keys";

function element(html: string, selector: string): Element | null {
  const holder = document.createElement("div");
  holder.innerHTML = html;
  document.body.append(holder);
  return holder.querySelector(selector);
}

afterEach(() => {
  document.body.replaceChildren();
});

describe("a focused element", () => {
  it("takes letters when it is a box to type in", () => {
    expect(isTextBox(element("<input>", "input"))).toBe(true);
    expect(isTextBox(element('<input type="search">', "input"))).toBe(true);
    expect(isTextBox(element("<textarea></textarea>", "textarea"))).toBe(true);
    expect(isTextBox(element("<select></select>", "select"))).toBe(true);
    expect(isTextBox(element('<div contenteditable="true">x</div>', "div"))).toBe(true);
  });

  it("leaves letters to the debugger when it is not", () => {
    expect(isTextBox(null)).toBe(false);
    for (const type of ["checkbox", "radio", "button", "submit"]) {
      expect(isTextBox(element(`<input type="${type}">`, "input")), type).toBe(false);
    }
    expect(isTextBox(element("<button>go</button>", "button"))).toBe(false);
    expect(isTextBox(element("<div>x</div>", "div"))).toBe(false);
  });

  it("leaves letters to the debugger inside a region that claims them", () => {
    expect(isTextBox(element('<div data-keys="debugger"><input></div>', "input"))).toBe(false);
  });

  it("gives letters to a live drawing, as to a text box", () => {
    expect(isTextBox(element('<div data-keys="renderer"><canvas></canvas></div>', "canvas"))).toBe(
      true,
    );
  });

  it("keeps Escape inside a menu or dialog that closes on it, and only Escape", () => {
    const button = element('<div data-keys="escape"><button>a</button></div>', "button");
    expect(keepsEscape(button)).toBe(true);
    expect(isTextBox(button)).toBe(false);
    expect(keepsEscape(element("<button>a</button>", "button"))).toBe(false);
    expect(keepsEscape(null)).toBe(false);
  });
});
