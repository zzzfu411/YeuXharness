import { describe, expect, it } from "vitest";

import { sanitizeTerminalText } from "../src/terminal.js";

describe("sanitizeTerminalText", () => {
  it("removes terminal escape sequences and unsafe controls", () => {
    const text =
      "plain\u001b[31mred\u001b[0m\u001b]0;owned\u0007done\rnext\b!\u202eevil";

    expect(sanitizeTerminalText(text)).toBe("plainreddone\nnext!evil");
  });

  it("removes C1 control sequences and control strings", () => {
    const text = "a\u009b2Jb\u009dclipboard\u009cc\u0090payload\u009cd";

    expect(sanitizeTerminalText(text)).toBe("abcd");
  });

  it("preserves readable whitespace and neutralizes chunk-split escapes", () => {
    expect(sanitizeTerminalText("one\r\ntwo\tthree")).toBe("one\ntwo\tthree");
    expect(sanitizeTerminalText("\u001b")).toBe("");
    expect(sanitizeTerminalText("[31mtext")).toBe("[31mtext");
  });
});
