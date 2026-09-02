import { describe, expect, it } from "vitest";

import {
  ANSI_RESET,
  ASCII_GLYPHS,
  DEFAULT_THEME,
  NOCTURNE_ANSI_TOKENS,
  NOCTURNE_PALETTE,
  PAPER_PALETTE,
  UNICODE_GLYPHS,
  detectTerminalCapabilities,
  glyph,
  paint,
  statusGlyphName,
  statusLabel,
} from "../src/aesthetic.js";

const TRUECOLOR_ENV = {
  TERM: "xterm-256color",
  COLORTERM: "truecolor",
  LANG: "en_US.UTF-8",
} as const;

describe("terminal aesthetics", () => {
  it("uses Paper as the default theme and keeps both glyph sets aligned", () => {
    expect(DEFAULT_THEME).toBe("paper");
    expect(Object.keys(ASCII_GLYPHS).sort()).toEqual(Object.keys(UNICODE_GLYPHS).sort());
  });

  it("detects an interactive truecolor UTF-8 terminal", () => {
    expect(
      detectTerminalCapabilities({
        env: TRUECOLOR_ENV,
        isTTY: true,
        columns: 120,
      }),
    ).toEqual({
      isTTY: true,
      columns: 120,
      colorDepth: "truecolor",
      unicode: true,
      plain: false,
      reducedMotion: false,
    });
  });

  it("treats NO_COLOR as a hard colour and motion ceiling", () => {
    expect(
      detectTerminalCapabilities({
        env: { ...TRUECOLOR_ENV, NO_COLOR: "" },
        isTTY: true,
        color: "truecolor",
        reducedMotion: false,
      }),
    ).toMatchObject({
      colorDepth: "none",
      unicode: true,
      plain: false,
      reducedMotion: true,
    });
  });

  it("forces TERM=dumb to plain monochrome ASCII", () => {
    expect(
      detectTerminalCapabilities({
        env: { TERM: "dumb", LANG: "en_US.UTF-8" },
        isTTY: true,
        color: "truecolor",
        ascii: false,
        plain: false,
        reducedMotion: false,
      }),
    ).toMatchObject({
      colorDepth: "none",
      unicode: false,
      plain: true,
      reducedMotion: true,
    });
  });

  it("keeps pipes and CI in append-only monochrome mode", () => {
    expect(
      detectTerminalCapabilities({
        env: TRUECOLOR_ENV,
        isTTY: false,
        color: true,
      }),
    ).toMatchObject({ colorDepth: "none", plain: true, reducedMotion: true });

    expect(
      detectTerminalCapabilities({
        env: { ...TRUECOLOR_ENV, CI: "true" },
        isTTY: true,
      }),
    ).toMatchObject({ colorDepth: "none", plain: true, reducedMotion: true });
  });

  it("falls back to ASCII for non-UTF-8 locales or explicit requests", () => {
    const localeFallback = detectTerminalCapabilities({
      env: { TERM: "xterm", LANG: "C" },
      isTTY: true,
    });
    const explicitFallback = detectTerminalCapabilities({
      env: TRUECOLOR_ENV,
      isTTY: true,
      ascii: true,
    });

    expect(glyph("completed", localeFallback)).toBe("OK");
    expect(glyph("approvalStart", explicitFallback)).toBe("+-");
    expect(glyph("completed", { unicode: true })).toBe("✓");
  });

  it("paints renderer-owned text from the centralized ANSI tokens", () => {
    const token = NOCTURNE_ANSI_TOKENS.focus;
    expect(paint("signal", "focus", { colorDepth: "truecolor" }, "nocturne")).toBe(
      `\u001b[38;2;${token.rgb.join(";")}msignal${ANSI_RESET}`,
    );
    expect(paint("signal", "focus", { colorDepth: "ansi256" }, "nocturne")).toBe(
      `\u001b[38;5;${token.ansi256}msignal${ANSI_RESET}`,
    );
    expect(paint("signal", "focus", { colorDepth: "none" })).toBe("signal");
    expect(paint("signal", "focus", { colorDepth: "truecolor" }, "mono")).toBe(
      "signal",
    );
  });

  it("keeps Nocturne seal colours distinct from Paper vermillion", () => {
    expect(NOCTURNE_PALETTE.approval).toBe("#D17968");
    expect(NOCTURNE_PALETTE.danger).toBe("#D17968");
    expect(NOCTURNE_ANSI_TOKENS.approval.rgb).toEqual([209, 121, 104]);
    expect(PAPER_PALETTE.approval).toBe("#8C3A2C");
  });

  it("uses dark text tokens for the Paper theme in 256-colour terminals", () => {
    expect(paint("ink", "text", { colorDepth: "ansi256" }, "paper")).toBe(
      `\u001b[38;5;234m\u001b[48;5;188mink${ANSI_RESET}`,
    );
  });

  it("paints a paper background so default dark ink stays readable", () => {
    expect(paint("signal", "focus", { colorDepth: "truecolor" })).toBe(
      `\u001b[38;2;49;86;107m\u001b[48;2;216;211;204msignal${ANSI_RESET}`,
    );
    expect(paint("signal", "focus", { colorDepth: "truecolor" }, "nocturne")).not.toContain(
      "48;2;",
    );
  });

  it("keeps non-terminal turn phases textual when glyphs are secondary", () => {
    expect(statusGlyphName("scheduling")).toBe("accepted");
    expect(statusGlyphName("waiting_for_input")).toBe("approval");
    expect(statusLabel("unknown")).toBe("UNKNOWN · RECONCILIATION REQUIRED");
  });
});
